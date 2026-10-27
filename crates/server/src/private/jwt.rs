//! Verifying Cloudflare Access's `Cf-Access-Jwt-Assertion` header.
//!
//! Access signs a short-lived RS256 JWT for every request it lets through.
//! Its signing keys are published at
//! `https://<team>.cloudflareaccess.com/cdn-cgi/access/certs` and rotate (old
//! and new are published side by side for a while). Keys are cached by `kid`;
//! an unknown `kid` refetches the set (at most every [`REFETCH_MIN`]), and the
//! cache is refreshed in the background of the next request after [`MAX_AGE`].

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;

/// Refresh keys older than this.
const MAX_AGE: Duration = Duration::from_secs(3600);
/// An unknown `kid` refetches the keys at most this often (a flood of forged
/// tokens must not turn into a flood of requests to Cloudflare).
const REFETCH_MIN: Duration = Duration::from_secs(10);
const LEEWAY_SECS: u64 = 30;

/// Where signing keys come from (Cloudflare; a fixed set in tests).
pub trait KeySource: Send + Sync {
    fn fetch<'a>(&'a self, team_domain: &'a str) -> BoxFuture<'a, anyhow::Result<JwkSet>>;
}

/// Fetches `https://<team>/cdn-cgi/access/certs`.
pub struct CloudflareCerts;

impl KeySource for CloudflareCerts {
    fn fetch<'a>(&'a self, team_domain: &'a str) -> BoxFuture<'a, anyhow::Result<JwkSet>> {
        Box::pin(async move {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let http = reqwest::Client::builder()
                .user_agent(concat!("modula-server/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?;
            let url = format!("https://{team_domain}/cdn-cgi/access/certs");
            let res = http.get(&url).send().await?.error_for_status()?;
            Ok(res.json::<JwkSet>().await?)
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JwtError {
    #[error("no Cloudflare Access token")]
    Missing,
    #[error("malformed Cloudflare Access token")]
    Malformed,
    #[error("Cloudflare Access token signed by an unknown key")]
    UnknownKey,
    #[error("invalid Cloudflare Access token: {0}")]
    Invalid(String),
    #[error("cannot fetch Cloudflare Access signing keys")]
    KeysUnavailable,
}

/// What Modula reads from a verified token.
#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    /// The signed-in person (absent for service tokens).
    #[serde(default)]
    pub email: Option<String>,
    /// A service token's client id (absent for people).
    #[serde(default)]
    pub common_name: Option<String>,
}

struct Keys {
    by_kid: HashMap<String, DecodingKey>,
    fetched_at: Option<Instant>,
}

pub struct Verifier {
    team_domain: String,
    issuer: String,
    aud: String,
    source: Arc<dyn KeySource>,
    keys: RwLock<Keys>,
    /// Serialises refetches.
    refetch: tokio::sync::Mutex<()>,
    refetch_min: Duration,
}

impl Verifier {
    pub fn new(team_domain: &str, aud: &str, source: Arc<dyn KeySource>) -> Self {
        let team_domain = team_domain
            .trim()
            .trim_end_matches('/')
            .to_ascii_lowercase();
        Self {
            issuer: format!("https://{team_domain}"),
            team_domain,
            aud: aud.to_owned(),
            source,
            keys: RwLock::new(Keys {
                by_kid: HashMap::new(),
                fetched_at: None,
            }),
            refetch: tokio::sync::Mutex::new(()),
            refetch_min: REFETCH_MIN,
        }
    }

    /// How soon an unknown `kid` may fetch the keys again (default 10 s).
    pub fn refetch_after(mut self, d: Duration) -> Self {
        self.refetch_min = d;
        self
    }

    pub fn team_domain(&self) -> &str {
        &self.team_domain
    }

    fn key(&self, kid: &str) -> Option<DecodingKey> {
        crate::sync::read(&self.keys).by_kid.get(kid).cloned()
    }

    fn age(&self) -> Option<Duration> {
        crate::sync::read(&self.keys)
            .fetched_at
            .map(|t| t.elapsed())
    }

    /// Fetch the key set again unless someone just did. Failures keep the old keys.
    async fn refresh(&self, force_min_age: Duration) -> Result<(), JwtError> {
        let _guard = self.refetch.lock().await;
        if self.age().is_some_and(|a| a < force_min_age) {
            return Ok(());
        }
        let set = match self.source.fetch(&self.team_domain).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), team = %self.team_domain, "cannot fetch Access signing keys");
                // Don't hammer Cloudflare: count the attempt as a fetch.
                crate::sync::write(&self.keys).fetched_at = Some(Instant::now());
                return Err(JwtError::KeysUnavailable);
            }
        };
        let by_kid = set
            .keys
            .iter()
            .filter_map(|k| {
                let kid = k.common.key_id.clone()?;
                DecodingKey::from_jwk(k).ok().map(|d| (kid, d))
            })
            .collect();
        *crate::sync::write(&self.keys) = Keys {
            by_kid,
            fetched_at: Some(Instant::now()),
        };
        Ok(())
    }

    /// Checks signature, `aud`, `iss` and `exp`.
    pub async fn verify(&self, token: &str) -> Result<Claims, JwtError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(JwtError::Missing);
        }
        let header = jsonwebtoken::decode_header(token).map_err(|_| JwtError::Malformed)?;
        if header.alg != Algorithm::RS256 {
            return Err(JwtError::Invalid(format!(
                "unexpected algorithm {:?}",
                header.alg
            )));
        }
        let kid = header.kid.ok_or(JwtError::Malformed)?;
        if self.age().is_none_or(|a| a > MAX_AGE) {
            // Stale keys are still tried if the refresh fails.
            let _ = self.refresh(Duration::ZERO).await;
        }
        let key = match self.key(&kid) {
            Some(k) => k,
            None => {
                // Possibly a rotation: look again.
                let _ = self.refresh(self.refetch_min).await;
                self.key(&kid).ok_or(JwtError::UnknownKey)?
            }
        };
        let mut v = Validation::new(Algorithm::RS256);
        v.set_audience(&[&self.aud]);
        v.set_issuer(&[&self.issuer]);
        v.set_required_spec_claims(&["exp", "aud", "iss"]);
        v.leeway = LEEWAY_SECS;
        jsonwebtoken::decode::<Claims>(token, &key, &v)
            .map(|d| d.claims)
            .map_err(|e| JwtError::Invalid(e.to_string()))
    }
}
