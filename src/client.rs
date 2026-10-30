use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rmcp::ErrorData as McpError;
use serde_json::Value;

use crate::auth::TokenProvider;
use crate::config::AppConfig;

/// reqwest has no timeout by default, so without these a stalled backend (or
/// Auth0 token endpoint, which shares this client) would hang a tool call
/// forever. The overall timeout is generous because a single zone-data
/// request can legitimately take a while on a cold backend.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// A rate-limited request is retried this many times before its error is
/// returned, waiting `Retry-After` if the response gives one, otherwise
/// `BASE_BACKOFF` doubled each time (~1s, ~2s, ~4s, jittered — see
/// `jittered`), capped at `MAX_BACKOFF`.
const MAX_RETRIES: u32 = 3;
const BASE_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Whether a response means "slow down" rather than a real failure. A live
/// STAC collection (e.g. sentinel-2-l2a) makes ogc-dggs-api search
/// Copernicus per request, and when Copernicus rate-limits that search the
/// API wraps it in a 500 ("STAC search failed: ... 429 Too Many Requests")
/// rather than passing a 429 through — so both are treated alike.
fn is_rate_limited(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || (status.is_server_error() && body.contains("Too Many Requests"))
}

/// `delay` scaled by a factor in [0.5, 1.5). Requests rate-limited in the
/// same burst would otherwise all retry at the same moment and be refused
/// together again. The sub-second clock is plenty random for spreading
/// retries; it isn't used for anything that needs real randomness.
fn jittered(delay: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    delay.mul_f64(0.5 + f64::from(nanos % 1_000_000) / 1_000_000.0)
}

/// `Retry-After` in seconds, when the response carries one (the HTTP-date
/// form is ignored and falls back to the exponential backoff).
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let seconds = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

/// Thin HTTP client for the ogc-dggs-api. Tools build a path and a list of
/// (already-formatted) query pairs; this client attaches auth, sends the
/// request, and turns the response into either a JSON `Value` or an
/// `McpError` that carries the backend's own error body along.
pub struct ApiClient {
    http: reqwest::Client,
    base_url: String,
    public_base_url: String,
    token_provider: Option<TokenProvider>,
    /// Total requests sent to ogc-dggs-api this process, logged per-request
    /// (see `send`) so a single tool call's real fan-out (e.g. `get_insight`
    /// hitting several zones) is visible, not just the one MCP tool call.
    request_count: AtomicU64,
}

impl ApiClient {
    pub fn new(config: &AppConfig) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("ogc-dggs-mcp/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("failed to build reqwest client");

        let token_provider = config
            .auth0
            .clone()
            .map(|auth0| TokenProvider::new(auth0, http.clone()));

        Self {
            http,
            base_url: config.base_url.clone(),
            public_base_url: config.public_base_url.clone(),
            token_provider,
            request_count: AtomicU64::new(0),
        }
    }

    /// Base URL of the ogc-dggs-api instance as the caller reaches it, for
    /// building links the caller fetches itself (see `get_data`'s
    /// `parquet_requests`) — not the one this client sends requests to.
    pub fn public_base_url(&self) -> &str {
        &self.public_base_url
    }

    /// Requests sent to ogc-dggs-api so far this process.
    pub fn request_count(&self) -> u64 {
        self.request_count.load(Ordering::Relaxed)
    }

    /// GET `{base_url}{path}` with the given query pairs. `caller_token`, when
    /// set (HTTP mode), is forwarded as-is and takes priority over this
    /// server's own Auth0 client-credentials — each customer's requests are
    /// authenticated as them, not as one shared service identity. Empty
    /// `path` segments and absent query values should be filtered by the
    /// caller before this is called.
    pub async fn get_json(
        &self,
        path: &str,
        query: &[(&str, String)],
        caller_token: Option<&str>,
    ) -> Result<Value, McpError> {
        let url = format!("{}{}", self.base_url, path);
        let request = self.http.get(&url).query(query);
        self.send(request, "GET", url, caller_token).await
    }

    /// POST `{base_url}{path}` with a JSON body and the given query pairs.
    /// See [`Self::get_json`] for `caller_token`.
    pub async fn post_json(
        &self,
        path: &str,
        query: &[(&str, String)],
        body: &Value,
        caller_token: Option<&str>,
    ) -> Result<Value, McpError> {
        let url = format!("{}{}", self.base_url, path);
        let request = self.http.post(&url).query(query).json(body);
        self.send(request, "POST", url, caller_token).await
    }

