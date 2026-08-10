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
            "{}, {}, {}, {ALIAS_TEXT}, {}, {ALIAS_SOURCE}",
            attr::ATTR_USER_ID,
            attr::ATTR_MEMORY_ID,
            attr::ATTR_KIND,
            attr::ATTR_CREATED_AT,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
