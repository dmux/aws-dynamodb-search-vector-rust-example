//! [`MemoryRepository`] backed by a DynamoDB table with a vector index.

use agent_memory_core::{
    Distance, Embedding, Memory, MemoryError, MemoryId, MemoryRepository, ScoredMemory, UserId,
    VectorQuery,
};
use async_trait::async_trait;
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::{AttributeValue, ReturnConsumedCapacity};

use crate::attr;
use crate::error::ItemError;

/// Placeholders and aliases used when building expressions.
///
/// `text` and `source` are DynamoDB reserved words, so they can only appear in
/// a projection through an expression attribute name. `user_id` and `kind` are
/// not reserved and are used directly.
const ALIAS_TEXT: &str = "#text";
const ALIAS_SOURCE: &str = "#source";
const VALUE_USER_ID: &str = ":user_id";
const VALUE_KIND: &str = ":kind";

#[derive(Debug, Clone)]
pub struct DynamoDbMemoryRepository {
    client: Client,
    table_name: String,
    index_name: String,
}

impl DynamoDbMemoryRepository {
    pub fn new(
        client: Client,
        table_name: impl Into<String>,
        index_name: impl Into<String>,
    ) -> Self {
        Self {
            client,
            table_name: table_name.into(),
            index_name: index_name.into(),
        }
    }

    /// Attributes requested from the vector index.
    ///
    /// Only attributes projected into the index can come back, and the vector
    /// attribute is excluded by default — which is what we want, since nothing
    /// downstream needs a 1024-float array.
    fn projection() -> String {
        format!(
            "{}, {}, {}, {ALIAS_TEXT}, {}, {ALIAS_SOURCE}, {}, {}, {}, {}",
            attr::ATTR_USER_ID,
            attr::ATTR_MEMORY_ID,
            attr::ATTR_KIND,
            attr::ATTR_CREATED_AT,
            attr::ATTR_GITHUB_LOGIN,
            attr::ATTR_GITHUB_REPO,
            attr::ATTR_RATING,
            attr::ATTR_ACTIVE,
        )
    }
}

#[async_trait]
impl MemoryRepository for DynamoDbMemoryRepository {
    async fn save(&self, memory: &Memory, embedding: &Embedding) -> Result<(), MemoryError> {
        let item = attr::to_item(memory, embedding).map_err(MemoryError::repository)?;

        self.client
            .put_item()
            .table_name(&self.table_name)
            .set_item(Some(item))
            .send()
            .await
            .map_err(|error| MemoryError::repository(Box::new(error)))?;

        Ok(())
    }

    async fn search(&self, query: &VectorQuery) -> Result<Vec<ScoredMemory>, MemoryError> {
        // The SearchSchema HASH key is mandatory: without it the request is
        // rejected. Inline filters, by contrast, are optional.
        let mut condition = format!("{} = {VALUE_USER_ID}", attr::ATTR_USER_ID);

        let mut request = self
            .client
            .search_vectors()
            .table_name(&self.table_name)
            .index_name(&self.index_name)
            // `search_vector()` APPENDS a single element per call. Passing a
            // 1024-dimension vector one element at a time would work but is a
            // trap; `set_search_vector` takes the whole list at once.
            //
            // Note the shape: a flat Vec<AttributeValue> of N, NOT an
            // AttributeValue::L. Stored vectors use the L wrapper, query
            // vectors never do.
            .set_search_vector(Some(attr::to_query_vector(&query.embedding)))
            .top_k(query.top_k.get() as i32)
            .expression_attribute_values(
                VALUE_USER_ID,
                AttributeValue::S(query.user_id.to_string()),
            )
            .expression_attribute_names(ALIAS_TEXT, attr::ATTR_TEXT)
            .expression_attribute_names(ALIAS_SOURCE, attr::ATTR_SOURCE)
            .projection_expression(Self::projection())
            .return_consumed_capacity(ReturnConsumedCapacity::Total);

        if let Some(kind) = query.kind {
            condition.push_str(&format!(" AND {} = {VALUE_KIND}", attr::ATTR_KIND));
            request = request
                .expression_attribute_values(VALUE_KIND, AttributeValue::S(kind.to_string()));
        }

        let output = request
            .search_condition_expression(condition)
            .send()
            .await
            .map_err(|error| MemoryError::repository(Box::new(error)))?;

        // Vector search is billed per byte processed, so this is the number
        // that predicts the bill. It is logged rather than returned, to keep an
        // infrastructure metric out of the domain port.
        if let Some(bytes) = output
            .consumed_capacity()
            .and_then(|capacity| capacity.vector_search_request_bytes())
        {
            tracing::info!(
                vector_search_request_bytes = bytes,
                index = %self.index_name,
                top_k = query.top_k.get(),
                "vector search completed"
            );
        }

        output
            .search_results()
            .iter()
            .map(|result| {
                let item = result
                    .item()
                    .ok_or_else(|| ItemError::MissingAttribute("item"))
                    .map_err(MemoryError::repository)?;
                let memory = attr::memory_from_item(item).map_err(MemoryError::repository)?;

                // The single place in the system that turns AWS's `Score` into
                // the domain's `Distance`. The index uses COSINE, where a lower
                // score means a closer match, and `Distance` sorts ascending —
                // so this is a straight conversion with no sign flip. Switching
                // the index to DOT_PRODUCT would have to be handled here.
                let distance =
                    Distance::new(result.score() as f32).map_err(MemoryError::repository)?;

                Ok(ScoredMemory { memory, distance })
            })
            .collect()
    }

