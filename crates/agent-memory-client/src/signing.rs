//! SigV4 request signing for `execute-api`.

use std::time::SystemTime;

use aws_config::SdkConfig;
use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4;

/// The service name API Gateway signs under. Getting this wrong produces a
/// signature mismatch that looks like a credentials problem.
const SERVICE: &str = "execute-api";

#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    /// Names the profile, because "no credentials" and "no credentials *for the
    /// profile you asked for*" send the reader to different places — and the
    /// second is what happens when a launcher forgets to pass one and the
    /// default profile turns out to be empty.
    #[error("no AWS credentials for profile `{profile}`; run `aws sso login --profile {profile}`")]
    NoCredentialsForProfile {
        profile: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(
        "no AWS credentials available and no profile was requested, so the default chain was \
         used; set AGENT_MEMORY_PROFILE or AWS_PROFILE, or set AWS_ACCESS_KEY_ID"
    )]
    NoCredentials(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("no AWS region configured; set AWS_REGION")]
    NoRegion,

    #[error("failed to sign the request")]
    Sign(#[source] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, Clone)]
pub struct Signer {
    config: SdkConfig,
    /// The profile the config was built from, if one was named. Carried only so
    /// a credentials failure can say which one it tried; `SdkConfig` does not
    /// remember.
    profile: Option<String>,
}

impl Signer {
    pub fn new(config: SdkConfig) -> Self {
        Self {
            config,
            profile: None,
        }
    }

    /// Record which named profile this signer's credentials came from.
    pub fn for_profile(mut self, profile: Option<String>) -> Self {
        self.profile = profile;
        self
    }

    fn no_credentials(&self, source: Box<dyn std::error::Error + Send + Sync>) -> SigningError {
        match &self.profile {
            Some(profile) => SigningError::NoCredentialsForProfile {
                profile: profile.clone(),
                source,
            },
            None => SigningError::NoCredentials(source),
        }
    }

    /// Produce the `Authorization` and related headers for one request.
    ///
    /// The body is signed too, so the payload cannot be altered in flight
    /// without invalidating the signature.
    pub async fn sign(
        &self,
        method: &str,
        url: &str,
        payload: &[u8],
    ) -> Result<Vec<(String, String)>, SigningError> {
        let provider = self
            .config
            .credentials_provider()
            .ok_or_else(|| self.no_credentials("no provider configured".into()))?;
        let credentials = provider
            .provide_credentials()
            .await
            .map_err(|error| self.no_credentials(Box::new(error)))?;
        let region = self.config.region().ok_or(SigningError::NoRegion)?;

        let identity = credentials.into();
        let mut settings = SigningSettings::default();
        settings.signature_location = SignatureLocation::Headers;

        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(region.as_ref())
            .name(SERVICE)
            .time(SystemTime::now())
            .settings(settings)
            .build()
            .map_err(|error| SigningError::Sign(Box::new(error)))?;

        let signable = SignableRequest::new(
            method,
            url,
            std::iter::empty(),
            SignableBody::Bytes(payload),
        )
        .map_err(|error| SigningError::Sign(Box::new(error)))?;

        let (instructions, _signature) = sign(signable, &params.into())
            .map_err(|error| SigningError::Sign(Box::new(error)))?
            .into_parts();

        let (headers, _query) = instructions.into_parts();
        Ok(headers
            .into_iter()
            .map(|header| (header.name().to_string(), header.value().to_string()))
            .collect())
    }
}
