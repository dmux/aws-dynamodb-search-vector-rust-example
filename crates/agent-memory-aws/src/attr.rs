//! Translation between domain types and DynamoDB `AttributeValue`s.
//!
//! # The one thing to get right here
//!
//! A vector has **two different shapes** in this API, and mixing them up is not
//! a type error — it is a runtime rejection or, worse, a silently wrong result:
//!
//! * **Stored on an item** it is a DynamoDB list: `{"L": [{"N": "0.1"}, ...]}`.
//! * **Sent to `SearchVectors`** it is a *flat* list of numbers with **no `L`
//!   wrapper**: `[{"N": "0.1"}, ...]`.
//!
//! [`to_item_attr`] and [`to_query_vector`] are the only two places that build
//! either shape, and they sit next to each other so the difference is visible.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use agent_memory_core::{Embedding, Memory, MemoryId, MemoryKind, UserId};
use aws_sdk_dynamodb::types::AttributeValue;

use crate::error::ItemError;

pub const ATTR_USER_ID: &str = "user_id";
pub const ATTR_MEMORY_ID: &str = "memory_id";
pub const ATTR_KIND: &str = "kind";
pub const ATTR_TEXT: &str = "text";
pub const ATTR_EMBEDDING: &str = "embedding";
pub const ATTR_CREATED_AT: &str = "created_at";
pub const ATTR_EXPIRES_AT: &str = "expires_at";
pub const ATTR_SOURCE: &str = "source";
pub const ATTR_GITHUB_LOGIN: &str = "github_login";
pub const ATTR_GITHUB_REPO: &str = "github_repo";
pub const ATTR_RATING: &str = "rating";
pub const ATTR_ACTIVE: &str = "active";

/// The vector **as stored on an item**: a DynamoDB list (`L`) of numbers (`N`).
pub fn to_item_attr(embedding: &Embedding) -> AttributeValue {
    AttributeValue::L(embedding.as_slice().iter().map(number).collect())
}

/// The vector **as sent to `SearchVectors`**: a flat list, no `L` wrapper.
///
/// Wrapping this in `AttributeValue::L` is the single easiest mistake to make
/// against this API, which is why it lives beside [`to_item_attr`].
pub fn to_query_vector(embedding: &Embedding) -> Vec<AttributeValue> {
    embedding.as_slice().iter().map(number).collect()
}

/// Numbers are infallible here because [`Embedding`] already rejected NaN and
/// infinity, which DynamoDB's `N` type cannot represent. The domain invariant
/// is what removes the `Result` from this conversion.
fn number(value: &f32) -> AttributeValue {
    AttributeValue::N(value.to_string())
}

/// Build the full item written by `PutItem`.
pub fn to_item(
    memory: &Memory,
    embedding: &Embedding,
) -> Result<HashMap<String, AttributeValue>, ItemError> {
    let mut item = HashMap::from([
        (
            ATTR_USER_ID.to_string(),
            AttributeValue::S(memory.user_id.to_string()),
        ),
        (
            ATTR_MEMORY_ID.to_string(),
            AttributeValue::S(memory.memory_id.to_string()),
        ),
        (
            ATTR_KIND.to_string(),
            AttributeValue::S(memory.kind.to_string()),
        ),
        (
            ATTR_TEXT.to_string(),
            AttributeValue::S(memory.text.clone()),
        ),
        (ATTR_EMBEDDING.to_string(), to_item_attr(embedding)),
        (
            ATTR_CREATED_AT.to_string(),
            AttributeValue::N(to_epoch_seconds(memory.created_at)?.to_string()),
        ),
    ]);

    // TTL only fires on items that carry the attribute, so absence means
    // "never expires" rather than "expires now".
    if let Some(expires_at) = memory.expires_at {
        item.insert(
            ATTR_EXPIRES_AT.to_string(),
            AttributeValue::N(to_epoch_seconds(expires_at)?.to_string()),
        );
    }
    if let Some(source) = &memory.source {
        item.insert(ATTR_SOURCE.to_string(), AttributeValue::S(source.clone()));
    }
    if let Some(login) = &memory.github_login {
        item.insert(
            ATTR_GITHUB_LOGIN.to_string(),
            AttributeValue::S(login.clone()),
        );
    }
    if let Some(repo) = &memory.github_repo {
        item.insert(
            ATTR_GITHUB_REPO.to_string(),
            AttributeValue::S(repo.clone()),
        );
    }
    if let Some(rating) = memory.rating {
        item.insert(
            ATTR_RATING.to_string(),
            AttributeValue::N(rating.to_string()),
        );
    }
    item.insert(ATTR_ACTIVE.to_string(), AttributeValue::Bool(memory.active));

    Ok(item)
}

