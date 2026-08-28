//! MCP protocol tests over stdio — spawn the real binary and speak JSON-RPC.
//!
//! Codifies the smoke tests that were run by hand at release time: handshake,
//! toolset loading for the entire registry, a real file-based tool call, and
//! the structured-error taxonomy the LLM relies on for recovery.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpProcess {
    fn spawn() -> Self {
        Self::spawn_in_dir(None)
    }

    /// Spawn with the process working directory set to `dir`, so
    /// `Config::load()`'s first search path (`konnect.toml` in cwd) picks up
    /// a test config file placed there.
    fn spawn_in_dir(dir: Option<&std::path::Path>) -> Self {
        Self::spawn_with(dir, &[], None)
    }

    /// Spawn with extra CLI arguments, and optionally a sandboxed `HOME`.
    ///
    /// `home` matters because the server auto-installs skills for its client on
    /// first launch, and `dirs::home_dir()` honours `$HOME`. Pointing it at a
    /// temp dir keeps a `--client codex` test from writing into the developer's
    /// real `~/.agents`.
    fn spawn_with(
        dir: Option<&std::path::Path>,
        args: &[&str],
        home: Option<&std::path::Path>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_konnect"));
        command.args(args);
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        if let Some(home) = home {
            command.env("HOME", home);
            command.env("USERPROFILE", home);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn konnect binary");
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        let mut p = McpProcess {
            child,
            stdin,
            reader,
            next_id: 1,
        };
        // MCP handshake
        let init = p.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "protocol-test", "version": "0"}
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "konnect");
        p.notify("notifications/initialized");
        p
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", msg).unwrap();
        self.stdin.flush().unwrap();
        // Read lines until the response with our id arrives (skips any
        // notifications the server might emit).
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).unwrap();
            assert!(
                n > 0,
                "server closed stdout waiting for response to {method}"
            );
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                return v;
            }
        }
    }

    fn notify(&mut self, method: &str) {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{}", msg).unwrap();
        self.stdin.flush().unwrap();
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Value {
        let resp = self.request("tools/call", json!({"name": name, "arguments": args}));
        resp["result"].clone()
    }

    /// Send a `tools/call`, then a fencing `ping`, and return every line the
    /// server emits up to and including the ping response. The fence
    /// guarantees the read loop terminates even when the tool call emits no
    /// notification (as in bug #19), so a test can assert on side-effect
    /// notifications without risking a hang.
    fn call_tool_then_fence(&mut self, name: &str, args: Value) -> Vec<Value> {
        let call_id = self.next_id;
        self.next_id += 1;
        let call = json!({
            "jsonrpc": "2.0", "id": call_id, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        });
        writeln!(self.stdin, "{}", call).unwrap();
        let fence_id = self.next_id;
        self.next_id += 1;
        let fence = json!({"jsonrpc": "2.0", "id": fence_id, "method": "ping", "params": {}});
        writeln!(self.stdin, "{}", fence).unwrap();
        self.stdin.flush().unwrap();

        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).unwrap();
            assert!(n > 0, "server closed stdout before fence response");
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            let is_fence = v.get("id").and_then(Value::as_i64) == Some(fence_id);
            lines.push(v);
            if is_fence {
                break;
            }
        }
        lines
    }

    /// Parse the JSON body of a tool result's first text content.
    fn tool_body(result: &Value) -> Value {
        let text = result["content"][0]["text"].as_str().unwrap_or("{}");
        serde_json::from_str(text).unwrap_or(Value::Null)
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn handshake_baseline_and_full_registry_loads() {
    let mut p = McpProcess::spawn();

    // Baseline tools/list: starter kit + meta-tools only (small context).
    let list = p.request("tools/list", json!({}));
    let baseline = list["result"]["tools"].as_array().unwrap().len();
    assert!(
        (10..30).contains(&baseline),
        "baseline tools/list should be the small starter kit, got {baseline}"
    );

    // list_toolboxes reports the registry; every toolset must load.
    let boxes = McpProcess::tool_body(&p.call_tool("list_toolboxes", json!({})));
    let toolsets: Vec<String> = boxes["toolsets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        toolsets.len() >= 17,
        "expected 17+ toolsets, got {}",
        toolsets.len()
    );
    // No license-era fields may reappear.
    assert!(boxes.get("license_tier").is_none());
    assert!(boxes["toolsets"][0].get("tier").is_none());

    let mut total = 0u64;
    for name in &toolsets {
        let loaded = McpProcess::tool_body(&p.call_tool("load_toolset", json!({"name": name})));
        let added = loaded["tools_added"].as_u64().unwrap_or(0);
        assert!(added > 0, "toolset '{name}' loaded no tools");
        total += added;
    }
    assert_eq!(
        total,
        boxes["total_tools"].as_u64().unwrap(),
        "sum of loaded tools disagrees with list_toolboxes total"
    );
}

#[test]
fn file_based_tool_roundtrip_in_temp_project() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("proto_demo");
    let mut p = McpProcess::spawn();

    let created = p.call_tool(
        "create_project",
        json!({"name": "proto_demo", "path": proj.to_string_lossy()}),
    );
    assert_ne!(
        created["isError"],
        json!(true),
        "create_project failed: {created}"
    );
    assert!(proj.join("proto_demo.kicad_sch").exists());

    let info = p.call_tool(
        "get_project_info",
        json!({"path": proj.join("proto_demo.kicad_pro").to_string_lossy()}),
    );
    assert_ne!(
        info["isError"],
        json!(true),
        "get_project_info failed: {info}"
    );
}

