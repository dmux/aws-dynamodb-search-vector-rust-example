//! Adapter-level errors.
//!
//! These never cross into `agent-memory-core`: they are wrapped as
//! [`MemoryError::Repository`] or [`MemoryError::EmbeddingProvider`] at the
//! port boundary, which keeps the domain free of any SDK vocabulary while still
//! preserving the underlying cause through `#[source]`.

use agent_memory_core::MemoryError;

/// Something is wrong with an item read from, or destined for, DynamoDB.
#[derive(Debug, thiserror::Error)]
pub enum ItemError {
    #[error("item is missing required attribute `{0}`")]
    MissingAttribute(&'static str),

    #[error("attribute `{name}` is not of the expected DynamoDB type `{expected}`")]
    UnexpectedType {
        name: &'static str,
        expected: &'static str,
    },

    #[error("attribute `{name}` is not a valid number: {value}")]
    InvalidNumber { name: &'static str, value: String },

    #[error("timestamps before the Unix epoch cannot be stored")]
    TimestampBeforeEpoch,

    #[error(transparent)]
    Domain(#[from] MemoryError),
}

/// Something went wrong talking to Bedrock.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("bedrock InvokeModel failed for model `{model_id}`")]
    InvokeModel {
        model_id: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("bedrock returned a body that is not valid JSON")]
    MalformedBody(#[source] serde_json::Error),

    #[error("bedrock response is missing the `embedding` array")]
    MissingEmbedding,

    #[error(transparent)]
    Domain(#[from] MemoryError),
}
