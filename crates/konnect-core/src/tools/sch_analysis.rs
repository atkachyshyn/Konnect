//! `sch_analysis` toolset — net connectivity, pin queries, trace paths, overlap/orphan detection.
//!
//! All operations are read-only S-expression analysis. Connectivity — the net
//! graph and what counts as attached at a point — lives in `sch_connectivity`.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::sch_connectivity::{net_graph_for, pt_key, ConnectivityIndex};
use crate::tools::{get_path, opt_f64, require_f64, require_str, ToolContext, ToolDef};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    geometry::{point_on_segment, points_coincident},
    schematic::{
        extract_all_net_labels, extract_labels, extract_symbol_instances, extract_wires,
        find_lib_symbol, read_schematic, symbol_bounds_for_instance, LabelKind, SymbolBounds, Wire,
    },
};
use serde_json::json;
use std::collections::{HashMap, HashSet};

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "list_schematic_wires",
            "List all wire segments in a schematic with start/end coordinates and UUIDs.",
            json!({ "type": "object",
                "properties": { "schematic": { "type": "string" } },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_list_wires(args, ctx).await }
        ),
        tool!(
            "list_schematic_nets",
            "List all distinct net names derived from net labels, global labels, and power symbols.",
            json!({ "type": "object",
                "properties": { "schematic": { "type": "string" } },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_list_nets(args, ctx).await }
        ),
        tool!(
            "list_schematic_labels",
            "List all label instances (net_label, global_label, hierarchical_label) \
             with their positions, net names, and types.",
            json!({ "type": "object",
                "properties": { "schematic": { "type": "string" } },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_list_labels(args, ctx).await }
        ),
        tool!(
            "get_net_connections",
            "Get all pins and labels connected to a named net.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string", "description": "Net name to query" }
                },
                "required": ["schematic", "net"] }),
            |args, ctx| async move { handle_get_net_connections(args, ctx).await }
        ),
        tool!(
            "get_net_connectivity",
            "Build the full connectivity graph for a net using union-find. \
             Returns all wire segments, labels, and T-junction locations.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string" }
                },
                "required": ["schematic", "net"] }),
            |args, ctx| async move { handle_get_net_connectivity(args, ctx).await }
        ),
        tool!(
            "get_pin_connections",
            "Get the net connected to a specific pin on a component by tracing wires from the pin endpoint.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "pin_number": { "type": "string" }
                },
                "required": ["schematic", "reference", "pin_number"] }),
            |args, ctx| async move { handle_get_pin_connections(args, ctx).await }
        ),
        tool!(
            "get_pin_net_name",
            "Return just the net name for a specific pin on a component.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "pin_number": { "type": "string" }
                },
                "required": ["schematic", "reference", "pin_number"] }),
            |args, ctx| async move { handle_get_pin_connections(args, ctx).await }
        ),
        tool!(
            "get_component_nets",
            "Get all nets connected to every pin of a component.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["schematic", "reference"] }),
            |args, ctx| async move { handle_get_component_nets(args, ctx).await }
        ),
        tool!(
            "get_net_components",
            "Get all components (and their pins) connected to a named net.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string" }
                },
                "required": ["schematic", "net"] }),
            |args, ctx| async move { handle_get_net_components(args, ctx).await }
        ),
        tool!(
            "trace_from_point",
            "Trace connectivity from any (X,Y) point — returns what is at that point and the net it belongs to.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "tolerance": { "type": "number", "default": 0.05 }
                },
                "required": ["schematic", "x", "y"] }),
            |args, ctx| async move { handle_trace_from_point(args, ctx).await }
        ),
        tool!(
            "find_orphan_items",
            "Find dangling wire ends, floating labels, and unconnected pin endpoints. \
             Pins, sheet pins, junctions, and no-connect flags all count as connections.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "tolerance": {
                        "type": "number", "exclusiveMinimum": 0, "default": 0.05
                    }
                },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_find_orphan_items(args, ctx).await }
        ),
        tool!(
            "find_shorted_nets",
            "Detect accidentally merged nets — pairs of distinct net names sharing a wire path.",
            json!({ "type": "object",
                "properties": { "schematic": { "type": "string" } },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_find_shorted_nets(args, ctx).await }
        ),
        tool!(
            "find_single_pin_nets",
            "Find nets with only one label/connection — often indicates a missing counterpart.",
            json!({ "type": "object",
                "properties": { "schematic": { "type": "string" } },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_find_single_pin_nets(args, ctx).await }
        ),
        tool!(
            "get_connected_items",
            "Get all wires, labels, and components connected to a given component reference \
             by tracing net connectivity from each of its pins.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "reference": { "type": "string", "description": "Component reference designator (e.g. 'R1')" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_get_connected_items(args, ctx).await }
        ),
        tool!(
            "check_schematic_overlaps",
            "Find overlapping symbols from their transformed drawing/pin bounds (excluding \
             free text), plus conflicting labels at the same location. Reports any symbol whose \
             embedded geometry could not be resolved and used the origin fallback instead.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "tolerance": {
                        "type": "number",
                        "description": "Coordinate tolerance in mm for label collisions and the origin fallback when library geometry is unavailable",
                        "default": 0.5
                    }
                },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_check_overlaps(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_list_wires(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sch = cse::Schematic::load(&sch_path)?;
    let items: Vec<serde_json::Value> = sch.wires.iter()
        .map(|w| json!({ "x1": w.start.0, "y1": w.start.1, "x2": w.end.0, "y2": w.end.1, "uuid": w.uuid }))
        .collect();
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "wires": items }),
    ))
}