#[test]
fn structured_errors_guide_recovery() {
    let mut p = McpProcess::spawn();

    // Known tool in an unloaded toolset → toolset_not_loaded naming the owner.
    let r = p.call_tool("route_trace", json!({}));
    assert_eq!(r["isError"], json!(true));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "toolset_not_loaded");
    assert_eq!(body["error"]["toolset"], "pcb_routing");

    // Unknown tool → unknown_tool.
    let r = p.call_tool("frobnicate_board", json!({}));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "unknown_tool");

    // Missing required argument → invalid_argument naming the field.
    let r = p.call_tool("create_project", json!({"path": "/tmp/x"}));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "invalid_argument");
    assert_eq!(body["error"]["field"], "name");
}

#[test]
fn unknown_method_is_json_rpc_error_not_crash() {
    let mut p = McpProcess::spawn();
    let resp = p.request("tools/definitely_not_a_method", json!({}));
    assert!(
        resp.get("error").is_some(),
        "expected JSON-RPC error: {resp}"
    );
    // Server must still be alive afterwards.
    let ping = p.request("ping", json!({}));
    assert!(ping.get("result").is_some());
}

/// Regression test for issue #19. After `load_toolset`, the server must emit
/// `notifications/tools/list_changed` **over stdio** — not only over HTTP/SSE.
/// Without it, stdio clients (Claude Code) never re-fetch `tools/list`, so
/// every tool added by `load_toolset` stays uncallable for the session.
#[test]
fn load_toolset_emits_list_changed_over_stdio() {
    let mut p = McpProcess::spawn();
    let lines = p.call_tool_then_fence("load_toolset", json!({"name": "sch_components"}));
    let saw_notification = lines.iter().any(|v| {
        v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
            && v.get("id").is_none()
    });
    assert!(
        saw_notification,
        "expected notifications/tools/list_changed after load_toolset (issue #19); saw: {lines:#?}"
    );
}

/// The same guarantee for `unload_toolset` — removing tools must also tell the
/// client to refresh its tool list.
#[test]
fn unload_toolset_emits_list_changed_over_stdio() {
    let mut p = McpProcess::spawn();
    let _ = p.call_tool_then_fence("load_toolset", json!({"name": "sch_components"}));
    let lines = p.call_tool_then_fence("unload_toolset", json!({"name": "sch_components"}));
    let saw_notification = lines.iter().any(|v| {
        v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
            && v.get("id").is_none()
    });
    assert!(
        saw_notification,
        "expected notifications/tools/list_changed after unload_toolset; saw: {lines:#?}"
    );
}