    async fn delete(&self, user_id: &UserId, memory_id: &MemoryId) -> Result<(), MemoryError> {
        self.client
            .delete_item()
            .table_name(&self.table_name)
            .set_key(Some(attr::key(user_id, memory_id)))
            .send()
            .await
            .map_err(|error| MemoryError::repository(Box::new(error)))?;
        Ok(())
    }

    async fn get(
        &self,
        user_id: &UserId,
        memory_id: &MemoryId,
    ) -> Result<Option<Memory>, MemoryError> {
        let output = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .set_key(Some(attr::key(user_id, memory_id)))
            .send()
            .await
            .map_err(|error| MemoryError::repository(Box::new(error)))?;

        output
            .item()
            .map(|item| attr::memory_from_item(item).map_err(MemoryError::repository))
            .transpose()
    }

    /// One page of a namespace, newest first.
    ///
    /// `scan_index_forward(false)` reverses the sort key, and the sort key is
    /// `memory_id` — which is only "newest first" because ids are generated in
    /// increasing order. That is a real coupling between the id generator and
    /// this query, and the alternative (a GSI on `created_at`) costs a second
    /// copy of every item to fix an ordering nobody has complained about.
    async fn list(
        &self,
        user_id: &UserId,
        query: &agent_memory_core::ListQuery,
    ) -> Result<agent_memory_core::MemoryPage, MemoryError> {
        let mut request = self
            .client
            .query()
            .table_name(&self.table_name)
            .key_condition_expression(format!("{} = {VALUE_USER_ID}", attr::ATTR_USER_ID))
            .expression_attribute_values(VALUE_USER_ID, AttributeValue::S(user_id.to_string()))
            .scan_index_forward(false)
            .limit(query.limit.get() as i32);

        if let Some(cursor) = &query.cursor {
            // The namespace comes from the verified caller, never from the
            // cursor — which is why the cursor can be handed out as an opaque
            // string without becoming a way to read someone else's memories.
            let memory_id =
                MemoryId::new(cursor.as_str()).map_err(|_| MemoryError::EmptyIdentifier)?;
            request = request.set_exclusive_start_key(Some(attr::key(user_id, &memory_id)));
        }

        let filter = build_filter(&query.filter);
        for (placeholder, value) in filter.values {
            request = request.expression_attribute_values(placeholder, value);
        }
        for (placeholder, name) in filter.names {
            request = request.expression_attribute_names(placeholder, name);
        }
        if let Some(expression) = filter.expression {
            request = request.filter_expression(expression);
        }

        let output = request
            .send()
            .await
            .map_err(|error| MemoryError::repository(Box::new(error)))?;

        // Read from `LastEvaluatedKey`, not from how many rows came back. The
        // filter runs after the page is read, so a full namespace can answer a
        // 25-row request with zero matches and still have more to give.
        let next = output
            .last_evaluated_key()
            .and_then(|key| key.get(attr::ATTR_MEMORY_ID))
            .and_then(|value| value.as_s().ok())
            .map(agent_memory_core::Cursor::new)
            .transpose()?;

        let memories = output
            .items()
            .iter()
            .map(|item| attr::memory_from_item(item).map_err(MemoryError::repository))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(agent_memory_core::MemoryPage { memories, next })
    }

