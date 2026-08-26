//! The 6 always-visible meta-tools.
//!
//! Discovery / routing:
//!   list_toolboxes()          — show every toolset with descriptions and load state
//!   load_toolset(name)        — activate a toolset, expose its tools in tools/list
//!   unload_toolset(name)      — deactivate a toolset, remove its tools from tools/list
//!   get_active_toolsets()     — list currently loaded toolsets
//!
//! Observability:
//!   get_recent_calls(limit?)  — last N tool calls (newest first) with timing + status
//!   server_stats()            — uptime, per-tool totals/errors, JSONL log path
//!
//! Dispatcher (exposed only when `ServerConfig::dispatcher_tools` is on):
//!   list_available_tools(..)  — browse the whole catalogue without loading it
//!   get_tool_schema(name)     — fetch one tool's input schema on demand
//!   execute_konnect_tool(..)  — call any registered tool, loaded or not
//!
//! At server startup only the STARTER_KIT (`project`, `config`) is pre-loaded so
//! baseline context stays small. The LLM reads `list_toolboxes` and calls
//! `load_toolset(name)` to expose the tools it actually needs for the task.
//!
//! That discovery loop depends on the client re-fetching `tools/list` when it
//! receives `notifications/tools/list_changed`. A client that caches the first
//! listing instead cannot use it at all: `load_toolset` reports the tools it
//! activated, but the client never learns their schemas, so it has nothing to
//! invoke (#134, #169). The dispatcher is the answer for those clients — three
//! tools that are always in the listing and can reach all of the rest, at a
//! fraction of the context cost of pre-loading the full catalogue.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::{CallToolResult, McpToolDescription};
use crate::tools::ToolContext;
use serde_json::{json, Value};

/// The 6 core meta-tool MCP descriptions (always in the tools/list response).
pub fn meta_tool_descriptions() -> Vec<McpToolDescription> {
    vec![
        McpToolDescription {
            name: "list_toolboxes".to_string(),
            description:
                "List all available KiCAD toolsets with descriptions, categories, tool counts, \
                 and whether each is currently loaded. Only the starter kit (project, config) \
                 is loaded at startup — call load_toolset(name) to expose additional tools \
                 in subsequent tools/list responses. Always call this first to discover what \
                 tools are available for the task."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        McpToolDescription {
            name: "load_toolset".to_string(),
            description:
                "Load a toolset by name so its tools appear in tools/list and can be called. \
                 Returns the list of tools that were added. Use list_toolboxes() first to \
                 see valid names. Pass an array to load several toolsets in one call -- \
                 cheaper, one tools/list refresh."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "anyOf": [
                            {"type": "string"},
                            {"type": "array", "items": {"type": "string"}}
                        ],
                        "description": "Toolset name (e.g. 'sch_components', 'pcb_routing'), or an array of names"
                    }
                },
                "required": ["name"]
            }),
        },
        McpToolDescription {
            name: "unload_toolset".to_string(),
            description: "Unload a toolset to remove its tools from the active session. \
                 Use this to keep the tool list manageable when switching tasks. \
                 With auto_load_toolsets enabled, tools reload on use."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Toolset name to unload"
                    }
                },
                "required": ["name"]
            }),
        },
        McpToolDescription {
            name: "get_active_toolsets".to_string(),
            description:
                "Return the list of currently loaded toolsets and how many tools each provides."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        McpToolDescription {
            name: "get_recent_calls".to_string(),
            description:
                "Return the most recent tool calls this session (newest first) with call_id, \
                 tool name, toolset, duration, status (ok/error/not_found), and \
                 error_kind when failed. Use this to self-diagnose — e.g. 'why did the last call \
                 fail?' or 'what tools have I been running?'"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Max number of calls to return (default 20, max 100). Pass 0 for all buffered calls.",
                        "default": 20
                    }
                },
                "required": []
            }),
        },
        McpToolDescription {
            name: "server_stats".to_string(),
            description:
                "Return server uptime, total/error call counts, per-tool statistics, and the \
                 path to the JSONL call log. Good for 'what's my error rate today?' and \
                 'which tool has been slowest?'."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
    ]
}