/// `load_toolset` accepts an array of names in one call: all listed toolsets
/// load, tools_added sums across them, and only one list_changed notification
/// fires for the whole batch.
#[test]
fn load_toolset_batch_form_loads_all_and_notifies_once() {
    let mut p = McpProcess::spawn();
    let lines = p.call_tool_then_fence(
        "load_toolset",
        json!({"name": ["sch_components", "sch_wiring"]}),
    );
    let r = lines
        .iter()
        .find(|v| v.get("result").is_some())
        .expect("expected a tools/call result")["result"]
        .clone();
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["tools_added"].as_u64(), Some(40));
    // tools items are {name, description} objects, matching the legacy
    // single-name result shape -- not bare name strings.
    let tools = body["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty());
    for t in tools {
        assert!(t.get("name").and_then(Value::as_str).is_some(), "{t:#?}");
        assert!(
            t.get("description").and_then(Value::as_str).is_some(),
            "{t:#?}"
        );
    }

    let notification_count = lines
        .iter()
        .filter(|v| {
            v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
                && v.get("id").is_none()
        })
        .count();
    assert_eq!(
        notification_count, 1,
        "expected exactly one list_changed notification for the batch; saw: {lines:#?}"
    );

    // Mixed valid/invalid names: partial failure is not isError, but the
    // errors array names the unknown toolset and loaded lists only the real one.
    let lines = p.call_tool_then_fence(
        "load_toolset",
        json!({"name": ["templates", "bogus_toolset"]}),
    );
    let r = lines
        .iter()
        .find(|v| v.get("result").is_some())
        .expect("expected a tools/call result")["result"]
        .clone();
    assert_ne!(r["isError"].as_bool(), Some(true), "{r:#?}");
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["loaded"], json!(["templates"]));
    let errors = body["errors"].as_array().expect("errors array");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].as_str().unwrap().contains("list_toolboxes"),
        "{errors:#?}"
    );
}

/// All names in one `load_toolset` call unknown -> a typed `invalid_argument`
/// error (not a JSON body with a hand-set `isError`), so the observer keeps a
/// real `error_kind` column instead of degrading to `handler_error`.
#[test]
fn load_toolset_batch_total_failure_is_typed_error() {
    let mut p = McpProcess::spawn();
    let r = p.call_tool("load_toolset", json!({"name": ["bogus_one", "bogus_two"]}));
    assert_eq!(r["isError"], json!(true));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "invalid_argument");
    assert_eq!(body["error"]["field"], "name");
    assert!(
        body["message"].as_str().unwrap().contains("list_toolboxes"),
        "{body:#?}"
    );
}

/// With `auto_load_toolsets = true` in `konnect.toml` (picked up from the
/// server process's cwd), calling a tool from an unloaded toolset auto-loads
/// it and executes in the same call instead of returning `toolset_not_loaded`.
/// Default-off behavior (no config file) is covered by
/// `structured_errors_guide_recovery`.
#[test]
fn auto_load_toolsets_config_loads_and_executes_on_miss() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("konnect.toml"),
        "auto_load_toolsets = true\n",
    )
    .unwrap();
    let mut p = McpProcess::spawn_in_dir(Some(tmp.path()));

    // route_trace is in pcb_routing, not loaded at startup. With auto-load on,
    // the toolset loads, a list_changed notification fires, and the call gets
    // as far as argument validation instead of failing with
    // toolset_not_loaded — which is what this test is about.
    //
    // The field named is `board`, the first entry in route_trace's own
    // `required` list. This used to be `net_name`, whichever argument the
    // handler happened to read first; since #218 the dispatch checks
    // `required` in schema order before the handler runs, which is the order
    // the client was shown.
    let lines = p.call_tool_then_fence("route_trace", json!({}));
    let r = lines
        .iter()
        .find(|v| v.get("result").is_some())
        .expect("expected a tools/call result")["result"]
        .clone();
    assert_eq!(r["isError"], json!(true));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "invalid_argument");
    assert_eq!(body["error"]["field"], "board");

    let saw_notification = lines.iter().any(|v| {
        v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
            && v.get("id").is_none()
    });
    assert!(
        saw_notification,
        "expected notifications/tools/list_changed after auto-load; saw: {lines:#?}"
    );
}

