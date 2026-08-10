//! Table and vector-index lifecycle.
//!
//! Used by the integration tests, and mirroring exactly what the Terraform
//! escape hatch does — the Terraform provider has no vector-index support, so
//! the index is created through `UpdateTable` either way. Keeping a Rust
//! implementation next to the shell one means the behaviour can be exercised in
//! a test instead of only in a `terraform apply`.

use std::time::{Duration, Instant};

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, BillingMode, CreateVectorIndexAction, DeleteVectorIndexAction,
    IndexStatus, KeySchemaElement, KeyType, Projection, ProjectionType, ScalarAttributeType,
    SearchSchemaElement, SearchSchemaElementType, TimeToLiveSpecification,
    VectorAttributeDefinition, VectorDistanceFunction, VectorIndex, VectorIndexUpdate,
};

use crate::attr;

/// How the vector index for this project is shaped.
#[derive(Debug, Clone)]
pub struct VectorIndexSpec {
    pub index_name: String,
    pub dimensions: i64,
}

impl VectorIndexSpec {
    pub fn new(index_name: impl Into<String>, dimensions: i64) -> Self {
        Self {
            index_name: index_name.into(),
            dimensions,
        }
    }

    /// `user_id` is both the table partition key and the vector index partition
    /// key. That is the single most consequential choice in this design:
    ///
    /// * a search only ever scans one user's slice of the vector space, and
    ///   since vector search is billed per byte processed, this is what keeps
    ///   the bill proportional to one user rather than to the whole corpus;
    /// * throughput quotas are per partition key value, so capacity grows with
    ///   the number of users instead of being shared;
    /// * an item can never be indexed without it, because it is the table's own
    ///   primary key — which removes the "silently missing from the index"
    ///   failure mode entirely.
    fn search_schema() -> Vec<SearchSchemaElement> {
        vec![
            SearchSchemaElement::builder()
                .attribute_name(attr::ATTR_USER_ID)
                .search_schema_element_type(SearchSchemaElementType::Hash)
                .build(),
            SearchSchemaElement::builder()
                .attribute_name(attr::ATTR_KIND)
                .search_schema_element_type(SearchSchemaElementType::InlineFilter)
                .build(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Only projected attributes can be returned by `SearchVectors`, and the
    /// projection cannot be changed without recreating the index.
    fn projection() -> Projection {
        Projection::builder()
            .projection_type(ProjectionType::Include)
            .non_key_attributes(attr::ATTR_TEXT)
            .non_key_attributes(attr::ATTR_CREATED_AT)
            .non_key_attributes(attr::ATTR_SOURCE)
            .build()
    }

    fn vector_attribute() -> Result<VectorAttributeDefinition, AdminError> {
        VectorAttributeDefinition::builder()
            .attribute_name(attr::ATTR_EMBEDDING)
            .build()
            .map_err(|error| AdminError::BuildRequest(Box::new(error)))
    }

    fn to_create_action(&self) -> Result<CreateVectorIndexAction, AdminError> {
        CreateVectorIndexAction::builder()
            .index_name(&self.index_name)
            .vector_attribute(Self::vector_attribute()?)
            .set_search_schema(Some(Self::search_schema()))
            .projection(Self::projection())
            .dimensions(self.dimensions)
            // COSINE compares direction and ignores magnitude, which is what
            // text embedding models encode meaning in. Lower scores mean closer
            // matches — the convention the domain's `Distance` type enforces.
            .distance_function(VectorDistanceFunction::Cosine)
            .build()
            .map_err(|error| AdminError::BuildRequest(Box::new(error)))
    }

    fn to_index(&self) -> Result<VectorIndex, AdminError> {
        VectorIndex::builder()
            .index_name(&self.index_name)
            .vector_attribute(Self::vector_attribute()?)
            .set_search_schema(Some(Self::search_schema()))
            .projection(Self::projection())
            .dimensions(self.dimensions)
            .distance_function(VectorDistanceFunction::Cosine)
            .build()
            .map_err(|error| AdminError::BuildRequest(Box::new(error)))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("failed to build a DynamoDB request")]
    BuildRequest(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("DynamoDB call failed")]
    Aws(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("vector index `{index_name}` was not searchable within {waited:?}")]
    NotSearchable {
        index_name: String,
        waited: Duration,
    },

    #[error("table `{0}` reports no vector index by that name")]
    IndexMissing(String),
}

/// Create the table with the vector index in a single `CreateTable`.
///
/// Note `PAY_PER_REQUEST`: vector indexes only exist on on-demand tables, and a
/// provisioned table is rejected outright.
pub async fn create_table_with_vector_index(
    client: &Client,
    table_name: &str,
    spec: &VectorIndexSpec,
) -> Result<(), AdminError> {
    let attribute = |name: &str| {
        AttributeDefinition::builder()
            .attribute_name(name)
            .attribute_type(ScalarAttributeType::S)
            .build()
            .map_err(|error| AdminError::BuildRequest(Box::new(error)))
    };
    let key = |name: &str, key_type: KeyType| {
        KeySchemaElement::builder()
            .attribute_name(name)
            .key_type(key_type)
            .build()
            .map_err(|error| AdminError::BuildRequest(Box::new(error)))
    };

    client
        .create_table()
        .table_name(table_name)
        .billing_mode(BillingMode::PayPerRequest)
        .attribute_definitions(attribute(attr::ATTR_USER_ID)?)
        .attribute_definitions(attribute(attr::ATTR_MEMORY_ID)?)
        // `kind` must be declared because the vector index SearchSchema refers
        // to it, exactly as a GSI key attribute would have to be.
        .attribute_definitions(attribute(attr::ATTR_KIND)?)
        .key_schema(key(attr::ATTR_USER_ID, KeyType::Hash)?)
        .key_schema(key(attr::ATTR_MEMORY_ID, KeyType::Range)?)
        .vector_indexes(spec.to_index()?)
        .send()
        .await
        .map_err(|error| AdminError::Aws(Box::new(error)))?;

    Ok(())
}

/// Add a vector index to an existing table.
///
/// This is the path Terraform has to take, because neither the AWS provider,
/// the `awscc` provider nor CloudFormation exposes vector indexes today. The
/// `attribute_definitions` argument is the interesting part: `kind` is only
/// referenced by the vector index SearchSchema, so it has to be registered in
/// the same call. `UpdateTable` documents that parameter in terms of global
/// secondary indexes, and the integration test
/// `adding_a_vector_index_can_register_its_search_schema_attribute` is what
/// proves it also works for a vector index.
pub async fn add_vector_index(
    client: &Client,
    table_name: &str,
    spec: &VectorIndexSpec,
) -> Result<(), AdminError> {
    let kind_attribute = AttributeDefinition::builder()
        .attribute_name(attr::ATTR_KIND)
        .attribute_type(ScalarAttributeType::S)
        .build()
        .map_err(|error| AdminError::BuildRequest(Box::new(error)))?;

    let update = VectorIndexUpdate::builder()
        .create(spec.to_create_action()?)
        .build();

    client
        .update_table()
        .table_name(table_name)
        .attribute_definitions(kind_attribute)
        .vector_index_updates(update)
        .send()
        .await
        .map_err(|error| AdminError::Aws(Box::new(error)))?;

    Ok(())
}

pub async fn delete_vector_index(
    client: &Client,
    table_name: &str,
    index_name: &str,
) -> Result<(), AdminError> {
    let update = VectorIndexUpdate::builder()
        .delete(
            DeleteVectorIndexAction::builder()
                .index_name(index_name)
                .build()
                .map_err(|error| AdminError::BuildRequest(Box::new(error)))?,
        )
        .build();

    client
        .update_table()
        .table_name(table_name)
        .vector_index_updates(update)
        .send()
        .await
        .map_err(|error| AdminError::Aws(Box::new(error)))?;

    Ok(())
}

/// Block until the index will actually answer a search.
///
/// There is no AWS waiter for this, and the obvious substitute is wrong:
/// `wait table-exists` returns as soon as `TableStatus` is `ACTIVE`, which
/// happens while the index is still `CREATING`. Worse, an index added to an
/// existing table goes `ACTIVE` *before* it finishes backfilling, and searching
/// during backfill returns an error. Both conditions have to hold.
pub async fn wait_until_searchable(
    client: &Client,
    table_name: &str,
    index_name: &str,
    timeout: Duration,
) -> Result<(), AdminError> {
    let started = Instant::now();
    let mut interval = Duration::from_secs(2);

    loop {
        let description = client
            .describe_table()
            .table_name(table_name)
            .send()
            .await
            .map_err(|error| AdminError::Aws(Box::new(error)))?;

        let index = description
            .table()
            .map(|table| table.vector_indexes())
            .unwrap_or_default()
            .iter()
            .find(|candidate| candidate.index_name() == Some(index_name));

        match index {
            None => return Err(AdminError::IndexMissing(index_name.to_string())),
            Some(index) => {
                let active = index.index_status() == Some(&IndexStatus::Active);
                // Absent means "not backfilling": DescribeTable omits the flag
                // for an index created as part of CreateTable.
                let backfilling = index.backfilling().unwrap_or(false);
                if active && !backfilling {
                    return Ok(());
                }
                tracing::debug!(
                    index = index_name,
                    status = ?index.index_status(),
                    backfilling,
                    "waiting for the vector index"
                );
            }
        }

        if started.elapsed() >= timeout {
            return Err(AdminError::NotSearchable {
                index_name: index_name.to_string(),
                waited: started.elapsed(),
            });
        }

        tokio::time::sleep(interval).await;
        interval = (interval * 2).min(Duration::from_secs(15));
    }
}

/// Turn on TTL for the expiry attribute.
///
/// When TTL deletes an item it also removes the corresponding entry from the
/// vector index, which is what makes "forgetting" free of any compaction job.
pub async fn enable_ttl(client: &Client, table_name: &str) -> Result<(), AdminError> {
    let specification = TimeToLiveSpecification::builder()
        .enabled(true)
        .attribute_name(attr::ATTR_EXPIRES_AT)
        .build()
        .map_err(|error| AdminError::BuildRequest(Box::new(error)))?;

    client
        .update_time_to_live()
        .table_name(table_name)
        .time_to_live_specification(specification)
        .send()
        .await
        .map_err(|error| AdminError::Aws(Box::new(error)))?;

    Ok(())
}

pub async fn delete_table(client: &Client, table_name: &str) -> Result<(), AdminError> {
    client
        .delete_table()
        .table_name(table_name)
        .send()
        .await
        .map_err(|error| AdminError::Aws(Box::new(error)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_search_schema_puts_the_user_on_the_partition_key() {
        let schema = VectorIndexSpec::search_schema();
        assert_eq!(schema.len(), 2);

        let hash = schema
            .iter()
            .find(|element| element.search_schema_element_type() == &SearchSchemaElementType::Hash)
            .expect("a partition key is required for per-user isolation and cost control");
        assert_eq!(hash.attribute_name(), attr::ATTR_USER_ID);

        let filter = schema
            .iter()
            .find(|element| {
                element.search_schema_element_type() == &SearchSchemaElementType::InlineFilter
            })
            .expect("kind is filtered at the storage layer");
        assert_eq!(filter.attribute_name(), attr::ATTR_KIND);
    }

    #[test]
    fn the_index_projects_what_search_results_need_and_nothing_more() {
        let projection = VectorIndexSpec::projection();
        assert_eq!(projection.projection_type(), Some(&ProjectionType::Include));

        let projected = projection.non_key_attributes();
        assert!(projected.contains(&attr::ATTR_TEXT.to_string()));
        assert!(
            !projected.contains(&attr::ATTR_EMBEDDING.to_string()),
            "the vector attribute is implicit in the index and must not be projected"
        );
    }

    #[test]
    fn the_index_uses_cosine_so_lower_scores_mean_closer_matches() {
        let spec = VectorIndexSpec::new("MemoryIndex", 1024);
        let action = spec.to_create_action().expect("builds");
        assert_eq!(action.distance_function(), &VectorDistanceFunction::Cosine);
        assert_eq!(action.dimensions(), 1024);
    }
}