/// Attempt to handle a meta-tool call. Returns `None` if the name is not a meta-tool.
/// The 3 dispatcher tool descriptions, appended to the listing when
/// `ServerConfig::dispatcher_tools` is on.
///
/// Kept separate from [`meta_tool_descriptions`] so a client that does not need
/// them does not pay for them: they are pure overhead for a client whose tool
/// list refreshes normally.
pub fn dispatcher_tool_descriptions() -> Vec<McpToolDescription> {
    vec![
        McpToolDescription {
            name: "list_available_tools".to_string(),
            description:
                "Browse every KiCAD tool this server has, including tools whose toolset is not \
                 loaded. Call with no arguments for a cheap overview: tool names grouped by \
                 toolset, no descriptions. Pass `toolset` to get full descriptions for one \
                 toolset, or `search` to find tools by name or description across all of them. \
                 Any tool listed here can be run with execute_konnect_tool(...) right away — \
                 there is no need to load_toolset() first."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "toolset": {
                        "type": "string",
                        "description": "Only list this toolset's tools, with descriptions (e.g. 'pcb_board')"
                    },
                    "search": {
                        "type": "string",
                        "description": "Case-insensitive substring matched against tool names and descriptions"
                    },
                    "with_descriptions": {
                        "type": "boolean",
                        "description": "Include descriptions in the no-argument overview. Costs considerably more context.",
                        "default": false
                    }
                },
                "required": []
            }),
        },
        McpToolDescription {
            name: "get_tool_schema".to_string(),
            description:
                "Get the full input schema for one or more tools by name, so you can build a \
                 valid argument object for execute_konnect_tool. Works for any registered tool \
                 whether or not its toolset is loaded. Pass an array to fetch several at once. \
                 Fetch schemas only for tools you are about to call — this is the on-demand \
                 half of the dispatcher, and each schema costs context."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "anyOf": [
                            {"type": "string"},
                            {"type": "array", "items": {"type": "string"}}
                        ],
                        "description": "Tool name (e.g. 'add_layer'), or an array of names"
                    }
                },
                "required": ["tool_name"]
            }),
        },
        McpToolDescription {
            name: "execute_konnect_tool".to_string(),
            description:
                "Run any registered KiCAD tool by name, whether or not its toolset is loaded. \
                 Arguments are validated against that tool's real schema and the call is \
                 dispatched to the real handler, so the result, the error taxonomy, the \
                 lock-file protections and the safe S-expression/IPC write path are all exactly \
                 what a direct call would give you. Use get_tool_schema(tool_name) first to see \
                 what `arguments` must contain. Tool-specific options such as `dry_run` go \
                 inside `arguments`, because they belong to the tool being run, not to this \
                 wrapper."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "Name of the tool to run (e.g. 'add_layer', 'run_erc')"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "The argument object for that tool, exactly as its own schema describes it",
                        "default": {}
                    }
                },
                "required": ["tool_name"]
            }),
        },
    ]
}

/// Names the dispatcher owns. `execute_konnect_tool` is handled in the MCP
/// handler rather than here, so that a dispatched call reuses the one real
/// validation-and-dispatch path instead of a parallel copy of it.
pub const DISPATCHER_TOOL_NAMES: &[&str] = &[
    "list_available_tools",
    "get_tool_schema",
    "execute_konnect_tool",
];

pub async fn handle_meta_tool(
    name: &str,
    args: &Value,
    ctx: &std::sync::Arc<ToolContext>,
) -> Option<CallToolResult> {
    match name {
        "list_toolboxes" => Some(handle_list_toolboxes(ctx).await),
        "load_toolset" => Some(handle_load_toolset(args, ctx).await),
        "unload_toolset" => Some(handle_unload_toolset(args, ctx).await),
        "get_active_toolsets" => Some(handle_get_active_toolsets(ctx).await),
        "get_recent_calls" => Some(handle_get_recent_calls(args, ctx).await),
        "server_stats" => Some(handle_server_stats(ctx).await),
        "list_available_tools" => Some(handle_list_available_tools(args, ctx).await),
        "get_tool_schema" => Some(handle_get_tool_schema(args, ctx).await),
        // `execute_konnect_tool` is deliberately absent: the handler unwraps it
        // before dispatch so the inner call goes through the same required-
        // argument check and the same handler invocation as a direct call.
        _ => None,
    }
}

