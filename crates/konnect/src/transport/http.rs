//! Streamable HTTP transport backed by the official Rust MCP SDK (`rmcp`).
//!
//! Konnect used to implement Streamable HTTP directly in Axum. That was enough
//! for permissive clients such as MCP Inspector, but it duplicated protocol
//! details that stricter clients rely on: Accept negotiation, SSE framing,
//! legacy `Mcp-Session-Id` lifecycle, GET stream behavior, version negotiation,
//! and the transition to the 2026 stateless lifecycle.
//!
//! The HTTP edge now delegates those protocol mechanics to
//! `rmcp::StreamableHttpService`, the same transport family used by fs-mcp-rs.
//! Konnect's existing `McpHandler` remains the single source of truth for tool
//! discovery and execution; `RmcpAdapter` is intentionally a thin typed bridge.

use anyhow::Result;
use axum::{
    http::HeaderMap,
    middleware::{self, Next},
    response::Response,
    routing::get,
    Router,
};
use konnect_core::mcp::handler::McpHandler;
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult as RmcpCallToolResult,
        Implementation, ListToolsResult as RmcpListToolsResult, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData as McpError, RoleServer, ServerHandler,
};
use serde_json::{json, Value};
use tracing::info;

#[derive(Clone)]
struct RmcpAdapter {
    handler: McpHandler,
}

impl RmcpAdapter {
    fn new(handler: McpHandler) -> Self {
        Self { handler }
    }

    /// Drive the existing Konnect MCP handler without reimplementing tool
    /// routing in the transport adapter. rmcp owns the wire protocol; Konnect
    /// still owns the semantic tool catalogue and execution behavior.
    async fn konnect_result(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let response = self
            .handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": "rmcp-adapter",
                "method": method,
                "params": params,
            }))
            .await
            .ok_or_else(|| {
                McpError::internal_error(
                    format!("Konnect handler returned no response for {method}"),
                    None,
                )
            })?;

        if let Some(error) = response.error {
            return Err(McpError::internal_error(error.message, error.data));
        }

        response.result.ok_or_else(|| {
            McpError::internal_error(
                format!("Konnect handler returned an empty result for {method}"),
                None,
            )
        })
    }
}

impl ServerHandler for RmcpAdapter {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("konnect", env!("CARGO_PKG_VERSION"))
                .with_description("KiCad MCP server with deterministic semantic tooling"),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<RmcpListToolsResult, McpError> {
        let value = self.konnect_result("tools/list", json!({})).await?;
        serde_json::from_value(value).map_err(|error| {
            McpError::internal_error(
                format!("Could not convert Konnect tools/list result to rmcp: {error}"),
                None,
            )
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        // `CallToolRequestParams` is a superset of Konnect's legacy
        // `{name, arguments}` shape. Serde ignores the newer request metadata
        // fields when the existing handler parses it, preserving one execution
        // path for both transports.
        let params = serde_json::to_value(request).map_err(|error| {
            McpError::internal_error(
                format!("Could not encode tools/call request: {error}"),
                None,
            )
        })?;
        let value = self.konnect_result("tools/call", params).await?;
        let result: RmcpCallToolResult = serde_json::from_value(value).map_err(|error| {
            McpError::internal_error(
                format!("Could not convert Konnect tools/call result to rmcp: {error}"),
                None,
            )
        })?;
        Ok(result.into())
    }
}

/// Run Konnect over standards-compliant MCP Streamable HTTP.
pub async fn run_http(handler: McpHandler, addr: &str) -> Result<()> {
    let adapter = RmcpAdapter::new(handler);

    // Keep rmcp's legacy session mode and SSE response behavior at their
    // defaults. Those defaults are the important compatibility behavior we
    // were missing in the hand-written transport: initialize creates a
    // `Mcp-Session-Id`, subsequent requests are routed through that session,
    // and simple responses may be SSE-framed.
    //
    // Konnect binds to loopback and is intentionally exposed through the
    // user's Caddy/Tailscale proxy. Disable rmcp's loopback-only Host allowlist
    // here because the reverse proxy preserves the public Host header. Once the
    // connection is proven, this can be tightened to configured proxy hosts.
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts();

    let mcp_service: StreamableHttpService<RmcpAdapter, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(adapter.clone()),
            LocalSessionManager::default().into(),
            config,
        );

    let app = Router::new()
        .route("/health", get(handle_health))
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn(log_request));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(
        "rmcp Streamable HTTP transport listening on http://{}/mcp",
        addr
    );

    axum::serve(listener, app).await?;
    Ok(())
}

/// Observability only. Protocol validation and response construction belong to
/// rmcp; this middleware deliberately does not alter or reject requests.
async fn log_request(headers: HeaderMap, request: axum::extract::Request, next: Next) -> Response {
    let protocol_version = headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-");
    let session_id = headers
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-");
    let accept = headers
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-");

    info!(
        http_method = %request.method(),
        uri = %request.uri(),
        protocol_version,
        session_id,
        accept,
        "mcp_http_request"
    );

    let response = next.run(request).await;
    info!(
        status = %response.status(),
        response_session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-"),
        content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-"),
        "mcp_http_response"
    );
    response
}

async fn handle_health() -> &'static str {
    "ok"
}
