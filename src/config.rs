use std::env;
use std::net::SocketAddr;

/// Auth0 machine-to-machine (client-credentials) configuration this server
/// uses to mint its OWN token when calling endpoints on ogc-dggs-api that
/// require a bearer token (e.g. `/concepts`). Only meaningful in stdio mode,
/// where this process has one fixed identity. Optional: unauthenticated
/// tools keep working without it.
#[derive(Debug, Clone)]
pub struct Auth0Config {
    pub domain: String,
    pub client_id: String,
    pub client_secret: String,
    pub audience: String,
}

/// Domain + audience needed to validate an *incoming caller's* JWT
/// (JWKS/RS256). Distinct from [`Auth0Config`]: no client id/secret — HTTP
/// mode never mints its own token, it only checks and forwards the caller's.
#[derive(Debug, Clone)]
pub struct Auth0ValidationConfig {
    pub domain: String,
    pub audience: String,
}

#[derive(Debug, Clone)]
pub enum Transport {
    /// Local process, one fixed caller identity via `Auth0Config`. Used by
    /// Claude Desktop/Code today.
    Stdio,
    /// Customer-facing HTTP service: every request must carry its own bearer
    /// token, validated against `Auth0ValidationConfig` and forwarded as-is
    /// to ogc-dggs-api. No server-wide service identity involved.
    Http {
        bind_addr: SocketAddr,
        /// Hostnames the `Host` header is allowed to carry (DNS-rebinding
        /// guard). Left at rmcp's loopback-only default for local runs;
        /// MUST be set to the real ingress hostname(s) in any real
        /// deployment or every request 400s on Host validation.
        allowed_hosts: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Base URL of the ogc-dggs-api instance, e.g. `http://localhost:30001`.
    pub base_url: String,
    pub transport: Transport,
    /// Only set (and only used) for stdio mode.
    pub auth0: Option<Auth0Config>,
    /// Only set (and required) for HTTP mode.
    pub http_auth: Option<Auth0ValidationConfig>,
}

/// `AUTH0_DOMAIN` is documented (and used internally here) as a bare host
/// with no scheme, e.g. `geoinsight-dev.eu.auth0.com` — but ogc-dggs-api's
/// own deployed value for the very same tenant is `https://geoinsight-dev.eu.auth0.com`
/// (confirmed in `deployment/creodias/ogc-dggs-api/overlays/*/patch-env.yaml`),
/// and the two must reference the same tenant/audience by construction. An
/// operator copying that value verbatim is the likely case, not an edge
/// case, so accept either form here instead of documenting a gotcha and
/// hoping nobody copy-pastes it wrong.
fn strip_scheme(domain: String) -> String {
    domain
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

impl AppConfig {
    pub fn from_env() -> Self {
        let base_url = env::var("OGC_DGGS_API_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:3001".to_string())
            .trim_end_matches('/')
            .to_string();

        let auth0 = match (
            env::var("AUTH0_DOMAIN"),
            env::var("AUTH0_CLIENT_ID"),
            env::var("AUTH0_CLIENT_SECRET"),
            env::var("AUTH0_AUDIENCE"),
        ) {
            (Ok(domain), Ok(client_id), Ok(client_secret), Ok(audience)) => Some(Auth0Config {
                domain: strip_scheme(domain),
                client_id,
                client_secret,
                audience,
            }),
            _ => None,
        };

        let transport_kind = env::var("MCP_TRANSPORT").unwrap_or_else(|_| "stdio".to_string());
        let transport = match transport_kind.as_str() {
            "stdio" => Transport::Stdio,
            "http" => {
                let bind_addr = env::var("MCP_HTTP_BIND_ADDR")
                    .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
                    .parse::<SocketAddr>()
                    .unwrap_or_else(|e| panic!("MCP_HTTP_BIND_ADDR is not a valid address: {e}"));
                let allowed_hosts = match env::var("MCP_HTTP_ALLOWED_HOST") {
                    Ok(hosts) => hosts.split(',').map(|h| h.trim().to_string()).collect(),
                    Err(_) => vec!["localhost".to_string(), "127.0.0.1".to_string(), "::1".to_string()],
                };
                Transport::Http { bind_addr, allowed_hosts }
            }
            other => panic!("MCP_TRANSPORT must be \"stdio\" or \"http\", got {other:?}"),
        };

        // HTTP mode validates every caller's own token instead of minting
        // one service-wide token, so it needs domain+audience regardless of
        // whether the four AUTH0_* vars above happen to be set too — there
        // is no safe way to serve a single request without a validator.
        let http_auth = match (env::var("AUTH0_DOMAIN"), env::var("AUTH0_AUDIENCE")) {
            (Ok(domain), Ok(audience)) => Some(Auth0ValidationConfig {
                domain: strip_scheme(domain),
                audience,
            }),
            _ => None,
        };
        if matches!(transport, Transport::Http { .. }) && http_auth.is_none() {
            panic!(
                "MCP_TRANSPORT=http requires AUTH0_DOMAIN and AUTH0_AUDIENCE to validate incoming callers"
            );
        }

        Self {
            base_url,
            transport,
            auth0,
            http_auth,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_scheme_handles_both_forms_and_trailing_slash() {
        assert_eq!(strip_scheme("geoinsight-dev.eu.auth0.com".to_string()), "geoinsight-dev.eu.auth0.com");
        assert_eq!(strip_scheme("https://geoinsight-dev.eu.auth0.com".to_string()), "geoinsight-dev.eu.auth0.com");
        assert_eq!(strip_scheme("http://geoinsight-dev.eu.auth0.com".to_string()), "geoinsight-dev.eu.auth0.com");
        assert_eq!(strip_scheme("https://geoinsight-dev.eu.auth0.com/".to_string()), "geoinsight-dev.eu.auth0.com");
    }
}