    async fn send(
        &self,
        mut request: reqwest::RequestBuilder,
        method: &'static str,
        url: String,
        caller_token: Option<&str>,
    ) -> Result<Value, McpError> {
        if let Some(token) = caller_token {
            request = request.header("Authorization", format!("Bearer {token}"));
        } else if let Some(token_provider) = &self.token_provider {
            let token = token_provider.token().await.map_err(|err| {
                McpError::internal_error(
                    format!("failed to obtain Auth0 access token: {err}"),
                    None,
                )
            })?;
            request = request.header("Authorization", format!("Bearer {token}"));
        }

        let mut attempt = 0;
        let (status, body) = loop {
            // JSON and query-only requests always clone; nothing here sends
            // a streaming body.
            let this_try = request
                .try_clone()
                .ok_or_else(|| McpError::internal_error("request to ogc-dggs-api can't be retried", None))?;
            let request_number = self.request_count.fetch_add(1, Ordering::Relaxed) + 1;

            let response = this_try.send().await.map_err(|err| {
                tracing::info!(request_number, method, %url, error = %err, "ogc-dggs-api call failed");
                McpError::internal_error(
                    format!("request to ogc-dggs-api ({url}) failed: {err}"),
                    None,
                )
            })?;

            let status = response.status();
            tracing::info!(request_number, method, %url, status = status.as_u16(), "ogc-dggs-api call");
            let wait = retry_after(response.headers());
            let body = response.text().await.unwrap_or_default();

            if attempt < MAX_RETRIES && is_rate_limited(status, &body) {
                let delay = wait.unwrap_or_else(|| jittered(BASE_BACKOFF * 2u32.pow(attempt))).min(MAX_BACKOFF);
                tracing::info!(request_number, %url, attempt, delay_ms = delay.as_millis() as u64, "rate-limited, retrying");
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }
            break (status, body);
        };

        if !status.is_success() {
            let data = serde_json::from_str::<Value>(&body).ok();
            let message = format!("ogc-dggs-api returned {status} for {url}");
            return Err(if status.as_u16() == 404 {
                McpError::resource_not_found(message, data)
            } else if status.as_u16() == 401 || status.as_u16() == 403 {
                McpError::invalid_request(message, data)
            } else if status.is_client_error() {
                McpError::invalid_params(message, data)
            } else {
                McpError::internal_error(message, data)
            });
        }

        serde_json::from_str::<Value>(&body).map_err(|err| {
            McpError::internal_error(
                format!("ogc-dggs-api returned non-JSON body: {err}"),
                Some(Value::String(body)),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

    use super::*;
    use crate::config::Transport;

    #[test]
    fn rate_limits_are_recognised_even_when_wrapped_in_a_500() {
        assert!(is_rate_limited(reqwest::StatusCode::TOO_MANY_REQUESTS, ""));
        assert!(is_rate_limited(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"hint":"STAC search failed: HTTP status client error (429 Too Many Requests) for url"}"#,
        ));
        assert!(!is_rate_limited(reqwest::StatusCode::INTERNAL_SERVER_ERROR, r#"{"hint":"No regions: bbox is empty"}"#));
        assert!(!is_rate_limited(reqwest::StatusCode::NOT_FOUND, "Too Many Requests"));
    }

    /// A local ogc-dggs-api stand-in whose `/thing` answers the first
    /// `fail_first` calls with `failure`, then 200; counts every call.
    async fn mock_api(fail_first: usize, failure: (StatusCode, &'static str)) -> (ApiClient, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let app = Router::new().route(
            "/thing",
            get(move || {
                let n = seen.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < fail_first {
                        // Retry-After: 0 keeps the test fast.
                        (failure.0, [("retry-after", "0")], failure.1).into_response()
                    } else {
                        (StatusCode::OK, r#"{"ok":true}"#).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = ApiClient::new(&AppConfig {
            base_url: format!("http://{addr}"),
            public_base_url: format!("http://{addr}"),
            transport: Transport::Stdio,
            auth0: None,
            http_auth: None,
        });
        (client, calls)
    }

    #[test]
    fn jitter_stays_within_half_to_one_and_a_half() {
        for _ in 0..100 {
            let d = jittered(Duration::from_secs(2));
            assert!(d >= Duration::from_secs(1) && d < Duration::from_secs(3), "{d:?}");
        }
    }

    #[tokio::test]
    async fn a_rate_limited_request_is_retried_until_it_succeeds() {
        let (client, calls) = mock_api(2, (StatusCode::TOO_MANY_REQUESTS, "slow down")).await;
        let body = client.get_json("/thing", &[], None).await.unwrap();
        assert_eq!(body, serde_json::json!({"ok": true}));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(client.request_count(), 3);
    }

    #[tokio::test]
    async fn a_wrapped_upstream_rate_limit_gives_up_after_the_retries() {
        let wrapped = r#"{"code":500,"hint":"STAC search failed: (429 Too Many Requests)"}"#;
        let (client, calls) = mock_api(usize::MAX, (StatusCode::INTERNAL_SERVER_ERROR, wrapped)).await;
        let err = client.get_json("/thing", &[], None).await.unwrap_err();
        assert!(err.message.contains("500"));
        assert_eq!(calls.load(Ordering::SeqCst), 1 + MAX_RETRIES as usize);
    }

    #[tokio::test]
    async fn other_server_errors_are_not_retried() {
        let (client, calls) = mock_api(usize::MAX, (StatusCode::INTERNAL_SERVER_ERROR, r#"{"hint":"bbox is empty"}"#)).await;
        assert!(client.get_json("/thing", &[], None).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
