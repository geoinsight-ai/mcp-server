//! The `DggsServer` MCP server.
//!
//! - `tools/`: what MCP clients see, one file per group of tools —
//!   `insights` (`list_insights`, `get_insight`), `collections`
//!   (`list_collections`, `get_collection`), `data` (`get_data`, with its
//!   output shaping in `data/rows` and `data/parquet`), `grid` (`get_zone`,
//!   `list_zones`: grid navigation without data), `stats`
//!   (`get_call_stats`). Each keeps the helper methods only it uses.
//! - `dggs/`: internal, shared by the tools — `extent` (point/bbox
//!   arguments), `zones` (grid, depth and zone lookups), `aggregate`
//!   (per-property statistics).

mod dggs;
mod tools;

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolRequestParams, CallToolResponse, ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool_handler};
use serde_json::Value;

use crate::client::ApiClient;

/// Caps how many zones a single `get_insight`/`get_data` call will fan out
/// to, so an oversized area (or too-fine a zone level) can't trigger
/// thousands of backend requests.
const MAX_ZONES_PER_QUERY: usize = 64;

/// How many per-zone backend requests one tool call keeps in flight. An area
/// query can fan out to `MAX_ZONES_PER_QUERY` zones, and on a live STAC
/// collection each one is its own upstream catalogue search: all 64 at once
/// got Copernicus to rate-limit Jasper National Park, and a burst of 8
/// still had half refused, while 4 at a time got through. (`ApiClient` also
/// retries rate-limited requests.)
const MAX_CONCURRENT_ZONE_REQUESTS: usize = 4;

/// Runs `futures` at most `MAX_CONCURRENT_ZONE_REQUESTS` at a time, returning
/// their outputs in input order (callers zip them back to their zone ids).
async fn bounded<F: std::future::Future>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    use futures::stream::StreamExt;
    // Collected first: streaming a lazy `.map(|zone| ..)` iterator directly
    // trips rustc's "FnOnce is not general enough" inside the `Send` tool
    // futures. Futures don't start until polled, so this changes nothing.
    let futures: Vec<F> = futures.into_iter().collect();
    futures::stream::iter(futures)
        .buffered(MAX_CONCURRENT_ZONE_REQUESTS)
        .collect()
        .await
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn enc(segment: &str) -> String {
    urlencoding::encode(segment).into_owned()
}

/// A failed per-zone fetch, reported alongside the results that did succeed
/// (`get_data`'s rows, `get_insight`'s values). Carries the backend's own
/// `hint` when it gave one.
fn zone_error(zone_id: &str, collection_id: Option<&str>, err: &McpError) -> Value {
    let hint = err.data.as_ref().and_then(|d| d.get("hint")).cloned();
    serde_json::json!({
        "zone_id": zone_id,
        "collection_id": collection_id,
        "message": err.message,
        "hint": hint,
    })
}

/// The error for a call where every sampled zone failed. Clients often show
/// a model only an error's message, not its data, so the backend's reason
/// (e.g. "invalid collection id") goes into the message itself; the
/// per-zone failures stay in the data.
fn no_data_error(errors: Vec<Value>) -> McpError {
    let reason = errors
        .iter()
        .find_map(|e| e["hint"].as_str().or_else(|| e["message"].as_str()))
        .unwrap_or("unknown error");
    McpError::invalid_params(
        format!("no data for any sampled zone ({} failed); first failure: {reason}", errors.len()),
        Some(serde_json::json!({ "errors": errors })),
    )
}

/// The caller's own bearer token, when this request arrived over the
/// customer-facing HTTP transport (`src/http_transport.rs`) with a validated
/// `Authorization` header. `None` on stdio — nothing ever inserts
/// `http::request::Parts` into a stdio request's extensions, so this simply
/// falls through to the server's own Auth0 client-credentials in `ApiClient`.
/// This extraction is infallible by construction: `RequestContext<RoleServer>`
/// and `Extensions::get` never error, they just return `None` when absent —
/// unlike `rmcp::handler::server::tool::Extension<T>`, which *requires* `T`
/// to be present and would break every stdio call if used here instead.
fn caller_bearer_token(ctx: &RequestContext<RoleServer>) -> Option<String> {
    let parts = ctx.extensions.get::<http::request::Parts>()?;
    let header = parts.headers.get(http::header::AUTHORIZATION)?;
    let value = header.to_str().ok()?;
    Some(value.strip_prefix("Bearer ").unwrap_or(value).to_string())
}

