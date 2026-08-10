//! In-memory implementations of every driven port.
//!
//! These exist so the domain and both driving adapters can be tested with no
//! network, no AWS account and no Bedrock spend. Production deliberately ships
//! only the real Bedrock embedder; the substitute lives here, behind the
//! `testing` feature, and never reaches a deployed binary.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::SystemTime;

use async_trait::async_trait;

use crate::error::MemoryError;
use crate::model::{Distance, Embedding, Memory, MemoryId, ScoredMemory};
use crate::ports::{Clock, EmbeddingProvider, IdGenerator, MemoryRepository, VectorQuery};

/// A clock frozen at a chosen instant.
#[derive(Debug, Clone)]
pub struct FixedClock(SystemTime);

impl FixedClock {
    pub fn new(instant: SystemTime) -> Self {
        Self(instant)
    }
}

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

/// Predictable identifiers: `mem-1`, `mem-2`, ...
#[derive(Debug, Default)]
pub struct SeqIdGenerator {
    counter: AtomicU64,
}

impl IdGenerator for SeqIdGenerator {
    fn next_id(&self) -> MemoryId {
        let next = self.counter.fetch_add(1, AtomicOrdering::Relaxed) + 1;
        // The format is fixed and non-empty, so construction cannot fail.
        MemoryId::new(format!("mem-{next}")).unwrap_or_else(|_| unreachable!())
    }
}

/// Deterministic hashing embedder.
///
/// Not a language model: it hashes tokens into buckets and L2-normalises. That
/// is enough for tests that care about *ordering* — documents sharing words end
/// up closer — while staying reproducible and free.
#[derive(Debug)]
pub struct StubEmbedder {
    /// Length of the vectors actually produced.
    produced_dimensions: usize,
    /// Length this embedder claims through [`EmbeddingProvider::dimensions`].
    /// Normally identical to `produced_dimensions`; they are separated so a
    /// test can simulate a misconfigured provider.
    reported_dimensions: usize,
    calls: AtomicU64,
}

impl StubEmbedder {
    pub fn new(dimensions: usize) -> Self {
        Self {
            produced_dimensions: dimensions,
            reported_dimensions: dimensions,
            calls: AtomicU64::new(0),
        }
    }

    /// Make [`EmbeddingProvider::dimensions`] disagree with the vectors this
    /// embedder actually produces, to exercise mismatch handling.
    pub fn claiming_dimensions(mut self, reported: usize) -> Self {
        self.reported_dimensions = reported;
        self
    }

    /// How many times [`EmbeddingProvider::embed`] has been called. Used to
    /// assert that validation short-circuits before any paid call.
    pub fn calls(&self) -> u64 {
        self.calls.load(AtomicOrdering::Relaxed)
    }
}

#[async_trait]
impl EmbeddingProvider for StubEmbedder {
    fn dimensions(&self) -> usize {
        self.reported_dimensions
    }

    async fn embed(&self, text: &str) -> Result<Embedding, MemoryError> {
        self.calls.fetch_add(1, AtomicOrdering::Relaxed);

        let mut values = vec![0f32; self.produced_dimensions];
        for token in text
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|token| !token.is_empty())
        {
            let hash = fnv1a(token);
            let bucket = (hash % self.produced_dimensions as u64) as usize;
            let sign = if hash & (1 << 63) == 0 { 1.0 } else { -1.0 };
            values[bucket] += sign;
        }

        let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut values {
                *value /= norm;
            }
        } else {
            // An all-zero vector has no direction, which makes cosine distance
            // undefined. Point it somewhere fixed instead.
            values[0] = 1.0;
        }

        Embedding::new(values)
    }
}

fn fnv1a(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// A repository backed by a vector, scoring with cosine distance so it matches
/// what a `COSINE` DynamoDB vector index would rank.
#[derive(Debug, Default)]
pub struct InMemoryRepository {
    /// Memory alongside the vector that indexes it, mirroring how the real
    /// table stores the embedding as an attribute rather than as part of the
    /// domain entity.
    items: Mutex<Vec<(Memory, Embedding)>>,
}

impl InMemoryRepository {
    pub fn len(&self) -> usize {
        self.items().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read the contents, recovering from a poisoned lock rather than panicking:
    /// a test that already failed should not cascade into unrelated ones.
    fn items(&self) -> std::sync::MutexGuard<'_, Vec<(Memory, Embedding)>> {
        self.items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl MemoryRepository for InMemoryRepository {
    async fn save(&self, memory: &Memory, embedding: &Embedding) -> Result<(), MemoryError> {
        let mut items = self.items();
        items.retain(|(existing, _)| {
            existing.user_id != memory.user_id || existing.memory_id != memory.memory_id
        });
        items.push((memory.clone(), embedding.clone()));
        Ok(())
    }

    async fn search(&self, query: &VectorQuery) -> Result<Vec<ScoredMemory>, MemoryError> {
        let mut hits = self
            .items()
            .iter()
            .filter(|(memory, _)| memory.user_id == query.user_id)
            .filter(|(memory, _)| query.kind.is_none_or(|kind| memory.kind == kind))
            .map(|(memory, embedding)| {
                let distance = cosine_distance(query.embedding.as_slice(), embedding.as_slice());
                Distance::new(distance).map(|distance| ScoredMemory {
                    memory: memory.clone(),
                    distance,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        hits.sort_by_key(|hit| hit.distance);
        hits.truncate(query.top_k.get() as usize);
        Ok(hits)
    }

    async fn delete(
        &self,
        user_id: &crate::model::UserId,
        memory_id: &MemoryId,
    ) -> Result<(), MemoryError> {
        self.items()
            .retain(|(memory, _)| memory.user_id != *user_id || memory.memory_id != *memory_id);
        Ok(())
    }

    async fn get(
        &self,
        user_id: &crate::model::UserId,
        memory_id: &MemoryId,
    ) -> Result<Option<Memory>, MemoryError> {
        Ok(self
            .items()
            .iter()
            .find(|(memory, _)| memory.user_id == *user_id && memory.memory_id == *memory_id)
            .map(|(memory, _)| memory.clone()))
    }
}

/// Cosine distance: `1 - cosine similarity`, matching the `COSINE` distance
/// function of a DynamoDB vector index (0 for identical direction, 2 for
/// opposite).
fn cosine_distance(left: &[f32], right: &[f32]) -> f32 {
    let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
    let left_norm = left.iter().map(|v| v * v).sum::<f32>().sqrt();
    let right_norm = right.iter().map(|v| v * v).sum::<f32>().sqrt();
    if left_norm == 0.0 || right_norm == 0.0 {
        return 1.0;
    }
    1.0 - (dot / (left_norm * right_norm))
}
