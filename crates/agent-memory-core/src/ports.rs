//! The hexagon's boundary.
//!
//! [`MemoryService`] is the **primary (driving) port** — the semantic memory
//! layer itself. Both driving adapters depend only on this trait, which is what
//! lets the MCP server run against a real HTTP backend, against a local
//! DynamoDB-backed service, or against an in-memory fake, without any change to
//! the tool code.
//!
//! The remaining traits are **secondary (driven) ports**: what the domain needs
//! from the outside world.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::error::MemoryError;
use crate::model::{Embedding, Memory, MemoryId, MemoryKind, ScoredMemory, TopK, UserId};

/// Request to store a new memory.
#[derive(Debug, Clone)]
pub struct RememberCommand {
    pub user_id: UserId,
    pub kind: MemoryKind,
    pub text: String,
    /// How long the memory should survive. Enforced by DynamoDB TTL, which also
    /// removes the entry from the vector index when it fires.
    pub ttl: Option<Duration>,
    pub source: Option<String>,
    pub github_login: Option<String>,
}

/// Request to search a user's memories by meaning.
#[derive(Debug, Clone)]
pub struct RecallQuery {
    pub user_id: UserId,
    /// Natural-language query. The service embeds it with the same model used
    /// for storage — mixing models produces meaningless rankings.
    pub text: String,
    pub top_k: TopK,
    /// Optional `INLINE_FILTER` narrowing, applied at the storage layer.
    pub kind: Option<MemoryKind>,
    /// Drop anything farther than this. Because [`crate::model::Distance`] is
    /// "lower is better", this is an upper bound, not a threshold to exceed.
    pub max_distance: Option<crate::model::Distance>,
}

/// Vector-space search request handed to the repository.
///
/// Distinct from [`RecallQuery`] because by this point the text has already
/// been turned into a vector: the repository never embeds anything.
#[derive(Debug, Clone)]
pub struct VectorQuery {
    pub user_id: UserId,
    pub embedding: Embedding,
    pub top_k: TopK,
    pub kind: Option<MemoryKind>,
}

/// The semantic memory layer. This is the primary port.
#[async_trait]
pub trait MemoryService: Send + Sync {
    async fn remember(&self, command: RememberCommand) -> Result<Memory, MemoryError>;

    /// Returns matches ordered best-first, i.e. by ascending distance.
    async fn recall(&self, query: RecallQuery) -> Result<Vec<ScoredMemory>, MemoryError>;

    async fn forget(&self, user_id: &UserId, memory_id: &MemoryId) -> Result<(), MemoryError>;

    async fn get(
        &self,
        user_id: &UserId,
        memory_id: &MemoryId,
    ) -> Result<Option<Memory>, MemoryError>;
}

/// Persistence and similarity search. Driven port.
#[async_trait]
pub trait MemoryRepository: Send + Sync {
    /// Persist a memory together with the vector that indexes it.
    ///
    /// The two travel as separate arguments rather than as one struct so that
    /// the vector cannot outlive the write path — see [`Memory`] for why it is
    /// not a field.
    async fn save(&self, memory: &Memory, embedding: &Embedding) -> Result<(), MemoryError>;

    /// Implementations must return results ordered by ascending distance.
    /// [`crate::service::LocalMemoryService`] re-sorts defensively so the
    /// domain contract holds even if an adapter forgets.
    async fn search(&self, query: &VectorQuery) -> Result<Vec<ScoredMemory>, MemoryError>;

    async fn delete(&self, user_id: &UserId, memory_id: &MemoryId) -> Result<(), MemoryError>;

    async fn get(
        &self,
        user_id: &UserId,
        memory_id: &MemoryId,
    ) -> Result<Option<Memory>, MemoryError>;
}

/// Turns text into vectors. Driven port.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Must match the dimension the vector index was created with.
    fn dimensions(&self) -> usize;

    async fn embed(&self, text: &str) -> Result<Embedding, MemoryError>;
}

/// Wall clock. A port so that `created_at` and `expires_at` are deterministic
/// under test instead of a source of flakes.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

/// Identifier source. A port for the same reason as [`Clock`].
pub trait IdGenerator: Send + Sync {
    fn next_id(&self) -> MemoryId;
}
