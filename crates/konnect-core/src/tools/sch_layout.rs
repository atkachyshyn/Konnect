//! `sch_layout` toolset - deterministic local schematic layout planning.

use crate::tool;
use crate::tools::{sch_wiring, ToolDef};
use serde_json::json;

pub fn tools() -> Vec<ToolDef> {
    vec![tool!(
        "optimize_schematic_layout",
        "Preview or apply deterministic Top-K local schematic layout candidates for selected items and nets. \
         This is a bounded local planner: it resolves semantic connectivity, rejects foreign conductive \
         contacts before staging, routes orthogonal wires with progressive A* bounds, and applies only an \
         explicitly selected candidate with the exact dry-run plan_revision.",
        json!({
            "type": "object",
            "properties": {
                "schematic": { "type": "string" },
                "item_uuids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional schematic item UUIDs whose connected nets bound the local planning request."
                },
                "net_names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional existing net names to plan locally."
                },
                "candidate_count": {
                    "type": "integer",
                    "default": 3,
                    "maximum": 5
                },
                "grid": { "type": "number", "default": 1.27 },
                "dry_run": { "type": "boolean", "default": true },
                "candidate_id": {
                    "type": "string",
                    "description": "Required when dry_run=false."
                },
                "plan_revision": {
                    "type": "string",
                    "description": "Exact dry-run revision required when dry_run=false."
                }
            },
            "required": ["schematic"]
        }),
        |args, ctx| async move { sch_wiring::handle_optimize_schematic_layout(args, ctx).await }
    )]
}
