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
    pub github_repo: Option<String>,
    pub rating: Option<u8>,
    pub active: bool,
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

/// Where a listing resumes. Opaque to every caller above the repository.
///
/// It carries only the `memory_id` of the last row of the previous page: the
/// namespace is rebuilt from the authenticated caller on the way back in, so a
/// cursor handed to another user addresses nothing of the first user's. That is
/// worth more than it costs — an encoded key containing `user_id` would have to
/// be checked against the caller on every request, and forgetting that check
/// once is a cross-namespace read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(String);

impl Cursor {
    pub fn new(value: impl Into<String>) -> Result<Self, MemoryError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(MemoryError::EmptyIdentifier);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How many rows one page may contain.
///
/// Bounded for the same reason [`TopK`] is: the caller names a number that
/// costs money and time downstream, so the domain decides what is sane rather
/// than trusting a query string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageSize(u32);

impl PageSize {
    pub const MAX: u32 = 100;
    pub const DEFAULT: Self = Self(25);

    pub fn new(value: u32) -> Result<Self, MemoryError> {
        if value == 0 || value > Self::MAX {
            return Err(MemoryError::PageSizeOutOfRange {
                requested: value,
                max: Self::MAX,
            });
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl Default for PageSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Narrowing applied to a listing. Every field is "and".
///
/// These are the memory's own metadata, not its meaning: none of this is a
/// substitute for [`MemoryService::recall`], which is the only thing that
/// searches by what a memory *says*. A filter answers "which memories came from
/// that repository", never "which memories are about hexagonal architecture".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryFilter {
    pub kind: Option<MemoryKind>,
    pub github_repo: Option<String>,
    pub github_login: Option<String>,
    /// Provenance, e.g. `"echobrain-macos"` — which client wrote it.
    pub source: Option<String>,
    pub active: Option<bool>,
    /// Keeps memories rated at least this highly. Unrated ones are excluded:
    /// "at least 3 stars" is a claim about a judgement that was made.
    pub min_rating: Option<u8>,
    /// Substring of the text, matched literally and case-sensitively.
    pub text_contains: Option<String>,
}

impl MemoryFilter {
    /// Whether this narrows anything at all.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// One page of a listing.
#[derive(Debug, Clone)]
pub struct MemoryPage {
    pub memories: Vec<Memory>,
    /// `None` means the end, and it is the **only** thing that does.
    ///
    /// A short page is not the end. DynamoDB applies a filter *after* reading
    /// the page, so a request for 25 rows can come back with three, or with
    /// none at all, and still have thousands behind it. A caller that stops
    /// when `memories.len() < limit` silently truncates the list — which is
    /// exactly the bug an infinite scroll is built to have.
    pub next: Option<Cursor>,
}

/// Request for one page of a user's memories, newest first.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub limit: PageSize,
    /// Where to resume. `None` starts at the beginning.
    pub cursor: Option<Cursor>,
    pub filter: MemoryFilter,
}

/// A partial edit: a field left `None` is one the caller is not touching.
#[derive(Debug, Clone)]
pub struct UpdateCommand {
    pub user_id: UserId,
    pub memory_id: MemoryId,
    pub text: Option<String>,
    pub kind: Option<MemoryKind>,
    pub ttl: Option<Duration>,
    /// Doubly optional because a rating has three fates, not two: `None` leaves
    /// it as it was, `Some(None)` clears it, and `Some(Some(n))` sets it.
    /// Collapsing the first two would make un-rating impossible to express —
    /// the caller says "no rating" and the server hears "no change".
    pub rating: Option<Option<u8>>,
    pub active: Option<bool>,
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

    async fn list(&self, user_id: &UserId, query: &ListQuery) -> Result<MemoryPage, MemoryError>;

    async fn update(&self, command: UpdateCommand) -> Result<Memory, MemoryError>;
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

    async fn list(&self, user_id: &UserId, query: &ListQuery) -> Result<MemoryPage, MemoryError>;

    async fn update(
        &self,
        command: UpdateCommand,
        new_embedding: Option<&Embedding>,
    ) -> Result<Memory, MemoryError>;
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
