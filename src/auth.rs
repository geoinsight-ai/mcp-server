use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::Auth0Config;

/// Fetches and caches an Auth0 machine-to-machine access token via the
/// client-credentials grant, refreshing it shortly before it expires.
pub struct TokenProvider {
    config: Auth0Config,
    http: reqwest::Client,
    cached: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

/// Refresh this long before actual expiry so a token never goes stale
/// mid-request on a slow connection.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

impl TokenProvider {
    pub fn new(config: Auth0Config, http: reqwest::Client) -> Self {
        Self {
            config,
            http,
            cached: Mutex::new(None),
        }
    }

    pub async fn token(&self) -> anyhow::Result<String> {
        let mut cached = self.cached.lock().await;

        if let Some(token) = cached.as_ref() {
            if token.expires_at > Instant::now() {
                return Ok(token.access_token.clone());
            }
        }

        let url = format!("https://{}/oauth/token", self.config.domain);
        let response = self
            .http
            .post(url)
            .json(&serde_json::json!({
                "grant_type": "client_credentials",
                "client_id": self.config.client_id,
                "client_secret": self.config.client_secret,
                "audience": self.config.audience,
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<TokenResponse>()
            .await?;

        let expires_at =
            Instant::now() + Duration::from_secs(response.expires_in).saturating_sub(EXPIRY_MARGIN);
        let access_token = response.access_token.clone();

        *cached = Some(CachedToken {
            access_token: response.access_token,
            expires_at,
        });

        Ok(access_token)
    }
}
