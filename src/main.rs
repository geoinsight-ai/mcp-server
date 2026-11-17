mod auth;
mod bearer_auth;
mod client;
mod config;
mod http_transport;
mod server;

use std::sync::Arc;

use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use bearer_auth::BearerValidator;
use client::ApiClient;
use config::{AppConfig, Transport};
use server::DggsServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout is the MCP transport in stdio mode (JSON-RPC over stdio); all
    // logging must go to stderr or it corrupts the protocol stream. HTTP
    // mode doesn't use stdout for protocol, but stderr logging is kept
    // consistent across both.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = AppConfig::from_env();
    let client = ApiClient::new(&config);

    match config.transport {
        Transport::Stdio => {
            tracing::info!(
                base_url = %config.base_url,
                auth0_configured = config.auth0.is_some(),
                "starting ogc-dggs-mcp (stdio)"
            );

            let server = DggsServer::for_stdio(client);
            let service = server.serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
        }
        Transport::Http { bind_addr, allowed_hosts, public_url } => {
            // Checked in AppConfig::from_env(): HTTP mode always has
            // http_auth set, or the process already panicked at startup.
            let http_auth = config
                .http_auth
                .expect("AppConfig::from_env guarantees http_auth is set when transport is Http");

            tracing::info!(
                base_url = %config.base_url,
                public_base_url = %config.public_base_url,
                %bind_addr,
                ?allowed_hosts,
                ?public_url,
                "starting ogc-dggs-mcp (http)"
            );

            let discovery = public_url.map(|url| http_transport::OAuthDiscovery::new(&url, &http_auth.domain));
            let validator = Arc::new(BearerValidator::new(http_auth, reqwest::Client::new()));
            http_transport::serve(client, bind_addr, allowed_hosts, validator, discovery).await?;
        }
    }

    Ok(())
}
