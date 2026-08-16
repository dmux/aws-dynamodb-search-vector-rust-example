//! Semantic memory for AI agents — the domain hexagon.
//!
//! This crate has **no AWS dependency, by construction**. It is a separate
//! crate rather than a module precisely so that the compiler enforces the
//! dependency direction: an accidental `use aws_sdk_dynamodb::...` in here
//! fails to build instead of quietly eroding the boundary.
//!
//! # Layout
//!
//! * [`model`] — entities and the newtypes that make vector-search mistakes
//!   unrepresentable.
//! * [`ports`] — [`ports::MemoryService`] (primary/driving port) plus the
//!   driven ports the domain needs.
//! * [`service`] — [`service::LocalMemoryService`], the orchestration.
//! * [`testing`] — in-memory ports, behind the `testing` feature.
//!
//! # Why the newtypes earn their keep
//!
//! A `COSINE` vector index returns a score where **lower means more similar**,
//! which inverts the usual intuition attached to the word "score". Rather than
//! documenting that and hoping, the domain exposes [`model::Distance`], whose
//! [`Ord`] sorts ascending. Ranking and threshold logic therefore cannot be
//! written backwards, and exactly one place — the DynamoDB adapter — ever
//! converts AWS's `Score` into it.

pub mod error;
pub mod model;
pub mod ports;
pub mod service;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use error::{BoxError, MemoryError};
pub use model::{Distance, Embedding, Memory, MemoryId, MemoryKind, ScoredMemory, TopK, UserId};
pub use ports::{
    Clock, Cursor, EmbeddingProvider, IdGenerator, ListQuery, MemoryFilter, MemoryPage,
    MemoryRepository, MemoryService, PageSize, RecallQuery, RememberCommand, UpdateCommand,
    VectorQuery,
};
pub use service::LocalMemoryService;
