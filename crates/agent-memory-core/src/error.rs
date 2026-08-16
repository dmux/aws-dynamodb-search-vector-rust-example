//! The single error type the domain speaks.
//!
//! Adapter failures are carried as opaque boxed sources so that no SDK type
//! ever leaks into the domain: `agent-memory-core` must keep compiling with no
//! AWS dependency at all.

/// Opaque adapter failure. Boxed so the domain never names an SDK type.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MemoryError {
    #[error("memory text must not be empty")]
    EmptyText,

    #[error("embedding must not be empty")]
    EmptyEmbedding,

    /// DynamoDB stores vectors as lists of `N`, which cannot represent NaN or
    /// infinity. Rejecting them here means the adapter can serialise without
    /// any fallible conversion.
    #[error("embedding value at index {index} is not finite: {value}")]
    NonFiniteEmbeddingValue { index: usize, value: f32 },

    #[error("embedding has {actual} dimensions, expected {expected}")]
    DimensionMismatch { expected: usize, actual: usize },

    #[error("top_k must be between 1 and {max}, got {requested}")]
    TopKOutOfRange { requested: u32, max: u32 },

    #[error("limit must be between 1 and {max}, got {requested}")]
    PageSizeOutOfRange { requested: u32, max: u32 },

    #[error("distance must be finite, got {0}")]
    NonFiniteDistance(f32),

    #[error("unknown memory kind: {0}")]
    UnknownMemoryKind(String),

    #[error("identifier must not be empty")]
    EmptyIdentifier,

    /// Raised by `update`, which edits and cannot create. `forget` deliberately
    /// does not raise it: deleting something already absent is the outcome the
    /// caller asked for.
    #[error("no memory with id {0}")]
    NotFound(String),

    #[error("repository failure")]
    Repository(#[source] BoxError),

    #[error("embedding provider failure")]
    EmbeddingProvider(#[source] BoxError),
}

impl MemoryError {
    /// Wrap an adapter error as a repository failure.
    pub fn repository(source: impl Into<BoxError>) -> Self {
        Self::Repository(source.into())
    }

    /// Wrap an adapter error as an embedding provider failure.
    pub fn embedding_provider(source: impl Into<BoxError>) -> Self {
        Self::EmbeddingProvider(source.into())
    }

    /// Whether the caller supplied bad input, as opposed to something failing
    /// downstream. Driving adapters use this to pick 4xx over 5xx without
    /// matching on every variant.
    pub fn is_invalid_input(&self) -> bool {
        matches!(
            self,
            Self::EmptyText
                | Self::EmptyEmbedding
                | Self::NonFiniteEmbeddingValue { .. }
                | Self::DimensionMismatch { .. }
                | Self::TopKOutOfRange { .. }
                | Self::PageSizeOutOfRange { .. }
                | Self::NonFiniteDistance(_)
                | Self::UnknownMemoryKind(_)
                | Self::EmptyIdentifier
        )
    }
}
