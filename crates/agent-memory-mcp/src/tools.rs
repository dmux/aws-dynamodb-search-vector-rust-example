//! The MCP tool surface.
//!
//! These tools depend only on [`MemoryService`], never on the HTTP client.
//! That is the payoff of the primary port: the same tool code runs against the
//! deployed API, against a DynamoDB-backed service in-process, or against an
//! in-memory fake in tests — which is how everything below is tested without a
//! network or an AWS account.
//!
//! # Tool descriptions are prompts
//!
//! The `description` strings are read by the model to decide whether to call a
//! tool. They therefore say when *not* to call as well as when to: without
//! that, an agent tends to recall on every turn, and each recall costs a
//! Bedrock embedding plus a billed vector search.

use std::sync::Arc;

use agent_memory_core::{
    MemoryError, MemoryId, MemoryKind, MemoryService, RecallQuery, RememberCommand, ScoredMemory,
    TopK, UserId,
};
use rmcp::handler::server::ServerHandler;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::model::{Implementation, InitializeResult, ServerCapabilities};
use rmcp::{tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

/// Placeholder namespace.
///
/// The real namespace is derived server-side from the IAM principal that signed
/// the request, so whatever is sent here is ignored. It exists only because the
/// domain's `RecallQuery` carries a `UserId`, and it is deliberately *not*
/// exposed as a tool argument: an argument the model can set would be an
/// argument the model can get wrong.
fn caller_placeholder() -> Result<UserId, ErrorData> {
    UserId::new("caller").map_err(to_error_data)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecallArgs {
    /// What to look for, in natural language.
    pub query: String,
    /// How many memories to return. Defaults to 5, maximum 100.
    #[serde(default)]
    pub top_k: Option<u32>,
    /// Restrict to one kind: "fact", "preference" or "episode".
    #[serde(default)]
    pub kind: Option<String>,
    /// Discard matches farther than this. Distance is 0 for an exact match and
    /// grows as relevance drops, so a smaller number is stricter.
    #[serde(default)]
    pub max_distance: Option<f32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RememberArgs {
    /// The thing to remember, written as a self-contained sentence.
    pub text: String,
    /// "fact", "preference" or "episode". Defaults to "fact".
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional lifetime in seconds. Omit for a memory that never expires.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ForgetArgs {
    /// The `memory_id` returned by a previous recall.
    pub memory_id: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecallResult {
    pub memories: Vec<RecalledMemory>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecalledMemory {
    pub memory_id: String,
    pub kind: String,
    pub text: String,
    /// Lower means a closer match.
    pub distance: f32,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RememberResult {
    pub memory_id: String,
    pub kind: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ForgetResult {
    pub forgotten: bool,
}

/// The tool surface, holding the primary port behind a trait object.
///
/// `#[tool_router]` cannot be applied to a generic `impl`, so the port is
/// erased here rather than monomorphised. The abstraction is unchanged — this
/// still accepts any `MemoryService`, which is what lets the tests below run
/// against an in-memory fake — and the dynamic call is immaterial next to the
/// network round trip it wraps.
#[derive(Clone)]
pub struct MemoryTools {
    service: Arc<dyn MemoryService>,
}

impl MemoryTools {
    pub fn new(service: Arc<dyn MemoryService>) -> Self {
        Self { service }
    }
}

impl std::fmt::Debug for MemoryTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTools").finish_non_exhaustive()
    }
}

#[tool_router]
impl MemoryTools {
    #[tool(
        name = "memory_recall",
        description = "Search the user's long-term memory by meaning. Call this before answering \
                       whenever the user's own preferences, past decisions, projects or personal \
                       facts could change the answer, and whenever the user refers to something \
                       previously discussed. Do not call it for general knowledge questions that \
                       any answer would be the same for. Results come back closest-match first."
    )]
    async fn memory_recall(
        &self,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<Json<RecallResult>, ErrorData> {
        // Bounds are checked before the call so an out-of-range top_k never
        // reaches the network, let alone a paid embedding.
        let top_k = match args.top_k {
            Some(value) => TopK::new(value).map_err(to_error_data)?,
            None => TopK::DEFAULT,
        };
        let kind = parse_kind(args.kind.as_deref())?;
        let max_distance = args
            .max_distance
            .map(agent_memory_core::Distance::new)
            .transpose()
            .map_err(to_error_data)?;

        let hits = self
            .service
            .recall(RecallQuery {
                user_id: caller_placeholder()?,
                text: args.query,
                top_k,
                kind,
                max_distance,
            })
            .await
            .map_err(to_error_data)?;

        Ok(Json(RecallResult {
            memories: hits.iter().map(to_recalled).collect(),
        }))
    }

    #[tool(
        name = "memory_remember",
        description = "Store something in the user's long-term memory. Use it for durable \
                       information the user would expect to be recalled in a later session: \
                       stated preferences, stable facts about them or their projects, and \
                       decisions with lasting consequences. Do not store transient context from \
                       the current conversation, anything the user asked to keep private, or \
                       secrets such as credentials."
    )]
    async fn memory_remember(
        &self,
        Parameters(args): Parameters<RememberArgs>,
    ) -> Result<Json<RememberResult>, ErrorData> {
        let kind = parse_kind(args.kind.as_deref())?.unwrap_or(MemoryKind::Fact);

        let memory = self
            .service
            .remember(RememberCommand {
                user_id: caller_placeholder()?,
                kind,
                text: args.text,
                ttl: args.ttl_seconds.map(std::time::Duration::from_secs),
                source: Some("mcp".to_string()),
                github_login: None,
            })
            .await
            .map_err(to_error_data)?;

        Ok(Json(RememberResult {
            memory_id: memory.memory_id.to_string(),
            kind: memory.kind.to_string(),
        }))
    }

    #[tool(
        name = "memory_forget",
        description = "Delete one memory by its id, which comes from a previous memory_recall. \
                       Use it when the user asks to be forgotten or corrects something that was \
                       stored wrongly."
    )]
    async fn memory_forget(
        &self,
        Parameters(args): Parameters<ForgetArgs>,
    ) -> Result<Json<ForgetResult>, ErrorData> {
        let memory_id = MemoryId::new(args.memory_id).map_err(to_error_data)?;
        self.service
            .forget(&caller_placeholder()?, &memory_id)
            .await
            .map_err(to_error_data)?;
        Ok(Json(ForgetResult { forgotten: true }))
    }
}

/// Identify the server and tell the model what this memory is for.
///
/// Written out rather than using the `server_handler` shorthand, which would
/// leave the server advertising rmcp's own name and version in every client's
/// server list. `instructions` is surfaced to the model as context, so it is
/// the right place for the usage guidance that does not fit in a tool
/// description.
#[tool_handler]
impl ServerHandler for MemoryTools {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                // Built from this crate's own env! values. `from_build_env()`
                // looks like the right helper but is not: it expands inside the
                // rmcp crate, so it reports the SDK's name and version and the
                // server shows up as "rmcp 3.1.2" in every client's list.
                Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                    .with_title("Agent Memory"),
            )
            .with_instructions(
                "Long-term memory for this user, searched by meaning rather than keywords. \
                 Recall before answering when the user's own preferences, past decisions or \
                 personal facts would change the answer. Remember only durable information \
                 that should outlive this conversation, and never store secrets. Memories are \
                 scoped to the caller's verified AWS identity, so they persist across sessions.",
            )
    }
}