#[derive(Clone)]
pub struct DggsServer {
    client: Arc<ApiClient>,
    // Read by the generated `ServerHandler` impl from `#[tool_handler]`;
    // dead-code analysis doesn't see through the macro expansion.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl DggsServer {
    /// Every tool, including `get_call_stats`: a stdio process serves one
    /// caller, so its process-wide request count is that caller's own.
    pub fn for_stdio(client: ApiClient) -> Self {
        Self {
            client: Arc::new(client),
            tool_router: Self::data_tools() + Self::stats_router(),
        }
    }

    /// Every tool except `get_call_stats`: over HTTP one process serves every
    /// customer, so its process-wide request count would expose all of
    /// their load to each of them.
    pub fn for_http(client: ApiClient) -> Self {
        Self {
            client: Arc::new(client),
            tool_router: Self::data_tools(),
        }
    }

    fn data_tools() -> ToolRouter<Self> {
        Self::insights_router() + Self::collections_router() + Self::data_router() + Self::grid_router()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DggsServer {
    /// Defined here, so `#[tool_handler]` doesn't generate it, only to
    /// log each call's `usage` event around the router's own dispatch.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let (logged, ctx) = (request.clone(), context.clone());
        crate::usage::record(&logged, &ctx, self.tool_router.call(ToolCallContext::new(self, request, context))).await
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Tools for geoinsight's ogc-dggs-api. Start with list_insights to see available \
             concepts (NDVI, burn scar analysis, ...), then get_insight(insight, lon/lat or bbox) \
             to get one aggregated value for a location or area — it handles DGGS zone \
             resolution and cross-zone aggregation internally. If you already know which \
             collection(s)/field(s) you want instead of a named insight, use get_data \
             (optionally joining fields from several collections in one call) for the raw, \
             unaggregated per-zone values. If no insight covers what the user asks for (an \
             index, a ratio, a difference), write the formula yourself: read the fields and \
             the description with get_collection, then pass get_data's `derived_fields` and \
             the API calculates it for each zone; list_collections(the place's lon/lat or bbox, plus a keyword `query`) \
             finds the collections with data there, and get_collection their fields. list_zones \
             and get_zone move around the DGGS grid itself (the zones covering a place, a \
             zone's parent, children and neighbours) without reading any data. In stdio mode, list_insights and \
             get_insight need this server's own Auth0 client-credentials configured (they call \
             the bearer-token-gated /concepts route); get_data, list_collections and \
             get_collection work without it. In HTTP mode, every request needs its own bearer \
             token instead, which is forwarded to ogc-dggs-api as-is. In stdio mode only, \
             get_call_stats reports how many backend HTTP requests this server has made in \
             total, since a single get_insight/get_data call can fan out to many.",
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn bounded_caps_concurrency_and_keeps_order() {
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let outputs = bounded((0..40).map(|i| {
            let (in_flight, peak) = (&in_flight, &peak);
            async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                // Later items finish first, so ordering is really tested.
                tokio::time::sleep(Duration::from_millis(40 - i)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                i
            }
        }))
        .await;
        assert_eq!(outputs, (0..40).collect::<Vec<_>>());
        assert_eq!(peak.load(Ordering::SeqCst), MAX_CONCURRENT_ZONE_REQUESTS);
    }

    fn client() -> ApiClient {
        ApiClient::new(&crate::config::AppConfig {
            base_url: "http://localhost:3001".to_string(),
            public_base_url: "http://localhost:3001".to_string(),
            transport: crate::config::Transport::Stdio,
            auth0: None,
            http_auth: None,
        })
    }

    #[test]
    fn call_stats_are_only_exposed_over_stdio() {
        let stdio = DggsServer::for_stdio(client());
        let http = DggsServer::for_http(client());

        assert!(stdio.tool_router.has_route("get_call_stats"));
        assert!(!http.tool_router.has_route("get_call_stats"));
        for tool in ["list_insights", "get_insight", "get_data", "list_collections", "get_collection", "get_zone", "list_zones"] {
            assert!(stdio.tool_router.has_route(tool) && http.tool_router.has_route(tool), "{tool}");
        }
    }
}
