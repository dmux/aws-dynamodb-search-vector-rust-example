//! HTTP routing and translation between the wire contract and the domain.

use std::time::{Duration, SystemTime};

use agent_memory_contract::{
    MemoryView, RecallRequest, RecallResponse, RememberRequest, ScoredMemoryView,
};
use agent_memory_core::{
    Distance, Memory, MemoryId, MemoryKind, MemoryService, RecallQuery, RememberCommand,
    ScoredMemory, TopK, UserId,
};
use lambda_http::{Body, Request, RequestExt, Response};

use crate::error::ApiError;
use crate::principal::{self, PrincipalError};

/// Route a request and always produce a response.
///
/// Errors are rendered rather than propagated so that a failure is a proper
/// HTTP status with the stable error envelope, never an opaque runtime error.
pub async fn route<S: MemoryService>(service: &S, request: Request) -> Response<Body> {
    match dispatch(service, request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn dispatch<S: MemoryService>(
    service: &S,
    request: Request,
) -> Result<Response<Body>, ApiError> {
    // The namespace comes from the verified IAM principal, never from the body.
    let user_id = caller_namespace(&request)?;

    let method = request.method().clone();
    let path = request.raw_http_path().to_string();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

    match (method.as_str(), segments.as_slice()) {
        ("POST", ["memories"]) => remember(service, &user_id, request.body()).await,
        ("POST", ["memories", "search"]) => recall(service, &user_id, request.body()).await,
        ("GET", ["memories", memory_id]) => get(service, &user_id, memory_id).await,
        ("DELETE", ["memories", memory_id]) => forget(service, &user_id, memory_id).await,
        _ => Err(ApiError::NotFound {
            method: method.to_string(),
            path,
        }),
    }
}

/// Extract the caller ARN from the request context.
///
/// This is why the API Gateway integration is pinned to **payload format 1.0**.
/// Format 2.0's `requestContext` carries only `authorizer.jwt` and has no
/// `identity` object at all, so an `AWS_IAM` route gives the function no way to
/// learn who called it. Format 1.0 carries `requestContext.identity.userArn`.
fn caller_namespace(request: &Request) -> Result<UserId, ApiError> {
    let arn = match request.request_context_ref() {
        Some(lambda_http::request::RequestContext::ApiGatewayV1(context)) => {
            context.identity.user_arn.as_deref()
        }
        // Any other shape means the integration is misconfigured — most likely
        // someone switched the integration to payload format 2.0.
        _ => None,
    };

    let arn = arn.ok_or(PrincipalError::Missing)?;
    Ok(principal::principal_to_namespace(arn)?)
}

async fn remember<S: MemoryService>(
    service: &S,
    user_id: &UserId,
    body: &Body,
) -> Result<Response<Body>, ApiError> {
    let request: RememberRequest = parse_json(body)?;
    let kind = request
        .kind
        .parse::<MemoryKind>()
        .map_err(ApiError::Domain)?;

    let memory = service
        .remember(RememberCommand {
            user_id: user_id.clone(),
            kind,
            text: request.text,
            ttl: request.ttl_seconds.map(Duration::from_secs),
            source: request.source,
            github_login: request.github_login,
        })
        .await?;

    json_response(201, &to_view(&memory))
}

async fn recall<S: MemoryService>(
    service: &S,
    user_id: &UserId,
    body: &Body,
) -> Result<Response<Body>, ApiError> {
    let request: RecallRequest = parse_json(body)?;

    // Validating top_k here rejects an out-of-range value before the service
    // spends money embedding the query.
    let top_k = match request.top_k {
        Some(value) => TopK::new(value).map_err(ApiError::Domain)?,
        None => TopK::DEFAULT,
    };
    let kind = request
        .kind
        .map(|kind| kind.parse::<MemoryKind>())
        .transpose()
        .map_err(ApiError::Domain)?;
    let max_distance = request
        .max_distance
        .map(Distance::new)
        .transpose()
        .map_err(ApiError::Domain)?;

    let hits = service
        .recall(RecallQuery {
            user_id: user_id.clone(),
            text: request.query,
            top_k,
            kind,
            max_distance,
        })
        .await?;

    json_response(
        200,
        &RecallResponse {
            results: hits.iter().map(to_scored_view).collect(),
        },
    )
}

async fn get<S: MemoryService>(
    service: &S,
    user_id: &UserId,
    memory_id: &str,
) -> Result<Response<Body>, ApiError> {
    let memory_id = MemoryId::new(memory_id).map_err(ApiError::Domain)?;
    match service.get(user_id, &memory_id).await? {
        Some(memory) => json_response(200, &to_view(&memory)),
        None => Err(ApiError::NotFound {
            method: "GET".into(),
            path: format!("/memories/{memory_id}"),
        }),
    }
}

async fn forget<S: MemoryService>(
    service: &S,
    user_id: &UserId,
    memory_id: &str,
) -> Result<Response<Body>, ApiError> {
    let memory_id = MemoryId::new(memory_id).map_err(ApiError::Domain)?;
    service.forget(user_id, &memory_id).await?;
    Ok(Response::builder()
        .status(204)
        .body(Body::Empty)
        .unwrap_or_else(|_| Response::new(Body::Empty)))
}

fn parse_json<T: serde::de::DeserializeOwned>(body: &Body) -> Result<T, ApiError> {
    // `Body` is #[non_exhaustive]; anything new is treated as an empty payload
    // so that a future variant surfaces as a normal 400 rather than a panic.
    let bytes: &[u8] = match body {
        Body::Empty => &[],
        Body::Text(text) => text.as_bytes(),
        Body::Binary(bytes) => bytes,
        _ => &[],
    };
    serde_json::from_slice(bytes).map_err(|error| ApiError::BadRequest(error.to_string()))
}

fn json_response<T: serde::Serialize>(
    status: u16,
    payload: &T,
) -> Result<Response<Body>, ApiError> {
    let body =
        serde_json::to_string(payload).map_err(|error| ApiError::BadRequest(error.to_string()))?;
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .map_err(|error| ApiError::BadRequest(error.to_string()))
}

fn to_view(memory: &Memory) -> MemoryView {
    MemoryView {
        user_id: memory.user_id.to_string(),
        memory_id: memory.memory_id.to_string(),
        kind: memory.kind.to_string(),
        text: memory.text.clone(),
        created_at: epoch_seconds(memory.created_at),
        expires_at: memory.expires_at.map(epoch_seconds),
        source: memory.source.clone(),
        github_login: memory.github_login.clone(),
    }
}

fn to_scored_view(hit: &ScoredMemory) -> ScoredMemoryView {
    ScoredMemoryView {
        memory: to_view(&hit.memory),
        distance: hit.distance.get(),
    }
}

fn epoch_seconds(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_memory_core::LocalMemoryService;
    use agent_memory_core::testing::{
        FixedClock, InMemoryRepository, SeqIdGenerator, StubEmbedder,
    };
    use lambda_http::request::from_str as request_from_str;

    use super::*;

    type TestService =
        LocalMemoryService<InMemoryRepository, StubEmbedder, FixedClock, SeqIdGenerator>;

    fn service() -> Arc<TestService> {
        Arc::new(LocalMemoryService::new(
            InMemoryRepository::default(),
            StubEmbedder::new(64),
            FixedClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            SeqIdGenerator::default(),
        ))
    }

    /// An API Gateway **payload format 1.0** event, which is the shape the
    /// Terraform integration is pinned to precisely because it carries
    /// `requestContext.identity.userArn`.
    fn event(method: &str, path: &str, body: &str, user_arn: Option<&str>) -> Request {
        let identity = match user_arn {
            Some(arn) => format!(r#"{{"userArn":"{arn}","sourceIp":"192.0.2.1"}}"#),
            None => r#"{"sourceIp":"192.0.2.1"}"#.to_string(),
        };
        let payload = format!(
            r#"{{
                "version": "1.0",
                "resource": "{path}",
                "path": "{path}",
                "httpMethod": "{method}",
                "headers": {{ "content-type": "application/json" }},
                "requestContext": {{
                    "accountId": "123456789012",
                    "apiId": "abcdef123",
                    "httpMethod": "{method}",
                    "path": "{path}",
                    "protocol": "HTTP/1.1",
                    "requestId": "test-request",
                    "resourcePath": "{path}",
                    "stage": "$default",
                    "identity": {identity}
                }},
                "body": {body},
                "isBase64Encoded": false
            }}"#
        );
        request_from_str(&payload).expect("a valid API Gateway v1 event")
    }

    fn json_body(response: &Response<Body>) -> serde_json::Value {
        serde_json::from_slice(response.body().as_ref()).expect("a JSON body")
    }

    #[tokio::test]
    async fn the_namespace_comes_from_the_iam_principal() {
        let service = service();
        let response = route(
            service.as_ref(),
            event(
                "POST",
                "/memories",
                r#""{\"kind\":\"fact\",\"text\":\"I live in Lisbon\"}""#,
                Some("arn:aws:iam::123456789012:user/rafael"),
            ),
        )
        .await;

        assert_eq!(response.status(), 201);
        assert_eq!(
            json_body(&response)["user_id"],
            "aws:123456789012:user/rafael"
        );
    }

    #[tokio::test]
    async fn a_user_id_in_the_body_is_ignored_rather_than_honoured() {
        // The security property this whole design rests on: a client cannot
        // address someone else's memories by editing the payload.
        let service = service();
        let response = route(
            service.as_ref(),
            event(
                "POST",
                "/memories",
                r#""{\"kind\":\"fact\",\"text\":\"x\",\"user_id\":\"aws:999:user/victim\"}""#,
                Some("arn:aws:iam::123456789012:user/rafael"),
            ),
        )
        .await;

        assert_eq!(response.status(), 201);
        assert_eq!(
            json_body(&response)["user_id"],
            "aws:123456789012:user/rafael",
            "the body must never influence the namespace"
        );
    }

    #[tokio::test]
    async fn a_request_without_a_caller_arn_is_rejected() {
        // This is what a misconfigured integration looks like: payload format
        // 2.0 carries no identity, so failing closed is the only safe answer.
        let service = service();
        let response = route(
            service.as_ref(),
            event(
                "POST",
                "/memories",
                r#""{\"kind\":\"fact\",\"text\":\"x\"}""#,
                None,
            ),
        )
        .await;

        assert_eq!(response.status(), 401);
        assert_eq!(json_body(&response)["error"]["code"], "unidentified_caller");
    }

    #[tokio::test]
    async fn recall_rejects_an_out_of_range_top_k_before_paying_for_an_embedding() {
        let service = service();
        let response = route(
            service.as_ref(),
            event(
                "POST",
                "/memories/search",
                r#""{\"query\":\"coffee\",\"top_k\":101}""#,
                Some("arn:aws:iam::123456789012:user/rafael"),
            ),
        )
        .await;

        assert_eq!(response.status(), 400);
        assert_eq!(json_body(&response)["error"]["code"], "invalid_input");
        assert_eq!(
            service.embedder().calls(),
            0,
            "validation must short-circuit before the paid Bedrock call"
        );
    }

    #[tokio::test]
    async fn a_full_remember_then_recall_round_trip_works() {
        let service = service();
        let caller = Some("arn:aws:iam::123456789012:user/rafael");

        let created = route(
            service.as_ref(),
            event(
                "POST",
                "/memories",
                r#""{\"kind\":\"preference\",\"text\":\"I prefer pour-over coffee\"}""#,
                caller,
            ),
        )
        .await;
        assert_eq!(created.status(), 201);
        let memory_id = json_body(&created)["memory_id"]
            .as_str()
            .expect("an id")
            .to_string();

        let recalled = route(
            service.as_ref(),
            event(
                "POST",
                "/memories/search",
                r#""{\"query\":\"coffee\"}""#,
                caller,
            ),
        )
        .await;
        assert_eq!(recalled.status(), 200);
        let results = json_body(&recalled);
        assert_eq!(results["results"][0]["memory_id"], memory_id);
        assert!(
            results["results"][0]["distance"].is_number(),
            "hits carry a distance, where lower means closer"
        );

        let fetched = route(
            service.as_ref(),
            event("GET", &format!("/memories/{memory_id}"), "null", caller),
        )
        .await;
        assert_eq!(fetched.status(), 200);

        let deleted = route(
            service.as_ref(),
            event("DELETE", &format!("/memories/{memory_id}"), "null", caller),
        )
        .await;
        assert_eq!(deleted.status(), 204);

        let missing = route(
            service.as_ref(),
            event("GET", &format!("/memories/{memory_id}"), "null", caller),
        )
        .await;
        assert_eq!(missing.status(), 404);
    }

    #[tokio::test]
    async fn two_sso_sessions_of_one_role_reach_the_same_memories() {
        let service = service();
        route(
            service.as_ref(),
            event(
                "POST",
                "/memories",
                r#""{\"kind\":\"fact\",\"text\":\"the standup is at nine\"}""#,
                Some("arn:aws:sts::123456789012:assumed-role/Developer/session-monday"),
            ),
        )
        .await;

        let response = route(
            service.as_ref(),
            event(
                "POST",
                "/memories/search",
                r#""{\"query\":\"standup\"}""#,
                Some("arn:aws:sts::123456789012:assumed-role/Developer/session-tuesday"),
            ),
        )
        .await;

        assert_eq!(response.status(), 200);
        assert_eq!(
            json_body(&response)["results"].as_array().map(Vec::len),
            Some(1),
            "a new SSO session must not lose yesterday's memories"
        );
    }

    #[tokio::test]
    async fn an_unknown_route_is_a_404() {
        let service = service();
        let response = route(
            service.as_ref(),
            event(
                "PATCH",
                "/memories",
                "null",
                Some("arn:aws:iam::123456789012:user/rafael"),
            ),
        )
        .await;
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn a_malformed_body_is_a_400() {
        let service = service();
        let response = route(
            service.as_ref(),
            event(
                "POST",
                "/memories",
                r#""not json at all""#,
                Some("arn:aws:iam::123456789012:user/rafael"),
            ),
        )
        .await;
        assert_eq!(response.status(), 400);
        assert_eq!(json_body(&response)["error"]["code"], "invalid_request");
    }
}