async fn handle_list_nets(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let (_, tree) = read_schematic(&sch_path)?;
    // Power symbols name nets too — the tool has always said so.
    let mut nets: Vec<String> = extract_all_net_labels(&tree)
        .into_iter()
        .map(|l| l.net)
        .collect();
    nets.sort();
    nets.dedup();
    Ok(CallToolResult::json(
        &json!({ "count": nets.len(), "nets": nets }),
    ))
}

async fn handle_list_labels(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sch = cse::Schematic::load(&sch_path)?;
    let mut items: Vec<serde_json::Value> = Vec::new();
    for l in sch.labels.iter() {
        items.push(json!({ "net": l.text, "type": "NetLabel", "x": l.at.x, "y": l.at.y, "rotation": l.at.rotation.unwrap_or(0.0) }));
    }
    for g in sch.global_labels.iter() {
        items.push(json!({ "net": g.text, "type": "GlobalLabel", "x": g.at.x, "y": g.at.y, "rotation": g.at.rotation.unwrap_or(0.0) }));
    }
    for h in sch.hierarchical_labels.iter() {
        items.push(json!({ "net": h.text, "type": "HierarchicalLabel", "x": h.at.x, "y": h.at.y, "rotation": h.at.rotation.unwrap_or(0.0) }));
    }
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "labels": items }),
    ))
}

