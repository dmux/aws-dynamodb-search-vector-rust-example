//! Domain model.
//!
//! The newtypes here exist to make three DynamoDB vector-search mistakes
//! unrepresentable rather than merely documented:
//!
//! * [`Distance`] fixes the direction of similarity once. A `COSINE` index
//!   returns *lower is better*, which is the opposite of what "score" suggests.
//! * [`TopK`] enforces the hard `SearchVectors` ceiling before a caller spends
//!   money on an embedding.
//! * [`Embedding`] rejects values DynamoDB cannot store, so the adapter that
//!   serialises them never needs a fallible conversion.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;
use std::time::SystemTime;

use crate::error::MemoryError;

/// Namespace a memory belongs to.
///
/// In the deployed system this is derived server-side from the caller's IAM
/// principal, never from the request body.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserId(String);

/// Identifier of a single memory within a [`UserId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemoryId(String);

macro_rules! string_newtype {
    ($name:ident) => {
        impl $name {
            /// Reject empty and whitespace-only identifiers, which would
            /// silently create an unreachable partition key.
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

            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_newtype!(UserId);
string_newtype!(MemoryId);

/// What kind of thing a memory records.
///
/// Stored as an `INLINE_FILTER` attribute on the vector index, so filtering by
/// kind happens at the storage layer instead of dragging candidates across the
/// network only to discard them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryKind {
    /// Something objectively true about the user or their world.
    Fact,
    /// A stated preference, taste or working style.
    Preference,
    /// Something that happened, tied to a point in time.
    Episode,
}

impl MemoryKind {
    pub const ALL: [MemoryKind; 3] = [Self::Fact, Self::Preference, Self::Episode];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Preference => "preference",
            Self::Episode => "episode",
        }
    }
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MemoryKind {
    type Err = MemoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fact" => Ok(Self::Fact),
            "preference" => Ok(Self::Preference),
            "episode" => Ok(Self::Episode),
            other => Err(MemoryError::UnknownMemoryKind(other.to_string())),
        }
    }
}

/// How far apart two vectors are: **smaller means more similar**.
///
/// This type is the whole reason the domain does not talk about "scores". A
/// `COSINE` vector index returns 0 for identical direction and 2 for opposite
/// direction, so ranking best-first means sorting *ascending*. Encoding that in
/// the [`Ord`] impl means no use case can get the comparison backwards, and the
/// one place that knows about AWS's `Score` is the DynamoDB adapter.
#[derive(Debug, Clone, Copy)]
pub struct Distance(f32);

impl Distance {
    pub fn new(value: f32) -> Result<Self, MemoryError> {
        if !value.is_finite() {
            return Err(MemoryError::NonFiniteDistance(value));
        }
        Ok(Self(value))
    }

    pub fn get(&self) -> f32 {
        self.0
    }
}

// `total_cmp` gives a total order over the finite values `new` admits, and the
// PartialEq impl is defined through it so that Eq and Ord stay consistent.
impl PartialEq for Distance {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == Ordering::Equal
    }
}

impl Eq for Distance {}

impl Ord for Distance {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl PartialOrd for Distance {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Distance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Number of results to return from a similarity search.
///
/// The 1..=100 bound is a hard DynamoDB `SearchVectors` quota rather than a
/// domain rule. It lives here on purpose: validating at the edge rejects a bad
/// request *before* the caller pays for an embedding, whereas discovering it in
/// the adapter would mean paying first and failing afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TopK(u32);

impl TopK {
    /// Hard limit from the DynamoDB service quotas.
    pub const MAX: u32 = 100;
    /// Sensible default for conversational recall.
    pub const DEFAULT: TopK = TopK(5);

    pub fn new(requested: u32) -> Result<Self, MemoryError> {
        if requested == 0 || requested > Self::MAX {
            return Err(MemoryError::TopKOutOfRange {
                requested,
                max: Self::MAX,
            });
        }
        Ok(Self(requested))
    }

    pub fn get(&self) -> u32 {
        self.0
    }
}

impl Default for TopK {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A validated embedding vector.
///
/// Values are `f32` because that is the precision the vector index stores;
/// truncating on the way in keeps the base table and the index in agreement
/// instead of letting them drift silently.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding(Vec<f32>);

impl Embedding {
    pub fn new(values: Vec<f32>) -> Result<Self, MemoryError> {
        if values.is_empty() {
            return Err(MemoryError::EmptyEmbedding);
        }
        if let Some((index, value)) = values
            .iter()
            .enumerate()
            .find(|(_, value)| !value.is_finite())
        {
            return Err(MemoryError::NonFiniteEmbeddingValue {
                index,
                value: *value,
            });
        }
        Ok(Self(values))
    }

