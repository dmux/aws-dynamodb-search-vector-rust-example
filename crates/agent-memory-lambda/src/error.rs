//! Mapping domain and adapter failures onto HTTP.

use agent_memory_contract::ErrorResponse;
use agent_memory_core::MemoryError;
use lambda_http::{Body, Response};

use crate::principal::PrincipalError;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    Domain(#[from] MemoryError),

    #[error(transparent)]
    Principal(#[from] PrincipalError),

    #[error("{0}")]
    BadRequest(String),

    #[error("no route matches {method} {path}")]
    NotFound { method: String, path: String },
}

impl ApiError {
    fn status(&self) -> u16 {
        match self {
            // A caller who cannot be identified is not merely malformed: with
            // AWS_IAM authorization on the route, an unidentified request means
            // the gateway is not passing through what we expect.
            Self::Principal(_) => 401,
            Self::BadRequest(_) => 400,
            Self::NotFound { .. } => 404,
            Self::Domain(error) if error.is_invalid_input() => 400,
            Self::Domain(_) => 500,
        }
    }

    /// Stable, machine-readable code. Clients may branch on these; the
    /// human-readable message may change freely.
    fn code(&self) -> &'static str {
        match self {
            Self::Principal(_) => "unidentified_caller",
            Self::BadRequest(_) => "invalid_request",
            Self::NotFound { .. } => "not_found",
            Self::Domain(error) if error.is_invalid_input() => "invalid_input",
            Self::Domain(_) => "internal_error",
        }
    }

    /// Render the error, deliberately withholding internals on 5xx.
    ///
    /// A repository or Bedrock failure carries SDK detail that belongs in
    /// CloudWatch, not in a response body, so the client gets a fixed sentence
    /// while the full chain is logged.
    pub fn into_response(self) -> Response<Body> {
        let status = self.status();
        let code = self.code();

        let message = if status >= 500 {
            tracing::error!(error = ?self, "request failed");
            "internal error".to_string()
        } else {
            tracing::warn!(error = %self, status, "request rejected");
            self.to_string()
        };

        let payload = ErrorResponse::new(code, message);
        let body = serde_json::to_string(&payload).unwrap_or_else(|_| {
            r#"{"error":{"code":"internal_error","message":"internal error"}}"#.to_string()
        });

        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap_or_else(|_| Response::new(Body::from(body_fallback())))
    }
}

fn body_fallback() -> &'static str {
    r#"{"error":{"code":"internal_error","message":"internal error"}}"#
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_of(error: ApiError) -> (u16, serde_json::Value) {
        let response = error.into_response();
        let status = response.status().as_u16();
        let body: serde_json::Value = serde_json::from_slice(response.body().as_ref())
            .expect("the error body is always valid JSON");
        (status, body)
    }

    #[test]
    fn bad_input_is_a_400_that_explains_itself() {
        let (status, body) = body_of(ApiError::Domain(MemoryError::TopKOutOfRange {
            requested: 101,
            max: 100,
        }));
        assert_eq!(status, 400);
        assert_eq!(body["error"]["code"], "invalid_input");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("a message")
                .contains("101"),
            "a 4xx should tell the caller what was wrong"
        );
    }

    #[test]
    fn downstream_failures_are_500s_that_reveal_nothing() {
        let (status, body) = body_of(ApiError::Domain(MemoryError::repository(
            "dynamodb says: AccessDeniedException for arn:aws:dynamodb:...",
        )));
        assert_eq!(status, 500);
        assert_eq!(body["error"]["code"], "internal_error");
        assert_eq!(
            body["error"]["message"], "internal error",
            "SDK detail belongs in CloudWatch, not in a response body"
        );
    }

    #[test]
    fn an_unidentified_caller_is_401_not_400() {
        let (status, body) = body_of(ApiError::Principal(PrincipalError::Missing));
        assert_eq!(status, 401);
        assert_eq!(body["error"]["code"], "unidentified_caller");
    }

    #[test]
    fn an_unknown_route_is_404() {
        let (status, _) = body_of(ApiError::NotFound {
            method: "PATCH".into(),
            path: "/memories".into(),
        });
        assert_eq!(status, 404);
    }
}