async fn handle_get_net_connections(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let (_, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let matching: Vec<_> = labels
        .iter()
        .filter(|l| l.net == net)
        .map(|l| json!({ "type": format!("{:?}", l.kind), "x": l.x, "y": l.y }))
        .collect();
    let mut g = net_graph_for(&tree, &wires, &labels);
    let pts = g.points_on_net(&net).len();
    Ok(CallToolResult::json(
        &json!({ "net": net, "label_count": matching.len(), "labels": matching, "connected_points": pts }),
    ))
}

async fn handle_get_net_connectivity(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let (_, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let mut g = net_graph_for(&tree, &wires, &labels);
    let net_pts: HashSet<(i64, i64)> = g.points_on_net(&net).into_iter().collect();
    let net_wires: Vec<_> = wires
        .iter()
        .filter(|w| net_pts.contains(&pt_key(w.x1, w.y1)) || net_pts.contains(&pt_key(w.x2, w.y2)))
        .map(|w| json!({ "x1": w.x1, "y1": w.y1, "x2": w.x2, "y2": w.y2 }))
        .collect();
    let net_labels: Vec<_> = labels
        .iter()
        .filter(|l| l.net == net)
        .map(|l| json!({ "type": format!("{:?}", l.kind), "x": l.x, "y": l.y }))
        .collect();
    let net_wire_objs: Vec<Wire> = wires
        .iter()
        .filter(|w| net_pts.contains(&pt_key(w.x1, w.y1)) || net_pts.contains(&pt_key(w.x2, w.y2)))
        .cloned()
        .collect();
    let t_junctions = konnect_sexp::schematic::find_t_junctions(&net_wire_objs, 0.01);
    Ok(CallToolResult::json(&json!({
        "net": net,
        "wires": net_wires,
        "labels": net_labels,
        "t_junctions": t_junctions.iter().map(|(x,y)| json!({"x": x, "y": y})).collect::<Vec<_>>()
    })))
}

async fn handle_get_pin_connections(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin_number = match require_str(args, "pin_number") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let inst = instances
        .iter()
        .find(|i| i.reference == reference)
        .ok_or_else(|| anyhow::anyhow!("Component '{}' not found", reference))?;
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let lib_sym = find_lib_symbol(&lib_syms, inst);
    let pin_info = lib_sym.and_then(|sym| {
        konnect_sexp::schematic::extract_lib_pins(sym)
            .into_iter()
            .find(|p| p.number == pin_number)
            .map(|p| {
                let (px, py) = konnect_sexp::schematic::pin_endpoint(&p, inst.pin_transform());
                (p, px, py)
            })
    });
    let (pin, px, py) = match pin_info {
        Some(info) => info,
        None => {
            return Ok(CallToolResult::error(format!(
                "Pin '{}' not found on '{}'",
                pin_number, reference
            )))
        }
    };
    let index = ConnectivityIndex::build(
        &tree,
        &wires,
        &labels,
        crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
    );
    let mut g = net_graph_for(&tree, &wires, &labels);
    let net = index.net_for_pin_at(&mut g, &pin.electrical_type, px, py);
    Ok(CallToolResult::json(
        &json!({ "reference": reference, "pin": pin_number, "pin_x": px, "pin_y": py, "net": net }),
    ))
}

async fn handle_get_component_nets(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let inst = instances
        .iter()
        .find(|i| i.reference == reference)
        .ok_or_else(|| anyhow::anyhow!("Component '{}' not found", reference))?;
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let lib_sym = find_lib_symbol(&lib_syms, inst);
    let index = ConnectivityIndex::build(
        &tree,
        &wires,
        &labels,
        crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
    );
    let mut g = net_graph_for(&tree, &wires, &labels);
    let pins: Vec<serde_json::Value> = if let Some(sym) = lib_sym {
        let t = inst.pin_transform();
        konnect_sexp::schematic::extract_lib_pins(sym).iter().map(|p| {
            let (px, py) = konnect_sexp::schematic::pin_endpoint(p, t);
            json!({ "pin": p.number, "name": p.name, "x": px, "y": py, "net": index.net_for_pin_at(&mut g, &p.electrical_type, px, py) })
        }).collect()
    } else {
        Vec::new()
    };
    Ok(CallToolResult::json(
        &json!({ "reference": reference, "pins": pins }),
    ))
}

async fn handle_get_net_components(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let index = ConnectivityIndex::build(
        &tree,
        &wires,
        &labels,
        crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
    );
    let mut g = net_graph_for(&tree, &wires, &labels);
    let result: Vec<serde_json::Value> = instances
        .iter()
        .filter_map(|inst| {
            let ls = find_lib_symbol(&lib_syms, inst)?;
            let t = inst.pin_transform();
            let connected: Vec<_> = konnect_sexp::schematic::extract_lib_pins(ls)
                .iter()
                .filter_map(|p| {
                    let (px, py) = konnect_sexp::schematic::pin_endpoint(p, t);
                    if index.net_for_pin_at(&mut g, &p.electrical_type, px, py).as_deref() == Some(net.as_str()) {
                        Some(json!({ "pin": p.number, "name": p.name }))
                    } else {
                        None
                    }
                })
                .collect();
            if connected.is_empty() {
                None
            } else {
                Some(json!({ "reference": inst.reference, "value": inst.value, "pins": connected }))
            }
        })
        .collect();
    Ok(CallToolResult::json(
        &json!({ "net": net, "components": result }),
    ))
}

async fn handle_trace_from_point(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let tol = opt_f64(args, "tolerance").unwrap_or(0.05);
    let (_, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let mut g = net_graph_for(&tree, &wires, &labels);
    let on_wire: Vec<_> = wires
        .iter()
        .filter(|w| {
            points_coincident(x, y, w.x1, w.y1, tol)
                || points_coincident(x, y, w.x2, w.y2, tol)
                || point_on_segment(x, y, w.x1, w.y1, w.x2, w.y2, tol)
        })
        .map(|w| json!({ "x1": w.x1, "y1": w.y1, "x2": w.x2, "y2": w.y2 }))
        .collect();
    let at_label: Vec<_> = labels
        .iter()
        .filter(|l| points_coincident(x, y, l.x, l.y, tol))
        .map(|l| json!({ "net": l.net, "type": format!("{:?}", l.kind) }))
        .collect();
    Ok(CallToolResult::json(
        &json!({ "x": x, "y": y, "net": g.net_at(x, y), "wires_here": on_wire, "labels_here": at_label }),
    ))
}

async fn handle_find_orphan_items(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let tolerance = opt_f64(args, "tolerance").unwrap_or(0.05);
    if !tolerance.is_finite() || tolerance <= 0.0 {
        return Ok(CallToolResult::error(
            "Invalid argument 'tolerance': must be finite and positive",
        ));
    }

    let (_, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    // Every net name, so the index is the same one every other tool builds. A
    // power symbol's pseudo-label sits on the pin already indexed, so feeding
    // them changes no coincidence answer below.
    let labels = extract_all_net_labels(&tree);
    let index = ConnectivityIndex::build(&tree, &wires, &labels, tolerance);

    let mut all: Vec<serde_json::Value> = Vec::new();

    // A wire end is dangling only when nothing terminates it. Ending on a
    // component or hierarchical sheet pin is the normal case.
    for (x, y, wire_uuid) in index.floating_wire_ends() {
        all.push(json!({
            "type": "dangling_wire_end",
            "x": x,
            "y": y,
            "wire_uuid": wire_uuid
        }));
    }

    // Labels connect anywhere along a wire, not only at its endpoint, or
    // directly on a bare symbol pin. Only real labels are reported: a power
    // symbol that connects to nothing is an unconnected pin, below, and
    // reporting it twice would be a second answer to one question.
    for label in index
        .labels()
        .iter()
        .filter(|label| label.kind != LabelKind::PowerSymbol)
    {
        if !index.on_wire(label.x, label.y) && !index.has_pin(label.x, label.y) {
            all.push(json!({
                "type": "floating_label",
                "net": label.net,
                "x": label.x,
                "y": label.y
            }));
        }
    }

    // Report the unconnected pins promised by the tool description. A pin
    // sitting mid-wire connects only through a junction dot (#104).
    for placed in index.placed_pins() {
        let (x, y) = placed.at;
        if placed.pin.electrical_type == "no_connect" || index.has_no_connect(x, y) {
            continue;
        }
        if !index.attaches_pin(x, y) {
            all.push(json!({
                "type": "unconnected_pin",
                "reference": placed.reference,
                "pin": placed.pin.number,
                "pin_name": placed.pin.name,
                "x": x,
                "y": y
            }));
        }
    }

    Ok(CallToolResult::json(&json!({
        "orphan_count": all.len(),
        "orphans": all,
        "tolerance": tolerance
    })))
}

async fn handle_find_shorted_nets(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let (_, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let mut g = net_graph_for(&tree, &wires, &labels);
    let mut root_nets: HashMap<(i64, i64), Vec<String>> = HashMap::new();
    for l in &labels {
        let root = g.find(pt_key(l.x, l.y));
        root_nets.entry(root).or_default().push(l.net.clone());
    }
    let shorts: Vec<serde_json::Value> = root_nets
        .into_values()
        .filter_map(|mut nets| {
            nets.sort();
            nets.dedup();
            if nets.len() > 1 {
                Some(json!({ "shorted_nets": nets }))
            } else {
                None
            }
        })
        .collect();
    Ok(CallToolResult::json(
        &json!({ "short_count": shorts.len(), "shorts": shorts }),
    ))
}

async fn handle_find_single_pin_nets(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let (_, tree) = read_schematic(&sch_path)?;
    let labels = extract_all_net_labels(&tree);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for l in &labels {
        *counts.entry(l.net.clone()).or_insert(0) += 1;
    }
    let singles: Vec<serde_json::Value> = counts
        .iter()
        .filter(|(_, &c)| c == 1)
        .map(|(net, _)| {
            let l = labels.iter().find(|l| &l.net == net).unwrap();
            json!({ "net": net, "x": l.x, "y": l.y, "type": format!("{:?}", l.kind) })
        })
        .collect();
    Ok(CallToolResult::json(
        &json!({ "single_pin_net_count": singles.len(), "nets": singles }),
    ))
}

async fn handle_get_connected_items(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let inst = match instances.iter().find(|i| i.reference == reference) {
        Some(i) => i,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    let lib_sym = find_lib_symbol(&lib_syms, inst);
    let index = ConnectivityIndex::build(
        &tree,
        &wires,
        &labels,
        crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
    );
    let mut g = net_graph_for(&tree, &wires, &labels);

    // Get nets for each pin
    let mut connected_nets: HashSet<String> = HashSet::new();
    if let Some(sym) = lib_sym {
        let t = inst.pin_transform();
        for p in konnect_sexp::schematic::extract_lib_pins(sym) {
            let (px, py) = konnect_sexp::schematic::pin_endpoint(&p, t);
            if let Some(net) = index.net_for_pin_at(&mut g, &p.electrical_type, px, py) {
                connected_nets.insert(net);
            }
        }
    }

    // Find all wires, labels, and components on those nets
    let mut all_net_pts: HashSet<(i64, i64)> = HashSet::new();
    for net in &connected_nets {
        for pt in g.points_on_net(net) {
            all_net_pts.insert(pt);
        }
    }

    let connected_wires: Vec<serde_json::Value> = wires
        .iter()
        .filter(|w| {
            all_net_pts.contains(&pt_key(w.x1, w.y1)) || all_net_pts.contains(&pt_key(w.x2, w.y2))
        })
        .map(|w| json!({ "x1": w.x1, "y1": w.y1, "x2": w.x2, "y2": w.y2, "uuid": w.uuid }))
        .collect();

    let connected_labels: Vec<serde_json::Value> = labels
        .iter()
        .filter(|l| connected_nets.contains(&l.net))
        .map(|l| json!({ "net": l.net, "type": format!("{:?}", l.kind), "x": l.x, "y": l.y }))
        .collect();

    // Find other components on the same nets (excluding the queried one)
    let connected_components: Vec<serde_json::Value> = instances.iter()
        .filter(|i| i.reference != reference)
        .filter_map(|i| {
            let ls = find_lib_symbol(&lib_syms, i)?;
            let t = i.pin_transform();
            let matching_pins: Vec<_> = konnect_sexp::schematic::extract_lib_pins(ls).iter()
                .filter_map(|p| {
                    let (px, py) = konnect_sexp::schematic::pin_endpoint(p, t);
                    if index.net_for_pin_at(&mut g, &p.electrical_type, px, py).is_some_and(|net| connected_nets.contains(&net)) {
                        Some(json!({ "pin": p.number, "name": p.name }))
                    } else { None }
                }).collect();
            if matching_pins.is_empty() { None }
            else { Some(json!({ "reference": i.reference, "value": i.value, "connected_pins": matching_pins })) }
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "nets": connected_nets.iter().collect::<Vec<_>>(),
        "connected_wires": connected_wires.len(),
        "wires": connected_wires,
        "labels": connected_labels,
        "connected_components": connected_components
    })))
}

async fn handle_check_overlaps(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let tol = opt_f64(args, "tolerance").unwrap_or(0.5);
    if !tol.is_finite() || tol < 0.0 {
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::InvalidArgument {
                field: "tolerance".to_string(),
                reason: "must be a finite, non-negative number".to_string(),
            },
            "Argument 'tolerance' must be a finite, non-negative number.",
        ));
    }
    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let lib_symbols = tree
        .find("lib_symbols")
        .map(|node| node.find_all("symbol"))
        .unwrap_or_default();

    // Compare the actual selected-unit graphic/pin envelopes. Distinct origins
    // can still place two large symbols on top of each other; the old origin
    // comparison missed that normal collision shape entirely.
    let placements = instances
        .iter()
        .map(|instance| {
            let bounds = find_lib_symbol(&lib_symbols, instance)
                .and_then(|symbol| symbol_bounds_for_instance(symbol, instance));
            (instance, bounds)
        })
        .collect::<Vec<_>>();
    let mut comp_overlaps: Vec<serde_json::Value> = Vec::new();
    let bounds_json = |bounds: SymbolBounds| {
        json!({
            "x_min": bounds.min_x,
            "y_min": bounds.min_y,
            "x_max": bounds.max_x,
            "y_max": bounds.max_y
        })
    };
    for (index, (a, a_bounds)) in placements.iter().enumerate() {
        for (b, b_bounds) in &placements[index + 1..] {
            match (a_bounds, b_bounds) {
                (Some(a_bounds), Some(b_bounds)) => {
                    let (overlap_x, overlap_y) = a_bounds.overlap_depth(*b_bounds);
                    // Edge/pin contact is a normal connection. Positive area
                    // on both axes means the placed symbol envelopes collide.
                    if overlap_x > 1e-9 && overlap_y > 1e-9 {
                        comp_overlaps.push(json!({
                            "type": "component_overlap",
                            "a": a.reference,
                            "unit_a": a.unit,
                            "b": b.reference,
                            "unit_b": b.unit,
                            "overlap_x_mm": overlap_x,
                            "overlap_y_mm": overlap_y,
                            "bounds_a": bounds_json(*a_bounds),
                            "bounds_b": bounds_json(*b_bounds),
                            "detection": "symbol_geometry"
                        }));
                    }
                }
                // Preserve a useful conservative check when an old or damaged
                // schematic lacks the embedded definition. The response names
                // every such fallback below so it cannot look authoritative.
                _ if points_coincident(a.x, a.y, b.x, b.y, tol) => {
                    comp_overlaps.push(json!({
                        "type": "component_overlap",
                        "a": a.reference,
                        "unit_a": a.unit,
                        "b": b.reference,
                        "unit_b": b.unit,
                        "x": a.x,
                        "y": a.y,
                        "detection": "origin_fallback"
                    }));
                }
                _ => {}
            }
        }
    }

    // The structural extractor already combines local, global, and
    // hierarchical labels.
    let all_labels = extract_labels(&tree);
    let mut label_overlaps: Vec<serde_json::Value> = Vec::new();
    for (i, a) in all_labels.iter().enumerate() {
        for b in &all_labels[i + 1..] {
            if points_coincident(a.x, a.y, b.x, b.y, tol) && a.net != b.net {
                label_overlaps.push(json!({ "type": "label_overlap", "net_a": a.net, "net_b": b.net, "x": a.x, "y": a.y }));
            }
        }
    }

    let mut all = comp_overlaps;
    all.extend(label_overlaps);
    let bounds_unresolved = placements
        .iter()
        .filter(|(_, bounds)| bounds.is_none())
        .map(|(instance, _)| instance.reference.as_str())
        .collect::<Vec<_>>();
    Ok(CallToolResult::json(&json!({
        "overlap_count": all.len(),
        "overlaps": all,
        "bounds_resolved": placements.len() - bounds_unresolved.len(),
        "bounds_unresolved": bounds_unresolved
    })))
}

#[cfg(test)]
mod placement_overlap_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
                dispatcher_tools: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// Retained graphic and pin nodes from KiCad 10's stock Device:R.
    const DEVICE_R: &str = r#"		(symbol "Device:R"
			(symbol "R_0_1"
				(rectangle
					(start -1.016 -2.54)
					(end 1.016 2.54)
					(stroke (width 0.254) (type default))
					(fill (type none))
				)
			)
			(symbol "R_1_1"
				(pin passive line
					(at 0 3.81 270)
					(length 1.27)
					(name "" (effects (font (size 1.27 1.27))))
					(number "1" (effects (font (size 1.27 1.27))))
				)
				(pin passive line
					(at 0 -3.81 90)
					(length 1.27)
					(name "" (effects (font (size 1.27 1.27))))
					(number "2" (effects (font (size 1.27 1.27))))
				)
			)
		)
"#;

    fn schematic(placements: &[(&str, f64, f64)]) -> String {
        let instances = placements
            .iter()
            .map(|(reference, x, y)| {
                format!(
                    "\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at {x} {y} 0)\n\t\t(unit 1)\n\t\t(uuid \"{reference}\")\n\t\t(property \"Reference\" \"{reference}\")\n\t\t(property \"Value\" \"10k\")\n\t)\n"
                )
            })
            .collect::<String>();
        format!(
            "(kicad_sch\n\t(version 20260206)\n\t(generator \"eeschema\")\n\t(lib_symbols\n{DEVICE_R}\t)\n{instances})\n"
        )
    }

    fn response_json(result: &CallToolResult) -> serde_json::Value {
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        serde_json::from_str(text).unwrap()
    }

    async fn overlaps(source: &str) -> serde_json::Value {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("overlap.kicad_sch");
        std::fs::write(&path, source).unwrap();
        let result =
            handle_check_overlaps(&json!({"schematic": path.to_string_lossy()}), &test_ctx())
                .await
                .unwrap();
        response_json(&result)
    }

    #[tokio::test]
    async fn distinct_origins_with_intersecting_symbol_bodies_are_reported() {
        let result = overlaps(&schematic(&[
            ("R1", 100.0, 50.0),
            ("R2", 101.0, 50.0),
            ("R3", 110.0, 50.0),
        ]))
        .await;

        assert_eq!(result["bounds_resolved"], 3);
        assert_eq!(result["bounds_unresolved"], json!([]));
        assert_eq!(result["overlap_count"], 1);
        assert_eq!(result["overlaps"][0]["a"], "R1");
        assert_eq!(result["overlaps"][0]["b"], "R2");
        assert_eq!(result["overlaps"][0]["detection"], "symbol_geometry");
        assert!(result["overlaps"][0]["overlap_x_mm"].as_f64().unwrap() > 1.0);
    }

    #[tokio::test]
    async fn symbols_whose_bounds_only_touch_are_not_collisions() {
        let result = overlaps(&schematic(&[("R1", 100.0, 50.0), ("R2", 102.032, 50.0)])).await;

        assert_eq!(result["overlap_count"], 0, "{result}");
    }

    #[tokio::test]
    async fn invalid_overlap_tolerance_is_a_named_argument_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("overlap.kicad_sch");
        std::fs::write(&path, schematic(&[("R1", 100.0, 50.0)])).unwrap();
        let result = handle_check_overlaps(
            &json!({"schematic": path.to_string_lossy(), "tolerance": -0.1}),
            &test_ctx(),
        )
        .await
        .unwrap();
        let result = response_json(&result);

        assert_eq!(result["error"]["kind"], "invalid_argument");
        assert_eq!(result["error"]["field"], "tolerance");
    }
}

