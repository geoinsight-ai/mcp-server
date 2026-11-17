use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use http::{HeaderValue, StatusCode, header::{AUTHORIZATION, WWW_AUTHENTICATE}};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};

use crate::bearer_auth::BearerValidator;
use crate::client::ApiClient;
use crate::server::DggsServer;

/// OAuth 2.0 Protected Resource Metadata (RFC 9728), which is how an MCP
/// client that only has this server's URL (claude.ai connectors) finds out
/// that Auth0 issues its tokens: the 401 points at the metadata document,
/// the document names Auth0, and the client runs the login there. Nothing
/// in here is secret — it's the same tenant URL any login page reveals.
#[derive(Clone)]
pub struct OAuthDiscovery {
    metadata: serde_json::Value,
    /// Pre-built `WWW-Authenticate` value for every 401.
    challenge: HeaderValue,
}

impl OAuthDiscovery {
    /// `public_url` is this server's origin (no trailing slash);
    /// `auth0_domain` a bare host, as `Auth0ValidationConfig` stores it.
    pub fn new(public_url: &str, auth0_domain: &str) -> Self {
        let metadata = serde_json::json!({
            // Must be the exact URL the client connects to, or a
            // spec-following client refuses the metadata.
            "resource": format!("{public_url}/mcp"),
            // Auth0's issuer identifier, trailing slash included.
            "authorization_servers": [format!("https://{auth0_domain}/")],
            "bearer_methods_supported": ["header"],
        });
        let challenge = HeaderValue::from_str(&format!(
            "Bearer resource_metadata=\"{public_url}/.well-known/oauth-protected-resource\""
        ))
        .expect("MCP_PUBLIC_URL must be a valid header value");
        Self { metadata, challenge }
    }
}

#[derive(Clone)]
struct AuthState {
    validator: Arc<BearerValidator>,
    discovery: Option<OAuthDiscovery>,
}

/// Runs the customer-facing HTTP transport: `POST/GET /mcp` behind a bearer
/// check, plus an unauthenticated `/healthz` for k8s probes. Rejection of a
/// missing/invalid token happens here, in ordinary axum middleware, before
/// the request body ever reaches `rmcp`'s JSON-RPC dispatch or any `#[tool]`
/// — the token itself is left on the request unchanged (not consumed), so
/// each `#[tool]` fn can read it back via `RequestContext::extensions` (see
/// `caller_bearer_token` in `src/server.rs`) and forward it to ogc-dggs-api.
pub async fn serve(
    client: ApiClient,
    bind_addr: SocketAddr,
    allowed_hosts: Vec<String>,
    validator: Arc<BearerValidator>,
    discovery: Option<OAuthDiscovery>,
) -> anyhow::Result<()> {
    let server = DggsServer::for_http(client);

    let config = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts);

    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let protected = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            AuthState { validator, discovery: discovery.clone() },
            require_bearer,
        ));

    let mut app = Router::new().route("/healthz", get(|| async { "ok" }));
    if let Some(discovery) = discovery {
        let metadata = Json(discovery.metadata);
        // Both the root form (what our 401 points at) and the path-suffixed
        // form RFC 9728 clients probe when a 401 carries no hint.
        app = app
            .route("/.well-known/oauth-protected-resource", get({
                let metadata = metadata.clone();
                || async move { metadata }
            }))
            .route("/.well-known/oauth-protected-resource/mcp", get(|| async move { metadata }));
    }
    let app = app.merge(protected);

    tracing::info!(%bind_addr, "ogc-dggs-mcp listening (HTTP transport)");
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn require_bearer(
    State(AuthState { validator, discovery }): State<AuthState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(token) = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).to_string())
    else {
        return unauthorized(discovery.as_ref());
    };

    match validator.validate(&token).await {
        Ok(claims) => {
            // The only thing done with `sub` here is logging who called —
            // the actual authorization decision downstream is ogc-dggs-api
            // re-validating this same forwarded token itself.
            tracing::info!(sub = %claims.sub, "authenticated MCP request");
            next.run(request).await
        }
        Err(err) => {
            tracing::info!(%err, "rejected MCP request: invalid bearer token");
            unauthorized(discovery.as_ref())
        }
    }
}

fn unauthorized(discovery: Option<&OAuthDiscovery>) -> Response {
    // Same wording ogc-dggs-api itself uses for an unauthenticated request
    // (`adapters/http/auth_extractor.rs`), so the product has one consistent
    // auth-error voice end to end.
    let mut response = (StatusCode::UNAUTHORIZED, "You don't have permission").into_response();
    if let Some(discovery) = discovery {
        response.headers_mut().insert(WWW_AUTHENTICATE, discovery.challenge.clone());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_points_the_challenge_at_metadata_naming_auth0() {
        let discovery = OAuthDiscovery::new("https://mcp.example.com", "tenant.eu.auth0.com");
        assert_eq!(discovery.metadata["resource"], "https://mcp.example.com/mcp");
        assert_eq!(discovery.metadata["authorization_servers"][0], "https://tenant.eu.auth0.com/");
        assert_eq!(
            discovery.challenge,
            "Bearer resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource\""
        );
    }
}