    async fn update(
        &self,
        command: agent_memory_core::UpdateCommand,
        new_embedding: Option<&Embedding>,
    ) -> Result<Memory, MemoryError> {
        // Fetch current to apply partial updates cleanly if necessary, or just use UpdateItem
        let current = self.get(&command.user_id, &command.memory_id).await?;
        if let Some(mut existing) = current {
            if let Some(ref text) = command.text {
                existing.text = text.clone();
            }
            if let Some(ref kind) = command.kind {
                existing.kind = *kind;
            }
            // `Some(None)` is "clear it", which is why this is not the same
            // shape as the assignments above. See `UpdateCommand::rating`.
            if let Some(rating) = command.rating {
                existing.rating = rating;
            }
            if let Some(active) = command.active {
                existing.active = active;
            }

            // An `UpdateItem` rather than a `save`: `save` demands an embedding
            // and `get` does not return one, so round-tripping through it would
            // either re-embed unchanged text — paying for it — or write a
            // vector that no longer matches the record.

            let mut update_expression = vec![];
            // Clearing an attribute is `REMOVE`, not `SET … = NULL`: a stored
            // NULL is a value, and it would come back as a rating that exists
            // and is nothing.
            let mut remove_expression: Vec<String> = vec![];
            let mut expr_vals = std::collections::HashMap::new();
            let mut expr_names = std::collections::HashMap::new();

            if let Some(ref text) = command.text {
                update_expression.push(format!("{ALIAS_TEXT} = :new_text"));
                expr_vals.insert(":new_text".to_string(), AttributeValue::S(text.clone()));
                expr_names.insert(ALIAS_TEXT.to_string(), attr::ATTR_TEXT.to_string());
                if let Some(emb) = new_embedding {
                    update_expression.push(format!("{} = :new_emb", attr::ATTR_EMBEDDING));
                    expr_vals.insert(":new_emb".to_string(), attr::to_item_attr(emb));
                }
            }
            if let Some(ref kind) = command.kind {
                update_expression.push(format!("{} = :new_kind", attr::ATTR_KIND));
                expr_vals.insert(":new_kind".to_string(), AttributeValue::S(kind.to_string()));
            }
            match command.rating {
                Some(Some(rating)) => {
                    update_expression.push(format!("{} = :new_rating", attr::ATTR_RATING));
                    expr_vals.insert(
                        ":new_rating".to_string(),
                        AttributeValue::N(rating.to_string()),
                    );
                }
                Some(None) => remove_expression.push(attr::ATTR_RATING.to_string()),
                None => {}
            }
            if let Some(active) = command.active {
                update_expression.push(format!("{} = :new_active", attr::ATTR_ACTIVE));
                expr_vals.insert(":new_active".to_string(), AttributeValue::Bool(active));
            }

            if !update_expression.is_empty() || !remove_expression.is_empty() {
                let mut clauses = Vec::new();
                if !update_expression.is_empty() {
                    clauses.push(format!("SET {}", update_expression.join(", ")));
                }
                if !remove_expression.is_empty() {
                    clauses.push(format!("REMOVE {}", remove_expression.join(", ")));
                }
                let update_expr = clauses.join(" ");
                let mut req = self
                    .client
                    .update_item()
                    .table_name(&self.table_name)
                    .set_key(Some(attr::key(&command.user_id, &command.memory_id)))
                    .update_expression(update_expr);
                // Both maps are sent only when populated: a lone `REMOVE`
                // carries no values, and DynamoDB rejects the empty map rather
                // than ignoring it.
                if !expr_vals.is_empty() {
                    req = req.set_expression_attribute_values(Some(expr_vals));
                }
                if !expr_names.is_empty() {
                    req = req.set_expression_attribute_names(Some(expr_names));
                }
                req.send()
                    .await
                    .map_err(|e| MemoryError::repository(Box::new(e)))?;
            }

            Ok(existing)
        } else {
            Err(MemoryError::NotFound(command.memory_id.to_string()))
        }
    }
}

/// A `FilterExpression` and the placeholders it refers to.
#[derive(Debug, Default)]
struct Filter {
    /// `None` when nothing was narrowed — an empty string is not a valid
    /// expression, and DynamoDB rejects the request rather than ignoring it.
    expression: Option<String>,
    values: Vec<(String, AttributeValue)>,
    names: Vec<(String, String)>,
}