#[cfg(test)]
#[cfg(test)]
mod orphan_item_tests {
    use super::*;
    use crate::tools::ServerConfig;
    use std::io::Write;
    use std::sync::Arc;

    /// Run the registered tool against a temporary schematic, exactly as the
    /// MCP dispatch layer does after selecting its `ToolDef`.
    async fn call_result(schematic: &str, mut args: serde_json::Value) -> CallToolResult {
        let mut file = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        file.write_all(schematic.as_bytes()).unwrap();
        file.flush().unwrap();

        args["schematic"] = json!(file.path().to_str().unwrap());
        let definition = tools()
            .into_iter()
            .find(|tool| tool.name == "find_orphan_items")
            .unwrap();
        let context = ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
                dispatcher_tools: false,
            },
            Arc::new(crate::router::ToolRouter::new()),
        );
        (definition.handler)(&args, Arc::new(context))
            .await
            .unwrap()
    }

    async fn call(schematic: &str, args: serde_json::Value) -> serde_json::Value {
        let result = call_result(schematic, args).await;
        assert!(!result.is_error, "find_orphan_items failed");
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(text).unwrap()
    }

    /// One wire from (90,100) to a zero-length pin of U1 at (100,100), a `SIG`
    /// label mid-segment, a stray `ORPHAN` label, and U2 with nothing on its pin.
    fn schematic(extra: &str) -> String {
        format!(
            r#"(kicad_sch
  (version 20260306)
  (generator "eeschema")
  (uuid "root")
  (lib_symbols
    (symbol "Test:P"
      (symbol "P_1_1"
        (pin passive line (at 0 0 0) (length 0) (name "A") (number "1"))
      )
    )
  )
  (wire (pts (xy 90 100) (xy 100 100)) (uuid "w1"))
  (label "SIG" (at 95 100 0))
  (label "ORPHAN" (at 200 200 0))
  (symbol (lib_id "Test:P") (at 100 100 0) (unit 1) (uuid "u1")
    (property "Reference" "U1" (at 100 100 0))
    (property "Value" "P" (at 100 100 0))
  )
  (symbol (lib_id "Test:P") (at 150 150 0) (unit 1) (uuid "u2")
    (property "Reference" "U2" (at 150 150 0))
    (property "Value" "P" (at 150 150 0))
  )
{extra}  (sheet_instances (path "/" (page "1")))
)
"#
        )
    }

    async fn orphans(extra: &str) -> Vec<serde_json::Value> {
        let body = call(&schematic(extra), json!({})).await;
        body["orphans"].as_array().unwrap().clone()
    }

    fn of_type<'a>(items: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
        items.iter().filter(|item| item["type"] == kind).collect()
    }

    #[tokio::test]
    async fn a_wire_ending_on_a_pin_is_not_dangling() {
        let items = orphans("").await;
        let dangling = of_type(&items, "dangling_wire_end");
        assert_eq!(dangling.len(), 1, "only the free end counts: {items:?}");
        assert_eq!(dangling[0]["x"], 90.0);
        assert_eq!(dangling[0]["wire_uuid"], "w1");
    }

    #[tokio::test]
    async fn a_label_mid_segment_is_not_floating() {
        let items = orphans("").await;
        let floating = of_type(&items, "floating_label");
        assert_eq!(floating.len(), 1, "only ORPHAN floats: {items:?}");
        assert_eq!(floating[0]["net"], "ORPHAN");
    }

    #[tokio::test]
    async fn a_pin_with_nothing_on_it_is_reported() {
        let items = orphans("").await;
        let pins = of_type(&items, "unconnected_pin");
        assert_eq!(pins.len(), 1, "U1's pin is wired: {items:?}");
        assert_eq!(pins[0]["reference"], "U2");
        assert_eq!(pins[0]["pin"], "1");
        assert_eq!(pins[0]["pin_name"], "A");
    }

    /// The reported #249 case: a label directly on a pin without a wire is a
    /// legal KiCAD connection and connects both items.
    #[tokio::test]
    async fn a_label_on_a_bare_pin_connects_both_items() {
        let items = orphans("  (label \"NC_SIG\" (at 150 150 0))\n").await;
        let floating = of_type(&items, "floating_label");
        assert_eq!(floating.len(), 1, "NC_SIG is on U2's pin: {items:?}");
        assert_eq!(floating[0]["net"], "ORPHAN");
        assert!(
            of_type(&items, "unconnected_pin").is_empty(),
            "the label connects U2: {items:?}"
        );
    }

    #[tokio::test]
    async fn a_no_connect_flag_exempts_its_pin() {
        let items = orphans("  (no_connect (at 150 150) (uuid \"nc1\"))\n").await;
        assert!(
            of_type(&items, "unconnected_pin").is_empty(),
            "no-connect covers U2: {items:?}"
        );
    }

    #[tokio::test]
    async fn an_intrinsically_no_connect_pin_is_exempt() {
        let no_connect_pin = schematic("").replace(
            "(pin passive line (at 0 0 0)",
            "(pin no_connect line (at 0 0 0)",
        );
        let body = call(&no_connect_pin, json!({})).await;
        let items = body["orphans"].as_array().unwrap();
        assert!(
            of_type(items, "unconnected_pin").is_empty(),
            "library no-connect pins are intentional: {items:?}"
        );
    }

    /// A multi-unit symbol must contribute only the pins from the placed unit.
    #[tokio::test]
    async fn another_unit_does_not_contribute_a_phantom_pin() {
        let schematic = r#"(kicad_sch
  (version 20260306)
  (generator "eeschema")
  (uuid "root")
  (lib_symbols
    (symbol "Test:D"
      (symbol "D_1_1"
        (pin passive line (at 0 0 0) (length 0) (name "A") (number "1"))
      )
      (symbol "D_2_1"
        (pin passive line (at 10 0 0) (length 0) (name "B") (number "2"))
      )
    )
  )
  (wire (pts (xy 90 100) (xy 100 100)) (uuid "w1"))
  (label "SIG" (at 90 100 0))
  (symbol (lib_id "Test:D") (at 100 100 0) (unit 1) (uuid "u1")
    (property "Reference" "U1" (at 100 100 0))
    (property "Value" "D" (at 100 100 0))
  )
  (sheet_instances (path "/" (page "1")))
)
"#;
        let body = call(schematic, json!({})).await;
        assert_eq!(body["orphan_count"], 0, "unit 2 is not placed here: {body}");
    }

    #[tokio::test]
    async fn a_wire_ending_on_a_sheet_pin_is_not_dangling() {
        let sheet = r#"  (sheet (at 80 95) (size 10 10) (uuid "s1")
    (property "Sheetname" "sub" (at 80 95 0))
    (property "Sheetfile" "sub.kicad_sch" (at 80 95 0))
    (pin "SIG" input (at 90 100 180) (uuid "sp1"))
  )