/// Rebuild a [`Memory`] from a stored item or a search result.
///
/// Works for both because [`Memory`] carries no embedding: the projection used
/// for search omits the vector, and nothing here asks for it.
pub fn memory_from_item(item: &HashMap<String, AttributeValue>) -> Result<Memory, ItemError> {
    Ok(Memory {
        user_id: UserId::new(string_field(item, ATTR_USER_ID)?)?,
        memory_id: MemoryId::new(string_field(item, ATTR_MEMORY_ID)?)?,
        kind: string_field(item, ATTR_KIND)?.parse::<MemoryKind>()?,
        text: string_field(item, ATTR_TEXT)?.to_string(),
        created_at: from_epoch_seconds(number_field(item, ATTR_CREATED_AT)?),
        expires_at: optional_number_field(item, ATTR_EXPIRES_AT)?.map(from_epoch_seconds),
        source: optional_string_field(item, ATTR_SOURCE)?.map(str::to_string),
        github_login: optional_string_field(item, ATTR_GITHUB_LOGIN)?.map(str::to_string),
        github_repo: optional_string_field(item, ATTR_GITHUB_REPO)?.map(str::to_string),
        rating: optional_number_field(item, ATTR_RATING)?.map(|n| n as u8),
        active: optional_bool_field(item, ATTR_ACTIVE)?.unwrap_or(true),
    })
}

/// The primary key of an item, used by `GetItem` and `DeleteItem`.
pub fn key(user_id: &UserId, memory_id: &MemoryId) -> HashMap<String, AttributeValue> {
    HashMap::from([
        (
            ATTR_USER_ID.to_string(),
            AttributeValue::S(user_id.to_string()),
        ),
        (
            ATTR_MEMORY_ID.to_string(),
            AttributeValue::S(memory_id.to_string()),
        ),
    ])
}

fn string_field<'a>(
    item: &'a HashMap<String, AttributeValue>,
    name: &'static str,
) -> Result<&'a str, ItemError> {
    optional_string_field(item, name)?.ok_or(ItemError::MissingAttribute(name))
}

fn optional_string_field<'a>(
    item: &'a HashMap<String, AttributeValue>,
    name: &'static str,
) -> Result<Option<&'a str>, ItemError> {
    match item.get(name) {
        None => Ok(None),
        Some(value) => {
            value
                .as_s()
                .map(|s| Some(s.as_str()))
                .map_err(|_| ItemError::UnexpectedType {
                    name,
                    expected: "S",
                })
        }
    }
}

fn optional_bool_field(
    item: &HashMap<String, AttributeValue>,
    name: &'static str,
) -> Result<Option<bool>, ItemError> {
    match item.get(name) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(|b| Some(*b))
            .map_err(|_| ItemError::UnexpectedType {
                name,
                expected: "BOOL",
            }),
    }
}

fn number_field(
    item: &HashMap<String, AttributeValue>,
    name: &'static str,
) -> Result<u64, ItemError> {
    optional_number_field(item, name)?.ok_or(ItemError::MissingAttribute(name))
}

fn optional_number_field(
    item: &HashMap<String, AttributeValue>,
    name: &'static str,
) -> Result<Option<u64>, ItemError> {
    let Some(value) = item.get(name) else {
        return Ok(None);
    };
    let raw = value.as_n().map_err(|_| ItemError::UnexpectedType {
        name,
        expected: "N",
    })?;
    raw.parse::<u64>()
        .map(Some)
        .map_err(|_| ItemError::InvalidNumber {
            name,
            value: raw.clone(),
        })
}