fn parse_kind(kind: Option<&str>) -> Result<Option<MemoryKind>, ErrorData> {
    kind.map(str::parse::<MemoryKind>)
        .transpose()
        .map_err(to_error_data)
}

fn to_recalled(hit: &ScoredMemory) -> RecalledMemory {
    RecalledMemory {
        memory_id: hit.memory.memory_id.to_string(),
        kind: hit.memory.kind.to_string(),
        text: hit.memory.text.clone(),
        distance: hit.distance.get(),
    }
}

/// Bad input becomes `invalid_params` so the model can correct itself; anything
/// else is an internal error it should not try to fix by retrying.
fn to_error_data(error: MemoryError) -> ErrorData {
    if error.is_invalid_input() {
        ErrorData::invalid_params(error.to_string(), None)
    } else {
        tracing::error!(error = ?error, "memory operation failed");
        ErrorData::internal_error(explain(&error), None)
    }
}

/// The whole chain, not just the outermost message.
///
/// `MemoryError::Repository` displays as "repository failure" and keeps the
/// reason as a `#[source]`. Reporting only the head therefore tells the caller
/// nothing: a missing AWS profile, an expired SSO session and a genuinely
/// unreachable table are one indistinguishable string, and the stderr log the
/// detail would have gone to belongs to a server the caller cannot see. Since
/// this crosses to an agent rather than to an end user, the causes are worth
/// far more than the tidiness of hiding them.
fn explain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use agent_memory_core::LocalMemoryService;
    use agent_memory_core::testing::{
        FixedClock, InMemoryRepository, SeqIdGenerator, StubEmbedder,
    };

    use super::*;

    fn tools() -> MemoryTools {
        MemoryTools::new(Arc::new(LocalMemoryService::new(
            InMemoryRepository::default(),
            StubEmbedder::new(64),
            FixedClock::new(SystemTime::UNIX_EPOCH),
            SeqIdGenerator::default(),
        )))
    }

    fn remember_args(text: &str, kind: Option<&str>) -> Parameters<RememberArgs> {
        Parameters(RememberArgs {
            text: text.to_string(),
            kind: kind.map(str::to_string),
            ttl_seconds: None,
        })
    }

    fn recall_args(query: &str) -> Parameters<RecallArgs> {
        Parameters(RecallArgs {
            query: query.to_string(),
            top_k: None,
            kind: None,
            max_distance: None,
        })
    }

    #[tokio::test]
    async fn remembering_then_recalling_returns_the_stored_memory() {
        let tools = tools();

        let stored = tools
            .memory_remember(remember_args(
                "I prefer pour-over coffee",
                Some("preference"),
            ))
            .await
            .expect("remember succeeds");

        let recalled = tools
            .memory_recall(recall_args("coffee"))
            .await
            .expect("recall succeeds");

        assert_eq!(recalled.0.memories.len(), 1);
        assert_eq!(recalled.0.memories[0].memory_id, stored.0.memory_id);
        assert_eq!(recalled.0.memories[0].kind, "preference");
    }

    #[tokio::test]
    async fn an_unspecified_kind_defaults_to_fact() {
        let tools = tools();
        let stored = tools
            .memory_remember(remember_args(
                "The office wifi password rotates monthly",
                None,
            ))
            .await
            .expect("remember succeeds");
        assert_eq!(stored.0.kind, "fact");
    }

    #[tokio::test]
    async fn an_out_of_range_top_k_is_invalid_params_not_an_internal_error() {
        // The model can act on invalid_params by retrying with a valid value;
        // an internal error would just look like a broken server.
        let tools = tools();
        let error = tools
            .memory_recall(Parameters(RecallArgs {
                query: "anything".into(),
                top_k: Some(101),
                kind: None,
                max_distance: None,
            }))
            .await
            .map(|_| ())
            .expect_err("out-of-range top_k is rejected");

        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn an_unknown_kind_is_reported_to_the_model() {
        let tools = tools();
        let error = tools
            .memory_remember(remember_args("x", Some("daydream")))
            .await
            .map(|_| ())
            .expect_err("unknown kinds are rejected");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn forgetting_removes_the_memory_from_later_recalls() {
        let tools = tools();
        let stored = tools
            .memory_remember(remember_args("a temporary note", None))
            .await
            .expect("remember succeeds");

        tools
            .memory_forget(Parameters(ForgetArgs {
                memory_id: stored.0.memory_id.clone(),
            }))
            .await
            .expect("forget succeeds");

        let recalled = tools
            .memory_recall(recall_args("temporary note"))
            .await
            .expect("recall succeeds");
        assert!(recalled.0.memories.is_empty());
    }

    #[tokio::test]
    async fn recall_orders_results_closest_first() {
        let tools = tools();
        for text in [
            "I drink pour-over coffee every morning",
            "kubernetes ingress controller configuration",
        ] {
            tools
                .memory_remember(remember_args(text, None))
                .await
                .expect("remember succeeds");
        }

        let recalled = tools
            .memory_recall(recall_args("pour-over coffee"))
            .await
            .expect("recall succeeds");

        assert!(recalled.0.memories[0].text.contains("coffee"));
        assert!(
            recalled
                .0
                .memories
                .windows(2)
                .all(|pair| pair[0].distance <= pair[1].distance),
            "lower distance means a closer match, so results must ascend"
        );
    }

    #[test]
    fn no_tool_exposes_a_user_id_argument() {
        // Identity is derived from the signed request, never chosen by the
        // model. Adding such an argument would be a privilege escalation.
        let schema =
            serde_json::to_string(&schemars::schema_for!(RecallArgs)).expect("schema serialises");
        assert!(!schema.contains("user_id"));

        let schema =
            serde_json::to_string(&schemars::schema_for!(RememberArgs)).expect("schema serialises");
        assert!(!schema.contains("user_id"));
    }

    #[test]
    fn every_tool_description_says_when_not_to_call_it() {
        // Without this guidance an agent recalls on every turn, and each recall
        // costs an embedding plus a billed vector search.
        let router = MemoryTools::tool_router();
        let tools = router.list_all();
        assert_eq!(tools.len(), 3);

        for tool in tools {
            let description = tool.description.clone().unwrap_or_default();
            assert!(
                description.contains("Do not") || description.contains("Use it when"),
                "tool {} should tell the model when to hold back, got: {description}",
                tool.name
            );
        }
    }
}
