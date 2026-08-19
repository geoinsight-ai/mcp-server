use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::Auth0ValidationConfig;

/// Claims of an incoming caller's JWT. Mirrors ogc-dggs-api's own
/// `Claims`/`Audience` shape (`ogc-dggs-api/src/models/auth/claims.rs`) so
/// both services agree on what a valid token looks like.
#[derive(Debug, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[allow(dead_code)]
    pub iss: String,
    #[allow(dead_code)]
    pub aud: Audience,
    #[allow(dead_code)]
    pub exp: usize,
    #[serde(default)]
    #[allow(dead_code)]
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
#[allow(dead_code)]
pub enum Audience {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Debug)]
pub enum BearerAuthError {
    Malformed,
    Invalid(String),
    UnknownKey,
    Unavailable(String),
}

impl std::fmt::Display for BearerAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BearerAuthError::Malformed => write!(f, "malformed bearer token"),
            BearerAuthError::Invalid(msg) => write!(f, "invalid bearer token: {msg}"),
            BearerAuthError::UnknownKey => write!(f, "no matching JWK for this token's kid"),
            BearerAuthError::Unavailable(msg) => write!(f, "could not fetch JWKS: {msg}"),
        }
    }
}

struct CachedJwks {
    keys: JwkSet,
    fetched_at: Instant,
}

const JWKS_TTL: Duration = Duration::from_secs(600);

/// Validates a caller-supplied bearer token against Auth0's JWKS
/// (RS256/JWKS, same mechanism ogc-dggs-api itself uses to validate tokens —
/// see `ogc-dggs-api/src/services/auth.rs`). This is defense-in-depth and UX
/// (a clean 401 instead of a wasted round-trip, and knowing `sub` for
/// logging): ogc-dggs-api re-validates every forwarded token itself
/// regardless, so a forged/expired token is rejected there even if this
/// check somehow had a bug.
pub struct BearerValidator {
    domain: String,
    audience: String,
    http: reqwest::Client,
    jwks_cache: Mutex<Option<CachedJwks>>,
}

impl BearerValidator {
    pub fn new(config: Auth0ValidationConfig, http: reqwest::Client) -> Self {
        Self {
            domain: config.domain,
            audience: config.audience,
            http,
            jwks_cache: Mutex::new(None),
        }
    }

    async fn jwks(&self, force_refresh: bool) -> Result<JwkSet, BearerAuthError> {
        let mut cache = self.jwks_cache.lock().await;

        if !force_refresh {
            if let Some(cached) = cache.as_ref() {
                if cached.fetched_at.elapsed() < JWKS_TTL {
                    return Ok(cached.keys.clone());
                }
            }
        }

        let url = format!("https://{}/.well-known/jwks.json", self.domain);
        let keys = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| BearerAuthError::Unavailable(e.to_string()))?
            .error_for_status()
            .map_err(|e| BearerAuthError::Unavailable(e.to_string()))?
            .json::<JwkSet>()
            .await
            .map_err(|e| BearerAuthError::Unavailable(e.to_string()))?;

        *cache = Some(CachedJwks {
            keys: keys.clone(),
            fetched_at: Instant::now(),
        });

        Ok(keys)
    }

    /// Validate a raw token (no `Bearer ` prefix). On success, `Claims.sub`
    /// identifies the caller (an Auth0 M2M client id — one per customer).
    pub async fn validate(&self, token: &str) -> Result<Claims, BearerAuthError> {
        let header = decode_header(token).map_err(|_| BearerAuthError::Malformed)?;
        let kid = header.kid.ok_or(BearerAuthError::Malformed)?;

        let mut jwks = self.jwks(false).await?;
        let jwk = match jwks.find(&kid) {
            Some(jwk) => jwk.clone(),
            // Auth0 rotates signing keys occasionally; one forced refetch
            // before giving up, in case our cache is just stale.
            None => {
                jwks = self.jwks(true).await?;
                jwks.find(&kid).cloned().ok_or(BearerAuthError::UnknownKey)?
            }
        };

        let decoding_key = DecodingKey::try_from(&jwk).map_err(|_| BearerAuthError::UnknownKey)?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(std::slice::from_ref(&self.audience));
        validation.set_issuer(&[format!("https://{}/", self.domain)]);

        decode::<Claims>(token, &decoding_key, &validation)
            .map(|data| data.claims)
            .map_err(|e| BearerAuthError::Invalid(e.to_string()))
    }
}