// ─── Codex tool exposure (#134, #169) ────────────────────────────────────────
//
// Codex reads `tools/list` once per task and never acts on
// `notifications/tools/list_changed`. Under the router's default lazy loading
// that makes `load_toolset` a trap: it reports success and names the tools it
// activated, but Codex never receives their schemas, so those exact names are
// not callable and the model is left with the starter kit it began with.
//
// Pre-loading every toolset cures it but costs ~34K tokens of tool schemas in
// every request for the whole task. The dispatcher cures it for ~3K: three
// tools that are always in the listing and can reach all 208 on demand.
//
// These tests pin that at the protocol boundary — the same place the bug showed
// up — rather than at the config layer, where it is easy to be right in
// isolation and still ship a server that does the wrong thing.

/// Every tool name the Codex acceptance criteria require to be reachable
/// without a prior `load_toolset`.
const CODEX_REQUIRED_TOOLS: &[&str] = &[
    "get_active_toolsets",
    "load_toolset",
    "get_board_info",
    "get_layer_list",
    "add_layer",
    "set_active_layer",
    "get_netclasses",
    "create_netclass",
    "assign_net_to_class",
    "get_design_rules",
    "set_design_rules",
    "set_layer_constraints",
    "list_symbol_libraries",
    "register_symbol_library",
    "unregister_symbol_library",
    "list_footprint_libraries",
    "register_footprint_library",
    "unregister_footprint_library",
    "get_symbol_info",
    "list_symbols_in_library",
    "run_erc",
    "run_drc",
    "export_schematic_svg",
    "generate_netlist",
];

const DISPATCHER_TOOLS: &[&str] = &[
    "list_available_tools",
    "get_tool_schema",
    "execute_konnect_tool",
];

