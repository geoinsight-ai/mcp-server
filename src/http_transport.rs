use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use http::{StatusCode, header::AUTHORIZATION};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};

use crate::bearer_auth::BearerValidator;
use crate::client::ApiClient;
use crate::server::DggsServer;

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
) -> anyhow::Result<()> {
    let server = DggsServer::new(client);

    let config = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts);

    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let protected = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(validator, require_bearer));

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected);

    tracing::info!(%bind_addr, "ogc-dggs-mcp listening (HTTP transport)");
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn require_bearer(
    State(validator): State<Arc<BearerValidator>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(token) = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).to_string())
    else {
        return unauthorized();
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
            unauthorized()
        }
    }
}

fn unauthorized() -> Response {
    // Same wording ogc-dggs-api itself uses for an unauthenticated request
    // (`adapters/http/auth_extractor.rs`), so the product has one consistent
    // auth-error voice end to end.
    (StatusCode::UNAUTHORIZED, "You don't have permission").into_response()
}
