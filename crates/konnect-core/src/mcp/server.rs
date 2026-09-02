//! MCP server session context.

use super::protocol::*;
use crate::router::ToolRouter;
use std::sync::Arc;

pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
pub const LATEST_LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";

const LEGACY_PROTOCOL_VERSIONS: &[&str] = &[
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

pub struct McpServerState {
    pub router: Arc<ToolRouter>,
}

impl McpServerState {
    pub fn new(router: Arc<ToolRouter>) -> Self {
        McpServerState { router }
    }

    pub fn build_initialize_result(
        requested_version: Option<&str>,
        tool_list_changed: bool,
    ) -> InitializeResult {
        let protocol_version = requested_version
            .filter(|version| LEGACY_PROTOCOL_VERSIONS.contains(version))
            .unwrap_or(LATEST_LEGACY_PROTOCOL_VERSION);
        InitializeResult {
            protocol_version: protocol_version.to_string(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(tool_list_changed),
                }),
                ..Default::default()
            },
            server_info: ServerInfo {
                name: "konnect".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        }
    }

    pub fn server_info_json() -> serde_json::Value {
        serde_json::json!({
            "name": "konnect",
            "version": env!("CARGO_PKG_VERSION")
        })
    }
}
