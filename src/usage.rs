//! One `usage` log event per tool call: who called which tool with what,
//! how it went, how long it took and how many ogc-dggs-api requests it
//! fanned out to. Filter the logs on target `usage` to count calls and
//! callers; with `LOG_FORMAT=json` each event is one JSON line.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolRequestParams, CallToolResponse};
use rmcp::service::RequestContext;
use rmcp::RoleServer;

/// The authenticated caller's Auth0 `sub`, put on the HTTP request by the
/// bearer middleware (`src/http_transport.rs`) once its token validated.
#[derive(Clone, Debug)]
pub struct CallerId(pub String);

tokio::task_local! {
    /// Backend requests sent while the current tool call runs. A task-local
    /// rather than a field, because `ApiClient` is shared by every
    /// concurrent call; the per-zone fan-out runs on the call's own task
    /// (`bounded` polls the futures in place), so all of it lands here.
    static BACKEND_REQUESTS: Arc<AtomicU32>;
}

/// Counts one request to ogc-dggs-api against the running tool call, if
/// there is one (retries count too: each is a real request).
pub fn count_backend_request() {
    let _ = BACKEND_REQUESTS.try_with(|count| count.fetch_add(1, Ordering::Relaxed));
}

/// `sub` over HTTP; `stdio` for the local transport, whose one caller is
/// whoever started the process.
fn caller(ctx: &RequestContext<RoleServer>) -> String {
    ctx.extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<CallerId>())
        .map(|caller| caller.0.clone())
        .unwrap_or_else(|| "stdio".to_string())
}

/// Runs one tool call and logs its `usage` event.
pub async fn record<F>(
    request: &CallToolRequestParams,
    ctx: &RequestContext<RoleServer>,
    call: F,
) -> Result<CallToolResponse, McpError>
where
    F: Future<Output = Result<CallToolResponse, McpError>>,
{
    let caller = caller(ctx);
    let tool = request.name.to_string();
    let arguments = request
        .arguments
        .as_ref()
        .map(|args| serde_json::Value::Object(args.clone()).to_string())
        .unwrap_or_default();

    let backend_requests = Arc::new(AtomicU32::new(0));
    let started = Instant::now();
    let result = BACKEND_REQUESTS.scope(backend_requests.clone(), call).await;
    let duration_ms = started.elapsed().as_millis() as u64;
    let backend_requests = backend_requests.load(Ordering::Relaxed);

    let (outcome, error) = match &result {
        Ok(CallToolResponse::Complete(done)) if done.is_error == Some(true) => ("tool_error", None),
        Ok(_) => ("ok", None),
        Err(err) => ("error", Some(err.message.to_string())),
    };
    tracing::info!(
        target: "usage",
        %caller,
        %tool,
        %arguments,
        outcome,
        error,
        duration_ms,
        backend_requests,
        "tool call"
    );
    result
}
