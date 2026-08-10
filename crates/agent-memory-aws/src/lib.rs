//! Driven adapters: DynamoDB vector search and Bedrock Titan embeddings.
//!
//! Everything AWS-shaped lives here. `agent-memory-core` depends on none of it,
//! and the crate split is what makes that a compile-time guarantee rather than
//! a convention.
//!
//! # Reading order
//!
//! Start with [`attr`]. It holds the one genuinely surprising detail of the
//! DynamoDB vector API — a vector has a different shape when stored on an item
//! than when passed to `SearchVectors` — and confining that to a single module
//! is why the rest of the code can stay unremarkable.

pub mod admin;
pub mod attr;
pub mod bedrock;
pub mod error;
pub mod repository;
pub mod system;

pub use bedrock::{BedrockEmbedder, TITAN_EMBED_TEXT_V2};
pub use error::{EmbeddingError, ItemError};
pub use repository::DynamoDbMemoryRepository;
pub use system::{SystemClock, UuidGenerator};