    /// Same as [`Embedding::new`] plus a dimension check against the index.
    ///
    /// A vector whose length does not match the index is rejected by DynamoDB
    /// on write, and produces meaningless results on search.
    pub fn with_dimensions(values: Vec<f32>, expected: usize) -> Result<Self, MemoryError> {
        if values.len() != expected {
            return Err(MemoryError::DimensionMismatch {
                expected,
                actual: values.len(),
            });
        }
        Self::new(values)
    }

    pub fn as_slice(&self) -> &[f32] {
        &self.0
    }

    pub fn dimensions(&self) -> usize {
        self.0.len()
    }
}

/// One stored memory.
///
/// Note what is *not* here: the embedding. A vector is a storage detail derived
/// from `text`, and `SearchVectors` excludes it from results by default because
/// it is large and nobody reading a memory needs it. Modelling it as a field
/// would force every read path either to project a 1024-float array it will
/// discard, or to carry an `Option` that is always `None` outside the write
/// path. Instead the vector travels beside the memory only where it is needed:
/// [`crate::ports::MemoryRepository::save`].
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub user_id: UserId,
    pub memory_id: MemoryId,
    pub kind: MemoryKind,
    pub text: String,
    pub created_at: SystemTime,
    /// When set, DynamoDB TTL deletes the item and removes it from the vector
    /// index automatically.
    pub expires_at: Option<SystemTime>,
    /// Free-form provenance, e.g. the conversation or tool that produced it.
    pub source: Option<String>,
    /// Human-readable attribution only. Never consulted for authorization.
    pub github_login: Option<String>,
}

/// A memory together with how close it was to the search vector.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredMemory {
    pub memory: Memory,
    pub distance: Distance,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_orders_ascending_so_the_best_match_sorts_first() {
        let mut distances = [
            Distance::new(0.9).expect("finite"),
            Distance::new(0.1).expect("finite"),
            Distance::new(0.5).expect("finite"),
        ];
        distances.sort();
        assert_eq!(distances[0].get(), 0.1, "smallest distance must rank first");
        assert_eq!(distances[2].get(), 0.9);
    }

    #[test]
    fn distance_accepts_negative_values() {
        // DOT_PRODUCT indexes can score below zero. Even though this project
        // uses COSINE, the type must not assume non-negative input.
        assert!(Distance::new(-1.0).is_ok());
    }

    #[test]
    fn distance_rejects_non_finite_values() {
        assert!(matches!(
            Distance::new(f32::NAN),
            Err(MemoryError::NonFiniteDistance(_))
        ));
    }

    #[test]
    fn top_k_rejects_zero_and_values_above_the_dynamodb_limit() {
        assert!(matches!(
            TopK::new(0),
            Err(MemoryError::TopKOutOfRange { requested: 0, .. })
        ));
        assert!(matches!(
            TopK::new(101),
            Err(MemoryError::TopKOutOfRange {
                requested: 101,
                max: 100
            })
        ));
        assert_eq!(TopK::new(100).expect("at the limit").get(), 100);
    }

    #[test]
    fn embedding_rejects_values_dynamodb_cannot_store() {
        assert!(matches!(
            Embedding::new(vec![0.1, f32::NAN]),
            Err(MemoryError::NonFiniteEmbeddingValue { index: 1, .. })
        ));
        assert!(matches!(
            Embedding::new(vec![f32::INFINITY]),
            Err(MemoryError::NonFiniteEmbeddingValue { index: 0, .. })
        ));
        assert!(matches!(
            Embedding::new(Vec::new()),
            Err(MemoryError::EmptyEmbedding)
        ));
    }

    #[test]
    fn embedding_rejects_a_dimension_the_index_would_refuse() {
        assert!(matches!(
            Embedding::with_dimensions(vec![0.1, 0.2], 1024),
            Err(MemoryError::DimensionMismatch {
                expected: 1024,
                actual: 2
            })
        ));
    }

    #[test]
    fn memory_kind_round_trips_through_its_wire_form() {
        for kind in MemoryKind::ALL {
            assert_eq!(
                kind.as_str().parse::<MemoryKind>().expect("known kind"),
                kind
            );
        }
        assert!(matches!(
            "  PREFERENCE ".parse::<MemoryKind>(),
            Ok(MemoryKind::Preference)
        ));
        assert!(matches!(
            "sonho".parse::<MemoryKind>(),
            Err(MemoryError::UnknownMemoryKind(_))
        ));
    }

    #[test]
    fn identifiers_reject_blank_input() {
        assert!(matches!(
            UserId::new("   "),
            Err(MemoryError::EmptyIdentifier)
        ));
        assert!(MemoryId::new("m-1").is_ok());
    }
}
