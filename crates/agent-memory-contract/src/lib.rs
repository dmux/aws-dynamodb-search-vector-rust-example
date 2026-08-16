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

use serde::{Deserialize, Deserializer, Serialize};

/// Deserialize a field that has to tell "absent" from "explicitly null".
///
/// `Option<Option<T>>` alone does not do it: serde's own `Option` impl maps a
/// JSON `null` onto the *outer* `None`, so a null and a missing key arrive
/// identically. Only `#[serde(default, deserialize_with = "double_option")]`
/// separates them — the default supplies `None` when the key is absent, and
/// this wraps whatever was actually present in `Some`.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    #[serde(default = "default_true")]
    pub active: bool,
}

fn default_true() -> bool {
    true
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_repo: Option<String>,
    pub rating: Option<u8>,
    pub active: bool,
}

/// A search hit. `distance` follows the domain convention: lower is closer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScoredMemoryView {
    #[serde(flatten)]
    pub memory: MemoryView,
    pub distance: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ListMemoriesResponse {
    pub memories: Vec<MemoryView>,
    /// Pass back as `?cursor=` for the next page. Absent means the end — and it
    /// is the only thing that does.
    ///
    /// A short page is not the end: the filter is applied after the page is
    /// read, so a page can arrive empty with thousands of rows still behind it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// A partial edit. An absent field is one the caller is not touching.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateMemoryRequest {
    pub text: Option<String>,
    pub kind: Option<String>,
    pub ttl_seconds: Option<u64>,
    /// The one field where absent and `null` mean different things: absent
    /// leaves the rating alone, `null` clears it. See [`double_option`] for why
    /// the plain type is not enough — and `skip_serializing_if` for the other
    /// half, without which every request would send an explicit `null` and
    /// erase the rating on any update that never mentioned it.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub rating: Option<Option<u8>>,
    pub active: Option<bool>,
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
    fn an_absent_rating_and_a_null_rating_are_different_requests() {
        // The whole reason `rating` is doubly optional. If these two parsed the
        // same, un-rating a memory would be impossible to say over HTTP and the
        // star would come back on the next read.
        let absent: UpdateMemoryRequest =
            serde_json::from_str(r#"{"text":null,"kind":null,"ttl_seconds":null,"active":null}"#)
                .expect("valid");
        assert_eq!(absent.rating, None, "absent means leave it alone");

        let cleared: UpdateMemoryRequest = serde_json::from_str(
            r#"{"text":null,"kind":null,"ttl_seconds":null,"rating":null,"active":null}"#,
        )
        .expect("valid");
        assert_eq!(cleared.rating, Some(None), "null means clear it");

        let set: UpdateMemoryRequest = serde_json::from_str(
            r#"{"text":null,"kind":null,"ttl_seconds":null,"rating":4,"active":null}"#,
        )
        .expect("valid");
        assert_eq!(set.rating, Some(Some(4)));
    }

    #[test]
    fn an_update_that_does_not_mention_the_rating_does_not_send_one() {
        // Without `skip_serializing_if`, this would serialise `"rating":null` —
        // and the receiver, correctly, would read that as "clear it". Every
        // edit of a memory's text would silently erase its rating.
        let request = UpdateMemoryRequest {
            text: Some("texto novo".to_string()),
            kind: None,
            ttl_seconds: None,
            rating: None,
            active: None,
        };

        let json = serde_json::to_string(&request).expect("serialises");
        assert!(!json.contains("rating"), "{json}");

        let clearing = UpdateMemoryRequest {
            rating: Some(None),
            ..request
        };
        assert!(
            serde_json::to_string(&clearing)
                .expect("serialises")
                .contains(r#""rating":null"#)
        );
    }

    #[test]
    fn remember_request_omits_absent_optionals() {
        let request = RememberRequest {
            kind: "preference".into(),
            text: "I prefer pour-over coffee".into(),
            ttl_seconds: None,
            source: None,
            github_login: None,
            github_repo: None,
            rating: None,
            active: true,
        };
        let json = serde_json::to_string(&request).expect("serialises");
        assert_eq!(
            json,
            r#"{"kind":"preference","text":"I prefer pour-over coffee","active":true}"#
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
                github_repo: None,
                rating: None,
                active: true,
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
