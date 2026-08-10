//! The HTTP wire format, owned by neither side.
//!
//! The Lambda serialises these types and the client deserialises them. Because
//! both depend on this crate rather than on hand-written mirror structs, a
//! field renamed on one side is a compile error on the other instead of a
//! runtime deserialisation failure discovered in production.
//!
//! Deliberately free of `agent-memory-core`: the wire format must be able to
//! evolve for compatibility reasons without dragging domain invariants along,
//! and vice versa. Translation happens at each edge.
//!
//! # `user_id` is absent on purpose
//!
//! No request carries the caller's identity. The Lambda derives it from the
//! IAM principal that signed the request, so a client cannot address another
//! user's memories by editing a payload. Responses do echo the resolved
//! `user_id` back, which is what the smoke test asserts on.

use serde::{Deserialize, Serialize};

/// `POST /memories`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RememberRequest {
    pub kind: String,
    pub text: String,
    /// Optional lifetime in seconds, enforced by DynamoDB TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Attribution label only; never used to authorize anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_login: Option<String>,
}

/// `POST /memories/search`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecallRequest {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Upper bound on distance. Smaller is more similar, so this excludes weak
    /// matches rather than requiring a minimum score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_distance: Option<f32>,
}

/// A memory as returned to clients. The embedding is intentionally omitted: it
/// is large, and no caller needs it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryView {
    pub user_id: String,
    pub memory_id: String,
    pub kind: String,
    pub text: String,
    /// Unix seconds.
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_login: Option<String>,
}

/// A search hit. `distance` follows the domain convention: lower is closer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScoredMemoryView {
    #[serde(flatten)]
    pub memory: MemoryView,
    pub distance: f32,
}

/// Search results.
///
/// DynamoDB reports `VectorSearchRequestBytes` on every search, and since
/// vector search is billed per byte processed that number is the one that
/// predicts the bill. It is deliberately *not* returned here: surfacing it
/// would mean threading an infrastructure metric through the domain port. The
/// repository logs it as a structured `tracing` field instead, which puts it in
/// CloudWatch where cost analysis actually happens.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecallResponse {
    pub results: Vec<ScoredMemoryView>,
}

/// Stable error envelope. Adapters map domain errors into this rather than
/// leaking SDK `Debug` output to callers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorBody {
    /// Machine-readable, stable across releases.
    pub code: String,
    /// Human-readable; may change.
    pub message: String,
}

impl ErrorResponse {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_request_omits_absent_optionals() {
        let request = RememberRequest {
            kind: "preference".into(),
            text: "I prefer pour-over coffee".into(),
            ttl_seconds: None,
            source: None,
            github_login: None,
        };
        let json = serde_json::to_string(&request).expect("serialises");
        assert_eq!(
            json,
            r#"{"kind":"preference","text":"I prefer pour-over coffee"}"#
        );
    }

    #[test]
    fn remember_request_accepts_a_minimal_payload() {
        let request: RememberRequest =
            serde_json::from_str(r#"{"kind":"fact","text":"I live in Lisbon"}"#)
                .expect("deserialises");
        assert_eq!(request.ttl_seconds, None);
        assert_eq!(request.text, "I live in Lisbon");
    }

    #[test]
    fn a_client_supplied_user_id_is_simply_not_part_of_the_contract() {
        // Extra fields are ignored, so a caller cannot smuggle an identity in.
        let request: RememberRequest =
            serde_json::from_str(r#"{"kind":"fact","text":"x","user_id":"someone-else"}"#)
                .expect("deserialises");
        assert_eq!(request.text, "x");
    }

    #[test]
    fn scored_memory_flattens_the_memory_alongside_its_distance() {
        let hit = ScoredMemoryView {
            memory: MemoryView {
                user_id: "aws:123:user/alice".into(),
                memory_id: "mem-1".into(),
                kind: "fact".into(),
                text: "hello".into(),
                created_at: 1_700_000_000,
                expires_at: None,
                source: None,
                github_login: None,
            },
            distance: 0.25,
        };

        let json = serde_json::to_value(&hit).expect("serialises");
        assert_eq!(
            json["memory_id"], "mem-1",
            "memory fields must be flattened"
        );
        assert_eq!(json["distance"], 0.25);

        let parsed: ScoredMemoryView = serde_json::from_value(json).expect("round-trips");
        assert_eq!(parsed, hit);
    }

    #[test]
    fn every_dto_round_trips() {
        let recall = RecallRequest {
            query: "what coffee do I like?".into(),
            top_k: Some(5),
            kind: Some("preference".into()),
            max_distance: Some(0.8),
        };
        let parsed: RecallRequest =
            serde_json::from_str(&serde_json::to_string(&recall).expect("serialises"))
                .expect("deserialises");
        assert_eq!(parsed, recall);

        let error = ErrorResponse::new("invalid_input", "top_k must be between 1 and 100");
        let parsed: ErrorResponse =
            serde_json::from_str(&serde_json::to_string(&error).expect("serialises"))
                .expect("deserialises");
        assert_eq!(parsed, error);
    }
}
