//! The `DggsServer` MCP server.
//!
//! - `tools/`: what MCP clients see, one file per group of tools —
//!   `insights` (`list_insights`, `get_insight`), `collections`
//!   (`list_collections`, `get_collection`), `data` (`get_data`, with its
//!   output shaping in `data/rows` and `data/parquet`), `stats`
//!   (`get_call_stats`). Each keeps the helper methods only it uses.
//! - `dggs/`: internal, shared by the tools — `extent` (point/bbox
//!   arguments), `zones` (grid, depth and zone lookups), `aggregate`
//!   (per-property statistics).

mod dggs;
mod tools;

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool_handler};
use serde_json::Value;

use crate::client::ApiClient;

/// Caps how many zones a single `get_insight`/`get_data` call will fan out
/// to, so an oversized area (or too-fine a zone level) can't trigger
/// thousands of backend requests.
const MAX_ZONES_PER_QUERY: usize = 64;

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
    pub fn new(client: ApiClient) -> Self {
        Self {
            client: Arc::new(client),
            tool_router: Self::insights_router()
                + Self::collections_router()
                + Self::data_router()
                + Self::stats_router(),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DggsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Tools for geoinsight's ogc-dggs-api. Start with list_insights to see available \
             concepts (NDVI, burn scar analysis, ...), then get_insight(insight, lon/lat or bbox) \
             to get one aggregated value for a location or area — it handles DGGS zone \
             resolution and cross-zone aggregation internally. If you already know which \
             collection(s)/field(s) you want instead of a named insight, use get_data \
             (optionally joining fields from several collections in one call) for the raw, \
             unaggregated per-zone values. list_collections/get_collection are a secondary \
             path for browsing raw dataset metadata. In stdio mode, list_insights and \
             get_insight need this server's own Auth0 client-credentials configured (they call \
             the bearer-token-gated /concepts route); get_data, list_collections and \
             get_collection work without it. In HTTP mode, every request needs its own bearer \
             token instead, which is forwarded to ogc-dggs-api as-is. get_call_stats reports how \
             many backend HTTP requests this session has made in total, since a single \
             get_insight/get_data call can fan out to many.",
        )
    }
}