fn listed_tool_names(p: &mut McpProcess) -> Vec<String> {
    let list = p.request("tools/list", json!({}));
    list["result"]["tools"]
        .as_array()
        .expect("tools/list must return an array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

fn codex() -> McpProcess {
    let home = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    McpProcess::spawn_with(None, &["--client", "codex"], Some(home.path()))
}

/// The acceptance test: launched as `--client codex`, every required tool is
/// reachable from the first listing onward — either directly present, or
/// resolvable through `get_tool_schema` and runnable through
/// `execute_konnect_tool`. No `load_toolset` round trip, so nothing depends on
/// the client ever refreshing.
#[test]
fn codex_can_reach_every_required_tool_without_loading_a_toolset() {
    let mut p = codex();
    let listed = listed_tool_names(&mut p);

    for want in DISPATCHER_TOOLS {
        assert!(
            listed.iter().any(|got| got == want),
            "codex startup listing is missing dispatcher tool {want}"
        );
    }

    // Ask for all 24 schemas in one call, with nothing loaded.
    let body = McpProcess::tool_body(&p.call_tool(
        "get_tool_schema",
        json!({ "tool_name": CODEX_REQUIRED_TOOLS }),
    ));
    let resolved: Vec<&str> = body["schemas"]
        .as_array()
        .expect("get_tool_schema must return schemas")
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();

    let missing: Vec<&&str> = CODEX_REQUIRED_TOOLS
        .iter()
        .filter(|want| !resolved.contains(*want))
        .collect();
    assert!(
        missing.is_empty(),
        "get_tool_schema could not resolve {missing:?} (unknown: {})",
        body["unknown"]
    );

    // Every schema must actually be usable to build a call.
    for schema in body["schemas"].as_array().unwrap() {
        assert_eq!(
            schema["input_schema"]["type"], "object",
            "tool {} has no usable input schema",
            schema["name"]
        );
    }
}

/// Resolving and running tools through the dispatcher must not enlarge
/// `tools/list`. If it did, the dispatcher would slowly become the eager path
/// it exists to avoid.
#[test]
fn dispatching_does_not_grow_the_tool_listing() {
    let mut p = codex();
    let before = listed_tool_names(&mut p).len();

    let _ = p.call_tool("get_tool_schema", json!({"tool_name": "add_layer"}));
    let _ = p.call_tool(
        "execute_konnect_tool",
        json!({"tool_name": "list_symbol_libraries", "arguments": {"scope": "global"}}),
    );

    assert_eq!(
        listed_tool_names(&mut p).len(),
        before,
        "dispatching must leave the loaded set untouched"
    );
}

/// A read-only tool whose toolset is not loaded must actually run. Listing a
/// path to a tool the server then refuses to dispatch would just move the bug
/// one step later.
#[test]
fn dispatcher_runs_an_unloaded_read_only_tool() {
    let mut p = codex();
    let result = p.call_tool(
        "execute_konnect_tool",
        json!({"tool_name": "list_symbol_libraries", "arguments": {"scope": "global"}}),
    );
    let body = McpProcess::tool_body(&result);
    assert!(
        body["error"]["kind"] != "toolset_not_loaded",
        "dispatcher must not return toolset_not_loaded: {body}"
    );
    assert!(
        body["error"]["kind"] != "unknown_tool",
        "list_symbol_libraries must resolve: {body}"
    );
}

/// Arguments are validated against the tool's real schema, not waved through.
/// A dispatched call that skipped validation would be a hole straight past the
/// check that direct calls get (#218).
#[test]
fn dispatcher_enforces_the_real_schema() {
    let mut p = codex();
    let body = McpProcess::tool_body(&p.call_tool(
        "execute_konnect_tool",
        json!({"tool_name": "add_layer", "arguments": {}}),
    ));
    assert_eq!(body["error"]["kind"], "invalid_argument");
    assert_eq!(
        body["error"]["field"], "board",
        "the error must name the tool's own missing field: {body}"
    );
}

/// The dispatcher's own argument errors are typed too, and it refuses to
/// dispatch into itself — which would otherwise recurse forever.
#[test]
fn dispatcher_refuses_unknown_names_and_self_reference() {
    let mut p = codex();

    let unknown = McpProcess::tool_body(
        &p.call_tool("execute_konnect_tool", json!({"tool_name": "no_such_tool"})),
    );
    assert_eq!(unknown["error"]["kind"], "unknown_tool");

    let looped = McpProcess::tool_body(&p.call_tool(
        "execute_konnect_tool",
        json!({"tool_name": "execute_konnect_tool"}),
    ));
    assert_eq!(looped["error"]["kind"], "invalid_argument");
    assert_eq!(looped["error"]["field"], "tool_name");

    let bad_args = McpProcess::tool_body(&p.call_tool(
        "execute_konnect_tool",
        json!({"tool_name": "add_layer", "arguments": []}),
    ));
    assert_eq!(bad_args["error"]["kind"], "invalid_argument");
    assert_eq!(bad_args["error"]["field"], "arguments");
}

/// `list_available_tools` must show the whole catalogue, not the loaded subset,
/// and must default to the cheap names-only shape.
#[test]
fn list_available_tools_covers_the_whole_catalogue_cheaply() {
    let mut p = codex();
    let body = McpProcess::tool_body(&p.call_tool("list_available_tools", json!({})));

    let total = body["total_tools"].as_u64().expect("total_tools");
    let registered: u64 = konnect_core::router::registry::ALL_TOOLSETS
        .iter()
        .map(|t| t.tool_count as u64)
        .sum();
    assert_eq!(
        total, registered,
        "overview must span every registered tool"
    );

    // Names-only by default: no description keys in the default shape.
    let first = &body["toolsets"][0]["tools"][0];
    assert!(
        first.is_string(),
        "default overview must be names only, got {first}"
    );

    // Narrowing to one toolset brings descriptions.
    let scoped = McpProcess::tool_body(
        &p.call_tool("list_available_tools", json!({"toolset": "pcb_board"})),
    );
    assert!(scoped["tools"][0]["description"].is_string());
}

/// A natural-language search must find snake_case tools. `search: "symbol text"`
/// finding nothing was the first thing an end-to-end run hit: the query was
/// substring-matched whole against `add_symbol_text`, so the space never
/// matched the underscore.
#[test]
fn search_finds_snake_case_tools_from_natural_language() {
    let mut p = codex();
    for (query, expected) in [
        ("symbol text", "add_symbol_text"),
        ("symbol graphics", "set_symbol_graphics"),
        ("net class", "create_netclass"),
    ] {
        let body =
            McpProcess::tool_body(&p.call_tool("list_available_tools", json!({ "search": query })));
        let names: Vec<&str> = body["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(
            names.contains(&expected),
            "search '{query}' should find {expected}, got {names:?}"
        );
    }
}

/// Every client gets the dispatcher, including the default one. Claude Desktop
/// is what #134/#169 were reported against and it launches with no `--client`,
/// so a default that omits the dispatcher leaves the original reporter broken.
///
/// The starter kit still stays small — the dispatcher adds three tools, not a
/// catalogue.
#[test]
fn default_client_gets_the_dispatcher_but_not_eager_exposure() {
    let home = tempfile::tempdir().unwrap();
    let mut p = McpProcess::spawn_with(None, &[], Some(home.path()));
    let names = listed_tool_names(&mut p);

    for d in DISPATCHER_TOOLS {
        assert!(
            names.iter().any(|n| n == d),
            "default client must get {d} — Claude Desktop passes no --client"
        );
    }
    assert!(names.iter().any(|n| n == "load_toolset"));
    assert!(
        names.len() < 30,
        "default listing should stay near the starter kit, got {}",
        names.len()
    );
    assert!(
        !names.iter().any(|n| n == "add_layer"),
        "eager exposure must stay off by default"
    );
}

/// Both switches work in both directions, for any client.
#[test]
fn exposure_switches_override_the_client_default() {
    let home = tempfile::tempdir().unwrap();

    let mut off = McpProcess::spawn_with(
        None,
        &["--client", "codex", "--no-dispatcher-tools"],
        Some(home.path()),
    );
    let names = listed_tool_names(&mut off);
    assert!(!names.iter().any(|n| n == "execute_konnect_tool"));

    let mut on = McpProcess::spawn_with(None, &["--dispatcher-tools"], Some(home.path()));
    assert!(listed_tool_names(&mut on)
        .iter()
        .any(|n| n == "execute_konnect_tool"));

    // Eager stays available for anyone who wants native schemas and has the
    // context budget for them.
    let mut eager = McpProcess::spawn_with(None, &["--eager-toolsets"], Some(home.path()));
    let names = listed_tool_names(&mut eager);
    assert!(names.iter().any(|n| n == "add_layer"));
    assert!(names.iter().any(|n| n == "create_netclass"));
}

/// Exposure must not depend on session state: a fresh process is what a new
/// Codex task gets, and it must look the same every time.
#[test]
fn codex_exposure_survives_a_server_restart() {
    let mut first = listed_tool_names(&mut codex());
    let mut second = listed_tool_names(&mut codex());
    first.sort();
    second.sort();
    assert_eq!(
        first, second,
        "a restarted server must expose the same tools"
    );
    assert!(first.iter().any(|n| n == "execute_konnect_tool"));
}
