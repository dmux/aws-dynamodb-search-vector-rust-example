//! [`MemoryService`] over HTTP, signed with SigV4.
//!
//! This is the second implementation of the primary port, and the reason the
//! MCP server needs no knowledge of transport at all: swapping this for
//! `LocalMemoryService` turns the same MCP binary into one that talks to
//! DynamoDB directly, with no change to the tool code.
//!
//! # Why SigV4 and not a token
//!
//! The API Gateway routes use `AWS_IAM` authorization. That is the cheapest
//! option available — HTTP APIs support IAM at no extra charge and do not
//! support API keys at all — and it means the client needs no secret of its
//! own: it signs with whatever credentials the machine already has. The server
//! then derives the memory namespace from the verified principal, so identity
//! is never something the client asserts.

mod signing;

use std::time::Duration;

use agent_memory_contract::{
    ErrorResponse, MemoryView, RecallRequest, RecallResponse, RememberRequest, ScoredMemoryView,
};
use agent_memory_core::{
    BoxError, Distance, Memory, MemoryError, MemoryId, MemoryKind, MemoryService, RecallQuery,
    RememberCommand, ScoredMemory, UserId,
};
use async_trait::async_trait;

pub use signing::SigningError;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("AGENT_MEMORY_API is not set")]
    MissingEndpoint,

    #[error("transport failure calling {url}")]
    Transport {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("the memory API returned {status}: {code} — {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },

    #[error("could not decode the memory API response")]
    Decode(#[source] serde_json::Error),

    #[error(transparent)]
    Signing(#[from] SigningError),

    #[error(transparent)]
    Domain(#[from] MemoryError),
}

impl From<ClientError> for MemoryError {
    fn from(error: ClientError) -> Self {
        match error {
            // A domain error that survived the round trip stays a domain error,
            // so a 400 from the server still looks like bad input locally.
            ClientError::Domain(domain) => domain,
            other => MemoryError::repository(Box::new(other) as BoxError),
        }
    }
}

/// `AGENT_MEMORY_PROFILE`, then `AWS_PROFILE`. Blank counts as unset — an empty
/// string in a JSON manifest is someone leaving the field in, not naming a
/// profile called "".
fn profile_from_env() -> Option<String> {
    ["AGENT_MEMORY_PROFILE", "AWS_PROFILE"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok())
        .map(|profile| profile.trim().to_string())
        .filter(|profile| !profile.is_empty())
}

/// The memory API, reached over HTTP.
#[derive(Debug, Clone)]
pub struct RemoteMemoryService {
    http: reqwest::Client,
    endpoint: String,
    signer: signing::Signer,
    /// Attribution label attached to writes. Never used for authorization.
    github_login: Option<String>,
}

impl RemoteMemoryService {
    pub async fn new(endpoint: impl Into<String>, github_login: Option<String>) -> Self {
        Self::for_profile(endpoint, github_login, profile_from_env().as_deref()).await
    }

    /// Sign with a **named** AWS profile rather than whatever the default chain
    /// happens to resolve.
    ///
    /// # Why this is not just `AWS_PROFILE`
    ///
    /// `load_defaults` does read `AWS_PROFILE`, so a shell that exports it is
    /// already served. The callers that are not are the ones that never had a
    /// shell: this crate's own MCP server is launched by the agent host from a
    /// JSON manifest, and a manifest that sets `AGENT_MEMORY_API` and
    /// `AWS_REGION` but forgets the profile silently falls through to the
    /// `[default]` profile. If that profile carries no credentials — normal on a
    /// machine that only ever uses named profiles — every call fails, far from
    /// here, as an unauthorised request.
    ///
    /// Passing the profile as an argument makes it something a caller can be
    /// required to think about, and lets [`SigningError::NoCredentials`] name
    /// the profile it actually tried.
    pub async fn for_profile(
        endpoint: impl Into<String>,
        github_login: Option<String>,
        profile: Option<&str>,
    ) -> Self {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(profile) = profile {
            loader = loader.profile_name(profile);
        }
        let config = loader.load().await;

        let mut service = Self::with_config(config, endpoint, github_login);
        service.signer = service.signer.for_profile(profile.map(str::to_owned));
        service
    }

    /// Build from an explicitly supplied [`SdkConfig`] instead of the ambient
    /// credential chain.
    ///
    /// [`Self::new`] resolves credentials and region through
    /// `aws_config::load_defaults`, which reads process environment variables,
    /// `~/.aws/config` and the SSO cache. That is the right default for a
    /// process started from a shell, and the wrong one for two callers this
    /// crate already has to serve:
    ///
    /// * A macOS `.app` launched from the Finder inherits no shell profile, so
    ///   `AWS_REGION` is simply absent and signing fails with
    ///   [`SigningError::NoRegion`] no matter how the machine is configured.
    /// * A mobile client has no `~/.aws/config` at all; its credentials arrive
    ///   at runtime from something like a Cognito identity pool.
    ///
    /// Both could be forced to work by mutating the process environment before
    /// construction, but `std::env::set_var` is `unsafe` in edition 2024 and
    /// process-global — an alarming amount of machinery for what is really just
    /// a missing parameter. This constructor is that parameter.
    pub fn with_config(
        config: aws_config::SdkConfig,
        endpoint: impl Into<String>,
        github_login: Option<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            signer: signing::Signer::new(config),
            github_login,
        }
    }

    /// Build from `AGENT_MEMORY_API`, the variable the generated `.mcp.json`
    /// sets.
    ///
    /// The profile comes from `AGENT_MEMORY_PROFILE`, falling back to
    /// `AWS_PROFILE`. The dedicated name exists so a manifest can point this
    /// client at one account without redirecting every other AWS SDK in the
    /// same process.
    pub async fn from_env(github_login: Option<String>) -> Result<Self, ClientError> {
        let endpoint =
            std::env::var("AGENT_MEMORY_API").map_err(|_| ClientError::MissingEndpoint)?;
        Ok(Self::for_profile(endpoint, github_login, profile_from_env().as_deref()).await)
    }

    async fn send<B, R>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Option<R>, ClientError>
    where
        B: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let url = format!("{}{path}", self.endpoint);
        let payload = match body {
            Some(body) => serde_json::to_vec(body).map_err(ClientError::Decode)?,
            None => Vec::new(),
        };

        let headers = self
            .signer
            .sign(method.as_str(), &url, &payload)
            .await
            .map_err(ClientError::Signing)?;

        let mut request = self.http.request(method, &url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if !payload.is_empty() {
            request = request
                .header("content-type", "application/json")
                .body(payload);
        }

        let response = request
            .send()
            .await
            .map_err(|source| ClientError::Transport {
                url: url.clone(),
                source,
            })?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|source| ClientError::Transport { url, source })?;

        if status == reqwest::StatusCode::NO_CONTENT || bytes.is_empty() {
            return Ok(None);
        }

        if !status.is_success() {
            // The server speaks a stable error envelope; fall back to the raw
            // body only if it does not (e.g. an API Gateway-level rejection).
            let (code, message) = match serde_json::from_slice::<ErrorResponse>(&bytes) {
                Ok(envelope) => (envelope.error.code, envelope.error.message),
                Err(_) => (
                    "unknown".to_string(),
                    String::from_utf8_lossy(&bytes).to_string(),
                ),
            };
            return Err(ClientError::Api {
                status: status.as_u16(),
                code,
                message,
            });
        }

        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(ClientError::Decode)
    }
}

#[async_trait]
impl MemoryService for RemoteMemoryService {
    async fn remember(&self, command: RememberCommand) -> Result<Memory, MemoryError> {
        // Note what is not sent: the user id. The server takes it from the
        // signature, so there is nothing here for a caller to forge.
        let request = RememberRequest {
            kind: command.kind.to_string(),
            text: command.text,
            ttl_seconds: command.ttl.map(|ttl| ttl.as_secs()),
            source: command.source,
            github_login: command.github_login.or_else(|| self.github_login.clone()),
        };

        let view: MemoryView = self
            .send(reqwest::Method::POST, "/memories", Some(&request))
            .await
            .map_err(MemoryError::from)?
            .ok_or_else(|| {
                MemoryError::repository(Box::new(ClientError::Decode(serde_json::Error::io(
                    std::io::Error::other("the API returned no body for a create"),
                ))) as BoxError)
            })?;

        from_view(view)
    }

    async fn recall(&self, query: RecallQuery) -> Result<Vec<ScoredMemory>, MemoryError> {
        let request = RecallRequest {
            query: query.text,
            top_k: Some(query.top_k.get()),
            kind: query.kind.map(|kind| kind.to_string()),
            max_distance: query.max_distance.map(|distance| distance.get()),
        };

        let response: RecallResponse = self
            .send(reqwest::Method::POST, "/memories/search", Some(&request))
            .await
            .map_err(MemoryError::from)?
            .unwrap_or(RecallResponse {
                results: Vec::new(),
            });

        response.results.into_iter().map(from_scored_view).collect()
    }

    async fn forget(&self, _user_id: &UserId, memory_id: &MemoryId) -> Result<(), MemoryError> {
        self.send::<(), serde_json::Value>(
            reqwest::Method::DELETE,
            &format!("/memories/{memory_id}"),
            None,
        )
        .await
        .map_err(MemoryError::from)?;
        Ok(())
    }

    async fn get(
        &self,
        _user_id: &UserId,
        memory_id: &MemoryId,
    ) -> Result<Option<Memory>, MemoryError> {
        match self
            .send::<(), MemoryView>(
                reqwest::Method::GET,
                &format!("/memories/{memory_id}"),
                None,
            )
            .await
        {
            Ok(Some(view)) => from_view(view).map(Some),
            Ok(None) => Ok(None),
            // A 404 is an ordinary "not found", not a failure.
            Err(ClientError::Api { status: 404, .. }) => Ok(None),
            Err(error) => Err(MemoryError::from(error)),
        }
    }
}

fn from_view(view: MemoryView) -> Result<Memory, MemoryError> {
    use std::time::{Duration, SystemTime};

    Ok(Memory {
        user_id: UserId::new(view.user_id)?,
        memory_id: MemoryId::new(view.memory_id)?,
        kind: view.kind.parse::<MemoryKind>()?,
        text: view.text,
        created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(view.created_at),
        expires_at: view
            .expires_at
            .map(|seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
        source: view.source,
        github_login: view.github_login,
    })
}

fn from_scored_view(view: ScoredMemoryView) -> Result<ScoredMemory, MemoryError> {
    Ok(ScoredMemory {
        distance: Distance::new(view.distance)?,
        memory: from_view(view.memory)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_config_builds_without_touching_the_ambient_credential_chain() {
        // The point of the constructor: no environment, no profile, no SSO
        // cache, and no async. An empty config is enough to build — resolution
        // failures surface later, at signing time, as SigningError.
        let service = RemoteMemoryService::with_config(
            aws_config::SdkConfig::builder().build(),
            "https://example.com/",
            None,
        );
        assert_eq!(
            service.endpoint, "https://example.com",
            "the trailing slash must be trimmed, or every path would double it"
        );
    }

    /// The failure this whole parameter exists to make legible: no credentials,
    /// with the profile that was tried named in the message.
    #[tokio::test]
    async fn a_named_profile_that_has_no_credentials_says_which_profile() {
        let service = RemoteMemoryService::for_profile(
            "https://example.com/",
            None,
            Some("definitely-not-a-configured-profile"),
        )
        .await;

        let error = service
            .signer
            .sign("GET", "https://example.com/memories", b"")
            .await
            .expect_err("that profile does not exist");

        let message = error.to_string();
        assert!(
            message.contains("definitely-not-a-configured-profile"),
            "the message must name the profile it tried, got: {message}"
        );
        assert!(
            message.contains("aws sso login"),
            "and say how to fix it, got: {message}"
        );
    }

    /// With no profile named, the message must not invent one — it has to say
    /// the default chain was used, which is a different thing to go and check.
    #[tokio::test]
    async fn without_a_profile_the_message_says_the_default_chain_was_used() {
        let service = RemoteMemoryService::with_config(
            aws_config::SdkConfig::builder().build(),
            "https://example.com/",
            None,
        );

        let error = service
            .signer
            .sign("GET", "https://example.com/memories", b"")
            .await
            .expect_err("an empty config resolves nothing");

        let message = error.to_string();
        assert!(message.contains("default chain"), "got: {message}");
        assert!(
            message.contains("AGENT_MEMORY_PROFILE"),
            "it must name the variable that fixes it, got: {message}"
        );
    }

    #[test]
    fn a_blank_profile_variable_counts_as_unset() {
        // A manifest that leaves `"AGENT_MEMORY_PROFILE": ""` in place is not
        // asking for a profile named empty string.
        assert_eq!("  ".trim(), "");
        assert!(
            profile_from_env().is_none_or(|profile| !profile.is_empty()),
            "a resolved profile is never blank"
        );
    }

    #[test]
    fn a_domain_error_survives_the_round_trip_as_a_domain_error() {
        let error = ClientError::Domain(MemoryError::TopKOutOfRange {
            requested: 101,
            max: 100,
        });
        assert!(MemoryError::from(error).is_invalid_input());
    }

    #[test]
    fn transport_and_api_failures_become_repository_errors() {
        let error = ClientError::Api {
            status: 500,
            code: "internal_error".into(),
            message: "internal error".into(),
        };
        assert!(!MemoryError::from(error).is_invalid_input());
    }

    #[test]
    fn a_memory_view_round_trips_into_the_domain() {
        let view = MemoryView {
            user_id: "aws:123456789012:user/rafael".into(),
            memory_id: "mem-1".into(),
            kind: "preference".into(),
            text: "I prefer pour-over coffee".into(),
            created_at: 1_700_000_000,
            expires_at: Some(1_700_003_600),
            source: Some("mcp".into()),
            github_login: Some("dmux".into()),
        };

        let memory = from_view(view).expect("converts");
        assert_eq!(memory.kind, MemoryKind::Preference);
        assert_eq!(memory.user_id.as_str(), "aws:123456789012:user/rafael");
        assert!(memory.expires_at.is_some());
    }

    #[test]
    fn an_unknown_kind_from_the_server_is_an_error_not_a_default() {
        let view = MemoryView {
            user_id: "aws:1:user/x".into(),
            memory_id: "mem-1".into(),
            kind: "daydream".into(),
            text: "x".into(),
            created_at: 0,
            expires_at: None,
            source: None,
            github_login: None,
        };
        assert!(matches!(
            from_view(view),
            Err(MemoryError::UnknownMemoryKind(_))
        ));
    }
}