/// Translate a [`MemoryFilter`] into DynamoDB's dialect.
///
/// Every clause is `AND`ed. Note what this costs: a `FilterExpression` is
/// applied after the page is read and billed, so filtering does not save a
/// single read unit — it only saves bandwidth and the caller's attention. The
/// alternative for the fields worth indexing would be a GSI per field, which
/// buys speed with a full copy of the table.
fn build_filter(filter: &agent_memory_core::MemoryFilter) -> Filter {
    let mut built = Filter::default();
    let mut clauses: Vec<String> = Vec::new();

    let mut equals = |attribute: &str, placeholder: &str, value: AttributeValue| {
        clauses.push(format!("{attribute} = {placeholder}"));
        built.values.push((placeholder.to_string(), value));
    };

    if let Some(kind) = filter.kind {
        equals(
            attr::ATTR_KIND,
            ":filter_kind",
            AttributeValue::S(kind.to_string()),
        );
    }
    if let Some(repo) = &filter.github_repo {
        equals(
            attr::ATTR_GITHUB_REPO,
            ":filter_repo",
            AttributeValue::S(repo.clone()),
        );
    }
    if let Some(login) = &filter.github_login {
        equals(
            attr::ATTR_GITHUB_LOGIN,
            ":filter_login",
            AttributeValue::S(login.clone()),
        );
    }
    if let Some(active) = filter.active {
        equals(
            attr::ATTR_ACTIVE,
            ":filter_active",
            AttributeValue::Bool(active),
        );
    }
    if let Some(source) = &filter.source {
        // `source` is a reserved word, so it can only appear through an alias.
        equals(
            ALIAS_SOURCE,
            ":filter_source",
            AttributeValue::S(source.clone()),
        );
        built
            .names
            .push((ALIAS_SOURCE.to_string(), attr::ATTR_SOURCE.to_string()));
    }
    if let Some(floor) = filter.min_rating {
        // `>=` alone would also match items with no `rating` attribute in some
        // readings; `attribute_exists` states the intent that an unrated memory
        // is not a badly rated one.
        clauses.push(format!(
            "(attribute_exists({0}) AND {0} >= :filter_rating)",
            attr::ATTR_RATING
        ));
        built.values.push((
            ":filter_rating".to_string(),
            AttributeValue::N(floor.to_string()),
        ));
    }
    if let Some(needle) = &filter.text_contains {
        // `text` is reserved too. `contains` is a literal, case-sensitive
        // substring — this is a filing aid, not the semantic search, which is
        // what `recall` is for.
        clauses.push(format!("contains({ALIAS_TEXT}, :filter_text)"));
        built.values.push((
            ":filter_text".to_string(),
            AttributeValue::S(needle.clone()),
        ));
        built
            .names
            .push((ALIAS_TEXT.to_string(), attr::ATTR_TEXT.to_string()));
    }

    built.expression = (!clauses.is_empty()).then(|| clauses.join(" AND "));
    built
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_filter_produces_no_expression() {
        // An empty `FilterExpression` is a validation error, not a no-op, so
        // "filter nothing" has to mean sending no expression at all.
        let built = build_filter(&agent_memory_core::MemoryFilter::default());
        assert!(built.expression.is_none());
        assert!(built.values.is_empty());
        assert!(built.names.is_empty());
    }

    #[test]
    fn every_reserved_word_in_a_filter_goes_through_an_alias() {
        // Naming `text` or `source` directly fails the whole request with a
        // validation error — the same trap the projection test guards.
        let built = build_filter(&agent_memory_core::MemoryFilter {
            source: Some("echobrain-macos".to_string()),
            text_contains: Some("hexagonal".to_string()),
            ..Default::default()
        });

        let expression = built.expression.expect("an expression");
        assert!(expression.contains(ALIAS_SOURCE), "{expression}");
        assert!(expression.contains(ALIAS_TEXT), "{expression}");
        assert!(!expression.contains(" source"), "{expression}");
        assert!(!expression.contains("(text"), "{expression}");
        assert_eq!(built.names.len(), 2);
    }

    #[test]
    fn filters_are_combined_with_and() {
        let built = build_filter(&agent_memory_core::MemoryFilter {
            github_repo: Some("dmux/EchoBrain".to_string()),
            active: Some(true),
            ..Default::default()
        });

        let expression = built.expression.expect("an expression");
        assert!(expression.contains(" AND "), "{expression}");
        assert_eq!(built.values.len(), 2);
    }

    #[test]
    fn a_rating_floor_excludes_unrated_memories() {
        let built = build_filter(&agent_memory_core::MemoryFilter {
            min_rating: Some(3),
            ..Default::default()
        });

        let expression = built.expression.expect("an expression");
        assert!(
            expression.contains("attribute_exists"),
            "an unrated memory is not a badly rated one: {expression}"
        );
    }

    #[test]
    fn the_projection_aliases_every_reserved_word() {
        let projection = DynamoDbMemoryRepository::projection();
        // `text` and `source` are DynamoDB reserved words: naming them directly
        // makes the whole request fail with a validation error.
        assert!(projection.contains(ALIAS_TEXT));
        assert!(projection.contains(ALIAS_SOURCE));
        assert!(!projection.contains(", text"));
        assert!(!projection.contains(", source"));
    }

    #[test]
    fn the_projection_never_asks_for_the_embedding() {
        // Pulling a 1024-float array back on every hit would inflate both the
        // response and the billed bytes for no benefit.
        assert!(!DynamoDbMemoryRepository::projection().contains(attr::ATTR_EMBEDDING));
    }
}