"#;
        let items = orphans(sheet).await;
        assert!(
            of_type(&items, "dangling_wire_end").is_empty(),
            "both ends terminate: {items:?}"
        );
    }

    #[tokio::test]
    async fn tolerance_must_be_positive() {
        for tolerance in [0.0, -0.05] {
            let result = call_result(&schematic(""), json!({ "tolerance": tolerance })).await;
            assert!(result.is_error, "accepted tolerance {tolerance}");
        }
    }
}

#[cfg(test)]
#[cfg(test)]
mod power_symbol_net_tests {
    use super::*;
    use crate::tools::ServerConfig;
    use std::io::Write;
    use std::sync::Arc;

    /// R1 with pin 1 on a `SIG` label and pin 2 wired down to a `power:GND`
    /// symbol. The rail is what a label-only net graph loses.
    const SCH: &str = include_str!("../../tests/fixtures/power_rail.kicad_sch");

    /// Run a tool by name against a temp file holding `sch`.
    async fn call(sch: &str, tool: &str, mut args: serde_json::Value) -> serde_json::Value {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(sch.as_bytes()).unwrap();
        f.flush().unwrap();

        args["schematic"] = json!(f.path().to_str().unwrap());
        let def = tools().into_iter().find(|t| t.name == tool).unwrap();
        let ctx = ToolContext::new(
            ServerConfig::default(),
            Arc::new(crate::router::ToolRouter::new()),
        );
        let result = (def.handler)(&args, Arc::new(ctx)).await.unwrap();
        assert!(!result.is_error, "{tool} failed");
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(text).unwrap()
    }

