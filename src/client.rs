use std::sync::atomic::{AtomicU64, Ordering};

use rmcp::ErrorData as McpError;
use serde_json::Value;

use crate::auth::TokenProvider;
use crate::config::AppConfig;

/// Thin HTTP client for the ogc-dggs-api. Tools build a path and a list of
/// (already-formatted) query pairs; this client attaches auth, sends the
/// request, and turns the response into either a JSON `Value` or an
/// `McpError` that carries the backend's own error body along.
pub struct ApiClient {
    http: reqwest::Client,
    base_url: String,
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
            .build()
            .expect("failed to build reqwest client");

        let token_provider = config
            .auth0
            .clone()
            .map(|auth0| TokenProvider::new(auth0, http.clone()));

        Self {
            http,
            base_url: config.base_url.clone(),
            token_provider,
            request_count: AtomicU64::new(0),
        }
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

        let request_number = self.request_count.fetch_add(1, Ordering::Relaxed) + 1;

        let response = request.send().await.map_err(|err| {
            tracing::info!(request_number, method, %url, error = %err, "ogc-dggs-api call failed");
            McpError::internal_error(
                format!("request to ogc-dggs-api ({url}) failed: {err}"),
                None,
            )
        })?;

        let status = response.status();
        tracing::info!(request_number, method, %url, status = status.as_u16(), "ogc-dggs-api call");

        let body = response.text().await.unwrap_or_default();

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