/// Browse the catalogue without loading any of it.
///
/// The default shape is names-only, grouped by toolset, because the whole point
/// of the dispatcher is that the caller does not pay for 208 descriptions it
/// will not read. Descriptions arrive when the caller narrows to one toolset or
/// searches, which is when they are actually useful.
async fn handle_list_available_tools(
    args: &Value,
    ctx: &std::sync::Arc<ToolContext>,
) -> CallToolResult {
    let all = ctx.router.all_tool_defs();

    if let Some(name) = args["toolset"].as_str() {
        let tools: Vec<Value> = all
            .iter()
            .filter(|(ts, _)| *ts == name)
            .map(|(_, d)| json!({ "name": d.name, "description": d.description }))
            .collect();
        if tools.is_empty() {
            return CallToolResult::error(format!(
                "Unknown toolset '{name}'. Call list_toolboxes() to see valid names."
            ));
        }
        return CallToolResult::json(&json!({
            "toolset": name,
            "count": tools.len(),
            "tools": tools,
            "hint": "Run any of these with execute_konnect_tool(tool_name, arguments). \
                     Call get_tool_schema(tool_name) first to see the argument shape."
        }));
    }

    if let Some(query) = args["search"].as_str() {
        let needle = query.to_lowercase();
        let matches: Vec<Value> = all
            .iter()
            .filter(|(_, d)| {
                d.name.to_lowercase().contains(&needle)
                    || d.description.to_lowercase().contains(&needle)
            })
            .map(|(ts, d)| json!({ "name": d.name, "toolset": ts, "description": d.description }))
            .collect();
        let builtin_matches: Vec<Value> = meta_tool_descriptions()
            .into_iter()
            .chain(dispatcher_tool_descriptions())
            .filter(|b| {
                b.name.to_lowercase().contains(&needle)
                    || b.description.to_lowercase().contains(&needle)
            })
            .map(|b| {
                json!({ "name": b.name, "toolset": Value::Null, "always_available": true,
                             "description": b.description })
            })
            .collect();
        return CallToolResult::json(&json!({
            "search": query,
            "count": matches.len() + builtin_matches.len(),
            "tools": matches,
            "always_available": builtin_matches,
            "hint": "Run any of these with execute_konnect_tool(tool_name, arguments). \
                     Entries under `always_available` are already in tools/list and can also \
                     be called directly."
        }));
    }

    let with_descriptions = args["with_descriptions"].as_bool().unwrap_or(false);
    let mut by_toolset: Vec<Value> = Vec::new();
    for ts in ctx.router.all_toolsets() {
        let tools: Vec<Value> = all
            .iter()
            .filter(|(name, _)| *name == ts.name)
            .map(|(_, d)| {
                if with_descriptions {
                    json!({ "name": d.name, "description": d.description })
                } else {
                    json!(d.name)
                }
            })
            .collect();
        by_toolset.push(json!({
            "toolset": ts.name,
            "category": ts.category,
            "description": ts.description,
            "tools": tools,
        }));
    }

    CallToolResult::json(&json!({
        "total_tools": all.len(),
        "toolsets": by_toolset,
        "hint": "Names only, to keep this cheap. Narrow with list_available_tools(toolset=...) \
                 or list_available_tools(search=...) for descriptions, then \
                 get_tool_schema(tool_name) for the argument shape, then \
                 execute_konnect_tool(tool_name, arguments) to run it. Loading a toolset is \
                 not required."
    }))
}