fn to_epoch_seconds(time: SystemTime) -> Result<u64, ItemError> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_| ItemError::TimestampBeforeEpoch)
}

fn from_epoch_seconds(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_memory() -> Memory {
        Memory {
            user_id: UserId::new("aws:123456789012:user/alice").expect("valid"),
            memory_id: MemoryId::new("mem-1").expect("valid"),
            kind: MemoryKind::Preference,
            text: "I prefer pour-over coffee".to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            expires_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_003_600)),
            source: Some("mcp".to_string()),
            github_login: Some("dmux".to_string()),
            github_repo: None,
            rating: None,
            active: true,
        }
    }

    fn sample_embedding() -> Embedding {
        Embedding::new(vec![0.125, -0.5, 0.0, 1.5]).expect("finite values")
    }

    #[test]
    fn stored_vector_is_wrapped_in_a_list_but_the_query_vector_is_not() {
        let embedding = sample_embedding();

        let stored = to_item_attr(&embedding);
        let inner = stored.as_l().expect("stored vector must be an L");
        assert_eq!(inner.len(), 4);
        assert!(matches!(inner[0], AttributeValue::N(_)));

        let query = to_query_vector(&embedding);
        assert_eq!(query.len(), 4);
        assert!(
            query
                .iter()
                .all(|value| matches!(value, AttributeValue::N(_))),
            "the query vector must be a flat list of N, never wrapped in L"
        );
    }

    #[test]
    fn both_vector_shapes_carry_identical_numbers() {
        let embedding = sample_embedding();
        let stored = to_item_attr(&embedding);
        let stored = stored.as_l().expect("an L");
        let query = to_query_vector(&embedding);
        assert_eq!(stored, &query);
    }

    #[test]
    fn item_round_trips_through_dynamodb_attributes() {
        let memory = sample_memory();
        let item = to_item(&memory, &sample_embedding()).expect("serialises");
        let parsed = memory_from_item(&item).expect("deserialises");
        assert_eq!(parsed, memory);
    }

    #[test]
    fn an_item_without_a_ttl_round_trips_as_never_expiring() {
        let memory = Memory {
            expires_at: None,
            source: None,
            github_login: None,
            github_repo: None,
            ..sample_memory()
        };
        let item = to_item(&memory, &sample_embedding()).expect("serialises");
        assert!(
            !item.contains_key(ATTR_EXPIRES_AT),
            "TTL only fires on items that carry the attribute, so it must be absent"
        );
        assert_eq!(memory_from_item(&item).expect("deserialises"), memory);
    }

    #[test]
    fn the_embedding_is_written_to_the_attribute_the_vector_index_watches() {
        let item = to_item(&sample_memory(), &sample_embedding()).expect("serialises");
        assert!(
            item.contains_key(ATTR_EMBEDDING),
            "an item without the vector attribute is simply never indexed"
        );
    }

    #[test]
    fn a_search_result_without_the_vector_still_rebuilds_a_memory() {
        // This is what SearchVectors actually returns: the projection omits the
        // embedding, so deserialisation must not depend on it.
        let mut item = to_item(&sample_memory(), &sample_embedding()).expect("serialises");
        item.remove(ATTR_EMBEDDING);
        assert_eq!(
            memory_from_item(&item).expect("deserialises"),
            sample_memory()
        );
    }

    #[test]
    fn a_missing_required_attribute_is_reported_by_name() {
        let mut item = to_item(&sample_memory(), &sample_embedding()).expect("serialises");
        item.remove(ATTR_TEXT);
        assert!(matches!(
            memory_from_item(&item),
            Err(ItemError::MissingAttribute("text"))
        ));
    }

    #[test]
    fn a_wrongly_typed_attribute_is_reported_rather_than_silently_skipped() {
        let mut item = to_item(&sample_memory(), &sample_embedding()).expect("serialises");
        item.insert(
            ATTR_CREATED_AT.to_string(),
            AttributeValue::S("nope".into()),
        );
        assert!(matches!(
            memory_from_item(&item),
            Err(ItemError::UnexpectedType {
                name: "created_at",
                ..
            })
        ));
    }
}
