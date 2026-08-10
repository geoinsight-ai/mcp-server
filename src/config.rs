use std::env;

/// Auth0 machine-to-machine (client-credentials) configuration for calling
/// endpoints on the ogc-dggs-api that require a bearer token (e.g.
/// `/concepts`). Optional: unauthenticated tools keep working without it.
#[derive(Debug, Clone)]
pub struct Auth0Config {
    pub domain: String,
    pub client_id: String,
    pub client_secret: String,
    pub audience: String,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Base URL of the ogc-dggs-api instance, e.g. `http://localhost:30001`.
    pub base_url: String,
    pub auth0: Option<Auth0Config>,
}

impl AppConfig {
    pub fn from_env() -> Self {
        let base_url = env::var("OGC_DGGS_API_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:30001".to_string())
            .trim_end_matches('/')
            .to_string();

        let auth0 = match (
            env::var("AUTH0_DOMAIN"),
            env::var("AUTH0_CLIENT_ID"),
            env::var("AUTH0_CLIENT_SECRET"),
            env::var("AUTH0_AUDIENCE"),
        ) {
            (Ok(domain), Ok(client_id), Ok(client_secret), Ok(audience)) => Some(Auth0Config {
                domain,
                client_id,
                client_secret,
                audience,
            }),
            _ => None,
        };

        Self { base_url, auth0 }
    }
}