/// Fetch one or more real input schemas on demand.
///
/// This reads from the registry rather than the loaded set, so a schema is
/// available for every tool at any time without changing what `tools/list`
/// reports.
async fn handle_get_tool_schema(args: &Value, ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    let names: Vec<String> = match &args["tool_name"] {
        Value::String(name) => vec![name.clone()],
        Value::Array(items) => {
            match items
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<String>>>()
            {
                Some(names) => names,
                None => {
                    return CallToolResult::error_kind(
                        ToolErrorKind::InvalidArgument {
                            field: "tool_name".to_string(),
                            reason: "array entries must be strings".to_string(),
                        },
                        "Argument 'tool_name' is invalid: array entries must be strings",
                    )
                }
            }
        }
        _ => {
            return CallToolResult::error_kind(
                ToolErrorKind::InvalidArgument {
                    field: "tool_name".to_string(),
                    reason: "must be a string or an array of strings".to_string(),
                },
                "Argument 'tool_name' is invalid: must be a string or an array of strings",
            )
        }
    };

    // Meta-tools and dispatcher tools live outside the toolset registry but are
    // real, callable names — `load_toolset` and `get_active_toolsets` among them.
    // Reporting those as unknown would be a lie the caller then has to work
    // around, so they resolve here too, marked with a null toolset because they
    // belong to none.
    let builtins: Vec<McpToolDescription> = meta_tool_descriptions()
        .into_iter()
        .chain(dispatcher_tool_descriptions())
        .collect();

    let mut found = Vec::new();
    let mut unknown = Vec::new();
    for name in &names {
        if let Some(def) = ctx.router.find_tool_def(name) {
            found.push(json!({
                "name": def.name,
                "toolset": ctx.router.find_toolset_for_tool(def.name),
                "description": def.description,
                "input_schema": def.input_schema,
            }));
        } else if let Some(b) = builtins.iter().find(|b| b.name == *name) {
            found.push(json!({
                "name": b.name,
                "toolset": Value::Null,
                "always_available": true,
                "description": b.description,
                "input_schema": b.input_schema,
            }));
        } else {
            unknown.push(name.clone());
        }
    }

    // All names unknown is a typed error the caller can act on; a partial miss
    // still returns the schemas that resolved, with the misses named.
    if found.is_empty() {
        let joined = unknown.join(", ");
        return CallToolResult::error_kind(
            ToolErrorKind::UnknownTool {
                tool: joined.clone(),
            },
            format!(
                "No registered tool named: {joined}. Call list_available_tools(search=...) \
                 to find the right name."
            ),
        );
    }

    CallToolResult::json(&json!({
        "schemas": found,
        "unknown": unknown,
        "hint": "Pass one of these names plus a matching argument object to \
                 execute_konnect_tool(tool_name, arguments)."
    }))
}

async fn handle_list_toolboxes(ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    use std::collections::HashSet;
    let active: HashSet<String> = ctx.router.active_names().await.into_iter().collect();

    let toolsets: Vec<Value> = ctx
        .router
        .all_toolsets()
        .iter()
        .map(|t| {
            let loaded = active.contains(t.name);
            json!({
                "name": t.name,
                "description": t.description,
                "category": t.category,
                "tool_count": t.tool_count,
                "loaded": loaded,
            })
        })
        .collect();

    CallToolResult::json(&json!({
        "toolsets": toolsets,
        "total_tools": toolsets.iter()
            .filter_map(|t| t["tool_count"].as_u64())
            .sum::<u64>(),
        "loaded_count": active.len(),
        "hint": "Only loaded toolsets contribute tools to tools/list. Call load_toolset(name) \
                 to expose a toolset's tools. Call unload_toolset(name) to prune tools you no \
                 longer need (keeps context small).",
    }))
}

