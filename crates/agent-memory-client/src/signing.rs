//! SigV4 request signing for `execute-api`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use aws_config::SdkConfig;
use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4;

/// The service name API Gateway signs under. Getting this wrong produces a
/// signature mismatch that looks like a credentials problem.
const SERVICE: &str = "execute-api";

/// How close to expiry a cached credential stops being reused.
///
/// A signature is checked when the request arrives, not when it was made, so the
/// only thing this has to cover is the flight time of one request plus the clock
/// skew between here and API Gateway. A minute is generous for both, and cheap:
/// it gives away the last minute of a credential that is usually good for an
/// hour.
const REFRESH_BUFFER: Duration = Duration::from_secs(60);

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

    /// The last credentials resolved, reused until they are nearly expired.
    ///
    /// # Why the signer caches at all
    ///
    /// `SdkConfig::credentials_provider` hands back the credential *chain*, not
    /// a cache: the SDK keeps those as two separate settings, and only its
    /// generated clients consult the identity cache. This crate signs by hand,
    /// so it gets the chain — and resolving it means running the whole chain
    /// again. On an SSO profile that is a `GetRoleCredentials` round trip,
    /// measured at roughly half a second per call against a live session, paid
    /// by *every* memory operation, in a path whose whole job is to feel
    /// immediate.
    ///
    /// Caching here does not weaken renewal, which is the reason the resolution
    /// was per-request in the first place. The entry is dropped a minute before
    /// it expires, and re-resolving runs the same chain — so a token the SDK
    /// refreshed in the background, or a fresh `aws sso login` performed while
    /// this process was running, is still picked up without a restart. What
    /// changes is only how often the question is asked: once an hour instead of
    /// once a request.
    ///
    /// Shared across clones on purpose: `RemoteMemoryService` is cheap to clone
    /// and a per-clone cache would quietly restore the old behaviour.
    cached: Arc<Mutex<Option<Credentials>>>,
}

impl Signer {
    pub fn new(config: SdkConfig) -> Self {
        Self {
            config,
            profile: None,
            cached: Arc::new(Mutex::new(None)),
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
        let credentials = self.credentials().await?;
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

    /// Credentials that will still be valid when the request they sign arrives.
    async fn credentials(&self) -> Result<Credentials, SigningError> {
        if let Some(credentials) = self.cached(SystemTime::now()) {
            return Ok(credentials);
        }

        let provider = self
            .config
            .credentials_provider()
            .ok_or_else(|| self.no_credentials("no provider configured".into()))?;
        let credentials = provider
            .provide_credentials()
            .await
            .map_err(|error| self.no_credentials(Box::new(error)))?;

        // Two requests arriving together both resolve, and the second overwrites
        // the first with an equivalent value. That is the deliberate trade: the
        // alternative is holding the lock across the await, which would queue
        // every request behind one network call — and with a `std::sync` guard
        // would not even compile, since the future stops being `Send`.
        *self.entry() = Some(credentials.clone());
        Ok(credentials)
    }

    /// The cached credentials, if they are still usable at `now`.
    fn cached(&self, now: SystemTime) -> Option<Credentials> {
        let entry = self.entry();
        let credentials = entry.as_ref()?;
        is_fresh(credentials, now).then(|| credentials.clone())
    }

    /// A poisoned lock guards nothing dangerous here — the worst state it can
    /// hold is a credential that a panicking thread was about to replace — so
    /// the cache is recovered rather than allowed to take the process down.
    fn entry(&self) -> std::sync::MutexGuard<'_, Option<Credentials>> {
        self.cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether `credentials` can still be used at `now`.
///
/// No expiry means static credentials — access keys from the environment or a
/// credentials file — which never go stale and never need resolving twice.
fn is_fresh(credentials: &Credentials, now: SystemTime) -> bool {
    match credentials.expiry() {
        None => true,
        Some(expiry) => expiry > now + REFRESH_BUFFER,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aws_credential_types::provider::SharedCredentialsProvider;
    use aws_credential_types::provider::future;

    use super::*;

    /// Counts how often the chain was actually asked, which is the whole point:
    /// on an SSO profile each of these is a network round trip.
    #[derive(Debug)]
    struct CountingProvider {
        calls: Arc<AtomicUsize>,
        valid_for: Duration,
    }

    impl ProvideCredentials for CountingProvider {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            future::ProvideCredentials::ready(Ok(Credentials::new(
                "AKIAEXAMPLE",
                "secret",
                None,
                Some(SystemTime::now() + self.valid_for),
                "counting-test",
            )))
        }
    }

    fn signer_over(calls: &Arc<AtomicUsize>, valid_for: Duration) -> Signer {
        Signer::new(
            SdkConfig::builder()
                .region(aws_config::Region::new("us-east-1"))
                .credentials_provider(SharedCredentialsProvider::new(CountingProvider {
                    calls: Arc::clone(calls),
                    valid_for,
                }))
                .build(),
        )
    }

    async fn sign_once(signer: &Signer) {
        signer
            .sign("GET", "https://example.com/memories", b"")
            .await
            .expect("the fake provider always resolves");
    }

    /// The regression this cache exists for. Before it, every signed request
    /// re-ran the credential chain — on an SSO profile, a `GetRoleCredentials`
    /// call of about half a second in front of every search and every capture.
    #[tokio::test]
    async fn signing_repeatedly_resolves_the_chain_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let signer = signer_over(&calls, Duration::from_secs(3600));

        for _ in 0..5 {
            sign_once(&signer).await;
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "five requests must cost one credential resolution, not five"
        );
    }

    /// The other half: caching must not outlive the credentials. These expire
    /// inside the buffer, so every request has to go back to the chain — which
    /// is also the path that picks up a refreshed SSO token.
    #[tokio::test]
    async fn credentials_about_to_expire_are_resolved_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let signer = signer_over(&calls, Duration::from_secs(30));

        sign_once(&signer).await;
        sign_once(&signer).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a credential inside the refresh buffer must not be reused"
        );
    }

    /// Clones share the cache. `RemoteMemoryService` is cheap to clone, and a
    /// per-clone cache would restore the per-request round trip without anyone
    /// noticing.
    #[tokio::test]
    async fn a_clone_signs_with_the_credentials_the_original_resolved() {
        let calls = Arc::new(AtomicUsize::new(0));
        let signer = signer_over(&calls, Duration::from_secs(3600));

        sign_once(&signer).await;
        sign_once(&signer.clone()).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Access keys from the environment carry no expiry, and re-resolving them
    /// would be pure waste — cheap waste, but the rule has to be explicit
    /// because `None` is otherwise easy to read as "unknown, so re-resolve".
    #[test]
    fn static_credentials_never_go_stale() {
        let credentials = Credentials::new("AKIAEXAMPLE", "secret", None, None, "static-test");
        assert!(is_fresh(&credentials, SystemTime::now()));
    }

    /// The boundary, from both sides: an hour of life is reused, and the last
    /// seconds are not.
    #[test]
    fn the_refresh_buffer_is_what_decides_reuse() {
        let now = SystemTime::now();
        let expiring_at = |offset: Duration| {
            Credentials::new("AKIAEXAMPLE", "secret", None, Some(now + offset), "test")
        };

        assert!(is_fresh(&expiring_at(Duration::from_secs(3600)), now));
        assert!(is_fresh(&expiring_at(REFRESH_BUFFER * 2), now));
        assert!(!is_fresh(&expiring_at(Duration::from_secs(30)), now));
        assert!(!is_fresh(&expiring_at(Duration::from_secs(0)), now));
    }
}