    /// The S-expression path: a pin reached only through a power symbol used to
    /// report no net at all.
    #[tokio::test]
    async fn a_pin_on_a_rail_reports_it() {
        let s = call(
            SCH,
            "get_pin_connections",
            json!({ "reference": "R1", "pin_number": "2" }),
        )
        .await;
        assert_eq!(s["net"], "GND");
    }

    #[tokio::test]
    async fn the_rail_lists_the_components_on_it() {
        let s = call(SCH, "get_net_components", json!({ "net": "GND" })).await;
        let refs: Vec<&str> = s["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["reference"].as_str().unwrap())
            .collect();
        assert!(refs.contains(&"R1"), "R1 is on GND, got {refs:?}");
    }

    #[tokio::test]
    async fn tracing_a_point_on_the_rail_names_it() {
        let s = call(SCH, "trace_from_point", json!({ "x": 100.0, "y": 103.81 })).await;
        assert_eq!(s["net"], "GND");
    }

    #[tokio::test]
    async fn the_rail_is_listed_among_the_nets() {
        let s = call(SCH, "list_schematic_nets", json!({})).await;
        let nets: Vec<&str> = s["nets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap())
            .collect();
        assert_eq!(nets, vec!["GND", "SIG"]);
    }

    /// A rail carried onto a wire that also carries a named label is a short,
    /// and it was invisible while the graph knew only labels.
    #[tokio::test]
    async fn a_rail_shorted_to_a_named_net_is_reported() {
        let shorted = SCH.replace(
            "(label \"SIG\" (at 100 96.19 0)",
            "(label \"SIG\" (at 100 106 0)",
        );
        let s = call(&shorted, "find_shorted_nets", json!({})).await;
        assert_eq!(s["short_count"], 1, "SIG and GND share one wire: {s}");
    }
}
