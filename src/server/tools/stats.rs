use rmcp::{tool, tool_router};

use crate::server::{DggsServer, pretty};

#[tool_router(router = stats_router, vis = "pub(crate)")]
impl DggsServer {
    #[tool(
        description = "How many HTTP requests this server has sent to ogc-dggs-api so far this session. Useful for understanding real load: a single get_insight/get_data call can fan out to many backend requests (one per sampled zone, plus lookups), not just one."
    )]
    async fn get_call_stats(&self) -> String {
        pretty(&serde_json::json!({
            "requests_to_ogc_dggs_api": self.client.request_count(),
        }))
    }
}