async fn handle_load_toolset(args: &Value, ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    match &args["name"] {
        // Legacy single-name form: result shape is byte-identical to the
        // pre-batch behavior (`loaded` is a string, `tools` echoes descriptions).
        Value::String(name) => match ctx.router.load(name).await {
            Some(tools) => {
                let tool_list: Vec<Value> = tools
                    .iter()
                    .map(|t| json!({ "name": t.name, "description": t.description }))
                    .collect();
                CallToolResult::json(&json!({
                    "loaded": name,
                    "tools_added": tools.len(),
                    "tools": tool_list
                }))
            }
            None => CallToolResult::error(format!(
                "Unknown toolset '{}'. Call list_toolboxes() to see valid names.",
                name
            )),
        },
        // New array form: one load, one tools/list_changed notification.
        Value::Array(arr) => {
            let mut names: Vec<String> =
                match arr.iter().map(|v| v.as_str().map(str::to_string)).collect() {
                    Some(names) => names,
                    None => return CallToolResult::error("name array must contain only strings"),
                };
            // Duplicate names in one call would double-count tools_added.
            let mut seen = std::collections::HashSet::new();
            names.retain(|n| seen.insert(n.clone()));

            let mut loaded = Vec::new();
            let mut tools_added = 0usize;
            let mut tool_list: Vec<Value> = Vec::new();
            let mut errors = Vec::new();

            for name in &names {
                match ctx.router.load(name).await {
                    Some(tools) => {
                        loaded.push(name.clone());
                        tools_added += tools.len();
                        tool_list.extend(
                            tools
                                .iter()
                                .map(|t| json!({ "name": t.name, "description": t.description })),
                        );
                    }
                    None => errors.push(format!(
                        "Unknown toolset '{}'. Call list_toolboxes() to see valid names.",
                        name
                    )),
                }
            }

            // Nothing loaded at all -- a typed error so the observer keeps a kind,
            // rather than a JSON body with a manually-set is_error flag.
            if loaded.is_empty() {
                let kind = ToolErrorKind::InvalidArgument {
                    field: "name".to_string(),
                    reason: names.join(", "),
                };
                return CallToolResult::error_kind(
                    kind,
                    format!(
                        "No toolsets loaded -- all names were unknown: {}. Call list_toolboxes() to see valid names.",
                        names.join(", ")
                    ),
                );
            }

            // Partial success (some names unknown, some loaded) is not an error --
            // the caller gets what loaded plus an errors array for the rest.
            CallToolResult::json(&json!({
                "loaded": loaded,
                "tools_added": tools_added,
                "tools": tool_list,
                "errors": errors,
            }))
        }
        _ => CallToolResult::error("Missing required argument: name (string or array of strings)"),
    }
}

async fn handle_unload_toolset(args: &Value, ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return CallToolResult::error("Missing required argument: name"),
    };

    if ctx.router.unload(name).await {
        CallToolResult::text(format!("Toolset '{}' unloaded.", name))
    } else {
        CallToolResult::error(format!("Unknown toolset '{}'.", name))
    }
}

async fn handle_get_recent_calls(
    args: &Value,
    ctx: &std::sync::Arc<ToolContext>,
) -> CallToolResult {
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(20);
    let records = ctx.observer.recent(limit).await;
    let count = records.len();
    CallToolResult::json(&json!({
        "count": count,
        "limit_applied": if limit == 0 { count } else { limit },
        "calls": records,
        "hint": "Calls are ordered newest-first. Use server_stats for aggregates.",
    }))
}

async fn handle_server_stats(ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    let snap = ctx.observer.snapshot().await;
    CallToolResult::json(&snap)
}

async fn handle_get_active_toolsets(ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    let active = ctx.router.active_names().await;
    let all = ctx.router.all_toolsets();

    let result: Vec<Value> = active
        .iter()
        .filter_map(|name| {
            all.iter().find(|t| t.name == name.as_str()).map(|meta| {
                json!({
                    "name": meta.name,
                    "description": meta.description,
                    "tool_count": meta.tool_count
                })
            })
        })
        .collect();

    CallToolResult::json(&json!({
        "active_toolsets": result,
        "total_active_tools": result.iter()
            .filter_map(|t| t["tool_count"].as_u64())
            .sum::<u64>()
    }))
}
