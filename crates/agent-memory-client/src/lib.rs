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
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
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
    pub async fn from_env(github_login: Option<String>) -> Result<Self, ClientError> {
        let endpoint =
            std::env::var("AGENT_MEMORY_API").map_err(|_| ClientError::MissingEndpoint)?;
        Ok(Self::new(endpoint, github_login).await)
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
