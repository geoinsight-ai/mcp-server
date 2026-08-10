mod auth;
mod client;
mod config;
mod server;

use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use client::ApiClient;
use config::AppConfig;
use server::DggsServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout is the MCP transport (JSON-RPC over stdio); all logging must go
    // to stderr or it corrupts the protocol stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = AppConfig::from_env();
    tracing::info!(base_url = %config.base_url, auth0_configured = config.auth0.is_some(), "starting ogc-dggs-mcp");

    let client = ApiClient::new(&config);
    let server = DggsServer::new(client);

    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;

    Ok(())
}
