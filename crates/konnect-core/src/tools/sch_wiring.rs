//! `sch_wiring` toolset — wires, net labels, power symbols, junctions, no-connects.
//!
//! Key rule: Every wire add operation must auto-detect T-junctions and insert
//! junction dots. This uses `konnect_sexp::schematic::find_t_junctions`.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    get_path, opt_f64, opt_str, project_name_for, require_array, require_f64, require_str,
    ToolContext, ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    commit_file_transaction,
    geometry::{points_coincident, snap_point},
    parser::parse_sexp,
    schematic::{
        extract_sheet_pins, extract_symbol_instances, extract_wires, find_lib_symbol,
        find_t_junctions, format_junction, format_wire, parse_at, pin_endpoint,
        pin_outward_direction, read_schematic, symbol_bounds_for_instance, Label, LabelKind,
        SymbolBounds, Wire,
    },
    writer::{
        apply_edits, find_balanced_block, find_block_starts, find_block_with_leading_whitespace,
        find_direct_child_blocks, find_enclosing_block, read_consistent, write_atomic_if_unchanged,
        SexpEdit,
    },
    DocumentRevision, FileTransition,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

const DEFAULT_LAYOUT_CANDIDATE_COUNT: usize = 3;
const MAX_LAYOUT_CANDIDATE_COUNT: usize = 5;
const MAX_LAYOUT_STATES_EVALUATED: usize = 128;
const GRID_STEP_COST: u32 = 1;
const BEND_COST: u32 = 8;
const CLEARANCE_PENALTY: u32 = 3;
const EXISTING_ITEM_MOVE_BASE_COST: u32 = 80;
const EXISTING_ORIENTATION_CHANGE_COST: u32 = 40;
const MOVE_GRID_STEP_COST: u32 = 2;
const WIRE_GRID_STEP_COST: u32 = 1;
const WIRE_BEND_COST: u32 = 8;
const APPROXIMATE_BODY_PENALTY: u32 = 100;
const SHEET_PIN_SIDE_CHANGE_COST: u32 = 5;
const LOCAL_LAYOUT_ALGORITHM_VERSION: &str = "local-layout-planner-v1";
const PROGRESSIVE_ROUTE_BOUNDS: [i64; 5] = [4, 8, 16, 32, 64];

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "add_wire",
            "Add a wire segment between two points. The wire must be horizontal or vertical. \
             T-junctions are automatically detected and junction dots inserted.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" }
                },
                "required": ["schematic", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_add_wire(args, ctx).await }
        ),
        tool!(
            "batch_add_wire",
            "Add multiple wire segments in a single file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "wires": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "x1": { "type": "number" }, "y1": { "type": "number" },
                                "x2": { "type": "number" }, "y2": { "type": "number" }
                            },
                            "required": ["x1", "y1", "x2", "y2"]
                        }
                    }
                },
                "required": ["schematic", "wires"]
            }),
            |args, ctx| async move { handle_batch_add_wire(args, ctx).await }
        ),
        tool!(
            "delete_schematic_wire",
            "Delete a wire segment by its UUID, or by matching BOTH endpoints \
             (all four of x1/y1/x2/y2, either direction). Fails without deleting \
             anything when no wire matches. Junction dots the wire leaves \
             unjustified are removed with it.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "uuid": { "type": "string", "description": "Wire UUID (preferred)" },
                    "x1": { "type": "number", "description": "Endpoint 1 X in mm (required with y1/x2/y2 when no uuid)" },
                    "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_delete_wire(args, ctx).await }
        ),
        tool!(
            "batch_delete_schematic_wire",
            "Delete multiple wire segments in a single file read/write cycle. \
             Junction dots the wires leave unjustified are removed with them, \
             reported as junctions_pruned_count.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "uuids": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["schematic", "uuids"]
            }),
            |args, ctx| async move { handle_batch_delete_wire(args, ctx).await }
        ),
        tool!(
            "split_wire_at_point",
            "Split a wire at a given point, creating two wire segments and a junction. \
             Note: a pin landing mid-wire only needs a junction dot to connect \
             (see add_junction) — splitting the wire is not required.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "x", "y"]
            }),
            |args, ctx| async move { handle_split_wire_at_point(args, ctx).await }
        ),
        tool!(
            "add_schematic_net_label",
            "Add a net label to the schematic. Type can be 'net_label', 'global_label', \
             or 'hierarchical_label'.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string", "description": "Net name" },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "rotation": { "type": "number", "default": 0 },
                    "label_type": {
                        "type": "string",
                        "enum": ["net_label", "global_label", "hierarchical_label"],
                        "default": "net_label"
                    },
                    "shape": {
                        "type": "string",
                        "description": "Shape for global/hierarchical labels (input/output/bidirectional/etc.)",
                        "default": "input"
                    }
                },
                "required": ["schematic", "net", "x", "y"]
            }),
            |args, ctx| async move { handle_add_net_label(args, ctx).await }
        ),
        tool!(
            "delete_schematic_net_label",
            "Delete a net label by net name and position.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "net", "x", "y"]
            }),
            |args, ctx| async move { handle_delete_net_label(args, ctx).await }
        ),
        tool!(
            "rotate_schematic_label",
            "Rotate a net label to a new angle and update its justify direction accordingly.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "rotation": { "type": "number" }
                },
                "required": ["schematic", "net", "x", "y", "rotation"]
            }),
            |args, ctx| async move { handle_rotate_label(args, ctx).await }
        ),
        tool!(
            "move_labels_by_offset",
            "Move all labels matching a net name by a given X/Y offset.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string" },
                    "dx": { "type": "number" }, "dy": { "type": "number" }
                },
                "required": ["schematic", "net", "dx", "dy"]
            }),
            |args, ctx| async move { handle_move_labels_by_offset(args, ctx).await }
        ),
        tool!(
            "batch_rotate_labels",
            "Rotate multiple labels by net name in a single file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "labels": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "net": { "type": "string" },
                                "x": { "type": "number" }, "y": { "type": "number" },
                                "rotation": { "type": "number" }
                            }
                        }
                    }
                },
                "required": ["schematic", "labels"]
            }),
            |args, ctx| async move { handle_batch_rotate_labels(args, ctx).await }
        ),
        tool!(
            "add_power_symbol",
            "Add a power symbol (VCC, GND, etc.) to the schematic. Auto-numbers the \
             internal #PWR reference to the lowest number free on the sheet.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "power_net": { "type": "string", "description": "Net name (e.g. 'VCC', 'GND')" },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "rotation": { "type": "number", "default": 0 }
                },
                "required": ["schematic", "power_net", "x", "y"]
            }),
            |args, ctx| async move { handle_add_power_symbol(args, ctx).await }
        ),
        tool!(
            "add_no_connect",
            "Add a no-connect flag (X marker) to an unconnected pin endpoint.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "x", "y"]
            }),
            |args, ctx| async move { handle_add_no_connect(args, ctx).await }
        ),
        tool!(
            "delete_no_connect",
            "Remove a no-connect flag at a given position.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "x", "y"]
            }),
            |args, ctx| async move { handle_delete_no_connect(args, ctx).await }
        ),
        tool!(
            "batch_delete_no_connect",
            "Delete multiple no-connect flags in a single file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "positions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "x": { "type": "number" }, "y": { "type": "number" } }
                        }
                    }
                },
                "required": ["schematic", "positions"]
            }),
            |args, ctx| async move { handle_batch_delete_no_connect(args, ctx).await }
        ),
        tool!(
            "batch_add_no_connect",
            "Add multiple no-connect flags in a single file read/write cycle. Marking the unused \
             pins of one MCU is routinely 15-20 flags, which is 15-20 round trips through \
             add_no_connect.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "positions": {
                        "type": "array",
                        "description": "Pin endpoints to flag, as {x, y}",
                        "items": {
                            "type": "object",
                            "properties": { "x": { "type": "number" }, "y": { "type": "number" } },
                            "required": ["x", "y"]
                        }
                    }
                },
                "required": ["schematic", "positions"]
            }),
            |args, ctx| async move { handle_batch_add_no_connect(args, ctx).await }
        ),
        tool!(
            "add_junction",
            "Add a junction dot at a point where wires cross or T-intersect, or where \
             a pin lands mid-wire. A junction alone connects a mid-wire pin; \
             splitting the wire is not required.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "x", "y"]
            }),
            |args, ctx| async move { handle_add_junction(args, ctx).await }
        ),
        tool!(
            "batch_add_junction",
            "Add multiple junction dots in a single file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "positions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "x": { "type": "number" }, "y": { "type": "number" } }
                        }
                    }
                },
                "required": ["schematic", "positions"]
            }),
            |args, ctx| async move { handle_batch_add_junction(args, ctx).await }
        ),
        tool!(
            "connect_to_net",
            "Connect a pin to a named net by adding a short wire stub and a net label. \
             Name the pin with reference + pin_number, or give its coordinates directly.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string",
                        "description": "Component reference, e.g. 'U1'. Use with pin_number instead of pin_x/pin_y." },
                    "pin_number": { "type": "string", "description": "Pin number, e.g. '3'" },
                    "pin_x": { "type": "number", "description": "Pin X in mm; alternative to reference + pin_number" },
                    "pin_y": { "type": "number", "description": "Pin Y in mm" },
                    "net": { "type": "string" },
                    "direction": {
                        "type": "string",
                        "description": "Direction to route the wire stub. 'auto' (default) points it \
                                        away from the symbol body so the label text does not run back \
                                        across the pin names; it falls back to 'right' on a bare point.",
                        "enum": ["auto", "right", "left", "up", "down"],
                        "default": "auto"
                    },
                    "stub_length": { "type": "number", "default": 2.54,
                        "description": "Length of the wire stub in mm" },
                    "label_type": {
                        "type": "string",
                        "enum": ["net_label", "global_label"],
                        "default": "net_label"
                    }
                },
                "required": ["schematic", "net"]
            }),
            |args, ctx| async move { handle_connect_to_net(args, ctx).await }
        ),
        tool!(
            "connect_pins",
            "Connect two component pins by reference and pin number. \
             Looks up pin coordinates automatically and routes a wire between them.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "ref1": { "type": "string", "description": "First component reference (e.g. 'R1')" },
                    "pin1": { "type": "string", "description": "First pin number (e.g. '1')" },
                    "ref2": { "type": "string", "description": "Second component reference (e.g. 'U1')" },
                    "pin2": { "type": "string", "description": "Second pin number (e.g. '3')" }
                },
                "required": ["schematic", "ref1", "pin1", "ref2", "pin2"]
            }),
            |args, ctx| async move { handle_connect_pins(args, ctx).await }
        ),
        tool!(
            "add_schematic_connection",
            "Connect two schematic points directly with a wire (auto-routes H+V segments). \
             Use connect_pins if you have component references instead of coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" }
                },
                "required": ["schematic", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_add_schematic_connection(args, ctx).await }
        ),
        tool!(
            "materialize_schematic_net",
            "Preview or apply net-aware visible wiring for an already-correct same-sheet net. \
             The tool resolves the parsed connectivity graph first, builds deterministic \
             orthogonal trunk/branch wire geometry only between endpoints already on the \
             requested net, validates component-pin endpoint membership and shorts before \
             writing, and can optionally remove redundant plain local labels while preserving \
             global/hierarchical labels and at least one local net name.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string", "description": "Existing net name to materialize" },
                    "pins": {
                        "type": "array",
                        "description": "Optional explicit endpoint subset. Each item is {reference,pin_number}. Every named pin must already belong to net.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "pin_number": { "type": "string" }
                            },
                            "required": ["reference", "pin_number"]
                        }
                    },
                    "strategy": {
                        "type": "object",
                        "description": "Deterministic trunk/branch routing hints.",
                        "properties": {
                            "preferred_orientation": { "type": "string", "enum": ["horizontal", "vertical"], "default": "horizontal" },
                            "trunk_x": { "type": "number", "description": "X coordinate for a vertical trunk" },
                            "trunk_y": { "type": "number", "description": "Y coordinate for a horizontal trunk" }
                        },
                        "additionalProperties": false
                    },
                    "grid": { "type": "number", "description": "Schematic grid used for endpoint diagnostics", "default": 1.27 },
                    "remove_redundant_local_labels": {
                        "type": "boolean",
                        "description": "After wire validation, remove redundant plain labels for this net. Global/hierarchical labels and one local name-preserving label are retained.",
                        "default": false
                    },
                    "dry_run": { "type": "boolean", "description": "Default true. false applies the already-reviewed plan.", "default": true },
                    "plan_revision": { "type": "string", "description": "Exact dry-run revision required when dry_run=false" }
                },
                "required": ["schematic", "net"]
            }),
            |args, ctx| async move { handle_materialize_schematic_net(args, ctx).await }
        ),
        tool!(
            "refactor_schematic_net_representation",
            "Preview or apply a transactional graphical refactor for an already-correct schematic net. \
             Modes include wired_local net materialization, expand_coincident_connection, \
             power_symbol conversion, and scope_diagnostic. Every apply compares final component-pin \
             endpoint membership and shorted net sets before writing.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string", "description": "Existing net name to refactor or inspect" },
                    "representation": {
                        "type": "string",
                        "enum": ["wired_local", "expand_coincident_connection", "power_symbol", "scope_diagnostic"],
                        "default": "wired_local"
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["sheet", "project"],
                        "default": "sheet"
                    },
                    "pins": {
                        "type": "array",
                        "description": "Optional explicit endpoint subset. Each item is {reference,pin_number}. Every named pin must already belong to net.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "pin_number": { "type": "string" }
                            },
                            "required": ["reference", "pin_number"]
                        }
                    },
                    "strategy": {
                        "type": "object",
                        "properties": {
                            "preferred_orientation": { "type": "string", "enum": ["horizontal", "vertical"], "default": "horizontal" },
                            "trunk_x": { "type": "number" },
                            "trunk_y": { "type": "number" },
                            "ordering": { "type": "array", "items": { "type": "string" } }
                        },
                        "additionalProperties": false
                    },
                    "expand": {
                        "type": "object",
                        "description": "Coincident-endpoint expansion. move_reference names the symbol instance to translate before wiring.",
                        "properties": {
                            "move_reference": { "type": "string" },
                            "dx": { "type": "number" },
                            "dy": { "type": "number" }
                        },
                        "additionalProperties": false
                    },
                    "grid": { "type": "number", "description": "Schematic grid used for endpoint diagnostics and optional snapping", "default": 1.27 },
                    "remove_redundant_local_labels": { "type": "boolean", "default": false },
                    "scope_policy": {
                        "type": "string",
                        "enum": ["preserve_existing_scope", "allow_global_power_scope"],
                        "default": "preserve_existing_scope"
                    },
                    "dry_run": { "type": "boolean", "default": true },
                    "plan_revision": { "type": "string", "description": "Exact dry-run revision required when dry_run=false" }
                },
                "required": ["schematic", "net"]
            }),
            |args, ctx| async move { handle_refactor_schematic_net_representation(args, ctx).await }
        ),
    ]
}

// ─── Shared: insert wires/labels BEFORE symbol instances ─────────────────────
//
// KiCAD 10 requires this element order in .kicad_sch files:
//   1. lib_symbols
//   2. wire, bus, junction, no_connect, net_label, global_label, text, etc.
//   3. symbol (instances) — MUST come last
//
// So wires and labels must be inserted before the first (symbol block,
// NOT at the end of the file.

pub(crate) fn insert_before_close(content: &str, new_sexp: &str) -> String {
    // Find the first top-level (symbol block — insert before it
    let item_pos = find_first_symbol_instance(content)
        .unwrap_or_else(|| content.rfind(')').unwrap_or(content.len()));
    let line_start = content[..item_pos].rfind('\n').map_or(0, |pos| pos + 1);
    let insert_pos = if content[line_start..item_pos]
        .chars()
        .all(char::is_whitespace)
    {
        line_start
    } else {
        item_pos
    };
    let insertion = format!("{}\n", new_sexp.trim_matches('\n'));
    let edits = vec![SexpEdit::insert(insert_pos, insertion)];
    apply_edits(content.to_string(), edits)
}

/// Find the byte offset of the first top-level symbol instance in the schematic.
/// Top-level instances have `(lib_id` as a child, while lib_symbols definitions don't.
/// Returns the position where wires/labels should be inserted BEFORE.
fn find_first_symbol_instance(content: &str) -> Option<usize> {
    for (start, end) in find_direct_child_blocks(content, "kicad_sch") {
        let block = &content[start..end];
        if block.starts_with("(symbol") && block.contains("(lib_id ") {
            return Some(start);
        }
    }
    None
}

// ─── Bridge: convert konnect-schematic-editor wires to konnect_sexp wires ──────

fn cse_wires_to_sexp(sch: &cse::Schematic) -> Vec<konnect_sexp::schematic::Wire> {
    sch.wires
        .iter()
        .map(|w| konnect_sexp::schematic::Wire {
            x1: w.start.0,
            y1: w.start.1,
            x2: w.end.0,
            y2: w.end.1,
            uuid: Some(w.uuid.clone()),
        })
        .collect()
}

// ─── Wire insertion with T-junction detection ─────────────────────────────────

/// Pin endpoints that lie strictly inside a wire segment. Each needs a
/// junction dot: KiCad connects a mid-wire pin only through a junction
/// (verified with kicad-cli 10 — no wire split required).
fn pins_mid_segment(pins: &[(f64, f64)], x1: f64, y1: f64, x2: f64, y2: f64) -> Vec<(f64, f64)> {
    let tol = 0.01;
    pins.iter()
        .copied()
        .filter(|&(px, py)| {
            konnect_sexp::geometry::point_on_segment(px, py, x1, y1, x2, y2, tol)
                && !konnect_sexp::geometry::points_coincident(px, py, x1, y1, tol)
                && !konnect_sexp::geometry::points_coincident(px, py, x2, y2, tol)
        })
        .collect()
}

/// Add a junction dot at each position that does not already carry one.
///
/// `find_t_junctions` reports every T on the sheet, not only the ones the new
/// wire made, so an unguarded loop re-emits a dot at every existing T on every
/// call — quadratic in a batch. `insert_wire_with_junctions` guards the same
/// way on the string path.
fn add_missing_junctions(sch: &mut cse::Schematic, positions: &[(f64, f64)]) {
    for &(x, y) in positions {
        if !sch
            .junctions
            .iter()
            .any(|j| konnect_sexp::geometry::points_coincident(x, y, j.x, j.y, 0.01))
        {
            sch.add_junction(x, y);
        }
    }
}

pub(crate) fn insert_wire_with_junctions(
    content: String,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
) -> String {
    // Parse existing wires to detect new T-junctions
    let tree = konnect_sexp::parse_sexp(&content).ok();
    let mut existing_wires = tree.as_ref().map(extract_wires).unwrap_or_default();

    // Existing junction positions, so a hit already marked isn't re-inserted
    // (L-bends, and any loop calling this repeatedly, would otherwise double it).
    let existing_junctions = tree
        .as_ref()
        .map(konnect_sexp::schematic::extract_junctions)
        .unwrap_or_default();

    // Add the new wire to the set before checking junctions (it may form T's too)
    let new_wire = konnect_sexp::schematic::Wire {
        x1,
        y1,
        x2,
        y2,
        uuid: None,
    };
    existing_wires.push(new_wire);

    let mut junctions = find_t_junctions(&existing_wires, 0.01);
    // Existing pins the new wire passes over also need junction dots.
    let pins = tree
        .as_ref()
        .map(crate::tools::all_pin_endpoints)
        .unwrap_or_default();
    for (px, py) in pins_mid_segment(&pins, x1, y1, x2, y2) {
        if !junctions
            .iter()
            .any(|&(jx, jy)| konnect_sexp::geometry::points_coincident(px, py, jx, jy, 0.01))
        {
            junctions.push((px, py));
        }
    }

    let mut c = content;
    c = insert_before_close(&c, &format_wire(x1, y1, x2, y2));
    for (jx, jy) in junctions {
        if existing_junctions
            .iter()
            .any(|(ex, ey)| konnect_sexp::geometry::points_coincident(jx, jy, *ex, *ey, 0.01))
        {
            continue;
        }
        c = insert_before_close(&c, &format_junction(jx, jy));
    }
    c
}

/// Route a wire between two points: a single straight wire when axis-aligned,
/// otherwise an H-then-V L-bend, each leg going through T-junction detection.
pub(crate) fn route_between(content: String, x1: f64, y1: f64, x2: f64, y2: f64) -> String {
    if (x1 - x2).abs() < 0.01 || (y1 - y2).abs() < 0.01 {
        insert_wire_with_junctions(content, x1, y1, x2, y2)
    } else {
        let mid_x = x2;
        let mid_y = y1;
        let content = insert_wire_with_junctions(content, x1, y1, mid_x, mid_y);
        insert_wire_with_junctions(content, mid_x, mid_y, x2, y2)
    }
}

// ─── Net-aware wire materialization ───────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
struct Segment {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

impl Segment {
    fn json(self) -> serde_json::Value {
        json!({ "x1": self.x1, "y1": self.y1, "x2": self.x2, "y2": self.y2 })
    }

    fn normalized(self) -> Self {
        if (self.x1, self.y1) <= (self.x2, self.y2) {
            self
        } else {
            Self {
                x1: self.x2,
                y1: self.y2,
                x2: self.x1,
                y2: self.y1,
            }
        }
    }

    fn is_zero(self) -> bool {
        points_coincident(self.x1, self.y1, self.x2, self.y2, 0.01)
    }
}

#[derive(Debug, Clone)]
struct NetEndpoint {
    kind: NetEndpointKind,
    reference: String,
    unit: u32,
    pin_number: String,
    pin_name: String,
    x: f64,
    y: f64,
    orientation_degrees: f64,
    grid: f64,
    on_grid: bool,
    nearest_grid_x: f64,
    nearest_grid_y: f64,
    attachment_depends_on_off_grid_position: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NetEndpointKind {
    ComponentPin,
    SheetPin {
        sheet_uuid: String,
        sheet_name: String,
        pin_uuid: String,
    },
}

impl NetEndpoint {
    fn key(&self) -> String {
        match &self.kind {
            NetEndpointKind::ComponentPin => {
                format!("{}:{}:{}", self.reference, self.unit, self.pin_number)
            }
            NetEndpointKind::SheetPin {
                sheet_uuid,
                pin_uuid,
                ..
            } => format!("sheet:{sheet_uuid}:{pin_uuid}"),
        }
    }

    fn json(&self) -> serde_json::Value {
        json!({
            "reference": self.reference,
            "unit": self.unit,
            "pin_number": self.pin_number,
            "pin_name": self.pin_name,
            "x": self.x,
            "y": self.y,
            "orientation_degrees": self.orientation_degrees,
            "grid": self.grid,
            "on_grid": self.on_grid,
            "nearest_grid_position": { "x": self.nearest_grid_x, "y": self.nearest_grid_y },
            "attachment_depends_on_off_grid_position": self.attachment_depends_on_off_grid_position,
            "kind": match &self.kind {
                NetEndpointKind::ComponentPin => json!("component_pin"),
                NetEndpointKind::SheetPin { sheet_uuid, sheet_name, pin_uuid } => json!({
                    "type": "sheet_pin",
                    "sheet_uuid": sheet_uuid,
                    "sheet_name": sheet_name,
                    "pin_uuid": pin_uuid
                }),
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SheetPinSide {
    Left,
    Right,
    Top,
    Bottom,
}

impl SheetPinSide {
    fn rotation(self) -> f64 {
        match self {
            Self::Left => 180.0,
            Self::Right => 0.0,
            Self::Top => 90.0,
            Self::Bottom => 270.0,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
            Self::Top => "TOP",
            Self::Bottom => "BOTTOM",
        }
    }

    fn is_vertical_edge(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

#[derive(Debug, Clone)]
struct SheetPinChange {
    sheet_uuid: String,
    sheet_name: String,
    pin_uuid: String,
    pin_name: String,
    from_side: SheetPinSide,
    to_side: SheetPinSide,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    rotation: f64,
}

impl SheetPinChange {
    fn json(&self) -> serde_json::Value {
        json!({
            "sheet_uuid": self.sheet_uuid,
            "sheet_name": self.sheet_name,
            "pin_uuid": self.pin_uuid,
            "pin_name": self.pin_name,
            "from_side": self.from_side.as_str(),
            "to_side": self.to_side.as_str(),
            "from": {"x": self.from_x, "y": self.from_y},
            "to": {"x": self.to_x, "y": self.to_y},
            "rotation": self.rotation
        })
    }
}

#[derive(Debug, Clone)]
struct SemanticSnapshot {
    pin_nets: BTreeMap<String, Option<String>>,
    shorts: Vec<Vec<String>>,
}

#[derive(Debug, Clone)]
struct MaterializeNetPlan {
    schematic: std::path::PathBuf,
    before: String,
    net: String,
    endpoints: Vec<NetEndpoint>,
    existing_wires: Vec<Wire>,
    labels: Vec<Label>,
    segments: Vec<Segment>,
    sheet_pin_changes: Vec<SheetPinChange>,
    removable_local_labels: Vec<(f64, f64)>,
    remove_redundant_local_labels: bool,
    before_snapshot: SemanticSnapshot,
    final_content: String,
    plan_revision: String,
}

impl MaterializeNetPlan {
    fn response(&self, dry_run: bool, applied: bool, labels_removed: usize) -> serde_json::Value {
        json!({
            "dry_run": dry_run,
            "applied": applied,
            "safe_to_apply": true,
            "plan_revision": self.plan_revision,
            "schematic": self.schematic.display().to_string(),
            "net": self.net,
            "endpoints": self.endpoints.iter().map(NetEndpoint::json).collect::<Vec<_>>(),
            "endpoint_count": self.endpoints.len(),
            "existing_visible_wires": self.existing_wires.iter().map(|wire| json!({
                "x1": wire.x1,
                "y1": wire.y1,
                "x2": wire.x2,
                "y2": wire.y2,
                "uuid": wire.uuid
            })).collect::<Vec<_>>(),
            "labels": self.labels.iter().map(|label| json!({
                "net": label.net,
                "kind": format!("{:?}", label.kind),
                "x": label.x,
                "y": label.y,
                "rotation": label.rotation,
                "uuid": label.uuid
            })).collect::<Vec<_>>(),
            "candidate_wire_topology": self.segments.iter().map(|segment| segment.json()).collect::<Vec<_>>(),
            "candidate_wire_count": self.segments.len(),
            "candidate_sheet_pin_changes": self.sheet_pin_changes.iter().map(SheetPinChange::json).collect::<Vec<_>>(),
            "remove_redundant_local_labels": self.remove_redundant_local_labels,
            "labels_that_would_be_removed": self.removable_local_labels.iter().map(|(x, y)| json!({"x": x, "y": y})).collect::<Vec<_>>(),
            "labels_removed": labels_removed,
            "semantic_connectivity_comparison": semantic_comparison(&self.before_snapshot, &self.final_content).unwrap_or_else(|error| json!({
                "preserved": false,
                "error": error.to_string()
            })),
            "ambiguity": [],
            "warnings": []
        })
    }
}

async fn handle_materialize_schematic_net(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let schematic = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let remove_redundant_local_labels = args["remove_redundant_local_labels"]
        .as_bool()
        .unwrap_or(false);

    let plan =
        match build_materialize_net_plan(&schematic, &net, args, remove_redundant_local_labels) {
            Ok(plan) => plan,
            Err(error) => return Ok(error),
        };
    if dry_run {
        return Ok(CallToolResult::json(&plan.response(true, false, 0)));
    }

    let supplied_revision = args["plan_revision"].as_str().unwrap_or_default();
    if supplied_revision != plan.plan_revision {
        return Ok(CallToolResult::error(format!(
            "plan_revision mismatch; rerun dry_run and apply exactly '{}'",
            plan.plan_revision
        )));
    }

    validate_semantic_snapshot(
        &plan.before_snapshot,
        &plan.final_content,
        "final transaction",
    )?;
    validate_kicad_netlist_if_configured(ctx, &plan.schematic, &plan.before, &plan.final_content)
        .await?;
    let labels_removed = if plan.remove_redundant_local_labels {
        plan.removable_local_labels.len()
    } else {
        0
    };
    write_atomic_if_unchanged(&plan.schematic, &plan.before, &plan.final_content)?;
    Ok(CallToolResult::json(&plan.response(
        false,
        true,
        labels_removed,
    )))
}

#[derive(Debug, Clone)]
struct OptimizeLayoutPlan {
    schematic: PathBuf,
    before: String,
    candidates: Vec<LocalLayoutCandidate>,
    plan_revision: String,
    before_snapshot: SemanticSnapshot,
}

impl OptimizeLayoutPlan {
    fn response(&self, dry_run: bool, applied: bool) -> serde_json::Value {
        json!({
            "dry_run": dry_run,
            "applied": applied,
            "safe_to_apply": true,
            "schematic": self.schematic.display().to_string(),
            "plan_revision": self.plan_revision,
            "candidate_count": self.candidates.len(),
            "algorithm": {
                "version": LOCAL_LAYOUT_ALGORITHM_VERSION,
                "default_top_k": DEFAULT_LAYOUT_CANDIDATE_COUNT,
                "maximum_top_k": MAX_LAYOUT_CANDIDATE_COUNT,
                "max_layout_states_evaluated": MAX_LAYOUT_STATES_EVALUATED,
                "progressive_route_bounds_grid_steps": PROGRESSIVE_ROUTE_BOUNDS,
                "a_star_state": ["x", "y", "incoming_direction"],
                "heuristic": "manhattan",
                "score_constants": {
                    "existing_item_move_base_cost": EXISTING_ITEM_MOVE_BASE_COST,
                    "existing_orientation_change_cost": EXISTING_ORIENTATION_CHANGE_COST,
                    "move_grid_step_cost": MOVE_GRID_STEP_COST,
                    "wire_grid_step_cost": WIRE_GRID_STEP_COST,
                    "wire_bend_cost": WIRE_BEND_COST,
                    "clearance_penalty": CLEARANCE_PENALTY,
                    "approximate_body_penalty": APPROXIMATE_BODY_PENALTY,
                    "sheet_pin_side_change_cost": SHEET_PIN_SIDE_CHANGE_COST
                }
            },
            "candidates": self.candidates.iter().map(LocalLayoutCandidate::json).collect::<Vec<_>>()
        })
    }
}

pub(crate) async fn handle_optimize_schematic_layout(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let schematic = get_path(args, "schematic")?;
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let plan = match build_optimize_layout_plan(&schematic, args) {
        Ok(plan) => plan,
        Err(error) => return Ok(error),
    };
    if dry_run {
        return Ok(CallToolResult::json(&plan.response(true, false)));
    }

    let candidate_id = args["candidate_id"].as_str().unwrap_or_default();
    if candidate_id.is_empty() {
        return Ok(CallToolResult::error(
            "candidate_id is required when dry_run=false",
        ));
    }
    let supplied_revision = args["plan_revision"].as_str().unwrap_or_default();
    if supplied_revision != plan.plan_revision {
        return Ok(CallToolResult::error(
            json!({
                "code": "stale_plan_revision",
                "message": "schematic or candidate plan changed; rerun dry_run and apply its exact plan_revision",
                "plan_revision": plan.plan_revision
            })
            .to_string(),
        ));
    }
    let Some(candidate) = plan
        .candidates
        .iter()
        .find(|candidate| candidate.candidate_id == candidate_id)
    else {
        return Ok(CallToolResult::error(format!(
            "candidate_id '{candidate_id}' was not found in this current plan"
        )));
    };

    let base_content = apply_sheet_pin_changes_to_content(
        &plan.schematic,
        &plan.before,
        &candidate.sheet_pin_changes,
    )?;
    let all_segments = candidate
        .routes_by_net
        .values()
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    let final_content = apply_segments(base_content, &all_segments)?;
    validate_semantic_snapshot(
        &plan.before_snapshot,
        &final_content,
        "layout optimization final transaction",
    )?;
    validate_kicad_netlist_if_configured(ctx, &plan.schematic, &plan.before, &final_content)
        .await?;
    write_atomic_if_unchanged(&plan.schematic, &plan.before, &final_content)?;
    Ok(CallToolResult::json(&plan.response(false, true)))
}

fn build_optimize_layout_plan(
    schematic: &PathBuf,
    args: &serde_json::Value,
) -> Result<OptimizeLayoutPlan, CallToolResult> {
    let (before, tree) =
        read_schematic(schematic).map_err(|error| CallToolResult::error(error.to_string()))?;
    let wires = extract_wires(&tree);
    let labels = konnect_sexp::schematic::extract_all_net_labels(&tree);
    let grid = args["grid"].as_f64().unwrap_or(1.27);
    let item_uuids = string_array_arg(args, "item_uuids")?;
    let mut net_names = string_array_arg(args, "net_names")?;
    if item_uuids.is_empty() && net_names.is_empty() {
        return Err(CallToolResult::error(
            "at least one item_uuids entry or net_names entry is required",
        ));
    }
    net_names.extend(nets_for_item_uuids(&tree, &wires, &labels, &item_uuids));
    net_names.sort();
    net_names.dedup();
    if net_names.is_empty() {
        return Err(CallToolResult::error(
            "selected item_uuids did not resolve to any named local nets",
        ));
    }

    let requested_count = args["candidate_count"]
        .as_u64()
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_LAYOUT_CANDIDATE_COUNT);
    if requested_count > MAX_LAYOUT_CANDIDATE_COUNT {
        return Err(CallToolResult::error(format!(
            "candidate_count must be <= {MAX_LAYOUT_CANDIDATE_COUNT}"
        )));
    }

    let mut endpoints_by_net = BTreeMap::new();
    for net in &net_names {
        let endpoints = net_endpoints_for(&tree, net, args, grid)?;
        if endpoints.len() < 2 {
            return Err(CallToolResult::error(format!(
                "net '{net}' has fewer than two selected endpoints on this sheet"
            )));
        }
        endpoints_by_net.insert(net.clone(), endpoints);
    }
    let mut planner = LocalLayoutPlanner::new(
        &before,
        schematic,
        &tree,
        &wires,
        &labels,
        &endpoints_by_net,
        args,
    );
    let mut all_candidates = planner.plan_top_k(requested_count)?;
    all_candidates.sort_by(local_layout_candidate_cmp);
    all_candidates.truncate(requested_count.clamp(1, MAX_LAYOUT_CANDIDATE_COUNT));
    for (index, candidate) in all_candidates.iter_mut().enumerate() {
        candidate.rank = index + 1;
    }
    if all_candidates.is_empty() {
        return Err(CallToolResult::error(
            "no complete local layout candidate routed every requested net",
        ));
    }
    let before_snapshot =
        semantic_snapshot(&before).map_err(|error| CallToolResult::error(error.to_string()))?;
    let plan_revision = optimize_layout_plan_revision(
        &before,
        &item_uuids,
        &net_names,
        requested_count,
        &all_candidates,
    );
    Ok(OptimizeLayoutPlan {
        schematic: schematic.clone(),
        before,
        candidates: all_candidates,
        plan_revision,
        before_snapshot,
    })
}

fn string_array_arg(args: &serde_json::Value, field: &str) -> Result<Vec<String>, CallToolResult> {
    let Some(values) = args[field].as_array() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let Some(text) = value.as_str() else {
            return Err(CallToolResult::error(format!(
                "{field}[{index}] must be a string"
            )));
        };
        if !text.is_empty() {
            out.push(text.to_string());
        }
    }
    Ok(out)
}

fn nets_for_item_uuids(
    tree: &konnect_sexp::SexpNode,
    wires: &[Wire],
    labels: &[Label],
    item_uuids: &[String],
) -> Vec<String> {
    let wanted = item_uuids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut graph = crate::tools::sch_connectivity::net_graph_for(tree, wires, labels);
    let mut nets = Vec::new();
    for label in labels {
        if label
            .uuid
            .as_deref()
            .is_some_and(|uuid| wanted.contains(uuid))
        {
            if let Some(net) = graph.net_at(label.x, label.y) {
                nets.push(net);
            }
        }
    }
    for wire in wires {
        if wire
            .uuid
            .as_deref()
            .is_some_and(|uuid| wanted.contains(uuid))
        {
            if let Some(net) = graph
                .net_at(wire.x1, wire.y1)
                .or_else(|| graph.net_at(wire.x2, wire.y2))
            {
                nets.push(net);
            }
        }
    }
    let index = crate::tools::sch_connectivity::ConnectivityIndex::build(
        tree,
        wires,
        labels,
        crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
    );
    let instances = extract_symbol_instances(tree);
    for instance in &instances {
        if !instance
            .uuid
            .as_deref()
            .is_some_and(|uuid| wanted.contains(uuid))
        {
            continue;
        }
        for pin in index
            .placed_pins()
            .iter()
            .filter(|pin| pin.reference == instance.reference && pin.unit == instance.unit)
        {
            if let Some(net) = graph.net_at(pin.at.0, pin.at.1) {
                nets.push(net);
            }
        }
    }
    nets
}

fn optimize_layout_plan_revision(
    before: &str,
    item_uuids: &[String],
    net_names: &[String],
    candidate_count: usize,
    candidates: &[LocalLayoutCandidate],
) -> String {
    let mut material = format!(
        "{}\n{LOCAL_LAYOUT_ALGORITHM_VERSION}\n{candidate_count}",
        DocumentRevision::of(before)
    );
    for uuid in item_uuids {
        material.push_str("\nitem:");
        material.push_str(uuid);
    }
    for net in net_names {
        material.push_str("\nnet:");
        material.push_str(net);
    }
    for candidate in candidates {
        material.push('\n');
        material.push_str(&candidate.candidate_id);
        material.push(':');
        material.push_str(&candidate.structural_signature);
        material.push(':');
        material.push_str(&candidate.score.total_cost().to_string());
    }
    format!(
        "optimize-schematic-layout:{}",
        DocumentRevision::of(&material)
    )
}

fn build_materialize_net_plan(
    schematic: &std::path::PathBuf,
    net: &str,
    args: &serde_json::Value,
    remove_redundant_local_labels: bool,
) -> Result<MaterializeNetPlan, CallToolResult> {
    let (before, tree) =
        read_schematic(schematic).map_err(|error| CallToolResult::error(error.to_string()))?;
    let wires = extract_wires(&tree);
    let labels = konnect_sexp::schematic::extract_all_net_labels(&tree);
    let grid = args["grid"].as_f64().unwrap_or(1.27);
    if !labels.iter().any(|label| label.net == net) {
        return Err(CallToolResult::error(format!(
            "net '{net}' was not found in schematic labels or power symbols"
        )));
    }

    let mut graph = crate::tools::sch_connectivity::net_graph_for(&tree, &wires, &labels);
    let index = crate::tools::sch_connectivity::ConnectivityIndex::build(
        &tree,
        &wires,
        &labels,
        crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
    );
    let selected_keys = requested_materialize_pin_keys(&tree, args, net, &mut graph)?;
    let mut endpoints = Vec::new();
    for placed in index.placed_pins() {
        let key = format!("{}:{}:{}", placed.reference, placed.unit, placed.pin.number);
        if selected_keys
            .as_ref()
            .is_some_and(|keys| !keys.contains(&key))
        {
            continue;
        }
        if graph.net_at(placed.at.0, placed.at.1).as_deref() == Some(net) {
            endpoints.push(NetEndpoint {
                kind: NetEndpointKind::ComponentPin,
                reference: placed.reference.clone(),
                unit: placed.unit,
                pin_number: placed.pin.number.clone(),
                pin_name: placed.pin.name.clone(),
                x: placed.at.0,
                y: placed.at.1,
                orientation_degrees: placed.orientation_degrees,
                grid,
                on_grid: point_on_grid(placed.at.0, placed.at.1, grid),
                nearest_grid_x: snap_point(placed.at.0, placed.at.1, grid).0,
                nearest_grid_y: snap_point(placed.at.0, placed.at.1, grid).1,
                attachment_depends_on_off_grid_position: attachment_depends_on_off_grid_position(
                    &mut graph,
                    placed.at.0,
                    placed.at.1,
                    grid,
                ),
            });
        }
    }
    if selected_keys.is_none() {
        endpoints.extend(sheet_pin_endpoints_for(&tree, &mut graph, net, grid));
    }
    endpoints.sort_by(|a, b| a.key().cmp(&b.key()));
    if endpoints.len() < 2 {
        return Err(CallToolResult::error(format!(
            "net '{net}' has fewer than two selected component-pin endpoints on this sheet"
        )));
    }

    let candidate = materialize_candidate(
        &before, schematic, &tree, &wires, &labels, net, &endpoints, args,
    )?;
    let before_snapshot =
        semantic_snapshot(&before).map_err(|error| CallToolResult::error(error.to_string()))?;
    let mut final_content =
        apply_sheet_pin_changes_to_content(schematic, &before, &candidate.sheet_pin_changes)
            .map_err(|error| CallToolResult::error(error.to_string()))?;
    let candidate_segments = candidate
        .segments_for_net(net)
        .ok_or_else(|| CallToolResult::error(format!("candidate did not include net '{net}'")))?
        .to_vec();
    final_content = candidate_segments
        .iter()
        .try_fold(final_content, |content, segment| {
            let tree =
                parse_sexp(&content).map_err(|error| CallToolResult::error(error.to_string()))?;
            let current_wires = extract_wires(&tree);
            Ok::<_, CallToolResult>(if wire_exists(&current_wires, *segment) {
                content
            } else {
                insert_wire_with_junctions(content, segment.x1, segment.y1, segment.x2, segment.y2)
            })
        })?;
    let removable_local_labels = redundant_plain_label_positions(&final_content, net);
    if remove_redundant_local_labels {
        let (without_labels, _) =
            remove_plain_labels_at(final_content, net, &removable_local_labels).map_err(
                |error| CallToolResult::error(format!("failed to preview label cleanup: {error}")),
            )?;
        final_content = without_labels;
    }
    validate_semantic_snapshot(
        &before_snapshot,
        &final_content,
        "preview final transaction",
    )
    .map_err(|error| CallToolResult::error(error.to_string()))?;

    let existing_wires = wires
        .iter()
        .cloned()
        .filter(|wire| {
            graph.net_at(wire.x1, wire.y1).as_deref() == Some(net)
                || graph.net_at(wire.x2, wire.y2).as_deref() == Some(net)
        })
        .collect::<Vec<_>>();
    let labels_for_net = labels
        .into_iter()
        .filter(|label| label.net == net)
        .collect::<Vec<_>>();
    let plan_revision = materialize_plan_revision(
        &before,
        net,
        &candidate_segments,
        &candidate.sheet_pin_changes,
        remove_redundant_local_labels,
        &endpoints,
    );
    Ok(MaterializeNetPlan {
        schematic: schematic.clone(),
        before,
        net: net.to_string(),
        endpoints,
        existing_wires,
        labels: labels_for_net,
        segments: candidate_segments,
        sheet_pin_changes: candidate.sheet_pin_changes,
        removable_local_labels,
        remove_redundant_local_labels,
        before_snapshot,
        final_content,
        plan_revision,
    })
}

fn requested_materialize_pin_keys(
    tree: &konnect_sexp::SexpNode,
    args: &serde_json::Value,
    net: &str,
    graph: &mut crate::tools::sch_connectivity::NetGraph,
) -> Result<Option<HashSet<String>>, CallToolResult> {
    let Some(pins) = args["pins"].as_array() else {
        return Ok(None);
    };
    let instances = extract_symbol_instances(tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let mut keys = HashSet::new();
    for (index, pin) in pins.iter().enumerate() {
        let Some(reference) = pin["reference"].as_str() else {
            return Err(CallToolResult::error(format!(
                "pins[{index}].reference is required"
            )));
        };
        let Some(pin_number) = pin["pin_number"].as_str() else {
            return Err(CallToolResult::error(format!(
                "pins[{index}].pin_number is required"
            )));
        };
        let (lib_pin, transform) = resolve_placed_pin(&instances, &lib_syms, reference, pin_number)
            .map_err(|error| CallToolResult::error(error.to_string()))?;
        let (x, y) = pin_endpoint(&lib_pin, transform);
        let actual = graph.net_at(x, y);
        if actual.as_deref() != Some(net) {
            return Err(CallToolResult::error(format!(
                "refusing to materialize net '{net}' through {reference}.{pin_number}: endpoint is on {}",
                actual.as_deref().unwrap_or("<unconnected>")
            )));
        }
        let unit = instances
            .iter()
            .find(|instance| instance.reference == reference)
            .map(|instance| instance.unit)
            .unwrap_or(1);
        keys.insert(format!("{reference}:{unit}:{pin_number}"));
    }
    Ok(Some(keys))
}

fn materialize_candidate(
    before: &str,
    schematic: &Path,
    tree: &konnect_sexp::SexpNode,
    wires: &[Wire],
    labels: &[Label],
    net: &str,
    endpoints: &[NetEndpoint],
    args: &serde_json::Value,
) -> Result<LocalLayoutCandidate, CallToolResult> {
    // TODO(KICAD_IPC):
    // Replace only the KiCad-10 wire/junction mutation compatibility backend
    // when the minimum supported KiCad release provides reliable arbitrary-sheet
    // schematic Create/Update support. Keep Konnect obstacle-aware route planning.
    let mut endpoints_by_net = BTreeMap::new();
    endpoints_by_net.insert(net.to_string(), endpoints.to_vec());
    let mut planner = LocalLayoutPlanner::new(
        before,
        schematic,
        tree,
        wires,
        labels,
        &endpoints_by_net,
        args,
    );
    let candidates = planner.plan_top_k(DEFAULT_LAYOUT_CANDIDATE_COUNT)?;
    Ok(candidates
        .into_iter()
        .next()
        .expect("planner returned at least one candidate"))
}

#[derive(Debug, Clone, Copy)]
struct RectObstacle {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl RectObstacle {
    fn from_bounds(bounds: SymbolBounds) -> Self {
        Self {
            min_x: bounds.min_x,
            min_y: bounds.min_y,
            max_x: bounds.max_x,
            max_y: bounds.max_y,
        }
    }

    fn padded(self, amount: f64) -> Self {
        Self {
            min_x: self.min_x - amount,
            min_y: self.min_y - amount,
            max_x: self.max_x + amount,
            max_y: self.max_y + amount,
        }
    }
}

struct MaterializeObstacles {
    bodies: Vec<RectObstacle>,
    foreign_pins: Vec<(f64, f64)>,
    foreign_wires: Vec<Segment>,
}

impl MaterializeObstacles {
    fn build(
        tree: &konnect_sexp::SexpNode,
        wires: &[Wire],
        labels: &[Label],
        net: &str,
        endpoints: &[NetEndpoint],
    ) -> Self {
        let endpoint_refs = endpoints
            .iter()
            .map(|endpoint| endpoint.reference.as_str())
            .collect::<HashSet<_>>();
        let endpoint_points = endpoints
            .iter()
            .map(|endpoint| (endpoint.x, endpoint.y))
            .collect::<Vec<_>>();
        let mut graph = crate::tools::sch_connectivity::net_graph_for(tree, wires, labels);
        let index = crate::tools::sch_connectivity::ConnectivityIndex::build(
            tree,
            wires,
            labels,
            crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
        );
        let instances = extract_symbol_instances(tree);
        let lib_syms = tree
            .find("lib_symbols")
            .map(|n| n.find_all("symbol"))
            .unwrap_or_default();

        let mut bodies = Vec::new();
        for instance in &instances {
            if endpoint_refs.contains(instance.reference.as_str()) {
                continue;
            }
            if let Some(lib_sym) = find_lib_symbol(&lib_syms, instance) {
                if let Some(bounds) = symbol_bounds_for_instance(lib_sym, instance) {
                    bodies.push(RectObstacle::from_bounds(bounds).padded(0.01));
                }
            }
        }
        bodies.extend(sheet_body_obstacles(tree));

        let foreign_pins = index
            .placed_pins()
            .iter()
            .map(|pin| pin.at)
            .filter(|&(x, y)| {
                !endpoint_points
                    .iter()
                    .any(|&(ex, ey)| points_coincident(x, y, ex, ey, 0.01))
            })
            .filter(|&(x, y)| graph.net_at(x, y).as_deref() != Some(net))
            .collect();

        let foreign_wires = wires
            .iter()
            .filter(|wire| {
                graph.net_at(wire.x1, wire.y1).as_deref() != Some(net)
                    && graph.net_at(wire.x2, wire.y2).as_deref() != Some(net)
            })
            .map(|wire| Segment {
                x1: wire.x1,
                y1: wire.y1,
                x2: wire.x2,
                y2: wire.y2,
            })
            .collect();

        Self {
            bodies,
            foreign_pins,
            foreign_wires,
        }
    }

    fn allows(&self, segment: Segment) -> bool {
        if segment.is_zero() {
            return true;
        }
        !self
            .bodies
            .iter()
            .any(|body| segment_crosses_rect_interior(segment, *body))
            && !self
                .foreign_pins
                .iter()
                .any(|&(x, y)| point_on_segment_inclusive(x, y, segment))
            && !self
                .foreign_wires
                .iter()
                .any(|wire| conductive_segments_contact(segment, *wire))
    }
}

#[derive(Debug, Clone, Default)]
struct LocalLayoutScore {
    existing_items_moved: usize,
    existing_orientation_changes: usize,
    total_move_grid_steps: u32,
    sheet_pin_side_changes: usize,
    wire_grid_steps: u32,
    wire_bends: u32,
    clearance_penalty: u32,
    approximate_body_penalty: u32,
}

impl LocalLayoutScore {
    fn total_cost(&self) -> u32 {
        (self.existing_items_moved as u32 * EXISTING_ITEM_MOVE_BASE_COST)
            + (self.existing_orientation_changes as u32 * EXISTING_ORIENTATION_CHANGE_COST)
            + (self.total_move_grid_steps * MOVE_GRID_STEP_COST)
            + (self.sheet_pin_side_changes as u32 * SHEET_PIN_SIDE_CHANGE_COST)
            + (self.wire_grid_steps * WIRE_GRID_STEP_COST)
            + (self.wire_bends * WIRE_BEND_COST)
            + self.clearance_penalty
            + self.approximate_body_penalty
    }

    fn json(&self) -> serde_json::Value {
        json!({
            "existing_items_moved": self.existing_items_moved,
            "existing_orientation_changes": self.existing_orientation_changes,
            "total_move_grid_steps": self.total_move_grid_steps,
            "sheet_pin_side_changes": self.sheet_pin_side_changes,
            "wire_grid_steps": self.wire_grid_steps,
            "wire_bends": self.wire_bends,
            "clearance_penalty": self.clearance_penalty,
            "approximate_body_penalty": self.approximate_body_penalty
        })
    }
}

#[derive(Debug, Clone)]
struct LocalLayoutCandidate {
    candidate_id: String,
    rank: usize,
    routes_by_net: BTreeMap<String, Vec<Segment>>,
    sheet_pin_changes: Vec<SheetPinChange>,
    score: LocalLayoutScore,
    structural_signature: String,
}

impl LocalLayoutCandidate {
    fn segments_for_net(&self, net: &str) -> Option<&[Segment]> {
        self.routes_by_net.get(net).map(Vec::as_slice)
    }

    fn json(&self) -> serde_json::Value {
        json!({
            "candidate_id": self.candidate_id,
            "rank": self.rank,
            "total_cost": self.score.total_cost(),
            "score_breakdown": self.score.json(),
            "validation": {
                "hard_constraints_pass": true,
                "component_pin_group_changes": [],
                "body_crossings": 0,
                "foreign_conductive_contacts": 0
            },
            "planned_changes": {
                "moved_item_uuids": [],
                "rotated_or_mirrored_item_uuids": [],
                "changed_sheet_pin_uuids": self.sheet_pin_changes.iter().map(|change| change.pin_uuid.clone()).collect::<Vec<_>>(),
                "changed_or_routed_nets": self.routes_by_net.keys().cloned().collect::<Vec<_>>()
            },
            "sheet_pin_changes": self.sheet_pin_changes.iter().map(SheetPinChange::json).collect::<Vec<_>>(),
            "routes_by_net": self.routes_by_net.iter().map(|(net, segments)| {
                (net.clone(), json!(segments.iter().map(|segment| segment.json()).collect::<Vec<_>>()))
            }).collect::<serde_json::Map<_, _>>(),
            "structural_signature": self.structural_signature
        })
    }
}

#[derive(Debug, Clone)]
struct NetRouteCandidate {
    segments: Vec<Segment>,
    score: LocalLayoutScore,
    structural_signature: String,
}

struct LocalLayoutPlanner<'a> {
    before: &'a str,
    schematic: &'a Path,
    endpoints_by_net: &'a BTreeMap<String, Vec<NetEndpoint>>,
    strategy: &'a serde_json::Value,
    grid: f64,
    escape: f64,
    states_evaluated: usize,
}

impl<'a> LocalLayoutPlanner<'a> {
    fn new(
        before: &'a str,
        schematic: &'a Path,
        _tree: &'a konnect_sexp::SexpNode,
        _wires: &'a [Wire],
        _labels: &'a [Label],
        endpoints_by_net: &'a BTreeMap<String, Vec<NetEndpoint>>,
        args: &'a serde_json::Value,
    ) -> Self {
        let grid = endpoints_by_net
            .values()
            .find_map(|endpoints| endpoints.first().map(|endpoint| endpoint.grid))
            .unwrap_or(1.27);
        Self {
            before,
            schematic,
            endpoints_by_net,
            strategy: &args["strategy"],
            grid,
            escape: args["escape_length"].as_f64().unwrap_or(grid.max(1.27)),
            states_evaluated: 0,
        }
    }

    fn plan_top_k(
        &mut self,
        requested_count: usize,
    ) -> Result<Vec<LocalLayoutCandidate>, CallToolResult> {
        let retain = requested_count.clamp(1, MAX_LAYOUT_CANDIDATE_COUNT);
        let orientation = self.strategy["preferred_orientation"]
            .as_str()
            .unwrap_or("horizontal");
        if !matches!(orientation, "horizontal" | "vertical") {
            return Err(CallToolResult::error(format!(
                "strategy.preferred_orientation '{orientation}' is invalid; expected horizontal or vertical"
            )));
        }

        let mut by_signature: BTreeMap<String, LocalLayoutCandidate> = BTreeMap::new();
        let mut obstacle_counts = (0usize, 0usize, 0usize);
        for sheet_pin_changes in self.sheet_pin_layout_states()? {
            if self.states_evaluated >= MAX_LAYOUT_STATES_EVALUATED {
                break;
            }
            self.states_evaluated += 1;
            let state_content =
                apply_sheet_pin_changes_to_content(self.schematic, self.before, &sheet_pin_changes)
                    .map_err(|error| CallToolResult::error(error.to_string()))?;
            let state_tree = parse_sexp(&state_content)
                .map_err(|error| CallToolResult::error(error.to_string()))?;
            let state_wires = extract_wires(&state_tree);
            let state_labels = konnect_sexp::schematic::extract_all_net_labels(&state_tree);

            let mut partials = vec![PartialLayoutCandidate::new(&sheet_pin_changes)];
            for (net, endpoints) in self.endpoints_by_net {
                let mut next_partials = Vec::new();
                for partial in &partials {
                    let mut prior_routes = partial
                        .routes_by_net
                        .values()
                        .flatten()
                        .copied()
                        .collect::<Vec<_>>();
                    let route_candidates = self.route_candidates_for_net(
                        &state_tree,
                        &state_wires,
                        &state_labels,
                        net,
                        endpoints,
                        &sheet_pin_changes,
                        &prior_routes,
                        retain,
                        orientation,
                    );
                    for route_candidate in route_candidates {
                        let mut candidate = partial.clone();
                        candidate.add_route(net, route_candidate);
                        next_partials.push(candidate);
                    }
                    prior_routes.clear();
                }
                next_partials.sort_by(partial_layout_candidate_cmp);
                next_partials.truncate(retain);
                if next_partials.is_empty() {
                    partials.clear();
                    break;
                }
                partials = next_partials;
            }

            for partial in partials {
                if partial.routes_by_net.len() != self.endpoints_by_net.len() {
                    continue;
                }
                obstacle_counts = (
                    obstacle_counts.0.max(partial.obstacle_counts.0),
                    obstacle_counts.1.max(partial.obstacle_counts.1),
                    obstacle_counts.2.max(partial.obstacle_counts.2),
                );
                let signature =
                    route_structural_signature(&partial.routes_by_net, &sheet_pin_changes);
                let candidate_id = format!(
                    "layout-{}",
                    DocumentRevision::of(&format!("{LOCAL_LAYOUT_ALGORITHM_VERSION}\n{signature}"))
                );
                let entry = LocalLayoutCandidate {
                    candidate_id,
                    rank: 0,
                    routes_by_net: partial.routes_by_net,
                    sheet_pin_changes: sheet_pin_changes.clone(),
                    score: partial.score,
                    structural_signature: signature.clone(),
                };
                match by_signature.get(&signature) {
                    Some(existing) if local_layout_candidate_cmp(&entry, existing).is_ge() => {}
                    _ => {
                        by_signature.insert(signature, entry);
                    }
                }
            }
        }

        let mut candidates = by_signature.into_values().collect::<Vec<_>>();
        candidates.sort_by(local_layout_candidate_cmp);
        candidates.truncate(retain);
        for (index, candidate) in candidates.iter_mut().enumerate() {
            candidate.rank = index + 1;
        }
        if candidates.is_empty() {
            return Err(CallToolResult::error(
                json!({
                    "reason": "no_obstacle_free_route",
                    "nets": self.endpoints_by_net.keys().cloned().collect::<Vec<_>>(),
                    "endpoint_count": self.endpoints_by_net.values().map(Vec::len).sum::<usize>(),
                    "routing_grid": self.grid,
                    "states_evaluated": self.states_evaluated,
                    "obstacles": {
                        "symbol_bodies": obstacle_counts.0,
                        "foreign_pins": obstacle_counts.1,
                        "foreign_wires": obstacle_counts.2
                    }
                })
                .to_string(),
            ));
        }
        Ok(candidates)
    }

    fn sheet_pin_layout_states(&self) -> Result<Vec<Vec<SheetPinChange>>, CallToolResult> {
        let mut states = vec![Vec::new()];
        let changes = composite_peer_facing_sheet_pin_changes(
            self.before,
            self.schematic,
            self.endpoints_by_net,
            self.grid,
        )?;
        if !changes.is_empty() {
            states.push(changes);
        }
        Ok(states)
    }

    #[allow(clippy::too_many_arguments)]
    fn route_candidates_for_net(
        &self,
        tree: &konnect_sexp::SexpNode,
        wires: &[Wire],
        labels: &[Label],
        net: &str,
        endpoints: &[NetEndpoint],
        sheet_pin_changes: &[SheetPinChange],
        prior_routes: &[Segment],
        retain: usize,
        orientation: &str,
    ) -> Vec<NetRouteCandidate> {
        let state_endpoints = endpoints_with_sheet_pin_changes(endpoints, sheet_pin_changes);
        let mut obstacles = MaterializeObstacles::build(tree, wires, labels, net, &state_endpoints);
        obstacles.foreign_wires.extend_from_slice(prior_routes);
        let escaped_points = state_endpoints
            .iter()
            .map(|endpoint| escaped_endpoint(endpoint, self.escape, &obstacles))
            .collect::<Vec<_>>();
        let mut points = escaped_points.clone();
        points.sort_by(|a, b| a.partial_cmp(b).expect("finite endpoint coordinates"));
        points.dedup_by(|a, b| points_coincident(a.0, a.1, b.0, b.1, 0.01));

        let mut raw_candidates = Vec::new();
        if points.len() == 2 {
            raw_candidates.extend(two_point_candidates(points[0], points[1], &obstacles));
        }
        raw_candidates.extend(trunk_candidates(
            &points,
            orientation,
            self.strategy,
            &obstacles,
        ));
        raw_candidates.extend(trunk_candidates(
            &points,
            if orientation == "horizontal" {
                "vertical"
            } else {
                "horizontal"
            },
            self.strategy,
            &obstacles,
        ));
        if let Some(route) = obstacle_free_manhattan_route(&escaped_points, self.grid, &obstacles) {
            raw_candidates.push(route);
        }

        let mut by_signature = BTreeMap::new();
        for mut raw in raw_candidates {
            raw.extend(pin_escape_segments(&state_endpoints, &escaped_points));
            let segments = normalize_segments(raw);
            if segments.is_empty() || !segments.iter().all(|segment| obstacles.allows(*segment)) {
                continue;
            }
            let score = local_layout_route_score(&segments, self.grid, &obstacles);
            let signature = route_signature_for_net(net, &segments);
            let candidate = NetRouteCandidate {
                segments,
                score,
                structural_signature: signature.clone(),
            };
            match by_signature.get(&signature) {
                Some(existing) if net_route_candidate_cmp(&candidate, existing).is_ge() => {}
                _ => {
                    by_signature.insert(signature, candidate);
                }
            }
        }
        let mut candidates = by_signature.into_values().collect::<Vec<_>>();
        candidates.sort_by(net_route_candidate_cmp);
        candidates.truncate(retain);
        candidates
    }
}

#[derive(Debug, Clone)]
struct PartialLayoutCandidate {
    routes_by_net: BTreeMap<String, Vec<Segment>>,
    score: LocalLayoutScore,
    route_signature_material: Vec<String>,
    obstacle_counts: (usize, usize, usize),
}

impl PartialLayoutCandidate {
    fn new(sheet_pin_changes: &[SheetPinChange]) -> Self {
        let mut score = LocalLayoutScore::default();
        score.sheet_pin_side_changes = sheet_pin_changes.len();
        Self {
            routes_by_net: BTreeMap::new(),
            score,
            route_signature_material: Vec::new(),
            obstacle_counts: (0, 0, 0),
        }
    }

    fn add_route(&mut self, net: &str, route: NetRouteCandidate) {
        self.score.wire_grid_steps += route.score.wire_grid_steps;
        self.score.wire_bends += route.score.wire_bends;
        self.score.clearance_penalty += route.score.clearance_penalty;
        self.score.approximate_body_penalty += route.score.approximate_body_penalty;
        self.route_signature_material
            .push(route.structural_signature.clone());
        self.routes_by_net.insert(net.to_string(), route.segments);
    }
}

fn partial_layout_candidate_cmp(
    a: &PartialLayoutCandidate,
    b: &PartialLayoutCandidate,
) -> std::cmp::Ordering {
    a.score
        .total_cost()
        .cmp(&b.score.total_cost())
        .then_with(|| a.route_signature_material.cmp(&b.route_signature_material))
}

fn local_layout_candidate_cmp(
    a: &LocalLayoutCandidate,
    b: &LocalLayoutCandidate,
) -> std::cmp::Ordering {
    a.score
        .total_cost()
        .cmp(&b.score.total_cost())
        .then_with(|| a.structural_signature.cmp(&b.structural_signature))
        .then_with(|| a.candidate_id.cmp(&b.candidate_id))
}

fn net_route_candidate_cmp(a: &NetRouteCandidate, b: &NetRouteCandidate) -> std::cmp::Ordering {
    a.score
        .total_cost()
        .cmp(&b.score.total_cost())
        .then_with(|| a.structural_signature.cmp(&b.structural_signature))
}

fn local_layout_route_score(
    segments: &[Segment],
    grid: f64,
    obstacles: &MaterializeObstacles,
) -> LocalLayoutScore {
    let wire_grid_steps = segments
        .iter()
        .map(|segment| ((segment.x1 - segment.x2).abs() + (segment.y1 - segment.y2).abs()) / grid)
        .sum::<f64>()
        .round() as u32;
    let clearance_penalty = segments
        .iter()
        .map(|segment| {
            obstacles
                .foreign_pins
                .iter()
                .filter(|&&(x, y)| point_near_segment(x, y, *segment, grid))
                .count() as u32
                * CLEARANCE_PENALTY
        })
        .sum();
    LocalLayoutScore {
        wire_grid_steps,
        wire_bends: route_bends(segments) as u32,
        clearance_penalty,
        ..Default::default()
    }
}

fn endpoints_with_sheet_pin_changes(
    endpoints: &[NetEndpoint],
    changes: &[SheetPinChange],
) -> Vec<NetEndpoint> {
    endpoints
        .iter()
        .cloned()
        .map(|mut endpoint| {
            if let NetEndpointKind::SheetPin { pin_uuid, .. } = &endpoint.kind {
                if let Some(change) = changes.iter().find(|change| &change.pin_uuid == pin_uuid) {
                    endpoint.x = change.to_x;
                    endpoint.y = change.to_y;
                    endpoint.orientation_degrees = change.rotation;
                    endpoint.on_grid = point_on_grid(change.to_x, change.to_y, endpoint.grid);
                    endpoint.nearest_grid_x = snap_point(change.to_x, change.to_y, endpoint.grid).0;
                    endpoint.nearest_grid_y = snap_point(change.to_x, change.to_y, endpoint.grid).1;
                }
            }
            endpoint
        })
        .collect()
}

fn apply_sheet_pin_changes_to_content(
    schematic: &Path,
    content: &str,
    changes: &[SheetPinChange],
) -> anyhow::Result<String> {
    if changes.is_empty() {
        return Ok(content.to_string());
    }
    let mut sch = write_temp_and_load(schematic, content)?;
    for change in changes {
        let sheet = sch
            .sheets
            .by_uuid_mut(&change.sheet_uuid)
            .ok_or_else(|| anyhow::anyhow!("Sheet '{}' not found", change.sheet_uuid))?;
        let pin = sheet
            .pins
            .iter_mut()
            .find(|pin| pin.uuid == change.pin_uuid)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Pin '{}' not found on sheet '{}'",
                    change.pin_uuid,
                    change.sheet_uuid
                )
            })?;
        pin.at.x = change.to_x;
        pin.at.y = change.to_y;
        pin.at.rotation = Some(change.rotation);
    }
    Ok(sch.to_source())
}

fn composite_peer_facing_sheet_pin_changes(
    before: &str,
    schematic: &Path,
    endpoints_by_net: &BTreeMap<String, Vec<NetEndpoint>>,
    grid: f64,
) -> Result<Vec<SheetPinChange>, CallToolResult> {
    let sch = write_temp_and_load(schematic, before)
        .map_err(|error| CallToolResult::error(error.to_string()))?;
    let mut by_pin: BTreeMap<String, (String, String, String, SheetPinSide)> = BTreeMap::new();
    for endpoints in endpoints_by_net.values() {
        for (sheet_uuid, sheet_name, pin_uuid, pin_name, desired) in
            peer_facing_sheet_pin_raw_changes(&sch, endpoints)
        {
            match by_pin.get(&pin_uuid) {
                Some((_, _, _, existing)) if *existing != desired => {
                    return Err(CallToolResult::error(
                        json!({
                            "reason": "conflicting_sheet_pin_state",
                            "pin_uuid": pin_uuid,
                            "existing_side": existing.as_str(),
                            "proposed_side": desired.as_str()
                        })
                        .to_string(),
                    ));
                }
                Some(_) => {}
                None => {
                    by_pin.insert(pin_uuid, (sheet_uuid, sheet_name, pin_name, desired));
                }
            }
        }
    }
    let raw_changes = by_pin
        .into_iter()
        .map(|(pin_uuid, (sheet_uuid, sheet_name, pin_name, desired))| {
            (sheet_uuid, sheet_name, pin_uuid, pin_name, desired)
        })
        .collect::<Vec<_>>();
    place_sheet_pin_changes(&sch, raw_changes, grid)
}

fn peer_facing_sheet_pin_raw_changes(
    sch: &cse::Schematic,
    endpoints: &[NetEndpoint],
) -> Vec<(String, String, String, String, SheetPinSide)> {
    let mut sheet_endpoints = endpoints
        .iter()
        .filter_map(|endpoint| match &endpoint.kind {
            NetEndpointKind::SheetPin {
                sheet_uuid,
                sheet_name,
                pin_uuid,
            } => Some((
                sheet_uuid.clone(),
                sheet_name.clone(),
                pin_uuid.clone(),
                endpoint.pin_name.clone(),
            )),
            NetEndpointKind::ComponentPin => None,
        })
        .collect::<Vec<_>>();
    sheet_endpoints.sort();
    sheet_endpoints.dedup();
    if sheet_endpoints.len() < 2 {
        return Vec::new();
    }

    let mut raw_changes = Vec::new();
    for (sheet_uuid, sheet_name, pin_uuid, pin_name) in &sheet_endpoints {
        let Some(sheet) = sch.sheets.by_uuid(sheet_uuid) else {
            continue;
        };
        let Some(peer) = sheet_endpoints
            .iter()
            .filter(|(peer_uuid, _, _, _)| peer_uuid != sheet_uuid)
            .filter_map(|(peer_uuid, _, _, _)| sch.sheets.by_uuid(peer_uuid))
            .min_by(|a, b| {
                sheet_center_distance(sheet, a)
                    .partial_cmp(&sheet_center_distance(sheet, b))
                    .expect("finite sheet distance")
            })
        else {
            continue;
        };
        let desired = peer_facing_side(sheet, peer);
        let Some(pin) = sheet.pins.iter().find(|pin| pin.uuid == *pin_uuid) else {
            continue;
        };
        let Some(current) = sheet_pin_side(sheet, pin.at.x, pin.at.y) else {
            continue;
        };
        if current == desired {
            continue;
        }
        raw_changes.push((
            sheet_uuid.clone(),
            sheet_name.clone(),
            pin_uuid.clone(),
            pin_name.clone(),
            desired,
        ));
    }
    raw_changes
}

fn place_sheet_pin_changes(
    sch: &cse::Schematic,
    raw_changes: Vec<(String, String, String, String, SheetPinSide)>,
    grid: f64,
) -> Result<Vec<SheetPinChange>, CallToolResult> {
    if raw_changes.is_empty() {
        return Ok(Vec::new());
    }

    let mut by_sheet_side: BTreeMap<(String, SheetPinSide), Vec<(String, String, String)>> =
        BTreeMap::new();
    for (sheet_uuid, sheet_name, pin_uuid, pin_name, desired) in raw_changes {
        by_sheet_side
            .entry((sheet_uuid, desired))
            .or_default()
            .push((sheet_name, pin_uuid, pin_name));
    }

    let mut changes = Vec::new();
    for ((sheet_uuid, side), mut pins) in by_sheet_side {
        let sheet = sch
            .sheets
            .by_uuid(&sheet_uuid)
            .ok_or_else(|| CallToolResult::error(format!("Sheet '{sheet_uuid}' not found")))?;
        pins.sort_by(|(_, a_uuid, a_name), (_, b_uuid, b_name)| {
            let a_pin = sheet.pins.iter().find(|pin| pin.uuid == *a_uuid);
            let b_pin = sheet.pins.iter().find(|pin| pin.uuid == *b_uuid);
            let a_axis = a_pin
                .map(|pin| sheet_pin_order_axis(pin, side))
                .unwrap_or(0.0);
            let b_axis = b_pin
                .map(|pin| sheet_pin_order_axis(pin, side))
                .unwrap_or(0.0);
            a_axis
                .partial_cmp(&b_axis)
                .expect("finite sheet pin order")
                .then_with(|| a_name.cmp(b_name))
                .then_with(|| a_uuid.cmp(b_uuid))
        });
        let occupied = sheet
            .pins
            .iter()
            .filter(|pin| {
                !pins.iter().any(|(_, uuid, _)| uuid == &pin.uuid)
                    && sheet_pin_side(sheet, pin.at.x, pin.at.y) == Some(side)
            })
            .map(|pin| sheet_pin_order_axis(pin, side))
            .collect::<Vec<_>>();
        let desired_axes = pins
            .iter()
            .map(|(_, uuid, _)| {
                sheet
                    .pins
                    .iter()
                    .find(|pin| pin.uuid == *uuid)
                    .map(|pin| sheet_pin_order_axis(pin, side))
                    .unwrap_or(0.0)
            })
            .collect::<Vec<_>>();
        let placed_axes = nearest_legal_edge_axes(sheet, side, &desired_axes, &occupied, grid);
        for ((sheet_name, pin_uuid, pin_name), axis) in pins.into_iter().zip(placed_axes) {
            let pin = sheet
                .pins
                .iter()
                .find(|pin| pin.uuid == pin_uuid)
                .ok_or_else(|| {
                    CallToolResult::error(format!(
                        "Pin '{pin_uuid}' not found on sheet '{sheet_uuid}'"
                    ))
                })?;
            let from_side = sheet_pin_side(sheet, pin.at.x, pin.at.y).ok_or_else(|| {
                CallToolResult::error(format!(
                    "Pin '{}' on sheet '{}' is not on a sheet edge",
                    pin.name, sheet_name
                ))
            })?;
            let (to_x, to_y) = sheet_pin_position_on_side(sheet, side, axis);
            changes.push(SheetPinChange {
                sheet_uuid: sheet_uuid.clone(),
                sheet_name,
                pin_uuid,
                pin_name,
                from_side,
                to_side: side,
                from_x: pin.at.x,
                from_y: pin.at.y,
                to_x,
                to_y,
                rotation: side.rotation(),
            });
        }
    }
    changes.sort_by(|a, b| {
        a.sheet_uuid
            .cmp(&b.sheet_uuid)
            .then_with(|| a.pin_uuid.cmp(&b.pin_uuid))
    });
    Ok(changes)
}

fn sheet_center(sheet: &cse::Sheet) -> (f64, f64) {
    (
        sheet.at.x + sheet.width / 2.0,
        sheet.at.y + sheet.height / 2.0,
    )
}

fn sheet_center_distance(a: &cse::Sheet, b: &cse::Sheet) -> f64 {
    let (ax, ay) = sheet_center(a);
    let (bx, by) = sheet_center(b);
    (ax - bx).abs() + (ay - by).abs()
}

fn peer_facing_side(sheet: &cse::Sheet, peer: &cse::Sheet) -> SheetPinSide {
    let (sx, sy) = sheet_center(sheet);
    let (px, py) = sheet_center(peer);
    let dx = px - sx;
    let dy = py - sy;
    if dx.abs() >= dy.abs() {
        if dx > 0.0 {
            SheetPinSide::Right
        } else {
            SheetPinSide::Left
        }
    } else if dy > 0.0 {
        SheetPinSide::Bottom
    } else {
        SheetPinSide::Top
    }
}

fn sheet_pin_side(sheet: &cse::Sheet, x: f64, y: f64) -> Option<SheetPinSide> {
    if (x - sheet.at.x).abs() < 0.01
        && y >= sheet.at.y - 0.01
        && y <= sheet.at.y + sheet.height + 0.01
    {
        Some(SheetPinSide::Left)
    } else if (x - (sheet.at.x + sheet.width)).abs() < 0.01
        && y >= sheet.at.y - 0.01
        && y <= sheet.at.y + sheet.height + 0.01
    {
        Some(SheetPinSide::Right)
    } else if (y - sheet.at.y).abs() < 0.01
        && x >= sheet.at.x - 0.01
        && x <= sheet.at.x + sheet.width + 0.01
    {
        Some(SheetPinSide::Top)
    } else if (y - (sheet.at.y + sheet.height)).abs() < 0.01
        && x >= sheet.at.x - 0.01
        && x <= sheet.at.x + sheet.width + 0.01
    {
        Some(SheetPinSide::Bottom)
    } else {
        None
    }
}

fn sheet_pin_order_axis(pin: &cse::SheetPin, side: SheetPinSide) -> f64 {
    if side.is_vertical_edge() {
        pin.at.y
    } else {
        pin.at.x
    }
}

fn sheet_pin_position_on_side(sheet: &cse::Sheet, side: SheetPinSide, axis: f64) -> (f64, f64) {
    match side {
        SheetPinSide::Left => (sheet.at.x, axis),
        SheetPinSide::Right => (sheet.at.x + sheet.width, axis),
        SheetPinSide::Top => (axis, sheet.at.y),
        SheetPinSide::Bottom => (axis, sheet.at.y + sheet.height),
    }
}

fn nearest_legal_edge_axes(
    sheet: &cse::Sheet,
    side: SheetPinSide,
    desired_axes: &[f64],
    occupied_axes: &[f64],
    grid: f64,
) -> Vec<f64> {
    let (min_axis, max_axis) = if side.is_vertical_edge() {
        (sheet.at.y + grid, sheet.at.y + sheet.height - grid)
    } else {
        (sheet.at.x + grid, sheet.at.x + sheet.width - grid)
    };
    let min_i = grid_index(min_axis, grid);
    let max_i = grid_index(max_axis, grid);
    let mut used = occupied_axes
        .iter()
        .map(|axis| grid_index(*axis, grid))
        .collect::<HashSet<_>>();
    let mut out = Vec::new();
    for desired in desired_axes {
        let preferred = grid_index((*desired).clamp(min_axis, max_axis), grid);
        let mut chosen = None;
        for delta in 0..=(max_i - min_i).abs() + desired_axes.len() as i64 + 2 {
            for candidate in [preferred - delta, preferred + delta] {
                if candidate < min_i || candidate > max_i || used.contains(&candidate) {
                    continue;
                }
                chosen = Some(candidate);
                break;
            }
            if chosen.is_some() {
                break;
            }
        }
        let chosen = chosen.unwrap_or(preferred.clamp(min_i, max_i));
        used.insert(chosen);
        out.push(grid_coord(chosen, grid));
    }
    out
}

fn route_signature_for_net(net: &str, segments: &[Segment]) -> String {
    let mut material = net.to_string();
    for segment in segments {
        material.push('\n');
        material.push_str(&segment.normalized().json().to_string());
    }
    material
}

fn route_structural_signature(
    routes_by_net: &BTreeMap<String, Vec<Segment>>,
    changes: &[SheetPinChange],
) -> String {
    let mut material = String::new();
    for change in changes {
        material.push('\n');
        material.push_str(&format!(
            "sheet-pin:{}:{}:{}:{}:{}:{}",
            change.sheet_uuid,
            change.pin_uuid,
            change.from_side.as_str(),
            change.to_side.as_str(),
            cse::types::fmt_f64(change.to_x),
            cse::types::fmt_f64(change.to_y)
        ));
    }
    for (net, segments) in routes_by_net {
        material.push('\n');
        material.push_str(net);
        for segment in segments {
            material.push('\n');
            material.push_str(&segment.normalized().json().to_string());
        }
    }
    material
}

fn obstacle_free_manhattan_route(
    points: &[(f64, f64)],
    grid: f64,
    obstacles: &MaterializeObstacles,
) -> Option<Vec<Segment>> {
    let mut gateways = Vec::new();
    let mut gateway_segments = Vec::new();
    for &point in points {
        let gateway = if point_on_grid(point.0, point.1, grid) {
            point
        } else {
            let snapped = snap_point(point.0, point.1, grid);
            let mut candidates = two_point_candidates(point, snapped, obstacles);
            candidates.sort_by(candidate_route_cmp);
            let candidate = candidates
                .into_iter()
                .find(|segments| segments.iter().all(|segment| obstacles.allows(*segment)))?;
            gateway_segments.extend(candidate);
            snapped
        };
        gateways.push(gateway);
    }
    gateways.sort_by(|a, b| a.partial_cmp(b).expect("finite route point"));
    gateways.dedup_by(|a, b| points_coincident(a.0, a.1, b.0, b.1, 0.01));
    if gateways.len() < 2 {
        return Some(normalize_segments(gateway_segments));
    }

    let mut tree_points = vec![gateways[0]];
    let mut tree_segments = Vec::new();
    for &target in gateways.iter().skip(1) {
        let mut choices = tree_points
            .iter()
            .filter_map(|&tree_point| {
                grid_search_route(target, tree_point, grid, obstacles)
                    .map(|route| (tree_point, route))
            })
            .collect::<Vec<_>>();
        choices.sort_by(|(_, a), (_, b)| candidate_route_cmp(a, b));
        let (_, route) = choices.into_iter().next()?;
        for segment in &route {
            tree_points.push((segment.x1, segment.y1));
            tree_points.push((segment.x2, segment.y2));
        }
        tree_segments.extend(route);
        tree_points.sort_by(|a, b| a.partial_cmp(b).expect("finite tree point"));
        tree_points.dedup_by(|a, b| points_coincident(a.0, a.1, b.0, b.1, 0.01));
    }
    gateway_segments.extend(tree_segments);
    let out = normalize_segments(gateway_segments);
    if out.iter().all(|segment| obstacles.allows(*segment)) {
        Some(out)
    } else {
        None
    }
}

fn candidate_route_cmp(a: &Vec<Segment>, b: &Vec<Segment>) -> std::cmp::Ordering {
    route_score(a).cmp(&route_score(b))
}

fn route_score(segments: &[Segment]) -> (usize, i64, i64, i64) {
    let bends = route_bends(segments);
    let length = segments
        .iter()
        .map(|s| ((s.x1 - s.x2).abs() + (s.y1 - s.y2).abs()) * 1000.0)
        .sum::<f64>()
        .round() as i64;
    let min_x = segments
        .iter()
        .flat_map(|s| [s.x1, s.x2])
        .fold(f64::INFINITY, f64::min);
    let min_y = segments
        .iter()
        .flat_map(|s| [s.y1, s.y2])
        .fold(f64::INFINITY, f64::min);
    (
        bends,
        length,
        (min_x * 1000.0).round() as i64,
        (min_y * 1000.0).round() as i64,
    )
}

fn route_bends(segments: &[Segment]) -> usize {
    segments
        .windows(2)
        .filter(|pair| segment_direction(pair[0]) != segment_direction(pair[1]))
        .count()
}

fn segment_direction(segment: Segment) -> u8 {
    if (segment.x1 - segment.x2).abs() < 0.01 {
        1
    } else {
        0
    }
}

fn grid_search_route(
    start: (f64, f64),
    goal: (f64, f64),
    grid: f64,
    obstacles: &MaterializeObstacles,
) -> Option<Vec<Segment>> {
    let sx = grid_index(start.0, grid);
    let sy = grid_index(start.1, grid);
    let gx = grid_index(goal.0, grid);
    let gy = grid_index(goal.1, grid);
    let obstacle_points = obstacles
        .bodies
        .iter()
        .flat_map(|body| [(body.min_x, body.min_y), (body.max_x, body.max_y)])
        .chain(obstacles.foreign_pins.iter().copied())
        .chain(
            obstacles
                .foreign_wires
                .iter()
                .flat_map(|wire| [(wire.x1, wire.y1), (wire.x2, wire.y2)]),
        )
        .collect::<Vec<_>>();
    for margin in PROGRESSIVE_ROUTE_BOUNDS {
        let mut min_ix = sx.min(gx) - margin;
        let mut max_ix = sx.max(gx) + margin;
        let mut min_iy = sy.min(gy) - margin;
        let mut max_iy = sy.max(gy) + margin;
        for &(x, y) in &obstacle_points {
            let ix = grid_index(x, grid);
            let iy = grid_index(y, grid);
            if ix >= sx.min(gx) - margin && ix <= sx.max(gx) + margin {
                min_iy = min_iy.min(iy - margin);
                max_iy = max_iy.max(iy + margin);
            }
            if iy >= sy.min(gy) - margin && iy <= sy.max(gy) + margin {
                min_ix = min_ix.min(ix - margin);
                max_ix = max_ix.max(ix + margin);
            }
        }
        if let Some(route) = grid_search_route_in_bounds(
            (sx, sy),
            (gx, gy),
            grid,
            obstacles,
            (min_ix, max_ix, min_iy, max_iy),
        ) {
            return Some(route);
        }
    }
    None
}

fn grid_search_route_in_bounds(
    start: (i64, i64),
    goal: (i64, i64),
    grid: f64,
    obstacles: &MaterializeObstacles,
    bounds: (i64, i64, i64, i64),
) -> Option<Vec<Segment>> {
    let (min_ix, max_ix, min_iy, max_iy) = bounds;
    let mut open = vec![(0_u32, 0_u32, 0_u32, start.0, start.1, 4_u8)];
    let mut best: HashMap<(i64, i64, u8), u32> = HashMap::new();
    let mut parent: HashMap<(i64, i64, u8), (i64, i64, u8)> = HashMap::new();
    best.insert((start.0, start.1, 4), 0);
    let dirs = [(1_i64, 0_i64, 0_u8), (0, 1, 1), (-1, 0, 2), (0, -1, 3)];

    while !open.is_empty() {
        open.sort_by(|a, b| b.cmp(a));
        let (_, bends, cost_so_far, ix, iy, dir) = open.pop().unwrap();
        if (ix, iy) == goal {
            return Some(grid_path_to_segments(
                (start.0, start.1, 4),
                (ix, iy, dir),
                grid,
                &parent,
            ));
        }
        if let Some(&known_cost) = best.get(&(ix, iy, dir)) {
            if cost_so_far > known_cost {
                continue;
            }
        }
        for (dx, dy, next_dir) in dirs {
            let nx = ix + dx;
            let ny = iy + dy;
            if nx < min_ix || nx > max_ix || ny < min_iy || ny > max_iy {
                continue;
            }
            let segment = Segment {
                x1: grid_coord(ix, grid),
                y1: grid_coord(iy, grid),
                x2: grid_coord(nx, grid),
                y2: grid_coord(ny, grid),
            };
            if !obstacles.allows(segment) {
                continue;
            }
            let next_bends = bends + u32::from(dir != 4 && dir != next_dir);
            let next_cost = cost_so_far
                + GRID_STEP_COST
                + if dir != 4 && dir != next_dir {
                    BEND_COST
                } else {
                    0
                };
            let distance = ((goal.0 - nx).abs() + (goal.1 - ny).abs()) as u32;
            let key = (nx, ny, next_dir);
            if best.get(&key).map(|old| next_cost < *old).unwrap_or(true) {
                best.insert(key, next_cost);
                parent.insert(key, (ix, iy, dir));
                open.push((
                    next_cost + (distance * GRID_STEP_COST),
                    next_bends,
                    next_cost,
                    nx,
                    ny,
                    next_dir,
                ));
            }
        }
    }
    None
}

fn grid_path_to_segments(
    start: (i64, i64, u8),
    end: (i64, i64, u8),
    grid: f64,
    parent: &HashMap<(i64, i64, u8), (i64, i64, u8)>,
) -> Vec<Segment> {
    let mut nodes = vec![end];
    let mut cursor = end;
    while cursor != start {
        cursor = parent[&cursor];
        nodes.push(cursor);
    }
    nodes.reverse();
    let points = nodes
        .into_iter()
        .map(|(ix, iy, _)| (grid_coord(ix, grid), grid_coord(iy, grid)))
        .collect::<Vec<_>>();
    polyline_to_segments(points)
}

fn polyline_to_segments(points: Vec<(f64, f64)>) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for pair in points.windows(2) {
        if points_coincident(pair[0].0, pair[0].1, pair[1].0, pair[1].1, 0.01) {
            continue;
        }
        let next = Segment {
            x1: pair[0].0,
            y1: pair[0].1,
            x2: pair[1].0,
            y2: pair[1].1,
        };
        if let Some(last) = out.last_mut() {
            if segment_direction(*last) == segment_direction(next)
                && points_coincident(last.x2, last.y2, next.x1, next.y1, 0.01)
            {
                last.x2 = next.x2;
                last.y2 = next.y2;
                continue;
            }
        }
        out.push(next);
    }
    out
}

fn grid_index(value: f64, grid: f64) -> i64 {
    (value / grid).round() as i64
}

fn grid_coord(index: i64, grid: f64) -> f64 {
    round6(index as f64 * grid)
}

fn sheet_body_obstacles(tree: &konnect_sexp::SexpNode) -> Vec<RectObstacle> {
    tree.find_all("sheet")
        .into_iter()
        .filter_map(|sheet| {
            let (x, y, _) = parse_at(sheet)?;
            let size = sheet.find("size")?;
            let (Some(width), Some(height)) = (size.get_f64(1), size.get_f64(2)) else {
                return None;
            };
            Some(RectObstacle {
                min_x: x,
                min_y: y,
                max_x: x + width,
                max_y: y + height,
            })
        })
        .collect()
}

fn escaped_endpoint(
    endpoint: &NetEndpoint,
    escape: f64,
    obstacles: &MaterializeObstacles,
) -> (f64, f64) {
    let dir = crate::tools::stub_direction("auto", Some(endpoint.orientation_degrees));
    for multiple in [1.0, 2.0, 3.0] {
        let candidate = (
            round6(endpoint.x + dir.dx * escape * multiple),
            round6(endpoint.y + dir.dy * escape * multiple),
        );
        let segment = Segment {
            x1: endpoint.x,
            y1: endpoint.y,
            x2: candidate.0,
            y2: candidate.1,
        };
        if obstacles.allows(segment) {
            return candidate;
        }
    }
    (
        round6(endpoint.x + dir.dx * escape),
        round6(endpoint.y + dir.dy * escape),
    )
}

fn pin_escape_segments(endpoints: &[NetEndpoint], escaped_points: &[(f64, f64)]) -> Vec<Segment> {
    endpoints
        .iter()
        .zip(escaped_points.iter())
        .map(|(endpoint, &(x, y))| Segment {
            x1: endpoint.x,
            y1: endpoint.y,
            x2: x,
            y2: y,
        })
        .collect()
}

fn two_point_candidates(
    a: (f64, f64),
    b: (f64, f64),
    obstacles: &MaterializeObstacles,
) -> Vec<Vec<Segment>> {
    let mut out = Vec::new();
    if points_coincident(a.0, a.1, b.0, b.1, 0.01) {
        return out;
    }
    if (a.0 - b.0).abs() < 0.01 || (a.1 - b.1).abs() < 0.01 {
        out.push(vec![Segment {
            x1: a.0,
            y1: a.1,
            x2: b.0,
            y2: b.1,
        }]);
    }
    out.push(vec![
        Segment {
            x1: a.0,
            y1: a.1,
            x2: b.0,
            y2: a.1,
        },
        Segment {
            x1: b.0,
            y1: a.1,
            x2: b.0,
            y2: b.1,
        },
    ]);
    out.push(vec![
        Segment {
            x1: a.0,
            y1: a.1,
            x2: a.0,
            y2: b.1,
        },
        Segment {
            x1: a.0,
            y1: b.1,
            x2: b.0,
            y2: b.1,
        },
    ]);
    for y in detour_rows_between(a, b, obstacles) {
        out.push(vec![
            Segment {
                x1: a.0,
                y1: a.1,
                x2: a.0,
                y2: y,
            },
            Segment {
                x1: a.0,
                y1: y,
                x2: b.0,
                y2: y,
            },
            Segment {
                x1: b.0,
                y1: y,
                x2: b.0,
                y2: b.1,
            },
        ]);
    }
    for x in detour_columns_between(a, b, obstacles) {
        out.push(vec![
            Segment {
                x1: a.0,
                y1: a.1,
                x2: x,
                y2: a.1,
            },
            Segment {
                x1: x,
                y1: a.1,
                x2: x,
                y2: b.1,
            },
            Segment {
                x1: x,
                y1: b.1,
                x2: b.0,
                y2: b.1,
            },
        ]);
    }
    out
}

fn trunk_candidates(
    points: &[(f64, f64)],
    orientation: &str,
    strategy: &serde_json::Value,
    obstacles: &MaterializeObstacles,
) -> Vec<Vec<Segment>> {
    let mut out = Vec::new();
    let coords = if orientation == "horizontal" {
        let mut rows = strategy["trunk_y"]
            .as_f64()
            .into_iter()
            .chain(points.first().map(|p| p.1))
            .chain(points.iter().map(|p| p.1))
            .chain(
                obstacles
                    .bodies
                    .iter()
                    .flat_map(|r| [round6(r.min_y - 1.27), round6(r.max_y + 1.27)]),
            )
            .chain(
                obstacles
                    .foreign_pins
                    .iter()
                    .flat_map(|(_, y)| [round6(*y - 1.27), round6(*y + 1.27)]),
            )
            .chain(obstacles.foreign_wires.iter().flat_map(|wire| {
                [
                    round6(wire.y1.min(wire.y2) - 1.27),
                    round6(wire.y1.max(wire.y2) + 1.27),
                ]
            }))
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| a.partial_cmp(b).expect("finite row"));
        rows.dedup_by(|a, b| (*a - *b).abs() < 0.01);
        rows
    } else {
        let mut cols = strategy["trunk_x"]
            .as_f64()
            .into_iter()
            .chain(points.first().map(|p| p.0))
            .chain(points.iter().map(|p| p.0))
            .chain(
                obstacles
                    .bodies
                    .iter()
                    .flat_map(|r| [round6(r.min_x - 1.27), round6(r.max_x + 1.27)]),
            )
            .chain(
                obstacles
                    .foreign_pins
                    .iter()
                    .flat_map(|(x, _)| [round6(*x - 1.27), round6(*x + 1.27)]),
            )
            .chain(obstacles.foreign_wires.iter().flat_map(|wire| {
                [
                    round6(wire.x1.min(wire.x2) - 1.27),
                    round6(wire.x1.max(wire.x2) + 1.27),
                ]
            }))
            .collect::<Vec<_>>();
        cols.sort_by(|a, b| a.partial_cmp(b).expect("finite column"));
        cols.dedup_by(|a, b| (*a - *b).abs() < 0.01);
        cols
    };

    for trunk in coords {
        let mut candidate = Vec::new();
        if orientation == "horizontal" {
            let min_x = points.iter().map(|(x, _)| *x).fold(f64::INFINITY, f64::min);
            let max_x = points
                .iter()
                .map(|(x, _)| *x)
                .fold(f64::NEG_INFINITY, f64::max);
            candidate.push(Segment {
                x1: min_x,
                y1: trunk,
                x2: max_x,
                y2: trunk,
            });
            for &(x, y) in points {
                candidate.push(Segment {
                    x1: x,
                    y1: y,
                    x2: x,
                    y2: trunk,
                });
            }
        } else {
            let min_y = points.iter().map(|(_, y)| *y).fold(f64::INFINITY, f64::min);
            let max_y = points
                .iter()
                .map(|(_, y)| *y)
                .fold(f64::NEG_INFINITY, f64::max);
            candidate.push(Segment {
                x1: trunk,
                y1: min_y,
                x2: trunk,
                y2: max_y,
            });
            for &(x, y) in points {
                candidate.push(Segment {
                    x1: x,
                    y1: y,
                    x2: trunk,
                    y2: y,
                });
            }
        }
        out.push(candidate);
    }
    out
}

fn normalize_segments(mut segments: Vec<Segment>) -> Vec<Segment> {
    segments.retain(|segment| !segment.is_zero());
    segments.sort_by(|a, b| {
        a.normalized()
            .json()
            .to_string()
            .cmp(&b.normalized().json().to_string())
    });
    segments.dedup_by(|a, b| a.normalized() == b.normalized());
    segments
}

fn detour_rows_between(a: (f64, f64), b: (f64, f64), obstacles: &MaterializeObstacles) -> Vec<f64> {
    let direct = Segment {
        x1: a.0,
        y1: a.1,
        x2: b.0,
        y2: a.1,
    };
    let mut rows = obstacles
        .bodies
        .iter()
        .filter(|rect| segment_crosses_rect_interior(direct, **rect))
        .flat_map(|rect| [round6(rect.min_y - 1.27), round6(rect.max_y + 1.27)])
        .collect::<Vec<_>>();
    rows.sort_by(|x, y| {
        (x - a.1)
            .abs()
            .partial_cmp(&(y - a.1).abs())
            .expect("finite detour rows")
    });
    for &(x, y) in &obstacles.foreign_pins {
        if point_strictly_on_segment(x, y, direct) {
            rows.push(round6(y - 1.27));
            rows.push(round6(y + 1.27));
        }
    }
    for wire in &obstacles.foreign_wires {
        if segments_intersect(direct, *wire) {
            rows.push(round6(wire.y1.min(wire.y2) - 1.27));
            rows.push(round6(wire.y1.max(wire.y2) + 1.27));
        }
    }
    rows.sort_by(|x, y| {
        (x - a.1)
            .abs()
            .partial_cmp(&(y - a.1).abs())
            .expect("finite detour rows")
    });
    rows.dedup_by(|a, b| (*a - *b).abs() < 0.01);
    rows
}

fn detour_columns_between(
    a: (f64, f64),
    b: (f64, f64),
    obstacles: &MaterializeObstacles,
) -> Vec<f64> {
    let direct = Segment {
        x1: a.0,
        y1: a.1,
        x2: a.0,
        y2: b.1,
    };
    let mut cols = obstacles
        .bodies
        .iter()
        .filter(|rect| segment_crosses_rect_interior(direct, **rect))
        .flat_map(|rect| [round6(rect.min_x - 1.27), round6(rect.max_x + 1.27)])
        .collect::<Vec<_>>();
    cols.sort_by(|x, y| {
        (x - a.0)
            .abs()
            .partial_cmp(&(y - a.0).abs())
            .expect("finite detour columns")
    });
    for &(x, y) in &obstacles.foreign_pins {
        if point_strictly_on_segment(x, y, direct) {
            cols.push(round6(x - 1.27));
            cols.push(round6(x + 1.27));
        }
    }
    for wire in &obstacles.foreign_wires {
        if segments_intersect(direct, *wire) {
            cols.push(round6(wire.x1.min(wire.x2) - 1.27));
            cols.push(round6(wire.x1.max(wire.x2) + 1.27));
        }
    }
    cols.sort_by(|x, y| {
        (x - a.0)
            .abs()
            .partial_cmp(&(y - a.0).abs())
            .expect("finite detour columns")
    });
    cols.dedup_by(|a, b| (*a - *b).abs() < 0.01);
    cols
}

fn segment_crosses_rect_interior(segment: Segment, rect: RectObstacle) -> bool {
    let min_x = segment.x1.min(segment.x2);
    let max_x = segment.x1.max(segment.x2);
    let min_y = segment.y1.min(segment.y2);
    let max_y = segment.y1.max(segment.y2);
    if (segment.y1 - segment.y2).abs() < 0.01 {
        segment.y1 > rect.min_y
            && segment.y1 < rect.max_y
            && max_x > rect.min_x
            && min_x < rect.max_x
    } else if (segment.x1 - segment.x2).abs() < 0.01 {
        segment.x1 > rect.min_x
            && segment.x1 < rect.max_x
            && max_y > rect.min_y
            && min_y < rect.max_y
    } else {
        false
    }
}

fn point_strictly_on_segment(x: f64, y: f64, segment: Segment) -> bool {
    if points_coincident(x, y, segment.x1, segment.y1, 0.01)
        || points_coincident(x, y, segment.x2, segment.y2, 0.01)
    {
        return false;
    }
    konnect_sexp::geometry::point_on_segment(
        x, y, segment.x1, segment.y1, segment.x2, segment.y2, 0.01,
    )
}

fn point_on_segment_inclusive(x: f64, y: f64, segment: Segment) -> bool {
    konnect_sexp::geometry::point_on_segment(
        x, y, segment.x1, segment.y1, segment.x2, segment.y2, 0.01,
    )
}

fn point_near_segment(x: f64, y: f64, segment: Segment, grid: f64) -> bool {
    if (segment.y1 - segment.y2).abs() < 0.01 {
        let min_x = segment.x1.min(segment.x2) - grid;
        let max_x = segment.x1.max(segment.x2) + grid;
        x >= min_x && x <= max_x && (y - segment.y1).abs() <= grid
    } else if (segment.x1 - segment.x2).abs() < 0.01 {
        let min_y = segment.y1.min(segment.y2) - grid;
        let max_y = segment.y1.max(segment.y2) + grid;
        y >= min_y && y <= max_y && (x - segment.x1).abs() <= grid
    } else {
        false
    }
}

fn conductive_segments_contact(a: Segment, b: Segment) -> bool {
    if a.is_zero() || b.is_zero() {
        return false;
    }
    let a_h = (a.y1 - a.y2).abs() < 0.01;
    let b_h = (b.y1 - b.y2).abs() < 0.01;
    if a_h && b_h && (a.y1 - b.y1).abs() < 0.01 {
        return ranges_overlap_inclusive(a.x1, a.x2, b.x1, b.x2);
    }
    if !a_h && !b_h && (a.x1 - b.x1).abs() < 0.01 {
        return ranges_overlap_inclusive(a.y1, a.y2, b.y1, b.y2);
    }
    if a_h == b_h {
        return false;
    }
    let (h, v) = if a_h { (a, b) } else { (b, a) };
    point_on_segment_inclusive(v.x1, h.y1, h) && point_on_segment_inclusive(v.x1, h.y1, v)
}

fn segments_intersect(a: Segment, b: Segment) -> bool {
    for (x, y) in [(a.x1, a.y1), (a.x2, a.y2), (b.x1, b.y1), (b.x2, b.y2)] {
        if point_strictly_on_segment(x, y, a) && point_strictly_on_segment(x, y, b) {
            return true;
        }
    }
    let a_h = (a.y1 - a.y2).abs() < 0.01;
    let b_h = (b.y1 - b.y2).abs() < 0.01;
    if a_h == b_h {
        if a_h && (a.y1 - b.y1).abs() < 0.01 {
            return ranges_overlap_strict(a.x1, a.x2, b.x1, b.x2);
        }
        if !a_h && (a.x1 - b.x1).abs() < 0.01 {
            return ranges_overlap_strict(a.y1, a.y2, b.y1, b.y2);
        }
        return false;
    }
    let (h, v) = if a_h { (a, b) } else { (b, a) };
    point_strictly_on_segment(v.x1, h.y1, h) && point_strictly_on_segment(v.x1, h.y1, v)
}

fn ranges_overlap_strict(a1: f64, a2: f64, b1: f64, b2: f64) -> bool {
    let a_min = a1.min(a2);
    let a_max = a1.max(a2);
    let b_min = b1.min(b2);
    let b_max = b1.max(b2);
    a_max > b_min + 0.01 && b_max > a_min + 0.01
}

fn ranges_overlap_inclusive(a1: f64, a2: f64, b1: f64, b2: f64) -> bool {
    let a_min = a1.min(a2);
    let a_max = a1.max(a2);
    let b_min = b1.min(b2);
    let b_max = b1.max(b2);
    a_max + 0.01 >= b_min && b_max + 0.01 >= a_min
}

fn semantic_snapshot(content: &str) -> anyhow::Result<SemanticSnapshot> {
    let tree = parse_sexp(content)?;
    let wires = extract_wires(&tree);
    let labels = konnect_sexp::schematic::extract_all_net_labels(&tree);
    let representation_refs = representation_symbol_refs(&tree);
    let index = crate::tools::sch_connectivity::ConnectivityIndex::build(
        &tree,
        &wires,
        &labels,
        crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
    );
    let mut graph = crate::tools::sch_connectivity::net_graph_for(&tree, &wires, &labels);
    let pin_nets = index
        .placed_pins()
        .iter()
        .filter(|pin| !representation_refs.contains(&(pin.reference.clone(), pin.unit)))
        .map(|pin| {
            (
                format!("{}:{}:{}", pin.reference, pin.unit, pin.pin.number),
                graph.net_at(pin.at.0, pin.at.1),
            )
        })
        .collect();
    Ok(SemanticSnapshot {
        pin_nets,
        shorts: shorted_label_sets(&tree),
    })
}

fn representation_symbol_refs(tree: &konnect_sexp::SexpNode) -> HashSet<(String, u32)> {
    extract_symbol_instances(tree)
        .into_iter()
        .filter(|instance| {
            instance.lib_id.starts_with("power:") || instance.reference.starts_with("#PWR")
        })
        .map(|instance| (instance.reference, instance.unit))
        .collect()
}

fn shorted_label_sets(tree: &konnect_sexp::SexpNode) -> Vec<Vec<String>> {
    let wires = extract_wires(tree);
    let labels = konnect_sexp::schematic::extract_all_net_labels(tree);
    let mut graph = crate::tools::sch_connectivity::net_graph_for(tree, &wires, &labels);
    let mut root_nets: HashMap<(i64, i64), Vec<String>> = HashMap::new();
    for label in &labels {
        let root = graph.find(crate::tools::sch_connectivity::pt_key(label.x, label.y));
        root_nets.entry(root).or_default().push(label.net.clone());
    }
    let mut shorts = root_nets
        .into_values()
        .filter_map(|mut nets| {
            nets.sort();
            nets.dedup();
            (nets.len() > 1).then_some(nets)
        })
        .collect::<Vec<_>>();
    shorts.sort();
    shorts
}

fn validate_semantic_snapshot(
    before: &SemanticSnapshot,
    after_content: &str,
    stage: &str,
) -> anyhow::Result<()> {
    let after = semantic_snapshot(after_content)?;
    if after.pin_nets != before.pin_nets {
        let mut diffs = Vec::new();
        for key in before.pin_nets.keys().chain(after.pin_nets.keys()) {
            let before_net = before.pin_nets.get(key).cloned().unwrap_or(None);
            let after_net = after.pin_nets.get(key).cloned().unwrap_or(None);
            if before_net != after_net {
                diffs.push(format!(
                    "{key}: {} -> {}",
                    before_net.as_deref().unwrap_or("<unconnected>"),
                    after_net.as_deref().unwrap_or("<unconnected>")
                ));
            }
        }
        diffs.sort();
        diffs.dedup();
        anyhow::bail!(
            "semantic endpoint membership changed {stage}: {}",
            diffs.join(", ")
        );
    }
    if after.shorts != before.shorts {
        anyhow::bail!("shorted net set changed {stage}");
    }
    Ok(())
}

fn wire_exists(wires: &[Wire], segment: Segment) -> bool {
    wires.iter().any(|wire| {
        (points_coincident(wire.x1, wire.y1, segment.x1, segment.y1, 0.01)
            && points_coincident(wire.x2, wire.y2, segment.x2, segment.y2, 0.01))
            || (points_coincident(wire.x1, wire.y1, segment.x2, segment.y2, 0.01)
                && points_coincident(wire.x2, wire.y2, segment.x1, segment.y1, 0.01))
    })
}

fn redundant_plain_label_positions(content: &str, net: &str) -> Vec<(f64, f64)> {
    let Ok(tree) = parse_sexp(content) else {
        return Vec::new();
    };
    let wires = extract_wires(&tree);
    let labels = konnect_sexp::schematic::extract_labels(&tree);
    let mut plain = labels
        .into_iter()
        .filter(|label| label.kind == LabelKind::NetLabel && label.net == net)
        .filter(|label| {
            wires.iter().any(|wire| {
                konnect_sexp::geometry::point_on_segment(
                    label.x, label.y, wire.x1, wire.y1, wire.x2, wire.y2, 0.01,
                )
            })
        })
        .map(|label| (label.x, label.y))
        .collect::<Vec<_>>();
    plain.sort_by(|a, b| a.partial_cmp(b).expect("finite label coordinates"));
    if plain.len() <= 1 {
        return Vec::new();
    }
    plain.remove(0);
    plain
}

fn remove_plain_labels_at(
    content: String,
    net: &str,
    positions: &[(f64, f64)],
) -> anyhow::Result<(String, usize)> {
    if positions.is_empty() {
        return Ok((content, 0));
    }
    let mut ranges = Vec::new();
    let mut consumed = vec![false; positions.len()];
    for label in find_label_blocks(&content)
        .into_iter()
        .filter(|label| label.kind == "label" && label.net == net)
    {
        if let Some(index) = positions.iter().enumerate().find_map(|(index, (x, y))| {
            (!consumed[index] && points_coincident(label.x, label.y, *x, *y, 0.01)).then_some(index)
        }) {
            consumed[index] = true;
            if let Some(range) = find_block_with_leading_whitespace(&content, label.start) {
                ranges.push(range);
            }
        }
    }
    ranges.sort_unstable();
    ranges.dedup();
    let removed = ranges.len();
    Ok((
        apply_edits(
            content,
            ranges
                .into_iter()
                .map(|(start, end)| SexpEdit::delete(start, end))
                .collect(),
        ),
        removed,
    ))
}

fn materialize_plan_revision(
    before: &str,
    net: &str,
    segments: &[Segment],
    sheet_pin_changes: &[SheetPinChange],
    remove_redundant_local_labels: bool,
    endpoints: &[NetEndpoint],
) -> String {
    let mut material = String::new();
    material.push_str(&DocumentRevision::of(before).to_string());
    material.push('\n');
    material.push_str(net);
    material.push('\n');
    material.push_str(if remove_redundant_local_labels {
        "remove-labels"
    } else {
        "keep-labels"
    });
    for endpoint in endpoints {
        material.push('\n');
        material.push_str(&endpoint.key());
    }
    for change in sheet_pin_changes {
        material.push('\n');
        material.push_str(&change.json().to_string());
    }
    for segment in segments {
        material.push('\n');
        material.push_str(&segment.json().to_string());
    }
    let revision = DocumentRevision::of(&material);
    format!("materialize-schematic-net:{revision}")
}

fn point_on_grid(x: f64, y: f64, grid: f64) -> bool {
    if grid <= 0.0 {
        return true;
    }
    let (sx, sy) = snap_point(x, y, grid);
    points_coincident(x, y, sx, sy, 0.001)
}

fn attachment_depends_on_off_grid_position(
    graph: &mut crate::tools::sch_connectivity::NetGraph,
    x: f64,
    y: f64,
    grid: f64,
) -> bool {
    if point_on_grid(x, y, grid) {
        return false;
    }
    let current = graph.net_at(x, y);
    let (sx, sy) = snap_point(x, y, grid);
    graph.net_at(sx, sy) != current
}

fn semantic_comparison(
    before: &SemanticSnapshot,
    after_content: &str,
) -> anyhow::Result<serde_json::Value> {
    let after = semantic_snapshot(after_content)?;
    let pin_membership_preserved = after.pin_nets == before.pin_nets;
    let shorts_preserved = after.shorts == before.shorts;
    let pin_net_differences = before
        .pin_nets
        .iter()
        .filter_map(|(pin, before_net)| {
            let after_net = after.pin_nets.get(pin).cloned().unwrap_or(None);
            (&after_net != before_net).then(|| {
                json!({
                    "pin": pin,
                    "before": before_net,
                    "after": after_net
                })
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "preserved": pin_membership_preserved && shorts_preserved,
        "pin_membership_preserved": pin_membership_preserved,
        "component_pin_groups_preserved": pin_membership_preserved,
        "shorted_net_sets_preserved": shorts_preserved,
        "component_pin_group_changes": pin_net_differences,
        "net_name_or_scope_changes": [],
        "pin_net_differences": pin_net_differences,
        "shorts_before": before.shorts,
        "shorts_after": after.shorts
    }))
}

async fn validate_kicad_netlist_if_configured(
    ctx: &ToolContext,
    schematic: &std::path::Path,
    before: &str,
    after: &str,
) -> anyhow::Result<()> {
    if ctx.config.kicad_cli.trim().is_empty() {
        return Ok(());
    }
    let before_snapshot = kicad_netlist_component_groups(&ctx.config.kicad_cli, schematic, before)
        .await
        .map_err(|error| anyhow::anyhow!("KiCad netlist BEFORE export failed: {error}"))?;
    let after_snapshot = kicad_netlist_component_groups(&ctx.config.kicad_cli, schematic, after)
        .await
        .map_err(|error| anyhow::anyhow!("KiCad netlist AFTER export failed: {error}"))?;
    if before_snapshot != after_snapshot {
        anyhow::bail!("KiCad netlist component-pin membership changed final transaction");
    }
    Ok(())
}

async fn kicad_netlist_component_groups(
    kicad_cli: &str,
    schematic: &std::path::Path,
    content: &str,
) -> anyhow::Result<BTreeSet<Vec<String>>> {
    let temp = tempfile::tempdir()?;
    let source_dir = schematic
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    for entry in std::fs::read_dir(source_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name() else {
            continue;
        };
        if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("kicad_sch" | "kicad_pro")
        ) {
            std::fs::copy(&path, temp.path().join(name))?;
        }
    }
    let temp_schematic = temp.path().join(
        schematic
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("schematic has no file name"))?,
    );
    std::fs::write(&temp_schematic, content)?;
    let netlist_path = temp.path().join("snapshot.net");
    crate::tools::cli::export_netlist(kicad_cli, &temp_schematic, &netlist_path, "kicadsexpr")
        .await?;
    let netlist = std::fs::read_to_string(&netlist_path)?;
    normalized_kicad_netlist_groups(&netlist)
}

fn normalized_kicad_netlist_groups(netlist: &str) -> anyhow::Result<BTreeSet<Vec<String>>> {
    let tree = parse_sexp(netlist)?;
    let mut groups = BTreeSet::new();
    let Some(nets) = tree.find("nets") else {
        return Ok(groups);
    };
    for net in nets.find_all("net") {
        let mut pins = net
            .find_all("node")
            .into_iter()
            .filter_map(|node| {
                let reference = node.find_str("ref")?;
                let pin = node.find_str("pin")?;
                Some(format!("{reference}.{pin}"))
            })
            .collect::<Vec<_>>();
        pins.sort();
        pins.dedup();
        if !pins.is_empty() {
            groups.insert(pins);
        }
    }
    Ok(groups)
}

#[derive(Debug, Clone)]
struct RefactorNetPlan {
    schematic: std::path::PathBuf,
    before: String,
    net: String,
    representation: String,
    endpoints: Vec<NetEndpoint>,
    segments: Vec<Segment>,
    moved_symbols: Vec<serde_json::Value>,
    removable_local_labels: Vec<(f64, f64)>,
    inserted_power_symbols: Vec<serde_json::Value>,
    power_normalization: PowerNormalizationReport,
    before_snapshot: SemanticSnapshot,
    final_content: String,
    plan_revision: String,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct ProjectSheet {
    path: PathBuf,
    parent_path: Option<PathBuf>,
    parent_sheet_uuid: Option<String>,
}

#[derive(Debug, Clone)]
struct ProjectFilePlan {
    path: PathBuf,
    before: String,
    after: String,
    power_symbols_added: Vec<serde_json::Value>,
    ordinary_labels_removed: usize,
    hierarchical_labels_removed: usize,
    global_labels_removed: usize,
    sheet_pins_removed: usize,
    wires_removed: usize,
    endpoints_before: Vec<NetEndpoint>,
    power_normalization: PowerNormalizationReport,
}

#[derive(Debug, Clone, Default)]
struct PowerNormalizationReport {
    canonical_unchanged: Vec<serde_json::Value>,
    geometry_normalizations: Vec<serde_json::Value>,
    noncanonical_replacements: Vec<serde_json::Value>,
    redundant_duplicates_removed: Vec<serde_json::Value>,
    obsolete_symbols_removed: Vec<serde_json::Value>,
    obsolete_stub_wires_removed: Vec<serde_json::Value>,
}

impl PowerNormalizationReport {
    fn changed(&self) -> bool {
        !self.geometry_normalizations.is_empty()
            || !self.noncanonical_replacements.is_empty()
            || !self.redundant_duplicates_removed.is_empty()
            || !self.obsolete_symbols_removed.is_empty()
            || !self.obsolete_stub_wires_removed.is_empty()
    }
}

#[derive(Debug, Clone)]
struct ProjectRefactorPlan {
    root: PathBuf,
    net: String,
    representation: String,
    participating_sheets: Vec<PathBuf>,
    files: Vec<ProjectFilePlan>,
    before_semantic: BTreeMap<String, SemanticSnapshot>,
    final_semantic: BTreeMap<String, SemanticSnapshot>,
    plan_revision: String,
    kicad_validation_available: bool,
    warnings: Vec<String>,
}

impl ProjectRefactorPlan {
    fn response(&self, dry_run: bool, applied: bool) -> serde_json::Value {
        json!({
            "dry_run": dry_run,
            "applied": applied,
            "safe_to_apply": true,
            "scope": "project",
            "root_schematic": self.root.display().to_string(),
            "target_net": self.net,
            "representation": self.representation,
            "participating_sheets": self.participating_sheets.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
            "touched_files": self.files.iter().map(|file| file.path.display().to_string()).collect::<Vec<_>>(),
            "power_symbols_to_add": self.files.iter().flat_map(|file| file.power_symbols_added.clone()).collect::<Vec<_>>(),
            "ordinary_labels_to_remove": self.files.iter().map(|file| json!({"schematic": file.path.display().to_string(), "count": file.ordinary_labels_removed})).collect::<Vec<_>>(),
            "hierarchical_labels_to_remove": self.files.iter().map(|file| json!({"schematic": file.path.display().to_string(), "count": file.hierarchical_labels_removed})).collect::<Vec<_>>(),
            "global_labels_to_remove": self.files.iter().map(|file| json!({"schematic": file.path.display().to_string(), "count": file.global_labels_removed})).collect::<Vec<_>>(),
            "sheet_pins_to_remove": self.files.iter().map(|file| json!({"schematic": file.path.display().to_string(), "count": file.sheet_pins_removed})).collect::<Vec<_>>(),
            "obsolete_parent_root_wires_to_remove": self.files.iter().map(|file| json!({"schematic": file.path.display().to_string(), "count": file.wires_removed})).collect::<Vec<_>>(),
            "canonical_unchanged": self.files.iter().flat_map(|file| file.power_normalization.canonical_unchanged.clone()).collect::<Vec<_>>(),
            "geometry_normalizations": self.files.iter().flat_map(|file| file.power_normalization.geometry_normalizations.clone()).collect::<Vec<_>>(),
            "noncanonical_replacements": self.files.iter().flat_map(|file| file.power_normalization.noncanonical_replacements.clone()).collect::<Vec<_>>(),
            "redundant_duplicates_removed": self.files.iter().flat_map(|file| file.power_normalization.redundant_duplicates_removed.clone()).collect::<Vec<_>>(),
            "obsolete_symbols_removed": self.files.iter().flat_map(|file| file.power_normalization.obsolete_symbols_removed.clone()).collect::<Vec<_>>(),
            "obsolete_stub_wires_removed": self.files.iter().flat_map(|file| file.power_normalization.obsolete_stub_wires_removed.clone()).collect::<Vec<_>>(),
            "before_endpoint_summary": self.files.iter().map(|file| json!({"schematic": file.path.display().to_string(), "component_pins": file.endpoints_before.iter().map(NetEndpoint::json).collect::<Vec<_>>() })).collect::<Vec<_>>(),
            "before_konnect_component_pin_membership": self.before_semantic.keys().collect::<Vec<_>>(),
            "predicted_final_component_pin_membership": self.final_semantic.keys().collect::<Vec<_>>(),
            "warnings": self.warnings,
            "project_plan_revision": self.plan_revision,
            "plan_revision": self.plan_revision,
            "kicad_validation_available": self.kicad_validation_available
        })
    }
}

impl RefactorNetPlan {
    fn response(&self, dry_run: bool, applied: bool) -> serde_json::Value {
        json!({
            "dry_run": dry_run,
            "applied": applied,
            "safe_to_apply": true,
            "plan_revision": self.plan_revision,
            "schematic": self.schematic.display().to_string(),
            "net": self.net,
            "representation": self.representation,
            "endpoints": self.endpoints.iter().map(NetEndpoint::json).collect::<Vec<_>>(),
            "proposed_moves": self.moved_symbols,
            "proposed_wires": self.segments.iter().map(|segment| segment.json()).collect::<Vec<_>>(),
            "proposed_removed_labels": self.removable_local_labels.iter().map(|(x, y)| json!({"x": x, "y": y})).collect::<Vec<_>>(),
            "proposed_power_symbols": self.inserted_power_symbols,
            "canonical_unchanged": self.power_normalization.canonical_unchanged,
            "geometry_normalizations": self.power_normalization.geometry_normalizations,
            "noncanonical_replacements": self.power_normalization.noncanonical_replacements,
            "redundant_duplicates_removed": self.power_normalization.redundant_duplicates_removed,
            "obsolete_symbols_removed": self.power_normalization.obsolete_symbols_removed,
            "obsolete_stub_wires_removed": self.power_normalization.obsolete_stub_wires_removed,
            "semantic_connectivity_comparison": semantic_comparison(&self.before_snapshot, &self.final_content).unwrap_or_else(|error| json!({
                "preserved": false,
                "error": error.to_string()
            })),
            "warnings": self.warnings
        })
    }
}

async fn handle_project_power_symbol_refactor(
    root: &PathBuf,
    net: &str,
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let plan = match build_project_power_symbol_refactor_plan(root, net, args, ctx) {
        Ok(plan) => plan,
        Err(error) => return Ok(error),
    };
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    if dry_run {
        return Ok(CallToolResult::json(&plan.response(true, false)));
    }
    if ctx.config.kicad_cli.trim().is_empty() {
        return Ok(CallToolResult::error(
            "project-scope power migration requires kicad-cli validation",
        ));
    }
    let supplied_revision = args["plan_revision"].as_str().unwrap_or_default();
    if supplied_revision != plan.plan_revision {
        return Ok(CallToolResult::error(format!(
            "plan_revision mismatch; rerun dry_run and apply exactly '{}'",
            plan.plan_revision
        )));
    }
    validate_project_semantic_snapshots(&plan)?;
    validate_project_kicad_netlist(ctx, &plan).await?;

    // TODO(KICAD_IPC):
    // Replace the KiCad-10 multi-file schematic hierarchy mutation backend when
    // the minimum supported KiCad release provides reliable arbitrary-sheet
    // hierarchy mutation, typed schematic deletion, and project-wide
    // transactional save semantics. Keep Konnect's project-level migration
    // planning and validation.
    let transitions = plan
        .files
        .iter()
        .map(|file| FileTransition::replace(&file.path, &file.before, &file.after))
        .collect::<Vec<_>>();
    commit_file_transaction(parent_dir(&plan.root), transitions)?;
    Ok(CallToolResult::json(&plan.response(false, true)))
}

fn build_project_power_symbol_refactor_plan(
    root: &PathBuf,
    net: &str,
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> Result<ProjectRefactorPlan, CallToolResult> {
    let allow_global_scope_change = args["allow_global_scope_change"].as_bool().unwrap_or(false);
    let scope_policy = args["scope_policy"]
        .as_str()
        .unwrap_or(if allow_global_scope_change {
            "allow_global_power_scope"
        } else {
            "preserve_existing_scope"
        });
    if scope_policy != "allow_global_power_scope" {
        return Err(CallToolResult::error(format!(
            "converting net '{net}' to power symbols changes to KiCad global power-symbol scope; rerun with scope_policy='allow_global_power_scope'"
        )));
    }

    let sheets = discover_project_sheets(root)?;
    let grid = args["grid"].as_f64().unwrap_or(1.27);
    let mut child_hier_removed: HashSet<(PathBuf, String)> = HashSet::new();
    let mut touched = Vec::new();
    let mut before_semantic = BTreeMap::new();
    let mut final_semantic = BTreeMap::new();
    let mut removed_sheet_pin_positions: HashMap<PathBuf, Vec<(f64, f64)>> = HashMap::new();

    for sheet in &sheets {
        let before = read_consistent(&sheet.path)
            .map_err(|error| CallToolResult::error(error.to_string()))?;
        let tree = parse_sexp(&before).map_err(|error| CallToolResult::error(error.to_string()))?;
        let mut sch = cse::Schematic::load(&sheet.path)
            .map_err(|error| CallToolResult::error(error.to_string()))?;
        let labels = konnect_sexp::schematic::extract_labels(&tree)
            .into_iter()
            .filter(|label| label.net == net)
            .collect::<Vec<_>>();
        let ordinary = labels
            .iter()
            .filter(|label| label.kind == LabelKind::NetLabel)
            .count();
        let global = labels
            .iter()
            .filter(|label| label.kind == LabelKind::GlobalLabel)
            .count();
        let hierarchical = labels
            .iter()
            .filter(|label| label.kind == LabelKind::HierarchicalLabel)
            .count();
        if hierarchical > 0 {
            if let (Some(parent), Some(uuid)) = (&sheet.parent_path, &sheet.parent_sheet_uuid) {
                child_hier_removed.insert((parent.clone(), uuid.clone()));
            }
        }
        let endpoints = net_endpoints_for(&tree, net, args, grid)?;
        let (power_normalization, mut attachment_segments) =
            normalize_existing_power_symbols(&mut sch, &sheet.path, &tree, net, grid)?;
        let existing_power = konnect_sexp::schematic::extract_all_net_labels(&tree)
            .into_iter()
            .filter(|label| label.net == net && label.kind == LabelKind::PowerSymbol)
            .map(|label| (label.x, label.y))
            .chain(
                power_normalization
                    .noncanonical_replacements
                    .iter()
                    .chain(power_normalization.geometry_normalizations.iter())
                    .filter_map(|symbol| Some((symbol["x"].as_f64()?, symbol["y"].as_f64()?))),
            )
            .collect::<Vec<_>>();
        let mut power_symbols_added = Vec::new();
        for label in &labels {
            if sheet.path == *root && endpoints.is_empty() {
                continue;
            }
            if existing_power
                .iter()
                .any(|(x, y)| points_coincident(*x, *y, label.x, label.y, 0.01))
            {
                continue;
            }
            let wires = extract_wires(&tree);
            let all_labels = konnect_sexp::schematic::extract_all_net_labels(&tree);
            let mut graph =
                crate::tools::sch_connectivity::net_graph_for(&tree, &wires, &all_labels);
            let attachment = power_symbol_anchor_for_label(label, net, grid, &mut graph)
                .map_err(|error| CallToolResult::error(error.to_string()))?;
            let placed = place_power_symbol_instance(
                &mut sch,
                &sheet.path,
                net,
                attachment.x,
                attachment.y,
                label.rotation,
            )?;
            let mut entry = placed.json();
            if let Some((old_x, old_y)) = attachment.normalized_from {
                entry["normalized_from"] = json!({"x": old_x, "y": old_y});
            }
            if !attachment.segments.is_empty() {
                entry["attachment_wires"] = json!(attachment
                    .segments
                    .iter()
                    .map(|segment| segment.json())
                    .collect::<Vec<_>>());
            }
            attachment_segments.extend(attachment.segments);
            power_symbols_added.push(entry);
        }
        sch.labels.retain(|label| label.text != net);
        sch.global_labels.retain(|label| label.text != net);
        sch.hierarchical_labels.retain(|label| label.text != net);
        let after = apply_segments(sch.to_source(), &attachment_segments)
            .map_err(|error| CallToolResult::error(error.to_string()))?;
        if after != before {
            before_semantic.insert(
                sheet.path.display().to_string(),
                semantic_snapshot(&before)
                    .map_err(|error| CallToolResult::error(error.to_string()))?,
            );
            final_semantic.insert(
                sheet.path.display().to_string(),
                semantic_snapshot(&after)
                    .map_err(|error| CallToolResult::error(error.to_string()))?,
            );
            touched.push(ProjectFilePlan {
                path: sheet.path.clone(),
                before,
                after,
                power_symbols_added,
                ordinary_labels_removed: ordinary,
                hierarchical_labels_removed: hierarchical,
                global_labels_removed: global,
                sheet_pins_removed: 0,
                wires_removed: 0,
                endpoints_before: endpoints,
                power_normalization,
            });
        }
    }

    for (parent_path, sheet_uuid) in child_hier_removed {
        let index = match touched.iter().position(|file| file.path == parent_path) {
            Some(index) => index,
            None => {
                let before = read_consistent(&parent_path)
                    .map_err(|error| CallToolResult::error(error.to_string()))?;
                touched.push(ProjectFilePlan {
                    path: parent_path.clone(),
                    before: before.clone(),
                    after: before,
                    power_symbols_added: Vec::new(),
                    ordinary_labels_removed: 0,
                    hierarchical_labels_removed: 0,
                    global_labels_removed: 0,
                    sheet_pins_removed: 0,
                    wires_removed: 0,
                    endpoints_before: Vec::new(),
                    power_normalization: PowerNormalizationReport::default(),
                });
                touched.len() - 1
            }
        };
        let mut sch = write_temp_and_load(&touched[index].path, &touched[index].after)
            .map_err(|error| CallToolResult::error(error.to_string()))?;
        let mut removed = Vec::new();
        for sheet in sch
            .sheets
            .iter_mut()
            .filter(|sheet| sheet.uuid == sheet_uuid)
        {
            let before_len = sheet.pins.len();
            sheet.pins.retain(|pin| {
                let keep = pin.name != net;
                if !keep {
                    removed.push(pin.position());
                }
                keep
            });
            touched[index].sheet_pins_removed += before_len - sheet.pins.len();
        }
        if !removed.is_empty() {
            removed_sheet_pin_positions
                .entry(touched[index].path.clone())
                .or_default()
                .extend(removed);
            touched[index].after = sch.to_source();
        }
    }

    for file in &mut touched {
        if let Some(positions) = removed_sheet_pin_positions.get(&file.path) {
            let mut sch = write_temp_and_load(&file.path, &file.after)
                .map_err(|error| CallToolResult::error(error.to_string()))?;
            let before_len = sch.wires.len();
            sch.wires.retain(|wire| {
                let a = positions
                    .iter()
                    .any(|(x, y)| points_coincident(*x, *y, wire.start.0, wire.start.1, 0.01));
                let b = positions
                    .iter()
                    .any(|(x, y)| points_coincident(*x, *y, wire.end.0, wire.end.1, 0.01));
                !(a && b)
            });
            file.wires_removed = before_len - sch.wires.len();
            file.after = sch.to_source();
        }
        parse_sexp(&file.after).map_err(|error| CallToolResult::error(error.to_string()))?;
        before_semantic.insert(
            file.path.display().to_string(),
            semantic_snapshot(&file.before)
                .map_err(|error| CallToolResult::error(error.to_string()))?,
        );
        final_semantic.insert(
            file.path.display().to_string(),
            semantic_snapshot(&file.after)
                .map_err(|error| CallToolResult::error(error.to_string()))?,
        );
    }

    touched.retain(|file| file.before != file.after);
    if touched.is_empty() {
        return Err(CallToolResult::error(format!(
            "project scope found no target-net hierarchy or labels for '{net}'"
        )));
    }
    touched.sort_by(|a, b| a.path.cmp(&b.path));
    let participating_sheets = sheets
        .iter()
        .map(|sheet| sheet.path.clone())
        .collect::<Vec<_>>();
    let plan_revision = project_plan_revision(net, scope_policy, &touched);
    Ok(ProjectRefactorPlan {
        root: root.clone(),
        net: net.to_string(),
        representation: "power_symbol".to_string(),
        participating_sheets,
        files: touched,
        before_semantic,
        final_semantic,
        plan_revision,
        kicad_validation_available: !ctx.config.kicad_cli.trim().is_empty(),
        warnings: vec![
            "KiCad power symbols have global power-rail scope; conversion required explicit scope_policy approval.".to_string(),
        ],
    })
}

fn parent_dir(path: &Path) -> PathBuf {
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn discover_project_sheets(root: &PathBuf) -> Result<Vec<ProjectSheet>, CallToolResult> {
    fn visit(
        path: PathBuf,
        parent_path: Option<PathBuf>,
        parent_sheet_uuid: Option<String>,
        out: &mut Vec<ProjectSheet>,
        seen: &mut HashSet<PathBuf>,
    ) -> Result<(), CallToolResult> {
        let canonical = path
            .canonicalize()
            .map_err(|error| CallToolResult::error(error.to_string()))?;
        if !seen.insert(canonical.clone()) {
            return Ok(());
        }
        out.push(ProjectSheet {
            path: canonical.clone(),
            parent_path,
            parent_sheet_uuid,
        });
        let sch = cse::Schematic::load(&canonical)
            .map_err(|error| CallToolResult::error(error.to_string()))?;
        let base = parent_dir(&canonical);
        for sheet in sch.sheets.iter() {
            let child = resolve_sheet_path(&base, sheet.file());
            visit(
                child,
                Some(canonical.clone()),
                Some(sheet.uuid.clone()),
                out,
                seen,
            )?;
        }
        Ok(())
    }

    let mut sheets = Vec::new();
    let mut seen = HashSet::new();
    visit(root.clone(), None, None, &mut sheets, &mut seen)?;
    Ok(sheets)
}

fn resolve_sheet_path(base: &Path, sheet_file: &str) -> PathBuf {
    let path = PathBuf::from(sheet_file);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn write_temp_and_load(path: &Path, content: &str) -> anyhow::Result<cse::Schematic> {
    let temp = tempfile::tempdir()?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("schematic has no file name"))?;
    let temp_path = temp.path().join(name);
    std::fs::write(&temp_path, content)?;
    cse::Schematic::load(&temp_path).map_err(anyhow::Error::from)
}

fn project_plan_revision(net: &str, scope_policy: &str, files: &[ProjectFilePlan]) -> String {
    let mut material = format!("{net}\npower_symbol\nproject\n{scope_policy}");
    for file in files {
        material.push('\n');
        material.push_str(&file.path.display().to_string());
        material.push('\n');
        material.push_str(&DocumentRevision::of(&file.before).to_string());
        material.push('\n');
        material.push_str(&file.power_symbols_added.len().to_string());
        material.push(':');
        material.push_str(&file.power_normalization.changed().to_string());
        material.push(':');
        material.push_str(&file.ordinary_labels_removed.to_string());
        material.push(':');
        material.push_str(&file.hierarchical_labels_removed.to_string());
        material.push(':');
        material.push_str(&file.sheet_pins_removed.to_string());
        material.push(':');
        material.push_str(&file.wires_removed.to_string());
    }
    format!(
        "refactor-schematic-net-project:{}",
        DocumentRevision::of(&material)
    )
}

fn validate_project_semantic_snapshots(plan: &ProjectRefactorPlan) -> anyhow::Result<()> {
    for file in &plan.files {
        let before = plan
            .before_semantic
            .get(&file.path.display().to_string())
            .ok_or_else(|| anyhow::anyhow!("missing semantic before snapshot"))?;
        validate_semantic_snapshot(before, &file.after, "project final transaction")?;
    }
    Ok(())
}

async fn validate_project_kicad_netlist(
    ctx: &ToolContext,
    plan: &ProjectRefactorPlan,
) -> anyhow::Result<()> {
    let before = kicad_netlist_component_groups_for_project(&ctx.config.kicad_cli, &plan.root, &[])
        .await
        .map_err(|error| anyhow::anyhow!("KiCad project netlist BEFORE export failed: {error}"))?;
    let overrides = plan
        .files
        .iter()
        .map(|file| (file.path.clone(), file.after.clone()))
        .collect::<Vec<_>>();
    let after =
        kicad_netlist_component_groups_for_project(&ctx.config.kicad_cli, &plan.root, &overrides)
            .await
            .map_err(|error| {
                anyhow::anyhow!("KiCad project netlist AFTER export failed: {error}")
            })?;
    if before != after {
        anyhow::bail!("KiCad project netlist component-pin membership changed final transaction");
    }
    Ok(())
}

async fn kicad_netlist_component_groups_for_project(
    kicad_cli: &str,
    root: &Path,
    overrides: &[(PathBuf, String)],
) -> anyhow::Result<BTreeSet<Vec<String>>> {
    let temp = tempfile::tempdir()?;
    let source_dir = parent_dir(root);
    for entry in std::fs::read_dir(&source_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name() else {
            continue;
        };
        if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("kicad_sch" | "kicad_pro")
        ) {
            std::fs::copy(&path, temp.path().join(name))?;
        }
    }
    for (path, content) in overrides {
        let name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("schematic has no file name"))?;
        std::fs::write(temp.path().join(name), content)?;
    }
    let root_name = root
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("root schematic has no file name"))?;
    let temp_root = temp.path().join(root_name);
    let netlist_path = temp.path().join("project-snapshot.net");
    crate::tools::cli::export_netlist(kicad_cli, &temp_root, &netlist_path, "kicadsexpr").await?;
    let netlist = std::fs::read_to_string(&netlist_path)?;
    normalized_kicad_netlist_groups(&netlist)
}

async fn handle_refactor_schematic_net_representation(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let schematic = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let representation = args["representation"].as_str().unwrap_or("wired_local");
    if representation == "scope_diagnostic" {
        return inspect_net_representation_scope(&schematic, &net);
    }
    let scope = args["scope"].as_str().unwrap_or("sheet");
    if scope == "project" {
        if representation != "power_symbol" {
            return Ok(CallToolResult::error(format!(
                "project scope supports representation='power_symbol', not '{representation}'"
            )));
        }
        return handle_project_power_symbol_refactor(&schematic, &net, args, ctx).await;
    } else if scope != "sheet" {
        return Ok(CallToolResult::error(format!(
            "scope '{scope}' is invalid; expected sheet or project"
        )));
    }
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let plan = match build_refactor_net_plan(&schematic, &net, representation, args) {
        Ok(plan) => plan,
        Err(error) => return Ok(error),
    };
    if dry_run {
        return Ok(CallToolResult::json(&plan.response(true, false)));
    }
    let supplied_revision = args["plan_revision"].as_str().unwrap_or_default();
    if supplied_revision != plan.plan_revision {
        return Ok(CallToolResult::error(format!(
            "plan_revision mismatch; rerun dry_run and apply exactly '{}'",
            plan.plan_revision
        )));
    }
    validate_semantic_snapshot(
        &plan.before_snapshot,
        &plan.final_content,
        "final transaction",
    )?;
    validate_kicad_netlist_if_configured(ctx, &plan.schematic, &plan.before, &plan.final_content)
        .await?;
    write_atomic_if_unchanged(&plan.schematic, &plan.before, &plan.final_content)?;
    Ok(CallToolResult::json(&plan.response(false, true)))
}

fn build_refactor_net_plan(
    schematic: &std::path::PathBuf,
    net: &str,
    representation: &str,
    args: &serde_json::Value,
) -> Result<RefactorNetPlan, CallToolResult> {
    match representation {
        "wired_local" => {
            let materialize = build_materialize_net_plan(
                schematic,
                net,
                args,
                args["remove_redundant_local_labels"]
                    .as_bool()
                    .unwrap_or(false),
            )?;
            Ok(RefactorNetPlan {
                schematic: materialize.schematic,
                before: materialize.before,
                net: materialize.net,
                representation: "wired_local".to_string(),
                endpoints: materialize.endpoints,
                segments: materialize.segments,
                moved_symbols: Vec::new(),
                removable_local_labels: materialize.removable_local_labels,
                inserted_power_symbols: Vec::new(),
                power_normalization: PowerNormalizationReport::default(),
                before_snapshot: materialize.before_snapshot,
                final_content: materialize.final_content,
                plan_revision: materialize
                    .plan_revision
                    .replace("materialize-schematic-net:", "refactor-schematic-net:"),
                warnings: Vec::new(),
            })
        }
        "expand_coincident_connection" => build_expand_coincident_net_plan(schematic, net, args),
        "power_symbol" => build_power_symbol_refactor_plan(schematic, net, args),
        other => Err(CallToolResult::error(format!(
            "representation '{other}' is invalid"
        ))),
    }
}

fn build_expand_coincident_net_plan(
    schematic: &std::path::PathBuf,
    net: &str,
    args: &serde_json::Value,
) -> Result<RefactorNetPlan, CallToolResult> {
    let (before, tree) =
        read_schematic(schematic).map_err(|error| CallToolResult::error(error.to_string()))?;
    let before_snapshot =
        semantic_snapshot(&before).map_err(|error| CallToolResult::error(error.to_string()))?;
    let grid = args["grid"].as_f64().unwrap_or(1.27);
    let expand = args.get("expand").ok_or_else(|| {
        CallToolResult::error("expand_coincident_connection requires an expand object")
    })?;
    let move_reference = expand["move_reference"]
        .as_str()
        .ok_or_else(|| CallToolResult::error("expand.move_reference is required"))?;
    let dx = expand["dx"]
        .as_f64()
        .ok_or_else(|| CallToolResult::error("expand.dx is required"))?;
    let dy = expand["dy"]
        .as_f64()
        .ok_or_else(|| CallToolResult::error("expand.dy is required"))?;

    let endpoints = net_endpoints_for(&tree, net, args, grid)?;
    let moved_before = endpoints
        .iter()
        .filter(|endpoint| endpoint.reference == move_reference)
        .cloned()
        .collect::<Vec<_>>();
    if moved_before.is_empty() {
        return Err(CallToolResult::error(format!(
            "{move_reference} has no selected endpoint on net '{net}'"
        )));
    }
    let coincident = moved_before.iter().any(|moved| {
        endpoints.iter().any(|other| {
            other.key() != moved.key()
                && points_coincident(moved.x, moved.y, other.x, other.y, 0.01)
        })
    });
    if !coincident {
        return Err(CallToolResult::error(format!(
            "selected endpoint for {move_reference} is not coincident with another endpoint on net '{net}'"
        )));
    }

    let moved_content = translate_symbol_instances_by_reference(&before, move_reference, dx, dy)
        .map_err(|error| CallToolResult::error(error.to_string()))?;
    let mut segments = moved_before
        .iter()
        .map(|endpoint| Segment {
            x1: endpoint.x,
            y1: endpoint.y,
            x2: endpoint.x + dx,
            y2: endpoint.y + dy,
        })
        .filter(|segment| !segment.is_zero())
        .collect::<Vec<_>>();
    segments.sort_by(|a, b| {
        a.normalized()
            .json()
            .to_string()
            .cmp(&b.normalized().json().to_string())
    });
    segments.dedup_by(|a, b| a.normalized() == b.normalized());
    let mut final_content = apply_segments(moved_content, &segments)
        .map_err(|error| CallToolResult::error(error.to_string()))?;
    let removable_local_labels = if args["remove_redundant_local_labels"]
        .as_bool()
        .unwrap_or(false)
    {
        redundant_plain_label_positions(&final_content, net)
    } else {
        Vec::new()
    };
    if !removable_local_labels.is_empty() {
        let (without_labels, _) =
            remove_plain_labels_at(final_content, net, &removable_local_labels)
                .map_err(|error| CallToolResult::error(error.to_string()))?;
        final_content = without_labels;
    }
    validate_semantic_snapshot(
        &before_snapshot,
        &final_content,
        "preview final transaction",
    )
    .map_err(|error| CallToolResult::error(error.to_string()))?;
    let mut material = format!(
        "{}\n{net}\nexpand\n{move_reference}\n{dx}\n{dy}",
        DocumentRevision::of(&before)
    );
    for segment in &segments {
        material.push('\n');
        material.push_str(&segment.json().to_string());
    }
    let plan_revision = format!("refactor-schematic-net:{}", DocumentRevision::of(&material));
    Ok(RefactorNetPlan {
        schematic: schematic.clone(),
        before,
        net: net.to_string(),
        representation: "expand_coincident_connection".to_string(),
        endpoints,
        segments,
        moved_symbols: vec![json!({
            "reference": move_reference,
            "dx": dx,
            "dy": dy
        })],
        removable_local_labels,
        inserted_power_symbols: Vec::new(),
        power_normalization: PowerNormalizationReport::default(),
        before_snapshot,
        final_content,
        plan_revision,
        warnings: Vec::new(),
    })
}

fn build_power_symbol_refactor_plan(
    schematic: &std::path::PathBuf,
    net: &str,
    args: &serde_json::Value,
) -> Result<RefactorNetPlan, CallToolResult> {
    let (before, tree) =
        read_schematic(schematic).map_err(|error| CallToolResult::error(error.to_string()))?;
    let before_snapshot =
        semantic_snapshot(&before).map_err(|error| CallToolResult::error(error.to_string()))?;
    let diagnostic = net_scope_diagnostic(&tree, schematic, net);
    let allow_global_scope_change = args["allow_global_scope_change"].as_bool().unwrap_or(false);
    let scope_policy = args["scope_policy"]
        .as_str()
        .unwrap_or(if allow_global_scope_change {
            "allow_global_power_scope"
        } else {
            "preserve_existing_scope"
        });
    if scope_policy != "allow_global_power_scope" {
        return Err(CallToolResult::error(format!(
            "converting net '{net}' to power symbols changes to KiCad global power-symbol scope; rerun with scope_policy='allow_global_power_scope'. Diagnostic: {diagnostic}"
        )));
    }
    let mut staged_schematic = cse::Schematic::load(schematic)
        .map_err(|error| CallToolResult::error(error.to_string()))?;
    let grid = args["grid"].as_f64().unwrap_or(1.27);
    let (power_normalization, mut attachment_segments) =
        normalize_existing_power_symbols(&mut staged_schematic, schematic, &tree, net, grid)?;
    let labels = konnect_sexp::schematic::extract_labels(&tree)
        .into_iter()
        .filter(|label| label.net == net && label.kind == LabelKind::NetLabel)
        .collect::<Vec<_>>();
    if labels.is_empty() && !power_normalization.changed() {
        return Err(CallToolResult::error(format!(
            "net '{net}' has no ordinary labels or legacy power symbols to convert"
        )));
    }
    let wires = extract_wires(&tree);
    let all_labels = konnect_sexp::schematic::extract_all_net_labels(&tree);
    let mut graph = crate::tools::sch_connectivity::net_graph_for(&tree, &wires, &all_labels);
    let existing_power = all_labels
        .iter()
        .filter(|label| label.net == net && label.kind == LabelKind::PowerSymbol)
        .map(|label| (label.x, label.y))
        .chain(
            power_normalization
                .noncanonical_replacements
                .iter()
                .chain(power_normalization.geometry_normalizations.iter())
                .filter_map(|symbol| Some((symbol["x"].as_f64()?, symbol["y"].as_f64()?))),
        )
        .collect::<Vec<_>>();
    let mut inserted = Vec::new();
    for label in &labels {
        if existing_power
            .iter()
            .any(|(x, y)| points_coincident(*x, *y, label.x, label.y, 0.01))
        {
            continue;
        }
        let attachment =
            power_symbol_anchor_for_label(label, net, grid, &mut graph).map_err(|error| {
                CallToolResult::error(format!(
                    "cannot convert '{}' label to power symbol: {error}",
                    net
                ))
            })?;
        let placed = place_power_symbol_instance(
            &mut staged_schematic,
            schematic,
            net,
            attachment.x,
            attachment.y,
            label.rotation,
        )?;
        let mut entry = placed.json();
        if let Some((old_x, old_y)) = attachment.normalized_from {
            entry["normalized_from"] = json!({"x": old_x, "y": old_y});
        }
        if !attachment.segments.is_empty() {
            entry["attachment_wires"] = json!(attachment
                .segments
                .iter()
                .map(|segment| segment.json())
                .collect::<Vec<_>>());
        }
        attachment_segments.extend(attachment.segments);
        inserted.push(entry);
    }
    let removable_local_labels = labels
        .iter()
        .map(|label| (label.x, label.y))
        .collect::<Vec<_>>();
    let uuids = labels
        .iter()
        .map(|label| label.uuid.clone().unwrap_or_default())
        .collect::<HashSet<_>>();
    staged_schematic
        .labels
        .retain(|label| !(label.text == net && uuids.contains(&label.uuid)));
    let final_content = apply_segments(staged_schematic.to_source(), &attachment_segments)
        .map_err(|error| CallToolResult::error(error.to_string()))?;
    validate_semantic_snapshot(
        &before_snapshot,
        &final_content,
        "preview final transaction",
    )
    .map_err(|error| CallToolResult::error(error.to_string()))?;
    let material = format!(
        "{}\n{net}\npower_symbol\n{scope_policy}\n{}",
        DocumentRevision::of(&before),
        inserted.len()
            + power_normalization.noncanonical_replacements.len()
            + power_normalization.geometry_normalizations.len()
            + power_normalization.redundant_duplicates_removed.len()
            + power_normalization.obsolete_symbols_removed.len()
            + power_normalization.obsolete_stub_wires_removed.len()
    );
    let plan_revision = format!("refactor-schematic-net:{}", DocumentRevision::of(&material));
    let endpoints = net_endpoints_for(&tree, net, args, args["grid"].as_f64().unwrap_or(1.27))?;
    let mut warnings = vec![
        "KiCad power symbols have global power-rail scope; conversion required explicit scope_policy approval.".to_string(),
    ];
    if diagnostic["mixed_connection_mechanisms"].as_bool() == Some(true) {
        warnings.push("Net already uses mixed connection mechanisms.".to_string());
    }
    Ok(RefactorNetPlan {
        schematic: schematic.clone(),
        before,
        net: net.to_string(),
        representation: "power_symbol".to_string(),
        endpoints,
        segments: Vec::new(),
        moved_symbols: Vec::new(),
        removable_local_labels,
        inserted_power_symbols: inserted,
        power_normalization,
        before_snapshot,
        final_content,
        plan_revision,
        warnings,
    })
}

fn inspect_net_representation_scope(
    schematic: &std::path::PathBuf,
    net: &str,
) -> anyhow::Result<CallToolResult> {
    let (_, tree) = read_schematic(schematic)?;
    Ok(CallToolResult::json(&net_scope_diagnostic(
        &tree, schematic, net,
    )))
}

fn net_scope_diagnostic(
    tree: &konnect_sexp::SexpNode,
    schematic: &std::path::Path,
    net: &str,
) -> serde_json::Value {
    let labels = konnect_sexp::schematic::extract_all_net_labels(tree)
        .into_iter()
        .filter(|label| label.net == net)
        .collect::<Vec<_>>();
    let ordinary = labels
        .iter()
        .filter(|label| label.kind == LabelKind::NetLabel)
        .count();
    let global = labels
        .iter()
        .filter(|label| label.kind == LabelKind::GlobalLabel)
        .count();
    let hierarchical = labels
        .iter()
        .filter(|label| label.kind == LabelKind::HierarchicalLabel)
        .count();
    let power = labels
        .iter()
        .filter(|label| label.kind == LabelKind::PowerSymbol)
        .count();
    let mechanisms = [ordinary, global, hierarchical, power]
        .into_iter()
        .filter(|count| *count > 0)
        .count();
    json!({
        "net": net,
        "sheets": [schematic.file_stem().and_then(|name| name.to_str()).unwrap_or("<sheet>")],
        "ordinary_label_scope": ordinary > 0,
        "hierarchical_connectivity": hierarchical > 0,
        "global_label_connectivity": global > 0,
        "power_symbol_connectivity": power > 0,
        "ordinary_labels": ordinary,
        "hierarchical_labels": hierarchical,
        "global_labels": global,
        "power_symbols": power,
        "mixed_connection_mechanisms": mechanisms > 1,
        "power_scope_conversion_requires_policy": (ordinary > 0 || hierarchical > 0 || global > 0) && power == 0
    })
}

fn net_endpoints_for(
    tree: &konnect_sexp::SexpNode,
    net: &str,
    args: &serde_json::Value,
    grid: f64,
) -> Result<Vec<NetEndpoint>, CallToolResult> {
    let wires = extract_wires(tree);
    let labels = konnect_sexp::schematic::extract_all_net_labels(tree);
    let mut graph = crate::tools::sch_connectivity::net_graph_for(tree, &wires, &labels);
    let index = crate::tools::sch_connectivity::ConnectivityIndex::build(
        tree,
        &wires,
        &labels,
        crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
    );
    let selected_keys = requested_materialize_pin_keys(tree, args, net, &mut graph)?;
    let mut endpoints = Vec::new();
    for placed in index.placed_pins() {
        let key = format!("{}:{}:{}", placed.reference, placed.unit, placed.pin.number);
        if selected_keys
            .as_ref()
            .is_some_and(|keys| !keys.contains(&key))
        {
            continue;
        }
        if graph.net_at(placed.at.0, placed.at.1).as_deref() == Some(net) {
            endpoints.push(NetEndpoint {
                kind: NetEndpointKind::ComponentPin,
                reference: placed.reference.clone(),
                unit: placed.unit,
                pin_number: placed.pin.number.clone(),
                pin_name: placed.pin.name.clone(),
                x: placed.at.0,
                y: placed.at.1,
                orientation_degrees: placed.orientation_degrees,
                grid,
                on_grid: point_on_grid(placed.at.0, placed.at.1, grid),
                nearest_grid_x: snap_point(placed.at.0, placed.at.1, grid).0,
                nearest_grid_y: snap_point(placed.at.0, placed.at.1, grid).1,
                attachment_depends_on_off_grid_position: attachment_depends_on_off_grid_position(
                    &mut graph,
                    placed.at.0,
                    placed.at.1,
                    grid,
                ),
            });
        }
    }
    if selected_keys.is_none() {
        endpoints.extend(sheet_pin_endpoints_for(tree, &mut graph, net, grid));
    }
    endpoints.sort_by(|a, b| a.key().cmp(&b.key()));
    Ok(endpoints)
}

fn sheet_pin_endpoints_for(
    tree: &konnect_sexp::SexpNode,
    graph: &mut crate::tools::sch_connectivity::NetGraph,
    net: &str,
    grid: f64,
) -> Vec<NetEndpoint> {
    let mut endpoints = Vec::new();
    for sheet_node in tree.find_all("sheet") {
        let Some((sheet_x, sheet_y, _)) = parse_at(sheet_node) else {
            continue;
        };
        let Some(size) = sheet_node.find("size") else {
            continue;
        };
        let (Some(width), Some(height)) = (size.get_f64(1), size.get_f64(2)) else {
            continue;
        };
        let Some(sheet_uuid) = sheet_node.find_str("uuid") else {
            continue;
        };
        let sheet_name = property_value_in_node(sheet_node, "Sheetname").unwrap_or(sheet_uuid);
        for pin_node in sheet_node.find_all("pin") {
            let Some(pin_name) = pin_node.get(1).and_then(|node| node.as_str()) else {
                continue;
            };
            let Some((pin_x, pin_y, rotation)) = parse_at(pin_node) else {
                continue;
            };
            let Some(pin_uuid) = pin_node.find_str("uuid") else {
                continue;
            };
            if pin_x < sheet_x - 0.01
                || pin_x > sheet_x + width + 0.01
                || pin_y < sheet_y - 0.01
                || pin_y > sheet_y + height + 0.01
            {
                continue;
            }
            if graph.net_at(pin_x, pin_y).as_deref() != Some(net) {
                continue;
            }
            endpoints.push(NetEndpoint {
                kind: NetEndpointKind::SheetPin {
                    sheet_uuid: sheet_uuid.to_string(),
                    sheet_name: sheet_name.to_string(),
                    pin_uuid: pin_uuid.to_string(),
                },
                reference: sheet_name.to_string(),
                unit: 0,
                pin_number: pin_name.to_string(),
                pin_name: pin_name.to_string(),
                x: pin_x,
                y: pin_y,
                orientation_degrees: rotation,
                grid,
                on_grid: point_on_grid(pin_x, pin_y, grid),
                nearest_grid_x: snap_point(pin_x, pin_y, grid).0,
                nearest_grid_y: snap_point(pin_x, pin_y, grid).1,
                attachment_depends_on_off_grid_position: false,
            });
        }
    }
    endpoints
}

fn apply_segments(content: String, segments: &[Segment]) -> anyhow::Result<String> {
    segments.iter().try_fold(content, |content, segment| {
        let tree = parse_sexp(&content)?;
        let current_wires = extract_wires(&tree);
        Ok(if wire_exists(&current_wires, *segment) {
            content
        } else {
            insert_wire_with_junctions(content, segment.x1, segment.y1, segment.x2, segment.y2)
        })
    })
}

fn translate_symbol_instances_by_reference(
    content: &str,
    reference: &str,
    dx: f64,
    dy: f64,
) -> anyhow::Result<String> {
    let mut edits = Vec::new();
    for (start, end) in find_direct_child_blocks(content, "kicad_sch") {
        let block = &content[start..end];
        if !block.trim_start().starts_with("(symbol") {
            continue;
        }
        let node = parse_sexp(block)?;
        if property_value_in_node(&node, "Reference") != Some(reference) {
            continue;
        }
        for (rel_start, rel_end, replacement) in coordinate_clause_replacements(block, dx, dy)? {
            edits.push(SexpEdit::replace(
                start + rel_start,
                start + rel_end,
                replacement,
            ));
        }
    }
    if edits.is_empty() {
        anyhow::bail!("symbol reference '{reference}' was not found");
    }
    Ok(apply_edits(content.to_string(), edits))
}

fn property_value_in_node<'a>(node: &'a konnect_sexp::SexpNode, name: &str) -> Option<&'a str> {
    node.find_all("property")
        .into_iter()
        .find(|property| property.get(1).and_then(|n| n.as_str()) == Some(name))
        .and_then(|property| property.get(2))
        .and_then(|value| value.as_str())
}

fn coordinate_clause_replacements(
    block: &str,
    dx: f64,
    dy: f64,
) -> anyhow::Result<Vec<(usize, usize, String)>> {
    let mut replacements = Vec::new();
    for start in find_block_starts(block, "at")
        .into_iter()
        .chain(find_block_starts(block, "start"))
        .chain(find_block_starts(block, "mid"))
        .chain(find_block_starts(block, "end"))
        .chain(find_block_starts(block, "center"))
        .chain(find_block_starts(block, "xy"))
    {
        let Some((clause_start, clause_end)) = find_balanced_block(block, start) else {
            continue;
        };
        let clause = &block[clause_start..clause_end];
        let Some(head_end) = clause.find(char::is_whitespace) else {
            continue;
        };
        let values_start = clause_start + head_end;
        let values_end = clause_end - 1;
        let values = block[values_start..values_end].trim();
        let mut parts = values.split_whitespace().collect::<Vec<_>>();
        if parts.len() < 2 {
            continue;
        }
        let (Some(x), Some(y)) = (
            parts.first().and_then(|value| value.parse::<f64>().ok()),
            parts.get(1).and_then(|value| value.parse::<f64>().ok()),
        ) else {
            continue;
        };
        let new_x = cse::types::fmt_f64(x + dx);
        let new_y = cse::types::fmt_f64(y + dy);
        parts[0] = &new_x;
        parts[1] = &new_y;
        replacements.push((values_start, values_end, format!(" {}", parts.join(" "))));
    }
    Ok(replacements)
}

fn power_symbol_template_lib_id(net: &str) -> String {
    match net {
        "GND" => "power:GND".to_string(),
        "+5V" => "power:+5V".to_string(),
        "+3V3" => "power:+3V3".to_string(),
        _ if net.starts_with("+5V") || net.starts_with("+5") => "power:+5V".to_string(),
        _ if net.starts_with("+3V3") || net.starts_with("+3.3V") => "power:+3V3".to_string(),
        _ => "power:VCC".to_string(),
    }
}

#[derive(Debug, Clone)]
struct PowerSymbolAttachment {
    x: f64,
    y: f64,
    normalized_from: Option<(f64, f64)>,
    segments: Vec<Segment>,
}

fn power_symbol_anchor_for_label(
    label: &Label,
    net: &str,
    grid: f64,
    graph: &mut crate::tools::sch_connectivity::NetGraph,
) -> anyhow::Result<PowerSymbolAttachment> {
    if point_on_grid(label.x, label.y, grid) {
        return Ok(PowerSymbolAttachment {
            x: label.x,
            y: label.y,
            normalized_from: None,
            segments: Vec::new(),
        });
    }

    let mut candidates = vec![snap_point(label.x, label.y, grid)];
    let snapped = candidates[0];
    for dx in [-grid, 0.0, grid] {
        for dy in [-grid, 0.0, grid] {
            candidates.push((round6(snapped.0 + dx), round6(snapped.1 + dy)));
        }
    }
    candidates.sort_by(|a, b| {
        let da = (a.0 - label.x).abs() + (a.1 - label.y).abs();
        let db = (b.0 - label.x).abs() + (b.1 - label.y).abs();
        da.partial_cmp(&db)
            .expect("finite power-symbol candidate distance")
            .then_with(|| a.partial_cmp(b).expect("finite power-symbol candidate"))
    });
    candidates.dedup_by(|a, b| points_coincident(a.0, a.1, b.0, b.1, 0.01));

    for (x, y) in candidates {
        match graph.net_at(x, y).as_deref() {
            Some(existing) if existing != net => continue,
            Some(_) => {
                return Ok(PowerSymbolAttachment {
                    x,
                    y,
                    normalized_from: Some((label.x, label.y)),
                    segments: Vec::new(),
                });
            }
            None => {}
        }
        let mut routes = two_point_candidates(
            (x, y),
            (label.x, label.y),
            &MaterializeObstacles {
                bodies: Vec::new(),
                foreign_pins: Vec::new(),
                foreign_wires: Vec::new(),
            },
        );
        routes.sort_by(candidate_route_cmp);
        if let Some(segments) = routes.into_iter().next() {
            return Ok(PowerSymbolAttachment {
                x,
                y,
                normalized_from: Some((label.x, label.y)),
                segments,
            });
        }
    }
    anyhow::bail!(
        "off-grid power label for '{net}' at ({}, {}) cannot be attached to a nearby schematic grid point without changing connectivity",
        label.x,
        label.y
    )
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_add_wire(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let (x1, y1) = snap_point(x1, y1, 1.27);
    let (x2, y2) = snap_point(x2, y2, 1.27);

    let mut sch = cse::Schematic::load(&sch_path)?;

    // T-junction detection: bridge cse wires to konnect_sexp wires
    let mut existing_wires = cse_wires_to_sexp(&sch);
    existing_wires.push(konnect_sexp::schematic::Wire {
        x1,
        y1,
        x2,
        y2,
        uuid: None,
    });
    let junctions = find_t_junctions(&existing_wires, 0.01);

    sch.add_wire(x1, y1, x2, y2);
    add_missing_junctions(&mut sch, &junctions);
    // Pins the new wire passes over mid-segment also need junction dots.
    let (_, tree) = read_schematic(&sch_path)?;
    let pins = crate::tools::all_pin_endpoints(&tree);
    add_missing_junctions(&mut sch, &pins_mid_segment(&pins, x1, y1, x2, y2));
    sch.overwrite()?;

    Ok(CallToolResult::json(
        &json!({ "added_wire": { "x1": x1, "y1": y1, "x2": x2, "y2": y2 } }),
    ))
}

async fn handle_batch_add_wire(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let wires = match require_array(args, "wires") {
        Ok(a) => a.clone(),
        Err(e) => return Ok(e),
    };

    // v0.6.0 made top-level required arguments refuse instead of defaulting,
    // but array items took the same silent path one level down (#234): an
    // element missing `x2` became a real wire to x=0 across the sheet,
    // reported as success. Validate every element before touching anything,
    // so a malformed batch changes nothing.
    for (i, w) in wires.iter().enumerate() {
        for key in ["x1", "y1", "x2", "y2"] {
            if w[key].as_f64().is_none() {
                return Ok(crate::tools::invalid_arg(
                    &format!("wires[{i}].{key}"),
                    "missing or not a number",
                ));
            }
        }
    }

    let mut sch = cse::Schematic::load(&sch_path)?;
    let mut added = 0usize;

    // Pin endpoints are fixed for the whole batch (only wires change below).
    let pins = read_schematic(&sch_path)
        .map(|(_, tree)| crate::tools::all_pin_endpoints(&tree))
        .unwrap_or_default();

    for w in &wires {
        let x1 = w["x1"].as_f64().unwrap_or(0.0);
        let y1 = w["y1"].as_f64().unwrap_or(0.0);
        let x2 = w["x2"].as_f64().unwrap_or(0.0);
        let y2 = w["y2"].as_f64().unwrap_or(0.0);
        let (x1, y1) = snap_point(x1, y1, 1.27);
        let (x2, y2) = snap_point(x2, y2, 1.27);

        // T-junction detection for each wire added incrementally.
        let mut existing_wires = cse_wires_to_sexp(&sch);
        existing_wires.push(konnect_sexp::schematic::Wire {
            x1,
            y1,
            x2,
            y2,
            uuid: None,
        });
        let junctions = find_t_junctions(&existing_wires, 0.01);

        sch.add_wire(x1, y1, x2, y2);
        add_missing_junctions(&mut sch, &junctions);
        // Pins this wire passes over mid-segment also need junction dots.
        add_missing_junctions(&mut sch, &pins_mid_segment(&pins, x1, y1, x2, y2));
        added += 1;
    }

    sch.overwrite()?;
    Ok(CallToolResult::json(&json!({ "added_wires": added })))
}

async fn handle_delete_wire(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let content = read_consistent(&sch_path)?;
    let expected = content.clone();

    let (del_start, del_end) = match locate_wire_for_delete(&content, args) {
        Ok(range) => range,
        Err(e) => return Ok(e),
    };

    let removed = wires_in_ranges(&content, &[(del_start, del_end)]);
    let edits = vec![SexpEdit::delete(del_start, del_end)];
    let new_content = apply_edits(content, edits);
    let (new_content, pruned) = prune_orphaned_junctions(new_content, &removed);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    Ok(CallToolResult::text(match pruned {
        0 => "Wire deleted.".to_string(),
        n => format!("Wire deleted. Removed {n} orphaned junction(s)."),
    }))
}

/// Byte range of the wire block the `uuid` or `x1/y1/x2/y2` arguments name.
fn locate_wire_for_delete(
    content: &str,
    args: &serde_json::Value,
) -> Result<(usize, usize), CallToolResult> {
    let range = if let Some(uuid) = opt_str(args, "uuid") {
        let search = format!(r#"(uuid "{uuid}")"#);
        let Some(wire_offset) = content.find(&search) else {
            return Err(CallToolResult::error(format!(
                "Wire UUID '{uuid}' not found"
            )));
        };
        wire_block_with_leading_whitespace(content, wire_offset)
    } else {
        let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
            opt_f64(args, "x1"),
            opt_f64(args, "y1"),
            opt_f64(args, "x2"),
            opt_f64(args, "y2"),
        ) else {
            return Err(CallToolResult::error(
                "Provide either uuid or all x1/y1/x2/y2 coordinates",
            ));
        };
        find_wire_block_by_endpoints(content, x1, y1, x2, y2)
    };

    range.ok_or_else(|| {
        CallToolResult::error("Cannot locate a wire block matching the requested identity")
    })
}

/// The wires inside the given byte ranges of the document.
fn wires_in_ranges(content: &str, ranges: &[(usize, usize)]) -> Vec<Wire> {
    // `extract_wires` expects wires as direct children of the parsed document
    // root, so wrap every range in one minimal root and parse it once.
    let mut wrapped = String::from("(kicad_sch ");
    for &(start, end) in ranges {
        wrapped.push_str(&content[start..end]);
    }
    wrapped.push(')');
    parse_sexp(&wrapped)
        .map(|node| extract_wires(&node))
        .unwrap_or_default()
}

/// Drop the junction dots the removed wires left with nothing to justify them.
///
/// Deleting a wire used to leave its dots behind, so relocating a block — delete
/// its wires, re-add them elsewhere — stranded junctions at the old coordinates.
/// A dot needs two wires to mean anything, with one exception: one wire plus a
/// pin landing mid-segment, which is exactly what `pins_mid_segment` creates.
/// So a junction is pruned when no wire is left through it, or one is and no pin
/// sits there. Junctions no removed wire touched are left alone, and moves that
/// strand a dot without deleting a wire are #120's half of the problem.
/// Round to the six decimals KiCAD writes, so arithmetic noise never reaches
/// the file.
fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

/// Re-evaluate the junction dots at `points` after geometry moved.
///
/// `prune_orphaned_junctions` answers the same question for a *wire* deletion:
/// it knows which dots to look at because it knows which wires went away. A
/// component move changes no wires at all — it moves *pins* — so the candidate
/// set has to come from the pins that appeared or disappeared, and the dot at
/// the vacated position has to be re-judged (#120).
///
/// All connectivity answers come from the shared [`ConnectivityIndex`] — this
/// must not become a fifth private definition of "what is attached here"
/// (#323). A dot is kept when anything still justifies it:
///
/// * two or more wires under the point — a T, a crossing joined on purpose, or
///   wire ends meeting: never touched;
/// * a pin or a hierarchical sheet pin still at the point;
/// * pins unresolvable (`lib_symbols` lookup failed) — that is not the same as
///   "no pin here", so only the unambiguous zero-wire case prunes then.
///
/// A dot is added only where a pin has landed mid-span on **exactly one** wire
/// and no no-connect flag sits there. One wire is the unambiguous case: with
/// two, wires that merely cross are separate nets until a junction says
/// otherwise, and adding the dot would merge them — the same silent
/// connectivity change this pass exists to stop. A no-connect is the user
/// saying "this pin stays unconnected", which a move must not override.
///
/// Returns the content plus (added, pruned).
pub(crate) fn reconcile_junctions_at(
    content: String,
    points: &[(f64, f64)],
) -> (String, usize, usize) {
    if points.is_empty() {
        return (content, 0, 0);
    }
    let Ok(tree) = parse_sexp(&content) else {
        return (content, 0, 0);
    };
    let wires = extract_wires(&tree);
    let labels = konnect_sexp::schematic::extract_all_net_labels(&tree);
    let idx = crate::tools::sch_connectivity::ConnectivityIndex::build(
        &tree,
        &wires,
        &labels,
        crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
    );
    // The shared index does not model buses yet, and KiCAD has bus junctions:
    // a dot on a bus tee joins bus segments, which no wire count can see. Any
    // candidate point touching a bus line is therefore outside this pass's
    // jurisdiction — neither pruned nor added at. This is a guard, not a fifth
    // attachment answer: the moment the index learns buses, it replaces this.
    let buses = konnect_sexp::schematic::extract_buses(&tree);
    let on_bus = |x: f64, y: f64| {
        buses.iter().any(|b| {
            konnect_sexp::geometry::point_on_segment(
                x,
                y,
                b.x1,
                b.y1,
                b.x2,
                b.y2,
                crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
            )
        })
    };
    let pins_known = !idx.placed_pins().is_empty() || extract_symbol_instances(&tree).is_empty();

    // Existing dots at the candidate points, with the byte range to delete —
    // the one thing the index does not hold.
    struct Dot {
        range: (usize, usize),
        at: (f64, f64),
    }
    let mut existing: Vec<Dot> = Vec::new();
    for start in find_block_starts(&content, "junction") {
        let Some((ws_start, block_end)) = find_block_with_leading_whitespace(&content, start)
        else {
            continue;
        };
        let Ok(node) = parse_sexp(&content[start..block_end]) else {
            continue;
        };
        let Some((jx, jy, _)) = parse_at(&node) else {
            continue;
        };
        existing.push(Dot {
            range: (ws_start, block_end),
            at: (jx, jy),
        });
    }

    const TOL: f64 = crate::tools::sch_connectivity::COINCIDENT_TOLERANCE;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut to_add: Vec<(f64, f64)> = Vec::new();
    for &(px, py) in points {
        if on_bus(px, py) {
            continue;
        }
        let here: Vec<&Dot> = existing
            .iter()
            .filter(|d| konnect_sexp::geometry::points_coincident(px, py, d.at.0, d.at.1, TOL))
            .collect();
        let attached = idx.has_pin(px, py) || idx.has_sheet_pin(px, py);
        if here.is_empty() {
            if idx.wires_at(px, py) == 1
                && idx.on_wire_interior(px, py)
                && idx.has_pin(px, py)
                && !idx.has_no_connect(px, py)
            {
                to_add.push((px, py));
            }
        } else {
            let keep = match idx.wires_at(px, py) {
                0 => false,
                1 => !pins_known || attached,
                _ => true,
            };
            if !keep {
                ranges.extend(here.iter().map(|d| d.range));
            }
        }
    }

    // Two coincident pin endpoints vacating one spot produce the same
    // candidate twice — dedup BEFORE counting, or the counts overstate what
    // happened and the add branch writes the same dot twice.
    ranges.sort_unstable();
    ranges.dedup();
    let pruned = ranges.len();
    let mut out = apply_edits(
        content,
        ranges
            .into_iter()
            .map(|(s, e)| SexpEdit::delete(s, e))
            .collect(),
    );
    let mut to_add: Vec<(f64, f64)> = to_add
        .into_iter()
        // Pin endpoints come out of arithmetic (136.19 + 3.81 = 139.70000000000002)
        // and `format_junction` interpolates the f64 verbatim, so round to the
        // 6 decimals KiCAD writes rather than leaking float noise into the file.
        .map(|(x, y)| (round6(x), round6(y)))
        .collect();
    to_add.sort_by(|a, b| a.partial_cmp(b).expect("rounded coordinates are finite"));
    to_add.dedup();
    let added = to_add.len();
    for (x, y) in to_add {
        out = insert_before_close(&out, &format_junction(x, y));
    }
    (out, added, pruned)
}

fn prune_orphaned_junctions(content: String, removed: &[Wire]) -> (String, usize) {
    const TOL: f64 = 0.01;
    // Two is as high as this needs to count, so it stops there.
    let wires_through = |wires: &[Wire], jx: f64, jy: f64| {
        wires
            .iter()
            .filter(|w| {
                konnect_sexp::geometry::point_on_segment(jx, jy, w.x1, w.y1, w.x2, w.y2, TOL)
            })
            .take(2)
            .count()
    };

    // The dots the removed wires ran through, with the block to delete. Most
    // deletions find none, and the document parse below is only for those.
    let mut candidates: Vec<((usize, usize), (f64, f64))> = Vec::new();
    for start in find_block_starts(&content, "junction") {
        let Some((ws_start, block_end)) = find_block_with_leading_whitespace(&content, start)
        else {
            continue;
        };
        let Ok(node) = parse_sexp(&content[start..block_end]) else {
            continue;
        };
        let Some((jx, jy, _)) = parse_at(&node) else {
            continue;
        };
        if wires_through(removed, jx, jy) > 0 {
            candidates.push(((ws_start, block_end), (jx, jy)));
        }
    }
    if candidates.is_empty() {
        return (content, 0);
    }

    let Ok(tree) = parse_sexp(&content) else {
        return (content, 0);
    };
    let remaining = extract_wires(&tree);
    // Resolving pins walks every symbol against `lib_symbols`, so it waits
    // until a dot is down to its last wire and the answer decides the case.
    let mut pins: Option<(Vec<(f64, f64)>, bool)> = None;

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (range, (jx, jy)) in candidates {
        let orphaned = match wires_through(&remaining, jx, jy) {
            0 => true,
            1 => {
                let (pins, pins_known) = pins.get_or_insert_with(|| {
                    let pins = crate::tools::all_pin_endpoints(&tree);
                    // A sheet with symbols but no resolvable pin means the
                    // `lib_symbols` lookup failed, not that nothing is
                    // connected. Only the unambiguous zero-wire case prunes
                    // then, rather than guess a pin isn't there.
                    let known = !pins.is_empty() || extract_symbol_instances(&tree).is_empty();
                    (pins, known)
                });
                *pins_known
                    && !pins.iter().any(|&(px, py)| {
                        konnect_sexp::geometry::points_coincident(px, py, jx, jy, TOL)
                    })
            }
            _ => false,
        };
        if orphaned {
            ranges.push(range);
        }
    }

    let pruned = ranges.len();
    let edits = ranges
        .into_iter()
        .map(|(s, e)| SexpEdit::delete(s, e))
        .collect();
    (apply_edits(content, edits), pruned)
}

async fn handle_batch_delete_wire(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let uuids: Vec<String> = match require_array(args, "uuids") {
        Ok(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut errors = Vec::new();

    // Collect all delete ranges first, then apply in reverse order
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for uuid in &uuids {
        let search = format!(r#"(uuid "{uuid}")"#);
        match content.find(&search) {
            Some(offset) => match wire_block_with_leading_whitespace(&content, offset) {
                Some(range) => ranges.push(range),
                None => errors.push(format!(
                    "UUID '{uuid}' exists but is not inside a parseable wire block"
                )),
            },
            None => errors.push(format!("Wire UUID '{uuid}' not found")),
        }
    }
    ranges.sort_unstable();
    ranges.dedup();
    let deleted = ranges.len();

    if deleted == 0 && !uuids.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No wires deleted: {}",
            errors.join("; ")
        )));
    }

    let removed = wires_in_ranges(&content, &ranges);
    let edits: Vec<SexpEdit> = ranges
        .into_iter()
        .map(|(s, e)| SexpEdit::delete(s, e))
        .collect();
    let content = apply_edits(content, edits);
    let (content, pruned) = prune_orphaned_junctions(content, &removed);
    write_atomic_if_unchanged(&sch_path, &expected, &content)?;
    Ok(CallToolResult::json(&json!({
        "deleted": deleted,
        "junctions_pruned_count": pruned,
        "errors": errors
    })))
}

fn wire_block_with_leading_whitespace(
    content: &str,
    contained_offset: usize,
) -> Option<(usize, usize)> {
    let (wire_start, _) = find_enclosing_block(content, "wire", contained_offset)?;
    find_block_with_leading_whitespace(content, wire_start)
}

fn find_wire_block_by_endpoints(
    content: &str,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
) -> Option<(usize, usize)> {
    const TOLERANCE: f64 = 1e-6;
    let same = |a: f64, b: f64| (a - b).abs() <= TOLERANCE;

    for start in find_block_starts(content, "wire") {
        let Some((block_start, block_end)) = find_balanced_block(content, start) else {
            continue;
        };
        let matches = wires_in_ranges(content, &[(block_start, block_end)])
            .into_iter()
            .any(|wire| {
                (same(wire.x1, x1) && same(wire.y1, y1) && same(wire.x2, x2) && same(wire.y2, y2))
                    || (same(wire.x1, x2)
                        && same(wire.y1, y2)
                        && same(wire.x2, x1)
                        && same(wire.y2, y1))
            });
        if matches {
            return find_block_with_leading_whitespace(content, block_start);
        }
    }
    None
}

async fn handle_split_wire_at_point(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let px = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let py = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let tree = parse_sexp(&content)?;
    let wires = extract_wires(&tree);

    // Find the wire that contains point (px, py) but is not an endpoint
    let target = wires.iter().find(|w| {
        !konnect_sexp::geometry::points_coincident(px, py, w.x1, w.y1, 0.01)
            && !konnect_sexp::geometry::points_coincident(px, py, w.x2, w.y2, 0.01)
            && konnect_sexp::geometry::point_on_segment(px, py, w.x1, w.y1, w.x2, w.y2, 0.01)
    });

    let w = match target {
        Some(w) => w.clone(),
        None => {
            return Ok(CallToolResult::error(
                "No wire found passing through that point",
            ))
        }
    };

    // Delete the original wire and insert two halves + junction. Not through
    // `handle_delete_wire`: the two halves cover the same segment, so the
    // wire's junctions stay justified and must not be pruned in between.
    let block = match &w.uuid {
        Some(uuid) => content
            .find(&format!(r#"(uuid "{uuid}")"#))
            .and_then(|offset| wire_block_with_leading_whitespace(&content, offset)),
        None => find_wire_block_by_endpoints(&content, w.x1, w.y1, w.x2, w.y2),
    };
    let Some((del_start, del_end)) = block else {
        return Ok(CallToolResult::error(
            "Cannot locate a wire block matching the requested identity",
        ));
    };
    let content = apply_edits(content, vec![SexpEdit::delete(del_start, del_end)]);

    let w1 = format_wire(w.x1, w.y1, px, py);
    let w2 = format_wire(px, py, w.x2, w.y2);
    let junc = format_junction(px, py);
    let insert = format!("{w1}{w2}{junc}");
    let new_content = insert_before_close(&content, &insert);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "split_at": { "x": px, "y": py },
        "wire_a": { "x1": w.x1, "y1": w.y1, "x2": px, "y2": py },
        "wire_b": { "x1": px, "y1": py, "x2": w.x2, "y2": w.y2 }
    })))
}

async fn handle_add_net_label(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let rotation = opt_f64(args, "rotation").unwrap_or(0.0);
    let label_type = opt_str(args, "label_type").unwrap_or("net_label");
    let shape = opt_str(args, "shape").unwrap_or("input");

    let mut sch = cse::Schematic::load(&sch_path)?;

    // set_rotation also writes the (effects … (justify …)) block. justify is
    // what turns the text away from the anchor, so a label created without one
    // renders backwards at 180°/270°, over whatever it points at.
    match label_type {
        "global_label" => {
            sch.add_global_label(&net, shape, x, y);
            let idx = sch.global_labels.len() - 1;
            if let Some(gl) = sch.global_labels.get_mut(idx) {
                gl.set_rotation(rotation);
            }
        }
        "hierarchical_label" => {
            sch.add_hierarchical_label(&net, shape, x, y);
            let idx = sch.hierarchical_labels.len() - 1;
            if let Some(hl) = sch.hierarchical_labels.get_mut(idx) {
                hl.set_rotation(rotation);
            }
        }
        _ => {
            let label = sch.add_label(&net, x, y);
            label.set_rotation(rotation);
        }
    }

    sch.overwrite()?;

    Ok(CallToolResult::json(
        &json!({ "added_label": net, "type": label_type, "x": x, "y": y }),
    ))
}

async fn handle_delete_net_label(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let target_x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let target_y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();

    let labels = find_label_blocks(&content);
    let named: Vec<&LabelBlock> = labels.iter().filter(|l| l.net == net).collect();

    if named.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No label named '{}' in this schematic",
            net
        )));
    }

    // Exact position match. Deleting the *nearest* label instead would silently
    // remove a same-named label elsewhere on the sheet — same-named labels are
    // how KiCAD joins nets, so they are the normal case, not an edge case.
    let matched: Vec<&&LabelBlock> = named
        .iter()
        .filter(|l| same_point(l.x, target_x) && same_point(l.y, target_y))
        .collect();

    let label = match matched.as_slice() {
        [one] => **one,
        [] => {
            let positions: Vec<String> = named
                .iter()
                .map(|l| format!("{} at ({}, {})", l.kind, l.x, l.y))
                .collect();
            return Ok(CallToolResult::error(format!(
                "No label '{}' at ({}, {}). Found {} label(s) named '{}': {}",
                net,
                target_x,
                target_y,
                named.len(),
                net,
                positions.join("; ")
            )));
        }
        _ => {
            return Ok(CallToolResult::error(format!(
                "{} labels named '{}' share position ({}, {}) — delete by uuid is not \
                 supported yet; remove the duplicates in eeschema",
                matched.len(),
                net,
                target_x,
                target_y
            )));
        }
    };

    let (del_start, del_end) = find_block_with_leading_whitespace(&content, label.start)
        .ok_or_else(|| anyhow::anyhow!("Cannot parse label block"))?;

    let kind = label.kind;
    let edits = vec![SexpEdit::delete(del_start, del_end)];
    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    Ok(CallToolResult::json(&json!({
        "deleted_label": net,
        "type": kind,
        "at": { "x": target_x, "y": target_y }
    })))
}

/// One label block located in the raw file text.
struct LabelBlock {
    /// Byte offset of the block's opening paren.
    start: usize,
    /// S-expression tag: `label`, `global_label`, or `hierarchical_label`.
    kind: &'static str,
    net: String,
    x: f64,
    y: f64,
}

/// KiCAD's three label tags. `label` is the plain net label — the type
/// `add_schematic_net_label` writes by default. (`net_label` is this codebase's
/// internal name for it and never appears in a .kicad_sch.)
const LABEL_TAGS: [&str; 3] = ["label", "global_label", "hierarchical_label"];

/// Locate every label block in `content` by scanning forward for the label tags
/// and parsing each block, rather than searching for a name string and walking
/// backwards — a quoted net name also appears in symbol properties, pin names,
/// and sheet pins, and walking back from one of those lands on an unrelated
/// block.
fn find_label_blocks(content: &str) -> Vec<LabelBlock> {
    let mut out = Vec::new();
    for kind in LABEL_TAGS {
        for start in find_block_starts(content, kind) {
            let Some((bs, be)) = find_balanced_block(content, start) else {
                continue;
            };
            let Ok(node) = parse_sexp(&content[bs..be]) else {
                continue;
            };
            // (label "NAME" (at X Y ROT) …) — the name is the first argument,
            // and (at) is a direct child, so a nested (at) on a global label's
            // intersheet-refs property can't be mistaken for the anchor.
            let Some(net) = node.get(1).and_then(|n| n.as_str()) else {
                continue;
            };
            let Some((x, y, _)) = parse_at(&node) else {
                continue;
            };
            out.push(LabelBlock {
                start: bs,
                kind,
                net: net.to_string(),
                x,
                y,
            });
        }
    }
    out
}

/// Compare schematic coordinates. KiCAD stores mm to 4 decimals, so this is an
/// exact match in practice while tolerating float round-trip noise.
fn same_point(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

async fn handle_rotate_label(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let rotation = match require_f64(args, "rotation") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();

    let labels = find_label_blocks(&content);
    let named: Vec<&LabelBlock> = labels.iter().filter(|l| l.net == net).collect();
    let Some(label) = named
        .iter()
        .find(|l| same_point(l.x, x) && same_point(l.y, y))
    else {
        let positions: Vec<String> = named
            .iter()
            .map(|l| format!("{} at ({}, {})", l.kind, l.x, l.y))
            .collect();
        return Ok(CallToolResult::error(if positions.is_empty() {
            format!("No label named '{}' in this schematic", net)
        } else {
            format!(
                "No label '{}' at ({}, {}). Found: {}",
                net,
                x,
                y,
                positions.join("; ")
            )
        }));
    };

    let (block_start, block_end) = find_balanced_block(&content, label.start)
        .ok_or_else(|| anyhow::anyhow!("Cannot parse label block"))?;
    let block = &content[block_start..block_end];

    let mut edits = Vec::new();

    // 1. The (at X Y ROT) anchor.
    let at_rel = block
        .find("(at ")
        .ok_or_else(|| anyhow::anyhow!("No (at) in label block"))?;
    let at_val = block_start + at_rel + "(at ".len();
    let at_close = content[at_val..]
        .find(')')
        .map(|o| at_val + o)
        .ok_or_else(|| anyhow::anyhow!("Malformed (at)"))?;
    edits.push(SexpEdit::replace(
        at_val,
        at_close,
        format!("{x} {y} {rotation}"),
    ));

    // 2. The justify, which is what actually turns the text — rotating the
    //    anchor alone leaves the text running back over whatever the label
    //    points at. Plain labels also carry `bottom` to lift text off the wire.
    let plain = label.kind == "label";
    let justify = konnect_sexp::schematic::label_justify(rotation);
    let justify_sexp = if plain {
        format!("(justify {justify} bottom)")
    } else {
        format!("(justify {justify})")
    };

    if let Some(j_rel) = block.find("(justify ") {
        // Replace the existing justify in place.
        let j_start = block_start + j_rel;
        let j_end = find_balanced_block(&content, j_start)
            .map(|(_, e)| e)
            .ok_or_else(|| anyhow::anyhow!("Malformed (justify)"))?;
        edits.push(SexpEdit::replace(j_start, j_end, justify_sexp));
    } else if let Some(e_rel) = block.find("(effects") {
        // An effects block with no justify — add one just inside it.
        let e_start = block_start + e_rel;
        let (_, e_end) = find_balanced_block(&content, e_start)
            .ok_or_else(|| anyhow::anyhow!("Malformed (effects)"))?;
        edits.push(SexpEdit::insert(e_end - 1, format!(" {justify_sexp}")));
    } else {
        // No effects at all — the shape add_schematic_net_label used to write.
        // Insert a complete block where eeschema puts it: before the uuid,
        // matching that line's indentation.
        let insert_at = block
            .find("(uuid")
            .map(|r| block_start + r)
            .unwrap_or(block_end - 1);
        let line_start = content[..insert_at]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(insert_at);
        let indent: String = content[line_start..insert_at]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        edits.push(SexpEdit::insert(
            insert_at,
            format!("(effects (font (size 1.27 1.27)) {justify_sexp})\n{indent}"),
        ));
    }

    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    Ok(CallToolResult::json(&json!({
        "rotated_label": net,
        "type": label.kind,
        "rotation": rotation,
        "justify": justify
    })))
}

async fn handle_move_labels_by_offset(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let dx = match require_f64(args, "dx") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let dy = match require_f64(args, "dy") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let labels = find_label_blocks(&content);
    let matching: Vec<&LabelBlock> = labels.iter().filter(|l| l.net == net).collect();
    if matching.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No label named '{}' in this schematic",
            net
        )));
    }

    // Edit each label's (at X Y ROT) anchor in place, preserving the rotation.
    let mut edits = Vec::new();
    for label in &matching {
        let (block_start, block_end) = find_balanced_block(&content, label.start)
            .ok_or_else(|| anyhow::anyhow!("Cannot parse label block"))?;
        let block = &content[block_start..block_end];
        let at_rel = block
            .find("(at ")
            .ok_or_else(|| anyhow::anyhow!("No (at) in label block"))?;
        let at_val = block_start + at_rel + "(at ".len();
        let at_close = content[at_val..]
            .find(')')
            .map(|o| at_val + o)
            .ok_or_else(|| anyhow::anyhow!("Malformed (at)"))?;
        let rotation = content[at_val..at_close]
            .split_whitespace()
            .nth(2)
            .unwrap_or("0")
            .to_string();
        edits.push(SexpEdit::replace(
            at_val,
            at_close,
            format!("{} {} {}", label.x + dx, label.y + dy, rotation),
        ));
    }

    let moved = edits.len();
    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "moved_labels": moved, "net": net }),
    ))
}

async fn handle_batch_rotate_labels(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let labels = match require_array(args, "labels") {
        Ok(a) => a.clone(),
        Err(e) => return Ok(e),
    };
    let mut rotated = 0usize;
    for label_arg in &labels {
        let full_args = json!({
            "schematic": sch_path.display().to_string(),
            "net": label_arg["net"],
            "x": label_arg["x"],
            "y": label_arg["y"],
            "rotation": label_arg["rotation"]
        });
        handle_rotate_label(&full_args, ctx).await?;
        rotated += 1;
    }
    Ok(CallToolResult::json(&json!({ "rotated": rotated })))
}

/// The lowest `#PWR` number no symbol on the sheet is using.
///
/// Counting the power symbols instead re-issues a live designator after a
/// deletion: drop `#PWR028` from a sheet of 29 and the count is 28, so the
/// next symbol is handed `#PWR029` — still in use, silently duplicated.
fn next_pwr_number(sch: &cse::Schematic) -> u32 {
    let used: std::collections::HashSet<u32> = sch
        .symbols
        .iter()
        .filter_map(|s| s.reference())
        .filter_map(|r| r.strip_prefix("#PWR"))
        .filter_map(|n| n.parse::<u32>().ok())
        .collect();
    (1u32..).find(|n| !used.contains(n)).unwrap_or(1)
}

#[derive(Debug, Clone)]
struct PlacedPowerSymbol {
    reference: String,
    template_lib_id: String,
    value: String,
    x: f64,
    y: f64,
    rotation: f64,
}

impl PlacedPowerSymbol {
    fn json(&self) -> serde_json::Value {
        json!({
            "reference": self.reference,
            "template_lib_id": self.template_lib_id,
            "net": self.value,
            "x": self.x,
            "y": self.y,
            "rotation": self.rotation
        })
    }
}

#[derive(Debug, Clone)]
struct ExistingPowerCandidate {
    uuid: String,
    reference: String,
    lib_id: String,
    value: String,
    x: f64,
    y: f64,
    rotation: f64,
    canonical: bool,
    on_grid: bool,
    attachment_key: (i64, i64),
}

impl ExistingPowerCandidate {
    fn json(&self, classification: &str) -> serde_json::Value {
        json!({
            "classification": classification,
            "uuid": self.uuid,
            "reference": self.reference,
            "lib_id": self.lib_id,
            "net": self.value,
            "x": self.x,
            "y": self.y,
            "rotation": self.rotation
        })
    }
}

fn symbol_is_target_power_candidate(symbol: &cse::Symbol, net: &str) -> bool {
    if symbol.value_str() != Some(net) {
        return false;
    }
    let reference_power = symbol.reference().is_some_and(|r| r.starts_with("#PWR"));
    reference_power
        || symbol.lib_id.starts_with("power:")
        || symbol.lib_symbol_name().starts_with("power:")
}

fn canonical_power_symbol_instance(symbol: &cse::Symbol, net: &str) -> bool {
    let template = power_symbol_template_lib_id(net);
    symbol.lib_id == template && symbol.lib_symbol_name() == template
}

fn endpoint_meaningful_for_power_stub(
    tree: &konnect_sexp::SexpNode,
    graph: &mut crate::tools::sch_connectivity::NetGraph,
    point: (f64, f64),
    symbol_uuid: &str,
) -> bool {
    let key = graph.find(crate::tools::sch_connectivity::pt_key(point.0, point.1));
    let wires = extract_wires(tree);
    let labels = konnect_sexp::schematic::extract_all_net_labels(tree);
    let index = crate::tools::sch_connectivity::ConnectivityIndex::build(
        tree,
        &wires,
        &labels,
        crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
    );
    if index.placed_pins().iter().any(|pin| {
        !pin.reference.starts_with("#PWR")
            && graph.find(crate::tools::sch_connectivity::pt_key(pin.at.0, pin.at.1)) == key
    }) {
        return true;
    }
    if extract_sheet_pins(tree)
        .into_iter()
        .any(|(x, y)| graph.find(crate::tools::sch_connectivity::pt_key(x, y)) == key)
    {
        return true;
    }
    if labels
        .iter()
        .filter(|label| label.kind != LabelKind::PowerSymbol)
        .any(|label| graph.find(crate::tools::sch_connectivity::pt_key(label.x, label.y)) == key)
    {
        return true;
    }
    extract_symbol_instances(tree).into_iter().any(|instance| {
        instance.uuid.as_deref() != Some(symbol_uuid)
            && instance.reference.starts_with("#PWR")
            && graph.find(crate::tools::sch_connectivity::pt_key(
                instance.x, instance.y,
            )) == key
    })
}

fn obsolete_stub_wire_uuids(
    sch: &cse::Schematic,
    tree: &konnect_sexp::SexpNode,
    graph: &mut crate::tools::sch_connectivity::NetGraph,
    symbol: &cse::Symbol,
) -> Option<Vec<String>> {
    let start = crate::tools::sch_connectivity::pt_key(symbol.at.x, symbol.at.y);
    let mut adjacency: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    let wires = sch.wires.iter().cloned().collect::<Vec<_>>();
    for (idx, wire) in wires.iter().enumerate() {
        adjacency
            .entry(crate::tools::sch_connectivity::pt_key(
                wire.start.0,
                wire.start.1,
            ))
            .or_default()
            .push(idx);
        adjacency
            .entry(crate::tools::sch_connectivity::pt_key(
                wire.end.0, wire.end.1,
            ))
            .or_default()
            .push(idx);
    }

    let mut stack = vec![start];
    let mut seen_points = HashSet::new();
    let mut seen_wires = HashSet::new();
    while let Some(point_key) = stack.pop() {
        if !seen_points.insert(point_key) {
            continue;
        }
        let degree = adjacency.get(&point_key).map_or(0, Vec::len);
        if degree > 2 {
            return None;
        }
        let point = (point_key.0 as f64 / 1000.0, point_key.1 as f64 / 1000.0);
        if point_key != start
            && endpoint_meaningful_for_power_stub(tree, graph, point, &symbol.uuid)
        {
            return None;
        }
        if let Some(wire_indices) = adjacency.get(&point_key) {
            for &wire_idx in wire_indices {
                if !seen_wires.insert(wire_idx) {
                    continue;
                }
                let wire = &wires[wire_idx];
                let a = crate::tools::sch_connectivity::pt_key(wire.start.0, wire.start.1);
                let b = crate::tools::sch_connectivity::pt_key(wire.end.0, wire.end.1);
                stack.push(if a == point_key { b } else { a });
            }
        }
    }
    if seen_wires.is_empty()
        && endpoint_meaningful_for_power_stub(tree, graph, (symbol.at.x, symbol.at.y), &symbol.uuid)
    {
        return None;
    }
    Some(
        seen_wires
            .into_iter()
            .map(|idx| wires[idx].uuid.clone())
            .collect(),
    )
}

fn normalize_existing_power_symbols(
    sch: &mut cse::Schematic,
    schematic: &Path,
    tree: &konnect_sexp::SexpNode,
    net: &str,
    grid: f64,
) -> Result<(PowerNormalizationReport, Vec<Segment>), CallToolResult> {
    let wires = extract_wires(tree);
    let all_labels = konnect_sexp::schematic::extract_all_net_labels(tree);
    let mut graph = crate::tools::sch_connectivity::net_graph_for(tree, &wires, &all_labels);
    let mut report = PowerNormalizationReport::default();
    let mut attachment_segments = Vec::new();

    let mut obsolete_symbol_uuids = HashSet::new();
    let mut obsolete_wire_uuids = HashSet::new();
    for symbol in sch.symbols.iter() {
        if !symbol_is_target_power_candidate(symbol, net) {
            continue;
        }
        if let Some(wire_uuids) = obsolete_stub_wire_uuids(sch, tree, &mut graph, symbol) {
            obsolete_symbol_uuids.insert(symbol.uuid.clone());
            report
                .obsolete_symbols_removed
                .push(json!({"uuid": symbol.uuid, "reference": symbol.reference().unwrap_or(""), "net": net, "x": symbol.at.x, "y": symbol.at.y}));
            for uuid in wire_uuids {
                obsolete_wire_uuids.insert(uuid.clone());
                report
                    .obsolete_stub_wires_removed
                    .push(json!({"uuid": uuid, "net": net}));
            }
        }
    }
    for uuid in &obsolete_symbol_uuids {
        sch.symbols.remove_by_uuid(uuid);
    }
    for uuid in &obsolete_wire_uuids {
        sch.wires.remove_by_uuid(uuid);
    }

    let mut candidates = sch
        .symbols
        .iter()
        .filter(|symbol| symbol_is_target_power_candidate(symbol, net))
        .map(|symbol| {
            let key = graph.find(crate::tools::sch_connectivity::pt_key(
                symbol.at.x,
                symbol.at.y,
            ));
            ExistingPowerCandidate {
                uuid: symbol.uuid.clone(),
                reference: symbol.reference().unwrap_or("").to_string(),
                lib_id: symbol.lib_id.clone(),
                value: net.to_string(),
                x: symbol.at.x,
                y: symbol.at.y,
                rotation: symbol.at.rotation.unwrap_or(0.0),
                canonical: canonical_power_symbol_instance(symbol, net),
                on_grid: point_on_grid(symbol.at.x, symbol.at.y, grid),
                attachment_key: key,
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| a.uuid.cmp(&b.uuid));

    let mut grouped: BTreeMap<(i64, i64), Vec<ExistingPowerCandidate>> = BTreeMap::new();
    for candidate in candidates {
        grouped
            .entry(candidate.attachment_key)
            .or_default()
            .push(candidate);
    }

    let mut delete_uuids = HashSet::new();
    for mut group in grouped.into_values() {
        group.sort_by(|a, b| {
            b.canonical
                .cmp(&a.canonical)
                .then_with(|| b.on_grid.cmp(&a.on_grid))
                .then_with(|| a.uuid.cmp(&b.uuid))
        });
        let survivor = group[0].clone();
        let duplicates = group.iter().skip(1).cloned().collect::<Vec<_>>();
        for duplicate in duplicates {
            delete_uuids.insert(duplicate.uuid.clone());
            report
                .redundant_duplicates_removed
                .push(duplicate.json("REDUNDANT_DUPLICATE"));
        }

        let needs_geometry = survivor.canonical && !survivor.on_grid;
        if survivor.canonical && survivor.on_grid {
            report
                .canonical_unchanged
                .push(survivor.json("REQUIRED_CANONICAL"));
            continue;
        }

        let (target_x, target_y, normalized_from) = if needs_geometry {
            let (sx, sy) = snap_point(survivor.x, survivor.y, grid);
            attachment_segments.push(Segment {
                x1: survivor.x,
                y1: survivor.y,
                x2: sx,
                y2: sy,
            });
            (sx, sy, Some((survivor.x, survivor.y)))
        } else {
            (survivor.x, survivor.y, None)
        };
        let placed = place_power_symbol_instance(
            sch,
            schematic,
            net,
            target_x,
            target_y,
            survivor.rotation,
        )?;
        delete_uuids.insert(survivor.uuid.clone());
        let mut entry = placed.json();
        entry["replaced_uuid"] = json!(survivor.uuid);
        entry["replaced_reference"] = json!(survivor.reference);
        entry["replaced_lib_id"] = json!(survivor.lib_id);
        if let Some((old_x, old_y)) = normalized_from {
            entry["normalized_from"] = json!({"x": old_x, "y": old_y});
            entry["attachment_wires"] = json!([Segment {
                x1: old_x,
                y1: old_y,
                x2: target_x,
                y2: target_y,
            }
            .json()]);
            report.geometry_normalizations.push(entry);
        } else {
            entry["classification"] = json!("NONCANONICAL_INSTANCE");
            report.noncanonical_replacements.push(entry);
        }
    }

    for uuid in delete_uuids {
        sch.symbols.remove_by_uuid(&uuid);
    }

    Ok((report, attachment_segments))
}

fn place_power_symbol_instance(
    sch: &mut cse::Schematic,
    sch_path: &Path,
    value: &str,
    x: f64,
    y: f64,
    rotation: f64,
) -> Result<PlacedPowerSymbol, CallToolResult> {
    // KiCad resolves the symbol body from lib_id, but the global rail name
    // comes from the placed instance's Value field. Custom rails therefore use
    // a stock power-symbol template and keep the requested rail in Value.
    let lib_id = power_symbol_template_lib_id(value);
    let src = crate::tools::library::KiCadSymbolSource::for_file(sch_path);
    if !cse::library::ensure_lib_symbol(sch, &lib_id, &src) {
        return Err(crate::tools::lib_symbol_not_found_error(&lib_id, &src));
    }

    let pwr_ref = format!("#PWR{:03}", next_pwr_number(sch));
    let mut sym = cse::Symbol::new(lib_id.clone(), x, y);
    sym.at.rotation = Some(rotation);
    sym.unit = 1;
    sym.in_bom = true;
    sym.on_board = true;
    sym.uuid = uuid::Uuid::new_v4().to_string();

    // Property (at …) is absolute sheet coords. Library field anchors keep GND
    // and VCC-family labels positioned the way the stock symbols expect.
    let anchors = cse::library::field_anchors(sch, &lib_id);
    let t = konnect_sexp::geometry::PinTransform {
        comp_x: x,
        comp_y: y,
        rotation_deg: rotation,
        mirror_x: false,
        mirror_y: false,
    };
    let (ref_x, ref_y, ref_rot) =
        crate::tools::field_at(anchors.reference_at, crate::tools::FALLBACK_REFERENCE_AT, t);
    let (val_x, val_y, val_rot) =
        crate::tools::field_at(anchors.value_at, crate::tools::FALLBACK_VALUE_AT, t);
    let positioned = crate::tools::positioned_property;
    let centred = cse::library::FieldJustify::default();
    sym.properties.push(positioned(
        "Reference",
        &pwr_ref,
        ref_x,
        ref_y,
        ref_rot,
        true,
        anchors.reference_justify,
    ));
    sym.properties.push(positioned(
        "Value",
        value,
        val_x,
        val_y,
        val_rot,
        false,
        anchors.value_justify,
    ));
    sym.properties
        .push(positioned("Footprint", "", x, y, 0.0, true, centred));
    sym.properties
        .push(positioned("Datasheet", "", x, y, 0.0, true, centred));

    let root_uuid = crate::tools::ensure_root_uuid(sch);
    sym.set_instance_path(
        &project_name_for(sch_path),
        &format!("/{}", root_uuid),
        &pwr_ref,
        1,
    );

    sch.add_symbol(sym);
    Ok(PlacedPowerSymbol {
        reference: pwr_ref,
        template_lib_id: lib_id,
        value: value.to_string(),
        x,
        y,
        rotation,
    })
}

async fn handle_add_power_symbol(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let power_net = match require_str(args, "power_net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let rotation = opt_f64(args, "rotation").unwrap_or(0.0);

    let mut sch = cse::Schematic::load(&sch_path)?;

    let placed = match place_power_symbol_instance(&mut sch, &sch_path, &power_net, x, y, rotation)
    {
        Ok(placed) => placed,
        Err(error) => return Ok(error),
    };
    sch.overwrite()?;

    // A power pin landing mid-segment on an existing wire needs a junction
    // dot, or KiCad ERC reports it as not connected.
    let junctions_added = crate::tools::add_pin_midwire_junctions(&sch_path, &placed.reference)?;

    Ok(CallToolResult::json(&json!({
        "added_power": power_net,
        "template_lib_id": placed.template_lib_id,
        "reference": placed.reference,
        "x": x, "y": y,
        "junctions_added": junctions_added.iter().map(|(x, y)| json!({"x": x, "y": y})).collect::<Vec<_>>()
    })))
}

async fn handle_add_no_connect(
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

    let mut sch = cse::Schematic::load(&sch_path)?;
    sch.add_no_connect(x, y);
    sch.overwrite()?;
    Ok(CallToolResult::json(
        &json!({ "added_no_connect": { "x": x, "y": y } }),
    ))
}

async fn handle_batch_add_no_connect(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let positions = match args["positions"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'positions' array")),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;
    let mut added: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for (i, p) in positions.iter().enumerate() {
        match (p["x"].as_f64(), p["y"].as_f64()) {
            (Some(x), Some(y)) => {
                sch.add_no_connect(x, y);
                added.push(json!({ "x": x, "y": y }));
            }
            _ => errors.push(format!("Position {i}: needs numeric x and y")),
        }
    }
    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "added_count": added.len(),
        "added": added,
        "errors": errors
    })))
}

async fn handle_delete_no_connect(
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

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let Some((del_start, del_end)) = find_no_connect_block_at(&content, x, y) else {
        return Ok(CallToolResult::error(
            "No-connect not found at that position",
        ));
    };
    let new_content = apply_edits(content, vec![SexpEdit::delete(del_start, del_end)]);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    Ok(CallToolResult::text("No-connect deleted."))
}

/// Byte range of the `(no_connect …)` block whose `(at …)` is `(x, y)`.
///
/// The previous implementation searched for the literal
/// `"(no_connect (at {x} {y})"`. No-connect blocks are never written on one
/// line — this crate's writer takes the multi-line branch for any node with
/// list children, and eeschema does the same with tabs — so that string never
/// matched anything and both delete tools were inert (#114). Same failure
/// class as the wire deletion in #64; this reuses the #69 block machinery the
/// wire path already uses, including its coordinate tolerance.
fn find_no_connect_block_at(content: &str, x: f64, y: f64) -> Option<(usize, usize)> {
    const TOLERANCE: f64 = 1e-6;
    let same = |a: f64, b: f64| (a - b).abs() <= TOLERANCE;

    for start in find_block_starts(content, "no_connect") {
        let Some((block_start, block_end)) = find_balanced_block(content, start) else {
            continue;
        };
        let Ok(node) = parse_sexp(&content[block_start..block_end]) else {
            continue;
        };
        let Some(at) = node.find("at") else { continue };
        let (Some(bx), Some(by)) = (at.get_f64(1), at.get_f64(2)) else {
            continue;
        };
        if same(bx, x) && same(by, y) {
            return find_block_with_leading_whitespace(content, block_start);
        }
    }
    None
}

async fn handle_batch_delete_no_connect(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let positions = match require_array(args, "positions") {
        Ok(a) => a.clone(),
        Err(e) => return Ok(e),
    };

    // One read, collect every range, one write — matching batch_delete_wire.
    // The old loop delegated to the single-item handler and counted `.is_ok()`,
    // but that handler returns `Ok(CallToolResult::error(..))` when nothing
    // matches, so every failure counted as a success and the tool reported
    // deletions it had not made (#114).
    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for pos in &positions {
        let (Some(x), Some(y)) = (pos["x"].as_f64(), pos["y"].as_f64()) else {
            errors.push(format!("Position {pos} needs numeric x and y"));
            continue;
        };
        match find_no_connect_block_at(&content, x, y) {
            Some(range) => ranges.push(range),
            None => errors.push(format!("No no-connect at ({x}, {y})")),
        }
    }
    ranges.sort_unstable();
    ranges.dedup();
    let deleted = ranges.len();

    if deleted == 0 && !positions.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No no-connects deleted: {}",
            errors.join("; ")
        )));
    }

    let edits: Vec<SexpEdit> = ranges
        .into_iter()
        .map(|(s, e)| SexpEdit::delete(s, e))
        .collect();
    let content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &content)?;
    Ok(CallToolResult::json(&json!({
        "deleted": deleted,
        "errors": errors
    })))
}

async fn handle_add_junction(
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

    let mut sch = cse::Schematic::load(&sch_path)?;
    sch.add_junction(x, y);
    sch.overwrite()?;
    Ok(CallToolResult::json(
        &json!({ "added_junction": { "x": x, "y": y } }),
    ))
}

async fn handle_batch_add_junction(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let positions = match require_array(args, "positions") {
        Ok(a) => a.clone(),
        Err(e) => return Ok(e),
    };
    let mut sch = cse::Schematic::load(&sch_path)?;
    for pos in &positions {
        let x = pos["x"].as_f64().unwrap_or(0.0);
        let y = pos["y"].as_f64().unwrap_or(0.0);
        sch.add_junction(x, y);
    }
    sch.overwrite()?;
    Ok(CallToolResult::json(&json!({ "added": positions.len() })))
}

async fn handle_connect_to_net(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let direction = opt_str(args, "direction").unwrap_or("auto");
    let stub_length = opt_f64(args, "stub_length").unwrap_or(2.54);
    let label_type = opt_str(args, "label_type").unwrap_or("net_label");

    let (_, tree) = read_schematic(&sch_path)?;

    // Name the pin, or give its coordinates. Naming it cannot miss the
    // endpoint, and the error says which pin was wrong.
    let (pin_x, pin_y, outward) = match (opt_str(args, "reference"), opt_str(args, "pin_number")) {
        (Some(reference), Some(pin_number)) => {
            let instances = extract_symbol_instances(&tree);
            let lib_syms = tree
                .find("lib_symbols")
                .map(|n| n.find_all("symbol"))
                .unwrap_or_default();
            match resolve_placed_pin(&instances, &lib_syms, reference, pin_number) {
                Ok((pin, t)) => {
                    let (x, y) = pin_endpoint(&pin, t);
                    (x, y, Some(pin_outward_direction(&pin, t)))
                }
                Err(e) => return Ok(CallToolResult::error(e.to_string())),
            }
        }
        (Some(_), None) | (None, Some(_)) => {
            return Ok(CallToolResult::error(
                "'reference' and 'pin_number' must be given together",
            ))
        }
        (None, None) => match (require_f64(args, "pin_x"), require_f64(args, "pin_y")) {
            (Ok(x), Ok(y)) => (x, y, None),
            // Structured, not free text: a missing coordinate is an
            // InvalidArgument like any other, and the reason names the way in
            // that the caller did not take.
            (x, _) => {
                let field = if x.is_err() { "pin_x" } else { "pin_y" };
                return Ok(CallToolResult::error_kind(
                    crate::mcp::error::ToolErrorKind::InvalidArgument {
                        field: field.to_string(),
                        reason: "give either 'reference' + 'pin_number' or 'pin_x' + 'pin_y'"
                            .into(),
                    },
                    format!(
                        "Missing '{field}': give either 'reference' + 'pin_number' \
                         or 'pin_x' + 'pin_y'"
                    ),
                ));
            }
        },
    };

    // Where the stub goes, and how the label at its end must read: text
    // running back over the symbol covers its pin names (pin_label_rotation).
    let dir = match outward {
        Some(d) => crate::tools::stub_direction(direction, Some(d)),
        None => crate::tools::resolve_stub_direction(direction, (pin_x, pin_y), &tree),
    };
    let (label_x, label_y) = (pin_x + dir.dx * stub_length, pin_y + dir.dy * stub_length);
    let label_rot = dir.label_rotation;

    let mut sch = cse::Schematic::load(&sch_path)?;

    // T-junction detection for the wire stub
    let mut existing_wires = cse_wires_to_sexp(&sch);
    existing_wires.push(konnect_sexp::schematic::Wire {
        x1: pin_x,
        y1: pin_y,
        x2: label_x,
        y2: label_y,
        uuid: None,
    });
    let junctions = find_t_junctions(&existing_wires, 0.01);

    // Add wire stub
    sch.add_wire(pin_x, pin_y, label_x, label_y);
    add_missing_junctions(&mut sch, &junctions);
    // Pins the stub passes over mid-segment also need junction dots.
    let pins = crate::tools::all_pin_endpoints(&tree);
    add_missing_junctions(
        &mut sch,
        &pins_mid_segment(&pins, pin_x, pin_y, label_x, label_y),
    );

    // set_rotation, not `at.rotation = …`: a bare rotation leaves `effects`
    // unset, and KiCad then centres the text on the anchor (#43).
    match label_type {
        "global_label" => {
            sch.add_global_label(&net, "input", label_x, label_y);
            let idx = sch.global_labels.len() - 1;
            if let Some(gl) = sch.global_labels.get_mut(idx) {
                gl.set_rotation(label_rot);
            }
        }
        _ => {
            sch.add_label(&net, label_x, label_y)
                .set_rotation(label_rot);
        }
    }

    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "connected": net,
        "direction": dir.name,
        "wire": { "x1": pin_x, "y1": pin_y, "x2": label_x, "y2": label_y },
        "label": { "x": label_x, "y": label_y, "rotation": label_rot }
    })))
}

async fn handle_connect_pins(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let ref1 = match require_str(args, "ref1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin1 = match require_str(args, "pin1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref2 = match require_str(args, "ref2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin2 = match require_str(args, "pin2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    // Parse the schematic tree
    let (content, tree) = read_schematic(&sch_path)?;
    let expected = content.clone();
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // Resolve pin1 board-space endpoint
    let (x1, y1) = resolve_pin_endpoint(&instances, &lib_syms, &ref1, &pin1)?;
    // Resolve pin2 board-space endpoint
    let (x2, y2) = resolve_pin_endpoint(&instances, &lib_syms, &ref2, &pin2)?;

    // Route wire(s) between the two pin endpoints
    let new_content = route_between(content, x1, y1, x2, y2);

    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "connected": {
            "from": { "ref": ref1, "pin": pin1, "x": x1, "y": y1 },
            "to":   { "ref": ref2, "pin": pin2, "x": x2, "y": y2 }
        }
    })))
}

/// Resolve a pin's schematic-space endpoint by reference and pin number.
/// Uses the same pattern as sch_analysis::handle_get_pin_connections.
pub(crate) fn resolve_pin_endpoint(
    instances: &[konnect_sexp::schematic::SymbolInstance],
    lib_syms: &[&konnect_sexp::parser::SexpNode],
    reference: &str,
    pin_number: &str,
) -> anyhow::Result<(f64, f64)> {
    let (pin, t) = resolve_placed_pin(instances, lib_syms, reference, pin_number)?;
    Ok(pin_endpoint(&pin, t))
}

/// The named pin and the transform placing it, for callers that need more than
/// its coordinates — `pin_outward_direction` and `pin_label_rotation` both take
/// this pair, and deriving them here beats searching the sheet for the point we
/// just computed.
pub(crate) fn resolve_placed_pin(
    instances: &[konnect_sexp::schematic::SymbolInstance],
    lib_syms: &[&konnect_sexp::parser::SexpNode],
    reference: &str,
    pin_number: &str,
) -> anyhow::Result<(
    konnect_sexp::schematic::LibPin,
    konnect_sexp::geometry::PinTransform,
)> {
    // A multi-unit part places one instance per unit, all sharing the
    // reference, so any of them may own the pin: an LM2904's power pins live
    // on the unit the schematic draws separately from either amplifier.
    let placed: Vec<_> = instances
        .iter()
        .filter(|i| i.reference == reference)
        .collect();
    let Some(first) = placed.first() else {
        anyhow::bail!("Component '{}' not found", reference);
    };

    let mut searched = Vec::new();
    for inst in &placed {
        // find_lib_symbol, not a lib_id match: an instance carrying a
        // (lib_name …) is a sheet-local derived symbol whose pins can sit
        // elsewhere than the base definition's, or whose base was never
        // embedded at all (#143).
        let Some(lib_sym) = find_lib_symbol(lib_syms, inst) else {
            continue;
        };
        searched.push(inst.unit);
        // Unit-aware (#35): only the unit that owns the pin may answer for it,
        // or the wire lands on a superimposed phantom.
        if let Some(lib_pin) =
            konnect_sexp::schematic::extract_lib_pins_for_unit(lib_sym, inst.unit)
                .into_iter()
                .find(|p| p.number == pin_number)
        {
            return Ok((lib_pin, inst.pin_transform()));
        }
    }

    if searched.is_empty() {
        anyhow::bail!("Library symbol '{}' not found", first.lib_id);
    }
    anyhow::bail!(
        "Pin '{}' not found on '{}' ({} {})",
        pin_number,
        reference,
        if searched.len() == 1 { "unit" } else { "units" },
        searched
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

async fn handle_add_schematic_connection(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let content = route_between(content, x1, y1, x2, y2);

    write_atomic_if_unchanged(&sch_path, &expected, &content)?;
    Ok(CallToolResult::json(&json!({
        "connected": { "from": [x1, y1], "to": [x2, y2] }
    })))
}

#[cfg(test)]
mod unit_aware_wiring_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
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

    /// A schematic with an embedded LM2904-style dual op-amp (unit 1 = pins
    /// 1-3, unit 2 = pins 5-7) placed twice: U1 as unit 1, U2 as unit 2.
    fn dual_opamp_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let pin = |num: &str, x: f64, y: f64, angle: u32| {
            format!(
                "\t\t\t(pin passive line (at {x} {y} {angle}) (length 2.54)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"{num}\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n"
            )
        };
        let lib_sym = format!(
            "\t\t(symbol \"Test:OP2\"\n\t\t\t(symbol \"OP2_1_1\"\n{}{}{}\t\t\t)\n\t\t\t(symbol \"OP2_2_1\"\n{}{}{}\t\t\t)\n\t\t)\n",
            pin("1", -7.62, 2.54, 0),
            pin("2", -7.62, -2.54, 0),
            pin("3", 7.62, 0.0, 180),
            pin("5", -7.62, 2.54, 0),
            pin("6", -7.62, -2.54, 0),
            pin("7", 7.62, 0.0, 180),
        );
        let inst = |reference: &str, unit: u32, x: f64, uuid: &str| {
            format!(
                "\t(symbol\n\t\t(lib_id \"Test:OP2\")\n\t\t(at {x} 80 0)\n\t\t(unit {unit})\n\t\t(uuid \"{uuid}\")\n\t\t(property \"Reference\" \"{reference}\"\n\t\t\t(at {x} 75 0)\n\t\t)\n\t)\n"
            )
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dual.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(uuid \"3af69a4c-1faa-40bd-91dc-c4fc245c4cbd\")\n\t(lib_symbols\n{}\t)\n{}{})\n",
                lib_sym,
                inst("U1", 1, 100.0, "aaaaaaaa-1111-1111-1111-111111111111"),
                inst("U2", 2, 150.0, "bbbbbbbb-2222-2222-2222-222222222222"),
            ),
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn connect_pins_uses_the_instance_unit() {
        let (_d, path) = dual_opamp_schematic();

        // U1 is unit 1: its pins are 1-3. U2 is unit 2: pins 5-7.
        let ok = handle_connect_pins(
            &json!({
                "schematic": path.display().to_string(),
                "ref1": "U1", "pin1": "1",
                "ref2": "U2", "pin2": "5"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(
            !ok.is_error,
            "unit-owned pins must connect: {:?}",
            ok.content
        );

        // Pin 5 belongs to unit 2 — asking for it on the unit-1 instance must
        // fail instead of wiring to a superimposed phantom position (#35).
        let err = handle_connect_pins(
            &json!({
                "schematic": path.display().to_string(),
                "ref1": "U1", "pin1": "5",
                "ref2": "U2", "pin2": "6"
            }),
            &test_ctx(),
        )
        .await;
        let msg = format!("{:?}", err);
        assert!(
            err.is_err() || err.as_ref().is_ok_and(|r| r.is_error),
            "pin 5 on a unit-1 instance must not resolve: {msg}"
        );
        assert!(
            msg.contains("unit 1"),
            "error should name the instance unit: {msg}"
        );
    }

    /// U1 has a single pin at (101.6, 76.2) — on the 1.27 grid so add_wire's
    /// snapping keeps the new wire exactly through it.
    fn single_pin_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pin.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Test:P1\"\n\t\t\t(symbol \"P1_1_1\"\n\t\t\t\t(pin passive line (at 0 0 0) (length 2.54)\n\t\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Test:P1\")\n\t\t(at 101.6 76.2 0)\n\t\t(unit 1)\n\t\t(uuid \"u1\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 101.6 71.12 0)\n\t\t)\n\t)\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n",
        )
        .unwrap();
        (dir, path)
    }

    /// Drawing a wire across an existing pin mid-segment must auto-insert a
    /// junction dot — KiCad connects a mid-wire pin only through a junction.
    #[tokio::test]
    async fn add_wire_over_pin_inserts_junction() {
        let (_d, path) = single_pin_schematic();
        let result = handle_add_wire(
            &json!({
                "schematic": path.display().to_string(),
                "x1": 96.52, "y1": 76.2, "x2": 106.68, "y2": 76.2
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        let after = std::fs::read_to_string(&path).unwrap();
        let tree = konnect_sexp::parse_sexp(&after).unwrap();
        let juncs = konnect_sexp::schematic::extract_junctions(&tree);
        assert!(
            juncs
                .iter()
                .any(|&(x, y)| (x - 101.6).abs() < 0.01 && (y - 76.2).abs() < 0.01),
            "junction expected at the mid-wire pin, got {juncs:?}"
        );
    }

    // ─── One junction per T, however many wires arrive ─────────────────────

    /// An empty sheet: these tests only need somewhere to put wires.
    fn bare_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bare.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols)\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n",
        )
        .unwrap();
        (dir, path)
    }

    /// A rail plus three taps hanging off it: each tap makes one T.
    fn rail_and_taps() -> (Vec<serde_json::Value>, Vec<(f64, f64)>) {
        let rail = json!({ "x1": 101.6, "y1": 101.6, "x2": 127.0, "y2": 101.6 });
        let taps: Vec<f64> = vec![106.68, 111.76, 116.84];
        let wires = std::iter::once(rail)
            .chain(
                taps.iter()
                    .map(|&x| json!({ "x1": x, "y1": 101.6, "x2": x, "y2": 106.68 })),
            )
            .collect();
        (wires, taps.into_iter().map(|x| (x, 101.6)).collect())
    }

    fn junctions_at(path: &std::path::Path, x: f64, y: f64) -> usize {
        let tree = konnect_sexp::parse_sexp(&std::fs::read_to_string(path).unwrap()).unwrap();
        konnect_sexp::schematic::extract_junctions(&tree)
            .iter()
            .filter(|&&(jx, jy)| (jx - x).abs() < 0.01 && (jy - y).abs() < 0.01)
            .count()
    }

    /// `find_t_junctions` reports every T on the sheet, so each call used to
    /// re-emit a dot at every T already there — five wires left five dots
    /// stacked on one point.
    #[tokio::test]
    async fn repeated_add_wire_leaves_one_junction_per_t() {
        let (_d, path) = bare_schematic();
        let (wires, tees) = rail_and_taps();
        for w in &wires {
            let mut args = json!({ "schematic": path.display().to_string() });
            for (k, v) in w.as_object().unwrap() {
                args[k] = v.clone();
            }
            let result = handle_add_wire(&args, &test_ctx()).await.unwrap();
            assert!(!result.is_error, "{:?}", result.content);
        }
        for (x, y) in tees {
            assert_eq!(junctions_at(&path, x, y), 1, "T at ({x}, {y})");
        }
    }

    /// The same in one batch, where the duplication was quadratic.
    #[tokio::test]
    async fn batch_add_wire_leaves_one_junction_per_t() {
        let (_d, path) = bare_schematic();
        let (wires, tees) = rail_and_taps();
        let result = handle_batch_add_wire(
            &json!({ "schematic": path.display().to_string(), "wires": wires }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        for (x, y) in tees {
            assert_eq!(junctions_at(&path, x, y), 1, "T at ({x}, {y})");
        }
    }

    /// Fixture provenance: built with Konnect tools against the stock `Device:R`,
    /// then rewritten by `kicad-cli sch upgrade` so the committed text is
    /// eeschema's own output — it carries `ki_fp_filters`, `embedded_fonts`,
    /// `exclude_from_sim` and full pin geometry that a hand-written sheet does
    /// not (CONTRIBUTING.md). One sheet holds every case at a distinct
    /// coordinate, so each test also proves the pass leaves the others alone.
    ///
    ///   (120.65, 139.7)   R1's pin mid-span on a lone wire, dot earned
    ///   (120.65, 170.18)  a real T of two wires, dot always justified
    ///   (190.5,  140.0)   two wires merely crossing, no dot
    ///   (190.5,  200.66)  R3's pin mid-span but flagged no-connect, no dot
    ///   (260.35, 140.0)   a bus tee with its dot
    const RECONCILE_SCH: &str = include_str!("../../tests/fixtures/junction_reconcile.kicad_sch");

    fn dots(src: &str) -> Vec<(String, String)> {
        regex_lite_junctions(src)
    }

    /// Junction coordinates, as written.
    fn regex_lite_junctions(src: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for chunk in src.split("(junction").skip(1) {
            if let Some(at) = chunk.split("(at ").nth(1) {
                if let Some(inner) = at.split(')').next() {
                    let mut it = inner.split_whitespace();
                    if let (Some(x), Some(y)) = (it.next(), it.next()) {
                        out.push((x.to_string(), y.to_string()));
                    }
                }
            }
        }
        out
    }

    fn has_dot(src: &str, x: &str, y: &str) -> bool {
        dots(src).iter().any(|(a, b)| a == x && b == y)
    }

    /// R1's pin still sits at (120.65, 139.7), so its dot is justified and must
    /// survive being asked about. The pruning case needs the pin to actually
    /// leave, which is a move — covered end-to-end in sch_batch against this
    /// same fixture.
    #[test]
    fn reconcile_keeps_a_dot_its_pin_still_justifies() {
        let (out, added, pruned) =
            reconcile_junctions_at(RECONCILE_SCH.to_string(), &[(120.65, 139.7)]);
        assert_eq!((added, pruned), (0, 0), "the pin is still there");
        assert!(has_dot(&out, "120.65", "139.7"));
    }

    /// A real T — two wires meeting — is never touched.
    #[test]
    fn reconcile_keeps_a_real_tee() {
        let (out, added, pruned) =
            reconcile_junctions_at(RECONCILE_SCH.to_string(), &[(120.65, 170.18)]);
        assert_eq!((added, pruned), (0, 0), "a T stays");
        assert!(has_dot(&out, "120.65", "170.18"));
    }

    /// Two wires that merely CROSS are separate nets in KiCad until a junction
    /// says otherwise. A pin landing there must NOT get one: that would merge
    /// two nets, the silent connectivity change #120 exists to stop.
    #[test]
    fn a_pin_landing_on_a_crossing_does_not_merge_the_nets() {
        let before = dots(RECONCILE_SCH).len();
        let (out, added, pruned) =
            reconcile_junctions_at(RECONCILE_SCH.to_string(), &[(190.5, 140.0)]);
        assert_eq!(
            (added, pruned),
            (0, 0),
            "an ambiguous crossing is left alone"
        );
        assert_eq!(dots(&out).len(), before, "no dot invented at the crossing");
    }

    /// A no-connect flag is the user saying "this pin stays unconnected" — a
    /// move landing it mid-span must not wire it up with a dot.
    #[test]
    fn reconcile_does_not_add_a_dot_over_a_no_connect() {
        let (out, added, pruned) =
            reconcile_junctions_at(RECONCILE_SCH.to_string(), &[(190.5, 200.66)]);
        assert_eq!((added, pruned), (0, 0), "a no-connect forbids the dot");
        assert!(!has_dot(&out, "190.5", "200.66"));
    }

    /// The fixture with R1's justified dot removed — the sheet as it would
    /// look after that dot was lost, so the ADD branch has real work.
    fn fixture_missing_r1s_dot() -> String {
        let src = RECONCILE_SCH;
        let at = src
            .find("(at 120.65 139.7)")
            .expect("fixture carries R1's dot");
        let start = src[..at].rfind("(junction").expect("inside a junction");
        let mut depth = 0usize;
        let mut end = start;
        for (offset, byte) in src[start..].bytes().enumerate() {
            match byte {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + offset + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        let stripped = format!("{}{}", &src[..start], &src[end..]);
        assert!(!has_dot(&stripped, "120.65", "139.7"), "dot removed");
        stripped
    }

    /// The positive half of the reconcile: a pin sitting mid-span on exactly
    /// one wire with no dot gets one, and float noise in the candidate
    /// coordinate never reaches the file.
    #[test]
    fn a_pin_left_mid_wire_gets_its_dot_back() {
        let (out, added, pruned) =
            reconcile_junctions_at(fixture_missing_r1s_dot(), &[(120.65, 139.70000000000002)]);
        assert_eq!((added, pruned), (1, 0), "the missing dot comes back");
        assert!(
            has_dot(&out, "120.65", "139.7"),
            "rounded, not 139.70000000000002"
        );
        assert!(
            !out.contains("139.70000000000002"),
            "no float noise in the file"
        );
    }

    /// Two coincident pin endpoints vacating or landing on one spot hand the
    /// reconciler the same candidate twice. The counts must describe what
    /// happened to the sheet, not how many times we asked — and the sheet
    /// must not gain two identical dots.
    #[test]
    fn duplicate_candidates_produce_one_dot_and_honest_counts() {
        let (out, added, _pruned) = reconcile_junctions_at(
            fixture_missing_r1s_dot(),
            &[(120.65, 139.7), (120.65, 139.7)],
        );
        assert_eq!(added, 1, "one dot added, however many times we were asked");
        let dots_there = dots(&out)
            .iter()
            .filter(|(x, y)| x == "120.65" && y == "139.7")
            .count();
        assert_eq!(dots_there, 1, "exactly one junction block written");
    }

    /// A dot on a bus tee is a BUS junction — it joins bus segments, which no
    /// wire count can see. Bus points are outside this pass's jurisdiction.
    #[test]
    fn reconcile_never_touches_a_dot_on_a_bus() {
        let (out, added, pruned) =
            reconcile_junctions_at(RECONCILE_SCH.to_string(), &[(260.35, 140.0)]);
        assert_eq!(
            (added, pruned),
            (0, 0),
            "bus junctions are not ours to judge"
        );
        assert!(has_dot(&out, "260.35", "140"), "the bus tee keeps its dot");
    }

    /// Regression for #234: a malformed element used to default its missing
    /// coordinate to 0 and land a real wire across the sheet, while the
    /// single-wire tool refused the same omission.
    #[tokio::test]
    async fn batch_add_wire_refuses_a_malformed_element_and_writes_nothing() {
        let (_d, path) = bare_schematic();
        let before = std::fs::read_to_string(&path).unwrap();

        let result = handle_batch_add_wire(
            &json!({
                "schematic": path.display().to_string(),
                "wires": [
                    { "x1": 150.0, "y1": 150.0, "x2": 160.0, "y2": 150.0 },
                    { "x1": 170.0, "y1": 170.0, "y2": 170.0 }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(result.is_error, "{:?}", result.content);
        let msg = format!("{:?}", result.content);
        assert!(
            msg.contains("wires[1].x2"),
            "must name the element and field: {msg}"
        );
        // The valid first element must not have been written either — the
        // batch fails atomically.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    // ─── connect_to_net stub orientation ───────────────────────────────────

    async fn connect_to_net(path: &std::path::Path, args: serde_json::Value) -> String {
        let mut full = json!({ "schematic": path.display().to_string() });
        for (k, v) in args.as_object().unwrap() {
            full[k] = v.clone();
        }
        let result = handle_connect_to_net(&full, &test_ctx()).await.unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        std::fs::read_to_string(path).unwrap()
    }

    /// U1's pins 1 and 2 face west, so the stub must leave westward and its
    /// label read right-to-left, clear of the body.
    #[tokio::test]
    async fn auto_routes_the_stub_away_from_the_symbol_body() {
        let (_d, path) = dual_opamp_schematic();
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "1", "net": "IN" }),
        )
        .await;
        // Pin 1 tip is (92.38, 77.46); the stub runs 2.54 mm further west.
        assert!(after.contains("(at 89.84 77.46 180)"), "{after}");
        // An east-facing pin on the same part goes the other way.
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "3", "net": "OUT" }),
        )
        .await;
        assert!(after.contains("(at 110.16 80 0)"), "{after}");
    }

    /// Labels here used to carry no `(effects)` at all, so KiCad centred the
    /// text on the anchor — the defect #43 fixed for add_schematic_net_label.
    #[tokio::test]
    async fn the_label_carries_a_justify_matching_its_rotation() {
        let (_d, path) = dual_opamp_schematic();
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "1", "net": "IN" }),
        )
        .await;
        assert!(
            after.contains("(justify right"),
            "a 180° label must be right-justified: {after}"
        );
    }

    /// An explicit direction still wins over the derived one.
    #[tokio::test]
    async fn an_explicit_direction_overrides_the_derived_one() {
        let (_d, path) = dual_opamp_schematic();
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "1", "net": "IN",
                    "direction": "right" }),
        )
        .await;
        assert!(after.contains("(at 94.92 77.46 0)"), "{after}");
    }

    /// A vertical stub keeps its text horizontal: of 2562 wire-anchored labels
    /// in the KiCad demos, only ~1% are rotated 90 or 270.
    #[tokio::test]
    async fn a_vertical_stub_keeps_its_label_horizontal() {
        let (_d, path) = dual_opamp_schematic();
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "1", "net": "IN",
                    "direction": "down" }),
        )
        .await;
        assert!(after.contains("(at 92.38 80 0)"), "{after}");
    }

    /// Coordinates still work, and a bare point falls back to the old default.
    #[tokio::test]
    async fn coordinates_still_work_and_a_bare_point_goes_right() {
        let (_d, path) = dual_opamp_schematic();
        let after = connect_to_net(
            &path,
            json!({ "pin_x": 50.0, "pin_y": 50.0, "net": "FREE" }),
        )
        .await;
        assert!(after.contains("(at 52.54 50 0)"), "{after}");
    }

    /// [`single_pin_schematic`] with a second part butted onto U1's pin: both
    /// tips sit at (101.6, 76.2) and face opposite ways.
    fn butted_pins_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, path) = single_pin_schematic();
        let content = std::fs::read_to_string(&path).unwrap();
        let u2 = "\t(symbol\n\t\t(lib_id \"Test:P1\")\n\t\t(at 101.6 76.2 180)\n\t\t(unit 1)\n\t\t(uuid \"u2\")\n\t\t(property \"Reference\" \"U2\"\n\t\t\t(at 101.6 71.12 0)\n\t\t)\n\t)\n";
        let at = content.find("\t(sheet_instances").unwrap();
        std::fs::write(&path, format!("{}{u2}{}", &content[..at], &content[at..])).unwrap();
        (dir, path)
    }

    /// Naming a pin carries its own direction. Deriving one from a coordinate
    /// cannot: two pins share this point and disagree about which way is out,
    /// so that path has to fall back to "right".
    #[tokio::test]
    async fn a_named_pin_outranks_the_coordinate_lookup_where_pins_stack() {
        let (_d, path) = butted_pins_schematic();
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "1", "net": "SHARED" }),
        )
        .await;
        assert!(after.contains("(at 99.06 76.2 180)"), "{after}");

        let (_d, path) = butted_pins_schematic();
        let after = connect_to_net(
            &path,
            json!({ "pin_x": 101.6, "pin_y": 76.2, "net": "SHARED" }),
        )
        .await;
        assert!(after.contains("(at 104.14 76.2 0)"), "{after}");
    }

    /// [`dual_opamp_schematic`] with both units placed under one reference.
    /// KiCad gives every unit its own `(symbol …)` node sharing the Reference
    /// property — 26 of the KiCad 10 demo schematics do this.
    fn multi_unit_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, path) = dual_opamp_schematic();
        let content = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"U2\"", "\"U1\"");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    /// Pin 5 belongs to unit 2, so resolving it means searching every unit
    /// placed under the reference. Stopping at the first would make an
    /// op-amp's power pins unreachable by name.
    #[tokio::test]
    async fn a_pin_on_another_unit_of_the_same_reference_resolves() {
        let (_d, path) = multi_unit_schematic();
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "5", "net": "PWR" }),
        )
        .await;
        // Unit 2 sits at x=150, putting pin 5's tip at (142.38, 77.46).
        assert!(after.contains("(at 139.84 77.46 180)"), "{after}");
    }

    /// A pin on no placed unit still fails, and the error names every unit
    /// that was searched rather than only the first.
    #[tokio::test]
    async fn a_pin_on_no_placed_unit_names_the_units_searched() {
        let (_d, path) = multi_unit_schematic();
        let result = handle_connect_to_net(
            &json!({ "schematic": path.display().to_string(),
                     "reference": "U1", "pin_number": "99", "net": "NOPE" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "{:?}", result.content);
        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("units 1, 2"), "{text}");
    }

    #[tokio::test]
    async fn naming_only_half_a_pin_is_an_error() {
        let (_d, path) = dual_opamp_schematic();
        let result = handle_connect_to_net(
            &json!({ "schematic": path.display().to_string(),
                     "reference": "U1", "net": "IN" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "{:?}", result.content);
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
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

    fn sch_with(labels: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("labels.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"3af69a4c-1faa-40bd-91dc-c4fc245c4cbd\")\n  (paper \"A4\")\n  (lib_symbols\n  )\n{labels}\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    async fn delete(path: &std::path::Path, net: &str, x: f64, y: f64) -> CallToolResult {
        handle_delete_net_label(
            &json!({ "schematic": path.display().to_string(), "net": net, "x": x, "y": y }),
            &test_ctx(),
        )
        .await
        .unwrap()
    }

    const TWO_PLAIN: &str = "  (label \"VCC\"\n    (at 100 100 0)\n    (uuid \"11111111-1111-1111-1111-111111111111\")\n  )\n  (label \"VCC\"\n    (at 200 100 0)\n    (uuid \"22222222-2222-2222-2222-222222222222\")\n  )";

    #[tokio::test]
    async fn deletes_the_plain_label_the_add_tool_writes() {
        let (_d, path) = sch_with(TWO_PLAIN);
        let result = delete(&path, "VCC", 200.0, 100.0).await;
        assert!(!result.is_error, "plain (label) blocks must be deletable");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("(at 100 100 0)"),
            "the label at (100,100) must survive"
        );
        assert!(
            !after.contains("(at 200 100 0)"),
            "the targeted label at (200,100) must be gone"
        );
    }

    #[tokio::test]
    async fn wrong_coordinates_delete_nothing_and_report_the_real_positions() {
        let (_d, path) = sch_with(TWO_PLAIN);
        let before = std::fs::read_to_string(&path).unwrap();

        let result = delete(&path, "VCC", 300.0, 300.0).await;
        assert!(result.is_error, "a miss must not fall back to nearest-wins");

        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected a text result");
        };
        assert!(
            text.contains("100") && text.contains("200"),
            "error should list the actual label positions: {text}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "file must be untouched when nothing matched"
        );
    }

    #[tokio::test]
    async fn same_name_label_of_another_kind_elsewhere_is_not_collateral() {
        // The old backwards-scan could walk from any occurrence of the quoted
        // name to an unrelated block and delete that instead.
        let (_d, path) = sch_with(
            "  (global_label \"VBUS\"\n    (shape input)\n    (at 50 50 0)\n    (uuid \"33333333-3333-3333-3333-333333333333\")\n  )\n  (label \"VBUS\"\n    (at 150 150 0)\n    (uuid \"44444444-4444-4444-4444-444444444444\")\n  )",
        );

        let result = delete(&path, "VBUS", 150.0, 150.0).await;
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("(global_label \"VBUS\""),
            "the global label at a different position must survive"
        );
        assert!(!after.contains("(at 150 150 0)"));
    }

    #[tokio::test]
    async fn global_and_hierarchical_labels_are_deletable_by_exact_position() {
        for (kind, block) in [
            (
                "global_label",
                "  (global_label \"NET\"\n    (shape input)\n    (at 10 20 0)\n    (uuid \"55555555-5555-5555-5555-555555555555\")\n  )",
            ),
            (
                "hierarchical_label",
                "  (hierarchical_label \"NET\"\n    (shape input)\n    (at 10 20 0)\n    (uuid \"66666666-6666-6666-6666-666666666666\")\n  )",
            ),
        ] {
            let (_d, path) = sch_with(block);
            let result = delete(&path, "NET", 10.0, 20.0).await;
            assert!(!result.is_error, "{kind} should be deletable");
            assert!(!std::fs::read_to_string(&path).unwrap().contains(kind));
        }
    }

    #[tokio::test]
    async fn a_net_name_appearing_in_a_property_does_not_confuse_the_match() {
        // "VCC" also occurs as a symbol property value; only the real label
        // block at the requested position may be deleted.
        let (_d, path) = sch_with(
            "  (symbol\n    (lib_id \"Device:R\")\n    (at 60 60 0)\n    (property \"Value\" \"VCC\"\n      (at 60 62 0)\n    )\n  )\n  (label \"VCC\"\n    (at 100 100 0)\n    (uuid \"77777777-7777-7777-7777-777777777777\")\n  )",
        );

        let result = delete(&path, "VCC", 100.0, 100.0).await;
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("(property \"Value\" \"VCC\""),
            "the symbol property must be untouched"
        );
        assert!(!after.contains("(label \"VCC\""));
    }

    #[tokio::test]
    async fn unknown_net_name_is_an_error() {
        let (_d, path) = sch_with(TWO_PLAIN);
        let result = delete(&path, "NOPE", 100.0, 100.0).await;
        assert!(result.is_error);
    }

    // ─── justify / rotation ────────────────────────────────────────────────

    async fn rotate(path: &std::path::Path, net: &str, x: f64, y: f64, rot: f64) -> CallToolResult {
        handle_rotate_label(
            &json!({ "schematic": path.display().to_string(), "net": net,
                     "x": x, "y": y, "rotation": rot }),
            &test_ctx(),
        )
        .await
        .unwrap()
    }

    fn justify_of(body: &str, net: &str) -> String {
        let start = body.find(&format!("\"{net}\"")).expect("label present");
        let block = &body[start..];
        let end = block.find("(uuid").unwrap_or(block.len());
        match block[..end].find("(justify ") {
            Some(j) => {
                let rest = &block[..end][j + "(justify ".len()..];
                rest[..rest.find(')').unwrap()].trim().to_string()
            }
            None => "<none>".to_string(),
        }
    }

    #[tokio::test]
    async fn rotate_creates_the_effects_block_when_absent() {
        // The shape add_schematic_net_label used to write: no (effects) at all.
        let (_d, path) = sch_with(
            "  (global_label \"EN\"\n    (shape input)\n    (at 10 20 0)\n    (uuid \"88888888-8888-8888-8888-888888888888\")\n  )",
        );
        let result = rotate(&path, "EN", 10.0, 20.0, 180.0).await;
        assert!(!result.is_error);

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("(at 10 20 180)"), "anchor must rotate");
        assert_eq!(
            justify_of(&body, "EN"),
            "right",
            "a 180° label must be right-justified or its text renders backwards"
        );
    }

    #[tokio::test]
    async fn rotate_replaces_an_existing_justify_and_keeps_the_font() {
        let (_d, path) = sch_with(
            "  (global_label \"EN\"\n    (shape input)\n    (at 10 20 0)\n    (effects (font (size 2.54 2.54)) (justify left))\n    (uuid \"99999999-9999-9999-9999-999999999999\")\n  )",
        );
        assert!(!rotate(&path, "EN", 10.0, 20.0, 180.0).await.is_error);

        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(justify_of(&body, "EN"), "right");
        assert!(
            body.contains("(size 2.54 2.54)"),
            "the file's own font must be preserved"
        );
        assert_eq!(body.matches("(justify").count(), 1, "no duplicate justify");
    }

    #[tokio::test]
    async fn rotate_adds_justify_to_an_effects_block_that_lacks_one() {
        let (_d, path) = sch_with(
            "  (global_label \"EN\"\n    (shape input)\n    (at 10 20 0)\n    (effects (font (size 1.27 1.27)))\n    (uuid \"aaaaaaaa-9999-9999-9999-999999999999\")\n  )",
        );
        assert!(!rotate(&path, "EN", 10.0, 20.0, 270.0).await.is_error);
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(justify_of(&body, "EN"), "right", "270° is right-justified");
        assert_eq!(body.matches("(effects").count(), 1);
    }

    #[tokio::test]
    async fn rotating_back_to_zero_restores_left() {
        let (_d, path) = sch_with(
            "  (global_label \"EN\"\n    (shape input)\n    (at 10 20 180)\n    (effects (font (size 1.27 1.27)) (justify right))\n    (uuid \"bbbbbbbb-9999-9999-9999-999999999999\")\n  )",
        );
        assert!(!rotate(&path, "EN", 10.0, 20.0, 0.0).await.is_error);
        assert_eq!(
            justify_of(&std::fs::read_to_string(&path).unwrap(), "EN"),
            "left"
        );
    }

    #[tokio::test]
    async fn plain_labels_keep_the_bottom_alignment_eeschema_writes() {
        let (_d, path) = sch_with(
            "  (label \"MID\"\n    (at 10 20 0)\n    (uuid \"cccccccc-9999-9999-9999-999999999999\")\n  )",
        );
        assert!(!rotate(&path, "MID", 10.0, 20.0, 180.0).await.is_error);
        assert_eq!(
            justify_of(&std::fs::read_to_string(&path).unwrap(), "MID"),
            "right bottom"
        );
    }

    #[tokio::test]
    async fn rotate_reports_real_positions_when_coordinates_miss() {
        let (_d, path) = sch_with(TWO_PLAIN);
        let result = rotate(&path, "VCC", 555.0, 555.0, 180.0).await;
        assert!(result.is_error, "must not rotate the nearest label instead");
    }

    // ─── move by offset ────────────────────────────────────────────────────

    #[tokio::test]
    async fn move_labels_by_offset_actually_moves_every_matching_label() {
        let (_d, path) = sch_with(TWO_PLAIN);
        let result = handle_move_labels_by_offset(
            &json!({ "schematic": path.display().to_string(), "net": "VCC", "dx": 2.54, "dy": -1.27 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("(at 102.54 98.73 0)"),
            "first label moved: {after}"
        );
        assert!(
            after.contains("(at 202.54 98.73 0)"),
            "second label moved: {after}"
        );
    }

    #[tokio::test]
    async fn move_labels_by_offset_errors_on_unknown_net() {
        let (_d, path) = sch_with(TWO_PLAIN);
        let before = std::fs::read_to_string(&path).unwrap();
        let result = handle_move_labels_by_offset(
            &json!({ "schematic": path.display().to_string(), "net": "NOPE", "dx": 1.0, "dy": 1.0 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "zero matches must not report success");
        assert_eq!(before, std::fs::read_to_string(&path).unwrap());
    }
}

#[cfg(test)]
mod wire_delete_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    const WIRE_1: &str = "11111111-1111-1111-1111-111111111111";
    const WIRE_2: &str = "22222222-2222-2222-2222-222222222222";

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

    fn tab_indented_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire-delete.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(generator_version \"10.0\")\n\t(uuid \"00000000-0000-0000-0000-000000000001\")\n\t(paper \"A4\")\n\t(wire\n\t\t(pts\n\t\t\t(xy 50.8 50.8) (xy 60.96 50.8)\n\t\t)\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{WIRE_1}\")\n\t)\n\t(wire\n\t\t(pts\n\t\t\t(xy 50.8 60.96) (xy 60.96 60.96)\n\t\t)\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{WIRE_2}\")\n\t)\n\t(sheet_instances\n\t\t(path \"/\" (page \"1\"))\n\t)\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn delete_wire_preserves_tab_indented_schematic_and_neighbors() {
        let (_dir, path) = tab_indented_schematic();
        let result = handle_delete_wire(
            &json!({ "schematic": path.display().to_string(), "uuid": WIRE_1 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains(WIRE_1));
        assert!(after.contains(WIRE_2));
        assert!(after.contains("(sheet_instances"));
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn delete_wire_matches_reversed_endpoint_coordinates() {
        let (_dir, path) = tab_indented_schematic();
        let result = handle_delete_wire(
            &json!({
                "schematic": path.display().to_string(),
                "x1": 60.96,
                "y1": 50.8,
                "x2": 50.8,
                "y2": 50.8
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains(WIRE_1));
        assert!(after.contains(WIRE_2));
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn batch_delete_wire_handles_tabs_and_duplicate_requests() {
        let (_dir, path) = tab_indented_schematic();
        let result = handle_batch_delete_wire(
            &json!({
                "schematic": path.display().to_string(),
                "uuids": [WIRE_1, WIRE_1, WIRE_2]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains(WIRE_1));
        assert!(!after.contains(WIRE_2));
        assert!(after.contains("(sheet_instances"));
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn batch_delete_wire_fails_closed_when_nothing_matches() {
        let (_dir, path) = tab_indented_schematic();
        let before = std::fs::read_to_string(&path).unwrap();
        let result = handle_batch_delete_wire(
            &json!({
                "schematic": path.display().to_string(),
                "uuids": ["ffffffff-ffff-ffff-ffff-ffffffffffff"]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    // ─── Deleting a wire takes its junction dots with it ───────────────────

    /// The same two wires the fixtures above use, named for their role here.
    const RAIL: &str = WIRE_1;
    const TAP: &str = WIRE_2;

    /// A rail with one tap onto it, the dot at the T, and a second dot far
    /// away that no delete below touches.
    fn t_junction_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junction-prune.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols)\n\t(wire\n\t\t(pts (xy 50.8 50.8) (xy 76.2 50.8))\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{RAIL}\")\n\t)\n\t(wire\n\t\t(pts (xy 63.5 50.8) (xy 63.5 63.5))\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{TAP}\")\n\t)\n\t(junction (at 63.5 50.8) (diameter 0) (color 0 0 0 0) (uuid \"aaaa0000-0000-0000-0000-000000000001\"))\n\t(junction (at 25.4 25.4) (diameter 0) (color 0 0 0 0) (uuid \"aaaa0000-0000-0000-0000-000000000002\"))\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    fn junctions_in(path: &std::path::Path) -> Vec<(f64, f64)> {
        let content = std::fs::read_to_string(path).unwrap();
        let tree = konnect_sexp::parse_sexp(&content).unwrap();
        konnect_sexp::schematic::extract_junctions(&tree)
    }

    #[tokio::test]
    async fn deleting_every_wire_through_a_junction_removes_the_dot() {
        let (_dir, path) = t_junction_schematic();
        let result = handle_batch_delete_wire(
            &json!({ "schematic": path.display().to_string(), "uuids": [RAIL, TAP] }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        // The far dot is on no deleted wire, so it stays whatever justifies it.
        assert_eq!(junctions_in(&path), vec![(25.4, 25.4)]);
    }

    #[tokio::test]
    async fn deleting_the_tap_removes_the_dot_left_on_the_rail() {
        let (_dir, path) = t_junction_schematic();
        let result = handle_delete_wire(
            &json!({ "schematic": path.display().to_string(), "uuid": TAP }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains(RAIL), "the rail must survive: {after}");
        assert_eq!(junctions_in(&path), vec![(25.4, 25.4)]);
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn a_dot_on_a_mid_segment_pin_survives_losing_its_second_wire() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pin-junction.kicad_sch");
        // U1's pin tip is at (101.6, 76.2): a rail straight through it, a stub
        // arriving from below, and the dot the T needs.
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Test:P1\"\n\t\t\t(symbol \"P1_1_1\"\n\t\t\t\t(pin passive line (at 0 0 0) (length 2.54)\n\t\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Test:P1\")\n\t\t(at 101.6 76.2 0)\n\t\t(unit 1)\n\t\t(uuid \"u1\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 101.6 71.12 0)\n\t\t)\n\t)\n\t(wire\n\t\t(pts (xy 96.52 76.2) (xy 106.68 76.2))\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{RAIL}\")\n\t)\n\t(wire\n\t\t(pts (xy 101.6 76.2) (xy 101.6 68.58))\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{TAP}\")\n\t)\n\t(junction (at 101.6 76.2) (diameter 0) (color 0 0 0 0) (uuid \"aaaa0000-0000-0000-0000-000000000003\"))\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();

        let result = handle_delete_wire(
            &json!({ "schematic": path.display().to_string(), "uuid": TAP }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        // One wire left through the dot, but the pin lands mid-segment there —
        // that is the connection, and pruning it would silently break the net.
        assert_eq!(junctions_in(&path), vec![(101.6, 76.2)]);
    }

    #[tokio::test]
    async fn split_wire_keeps_the_dots_on_the_wire_it_replaces() {
        let (_dir, path) = t_junction_schematic();
        let result = handle_split_wire_at_point(
            &json!({ "schematic": path.display().to_string(), "x": 68.58, "y": 50.8 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let mut juncs = junctions_in(&path);
        juncs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(juncs, vec![(25.4, 25.4), (63.5, 50.8), (68.58, 50.8)]);
    }

    #[tokio::test]
    async fn split_wire_without_uuid_deletes_by_complete_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire-split.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(wire\n\t\t(pts (xy 0 0) (xy 10 0))\n\t\t(stroke (width 0) (type default))\n\t)\n)\n",
        )
        .unwrap();

        let result = handle_split_wire_at_point(
            &json!({
                "schematic": path.display().to_string(),
                "x": 5.0,
                "y": 0.0
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        let parsed = konnect_sexp::parse_sexp(&after).unwrap();
        let wires = extract_wires(&parsed);
        assert_eq!(wires.len(), 2);
        assert!(after.contains("(junction"));
        assert!(!wires.iter().any(|wire| wire.x1 == 0.0 && wire.x2 == 10.0));
    }
}

#[cfg(test)]
mod power_symbol_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
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

    #[tokio::test]
    async fn add_power_symbol_places_hidden_reference_near_the_symbol() {
        // Pre-seed lib_symbols so ensure_lib_symbol succeeds without a KiCad install.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("power.kicad_sch");
        std::fs::write(
            &path,
            // Anchors copied from KiCad 10's power.kicad_sym: GND points down,
            // so both fields anchor below the origin in Y-up library space.
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"power:GND\"\n      (property \"Reference\" \"#PWR\" (at 0 -6.35 0))\n      (property \"Value\" \"GND\" (at 0 -3.81 0))\n    )\n  )\n)\n",
        )
        .unwrap();

        let result = handle_add_power_symbol(
            &json!({
                "schematic": path.display().to_string(),
                "power_net": "GND",
                "x": 100.0,
                "y": 80.0
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch
            .symbols
            .iter()
            .find(|s| s.reference() == Some("#PWR001"))
            .expect("power symbol instance");
        let ref_prop = sym
            .properties
            .iter()
            .find(|p| p.name == "Reference")
            .unwrap();
        let ref_sexp = cse::sexp::writer::write(&ref_prop.to_sexp());
        assert!(
            ref_sexp.contains("(at 100") && ref_sexp.contains("86.35"),
            "Reference must sit at the library's anchor near the symbol, not \
             sheet origin: {ref_sexp}"
        );
        let hide_at = ref_sexp
            .find("(hide yes)")
            .expect("KiCad 10 property-level hide");
        let effects_at = ref_sexp.find("(effects").expect("effects");
        assert!(
            hide_at < effects_at,
            "hide must be a property sibling before effects (not inside effects): {ref_sexp}"
        );
        let val_prop = sym.properties.iter().find(|p| p.name == "Value").unwrap();
        let val_sexp = cse::sexp::writer::write(&val_prop.to_sexp());
        assert!(
            val_sexp.contains("(at 100") && val_sexp.contains("83.81"),
            "Value must sit near the symbol: {val_sexp}"
        );
        assert!(
            !val_sexp.contains("hide"),
            "Value must stay visible on power symbols: {val_sexp}"
        );
        assert!(
            !after.contains("(property \"Reference\" \"#PWR001\")\n"),
            "must not write a bare Reference with no (at)"
        );
    }

    /// An up-pointing rail anchors its Value *above* the graphic, where a
    /// fixed +3.81 offset used to put it below (#101). VCC's library anchor is
    /// (0, +3.556) in Y-up space, so the sheet coordinate is y − 3.556.
    #[tokio::test]
    async fn up_pointing_power_symbol_puts_value_above_the_graphic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vcc.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"power:VCC\"\n      (property \"Reference\" \"#PWR\" (at 0 -3.81 0))\n      (property \"Value\" \"VCC\" (at 0 3.556 0))\n    )\n  )\n)\n",
        )
        .unwrap();

        let result = handle_add_power_symbol(
            &json!({
                "schematic": path.display().to_string(),
                "power_net": "VCC",
                "x": 100.0,
                "y": 80.0
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch
            .symbols
            .iter()
            .find(|s| s.reference() == Some("#PWR001"))
            .expect("power symbol instance");
        let val_prop = sym.properties.iter().find(|p| p.name == "Value").unwrap();
        let val_sexp = cse::sexp::writer::write(&val_prop.to_sexp());
        assert!(
            val_sexp.contains("76.444"),
            "VCC's Value belongs above the symbol at y-3.556, not below: {val_sexp}"
        );
    }

    /// Numbering by count re-issued a designator that was still on the sheet:
    /// delete `#PWR002` of three and the next add produced a second `#PWR003`.
    #[tokio::test]
    async fn add_power_symbol_fills_a_freed_number_instead_of_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gnd.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"power:GND\"\n      (property \"Reference\" \"#PWR\" (at 0 -6.35 0))\n      (property \"Value\" \"GND\" (at 0 -3.81 0))\n    )\n  )\n)\n",
        )
        .unwrap();

        let add = |x: f64| {
            let args = json!({
                "schematic": path.display().to_string(),
                "power_net": "GND",
                "x": x,
                "y": 80.0
            });
            async move {
                let result = handle_add_power_symbol(&args, &test_ctx()).await.unwrap();
                assert!(!result.is_error, "{result:?}");
            }
        };
        add(100.0).await;
        add(110.0).await;
        add(120.0).await;

        let mut sch = cse::Schematic::load(&path).unwrap();
        sch.symbols.retain(|s| s.reference() != Some("#PWR002"));
        sch.overwrite().unwrap();

        add(130.0).await;

        let sch = cse::Schematic::load(&path).unwrap();
        let mut refs: Vec<&str> = sch.symbols.iter().filter_map(|s| s.reference()).collect();
        refs.sort_unstable();
        assert_eq!(
            refs,
            ["#PWR001", "#PWR002", "#PWR003"],
            "the freed number belongs to the new symbol, and nothing may repeat"
        );
    }
}

#[cfg(test)]
mod no_connect_delete_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
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

    /// Tab-indented, multi-line no-connects — the shape eeschema and this
    /// crate's own writer both produce. The old literal-string search looked
    /// for `(no_connect (at X Y)` on one line, which no real file contains.
    fn schematic_with_two_no_connects() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nc.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch
	(version 20250610)
	(generator \"eeschema\")
	(uuid \"root\")
	(paper \"A4\")
	(no_connect
		(at 127 63.5)
		(uuid \"nc-1\")
	)
	(no_connect
		(at 140 70)
		(uuid \"nc-2\")
	)
)
",
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn delete_no_connect_removes_a_multiline_block() {
        let (_d, path) = schematic_with_two_no_connects();
        let result = handle_delete_no_connect(
            &json!({ "schematic": path.display().to_string(), "x": 127.0, "y": 63.5 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            !after.contains("nc-1"),
            "the targeted no-connect is still on disk: {after}"
        );
        assert!(after.contains("nc-2"), "deleted the wrong block: {after}");
        assert!(
            konnect_sexp::parse_sexp(&after).is_ok(),
            "file no longer parses: {after}"
        );
    }

    #[tokio::test]
    async fn deleting_a_missing_no_connect_reports_an_error() {
        let (_d, path) = schematic_with_two_no_connects();
        let before = std::fs::read_to_string(&path).unwrap();
        let result = handle_delete_no_connect(
            &json!({ "schematic": path.display().to_string(), "x": 999.0, "y": 999.0 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "a miss must not report success");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a failed delete must leave the file byte-identical"
        );
    }

    /// The batch variant used to count `.is_ok()` on a handler that returns
    /// `Ok(CallToolResult::error(..))` for a miss, so it reported a deletion
    /// for every position whether or not anything was removed.
    #[tokio::test]
    async fn batch_delete_counts_only_what_it_removed() {
        let (_d, path) = schematic_with_two_no_connects();
        let result = handle_batch_delete_no_connect(
            &json!({
                "schematic": path.display().to_string(),
                "positions": [ { "x": 127.0, "y": 63.5 }, { "x": 999.0, "y": 999.0 } ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["deleted"], 1, "only one position exists: {body}");
        assert_eq!(
            body["errors"].as_array().map(|e| e.len()),
            Some(1),
            "the missing position must be reported: {body}"
        );

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("nc-1"));
        assert!(after.contains("nc-2"));
    }
}

#[cfg(test)]
mod batch_no_connect_tests {
    use super::tools;
    use crate::tools::ToolContext;
    use serde_json::json;
    use std::io::Write;
    use std::sync::Arc;

    const SCH: &str = "(kicad_sch\n  (version 20260306)\n  (generator \"eeschema\")\n  (uuid \"root\")\n  (paper \"A4\")\n  (lib_symbols)\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n";

    async fn run(positions: serde_json::Value) -> (String, serde_json::Value) {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();
        let def = tools()
            .into_iter()
            .find(|t| t.name == "batch_add_no_connect")
            .unwrap();
        let cfg = crate::tools::ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
            dispatcher_tools: false,
        };
        let ctx = Arc::new(ToolContext::new(
            cfg,
            Arc::new(crate::router::ToolRouter::new()),
        ));
        let args = json!({ "schematic": f.path().to_str().unwrap(), "positions": positions });
        let res = (def.handler)(&args, ctx).await.unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &res.content[0] else {
            panic!("expected text content")
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        (std::fs::read_to_string(f.path()).unwrap(), body)
    }

    #[tokio::test]
    async fn writes_every_flag_in_one_pass() {
        let (out, body) = run(json!([{"x": 10.0, "y": 20.0}, {"x": 30.0, "y": 40.0}])).await;
        assert_eq!(body["added_count"], 2);
        assert_eq!(out.matches("(no_connect").count(), 2);
    }

    /// A malformed entry must not abort the rest — the useful failure mode when
    /// flagging twenty pins is "nineteen landed and one is reported".
    #[tokio::test]
    async fn a_bad_entry_is_reported_without_losing_the_others() {
        let (out, body) =
            run(json!([{"x": 10.0, "y": 20.0}, {"x": "nope"}, {"x": 1.0, "y": 2.0}])).await;
        assert_eq!(body["added_count"], 2);
        assert_eq!(body["errors"].as_array().unwrap().len(), 1);
        assert_eq!(out.matches("(no_connect").count(), 2);
    }

    #[tokio::test]
    async fn empty_list_is_a_no_op() {
        let (out, body) = run(json!([])).await;
        assert_eq!(body["added_count"], 0);
        assert_eq!(out.matches("(no_connect").count(), 0);
    }
}

#[cfg(test)]
mod materialize_net_tests {
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

    fn result_text(result: &CallToolResult) -> String {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        }
    }

    fn result_json(result: &CallToolResult) -> serde_json::Value {
        serde_json::from_str(&result_text(result)).unwrap()
    }

    fn ltc_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ltc.kicad_sch");
        std::fs::write(
            &path,
            r#"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "root")
  (paper "A4")
  (lib_symbols
    (symbol "Test:R"
      (symbol "R_1_1"
        (pin passive line (at -5 0 180) (length 0) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
        (pin passive line (at 5 0 0) (length 0) (name "~" (effects (font (size 1.27 1.27)))) (number "2" (effects (font (size 1.27 1.27)))))
      )
    )
    (symbol "Test:U"
      (symbol "U_1_1"
        (pin input line (at 0 0 180) (length 0) (name "SHDN" (effects (font (size 1.27 1.27)))) (number "6" (effects (font (size 1.27 1.27)))))
      )
    )
  )
  (label "AUX_5V" (at 45 50 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "label-aux"))
  (label "LTC_SHDN" (at 55 50 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "label-shdn-r"))
  (label "LTC_SHDN" (at 90 50 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "label-shdn-u"))
  (symbol
    (lib_id "Test:R")
    (at 50 50 0)
    (unit 1)
    (property "Reference" "R16" (at 50 48 0))
    (property "Value" "10k" (at 50 52 0))
    (uuid "r16")
  )
  (symbol
    (lib_id "Test:U")
    (at 90 50 0)
    (unit 1)
    (property "Reference" "U2" (at 90 48 0))
    (property "Value" "LTC" (at 90 52 0))
    (uuid "u2")
  )
)"#,
        )
        .unwrap();
        (dir, path)
    }

    fn one_pin_net_fixture(
        name: &str,
        labels_and_symbols: &str,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{name}.kicad_sch"));
        std::fs::write(
            &path,
            format!(
                r#"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "root")
  (paper "A4")
  (lib_symbols
    (symbol "Test:P"
      (symbol "P_1_1"
        (pin passive line (at 0 0 180) (length 0) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
      )
    )
    (symbol "power:GND"
      (power)
      (symbol "GND_0_1"
        (pin power_in line (at 0 0 270) (length 0) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
      )
    )
    (symbol "power:+5V"
      (power)
      (symbol "+5V_0_1"
        (pin power_in line (at 0 0 270) (length 0) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
      )
    )
    (symbol "power:VCC"
      (power)
      (symbol "VCC_0_1"
        (pin power_in line (at 0 0 270) (length 0) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
      )
    )
  )
{labels_and_symbols}
)"#
            ),
        )
        .unwrap();
        (dir, path)
    }

    fn one_pin(reference: &str, value: &str, x: f64, y: f64, uuid: &str) -> String {
        format!(
            r#"  (symbol
    (lib_id "Test:P")
    (at {x} {y} 0)
    (unit 1)
    (property "Reference" "{reference}" (at {x} {} 0))
    (property "Value" "{value}" (at {x} {} 0))
    (uuid "{uuid}")
  )"#,
            y - 2.0,
            y + 2.0
        )
    }

    fn label(net: &str, x: f64, y: f64, uuid: &str) -> String {
        format!(
            r#"  (label "{net}" (at {x} {y} 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "{uuid}"))"#
        )
    }

    fn global_label(net: &str, x: f64, y: f64, uuid: &str) -> String {
        format!(
            r#"  (global_label "{net}" (shape input) (at {x} {y} 0) (effects (font (size 1.27 1.27)) (justify left)) (uuid "{uuid}"))"#
        )
    }

    fn horizontal_sheet_fixture(
        name: &str,
        right_side_correct: bool,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{name}.kicad_sch"));
        let (right_x, right_rot, wire_right_x) = if right_side_correct {
            (90.0, 180.0, 90.0)
        } else {
            (120.0, 0.0, 120.0)
        };
        std::fs::write(
            &path,
            format!(
                r#"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "root")
  (paper "A4")
  (lib_symbols)
  (sheet (at 20 20) (size 30 30) (uuid "left-sheet")
    (property "Sheetname" "LEFT" (at 20 19 0))
    (property "Sheetfile" "left.kicad_sch" (at 20 51 0))
    (pin "SIG_A" output (at 50 25 0) (uuid "left-a"))
    (pin "SIG_B" output (at 50 30 0) (uuid "left-b"))
    (pin "SIG_C" output (at 50 35 0) (uuid "left-c"))
    (pin "KEEP" output (at 20 45 180) (uuid "left-keep"))
  )
  (sheet (at 90 20) (size 30 30) (uuid "right-sheet")
    (property "Sheetname" "RIGHT" (at 90 19 0))
    (property "Sheetfile" "right.kicad_sch" (at 90 51 0))
    (pin "SIG_A" input (at {right_x} 25 {right_rot}) (uuid "right-a"))
    (pin "SIG_B" input (at {right_x} 30 {right_rot}) (uuid "right-b"))
    (pin "SIG_C" input (at {right_x} 35 {right_rot}) (uuid "right-c"))
    (pin "KEEP" input (at 120 45 0) (uuid "right-keep"))
  )
  (label "SIG_A" (at 55 25 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "label-a"))
  (label "SIG_B" (at 55 30 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "label-b"))
  (label "SIG_C" (at 55 35 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "label-c"))
  (wire (pts (xy 50 25) (xy {wire_right_x} 25)) (uuid "wire-a"))
  (wire (pts (xy 50 30) (xy {wire_right_x} 30)) (uuid "wire-b"))
  (wire (pts (xy 50 35) (xy {wire_right_x} 35)) (uuid "wire-c"))
)"#
            ),
        )
        .unwrap();
        (dir, path)
    }

    fn vertical_sheet_fixture(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{name}.kicad_sch"));
        std::fs::write(
            &path,
            r#"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "root")
  (paper "A4")
  (lib_symbols)
  (sheet (at 40 20) (size 30 30) (uuid "top-sheet")
    (property "Sheetname" "TOP" (at 40 19 0))
    (property "Sheetfile" "top.kicad_sch" (at 40 51 0))
    (pin "SIG_V" output (at 45 50 270) (uuid "top-v"))
  )
  (sheet (at 40 90) (size 30 30) (uuid "bottom-sheet")
    (property "Sheetname" "BOTTOM" (at 40 89 0))
    (property "Sheetfile" "bottom.kicad_sch" (at 40 121 0))
    (pin "SIG_V" input (at 45 120 270) (uuid "bottom-v"))
  )
  (label "SIG_V" (at 45 60 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "label-v"))
  (wire (pts (xy 45 50) (xy 45 120)) (uuid "wire-v"))
)"#,
        )
        .unwrap();
        (dir, path)
    }

    fn two_net_component_fixture(
        name: &str,
        impossible_b: bool,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let body = [
            label("NET_A", 20.0, 20.0, "la1"),
            label("NET_A", 80.0, 20.0, "la2"),
            one_pin("JA1", "A", 20.0, 20.0, "ja1"),
            one_pin("JA2", "A", 80.0, 20.0, "ja2"),
            label("NET_B", 50.0, 0.0, "lb1"),
            label("NET_B", 50.0, 40.0, "lb2"),
            one_pin("JB1", "B", 50.0, 0.0, "jb1"),
            one_pin("JB2", "B", 50.0, 40.0, "jb2"),
            if impossible_b {
                r#"  (sheet (at 45 -5) (size 10 50) (uuid "blocking-sheet")
    (property "Sheetname" "BLOCK" (at 45 -6 0))
    (property "Sheetfile" "block.kicad_sch" (at 45 46 0))
  )"#
                .to_string()
            } else {
                String::new()
            },
        ]
        .join("\n");
        one_pin_net_fixture(name, &body)
    }

    fn three_sheet_peer_fixture(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{name}.kicad_sch"));
        std::fs::write(
            &path,
            r#"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "root")
  (paper "A4")
  (lib_symbols)
  (sheet (at 60 40) (size 20 20) (uuid "sheet-a")
    (property "Sheetname" "A" (at 60 39 0))
    (property "Sheetfile" "a.kicad_sch" (at 60 61 0))
    (pin "NET_AB" passive (at 80 45 0) (uuid "a-ab"))
    (pin "NET_AC" passive (at 80 55 0) (uuid "a-ac"))
  )
  (sheet (at 20 40) (size 20 20) (uuid "sheet-b")
    (property "Sheetname" "B" (at 20 39 0))
    (property "Sheetfile" "b.kicad_sch" (at 20 61 0))
    (pin "NET_AB" passive (at 40 45 0) (uuid "b-ab"))
  )
  (sheet (at 65 80) (size 20 20) (uuid "sheet-c")
    (property "Sheetname" "C" (at 65 79 0))
    (property "Sheetfile" "c.kicad_sch" (at 65 101 0))
    (pin "NET_AC" passive (at 85 85 0) (uuid "c-ac"))
  )
  (label "NET_AB" (at 50 45 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "lab"))
  (label "NET_AC" (at 80 70 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "lac"))
  (wire (pts (xy 80 45) (xy 40 45)) (uuid "wab"))
  (wire (pts (xy 80 55) (xy 80 85)) (uuid "wac1"))
  (wire (pts (xy 80 85) (xy 85 85)) (uuid "wac2"))
)"#,
        )
        .unwrap();
        (dir, path)
    }

    fn shared_sheet_peer_fixture(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{name}.kicad_sch"));
        std::fs::write(
            &path,
            r#"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "root")
  (paper "A4")
  (lib_symbols)
  (sheet (at 60 40) (size 20 20) (uuid "center-sheet")
    (property "Sheetname" "CENTER" (at 60 39 0))
    (property "Sheetfile" "center.kicad_sch" (at 60 61 0))
    (pin "NET_L" passive (at 80 45 0) (uuid "center-l"))
    (pin "NET_R" passive (at 60 55 180) (uuid "center-r"))
  )
  (sheet (at 20 40) (size 20 20) (uuid "left-peer")
    (property "Sheetname" "LEFT" (at 20 39 0))
    (property "Sheetfile" "left.kicad_sch" (at 20 61 0))
    (pin "NET_L" passive (at 40 45 0) (uuid "left-l"))
  )
  (sheet (at 100 40) (size 20 20) (uuid "right-peer")
    (property "Sheetname" "RIGHT" (at 100 39 0))
    (property "Sheetfile" "right.kicad_sch" (at 100 61 0))
    (pin "NET_R" passive (at 100 55 180) (uuid "right-r"))
  )
  (label "NET_L" (at 50 45 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "ll"))
  (label "NET_R" (at 90 55 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "lr"))
  (wire (pts (xy 80 45) (xy 40 45)) (uuid "wl"))
  (wire (pts (xy 60 55) (xy 100 55)) (uuid "wr"))
)"#,
        )
        .unwrap();
        (dir, path)
    }

    fn sheet_pin_at(content: &str, uuid: &str) -> (f64, f64, f64) {
        let tree = parse_sexp(content).unwrap();
        for sheet in tree.find_all("sheet") {
            for pin in sheet.find_all("pin") {
                if pin.find_str("uuid") == Some(uuid) {
                    return parse_at(pin).unwrap();
                }
            }
        }
        panic!("sheet pin {uuid} not found");
    }

    fn routes_contact(a: &[serde_json::Value], b: &[serde_json::Value]) -> bool {
        let parse = |value: &serde_json::Value| Segment {
            x1: value["x1"].as_f64().unwrap(),
            y1: value["y1"].as_f64().unwrap(),
            x2: value["x2"].as_f64().unwrap(),
            y2: value["y2"].as_f64().unwrap(),
        };
        a.iter().any(|left| {
            b.iter()
                .any(|right| conductive_segments_contact(parse(left), parse(right)))
        })
    }

    fn sheet_endpoint(
        sheet_uuid: &str,
        sheet_name: &str,
        pin_uuid: &str,
        pin_name: &str,
        x: f64,
        y: f64,
        rotation: f64,
    ) -> NetEndpoint {
        NetEndpoint {
            kind: NetEndpointKind::SheetPin {
                sheet_uuid: sheet_uuid.to_string(),
                sheet_name: sheet_name.to_string(),
                pin_uuid: pin_uuid.to_string(),
            },
            reference: sheet_name.to_string(),
            unit: 0,
            pin_number: pin_name.to_string(),
            pin_name: pin_name.to_string(),
            x,
            y,
            orientation_degrees: rotation,
            grid: 1.27,
            on_grid: true,
            nearest_grid_x: x,
            nearest_grid_y: y,
            attachment_depends_on_off_grid_position: false,
        }
    }

    fn placed_power(net: &str, reference: &str, x: f64, y: f64, uuid: &str) -> String {
        format!(
            r##"  (symbol
    (lib_id "power:{net}")
    (at {x} {y} 0)
    (unit 1)
    (property "Reference" "{reference}" (at {x} {y} 0) (hide yes))
    (property "Value" "{net}" (at {x} {y} 0))
    (uuid "{uuid}")
  )"##
        )
    }

    fn canonical_custom_power(net: &str, reference: &str, x: f64, y: f64, uuid: &str) -> String {
        let template = power_symbol_template_lib_id(net);
        format!(
            r##"  (symbol
    (lib_id "{template}")
    (at {x} {y} 0)
    (unit 1)
    (property "Reference" "{reference}" (at {x} {y} 0) (hide yes))
    (property "Value" "{net}" (at {x} {y} 0))
    (uuid "{uuid}")
  )"##
        )
    }

    fn legacy_power(net: &str, reference: &str, x: f64, y: f64, uuid: &str) -> String {
        placed_power(net, reference, x, y, uuid)
    }

    fn wire(x1: f64, y1: f64, x2: f64, y2: f64, uuid: &str) -> String {
        format!(r#"  (wire (pts (xy {x1} {y1}) (xy {x2} {y2})) (uuid "{uuid}"))"#)
    }

    fn insert_before_schematic_close(mut content: String, addition: &str) -> String {
        let close = content
            .rfind(')')
            .expect("test schematic must have closing paren");
        content.insert_str(close, addition);
        content
    }

    async fn power_symbol_preview(path: &std::path::Path, net: &str) -> serde_json::Value {
        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": net,
                "representation": "power_symbol",
                "scope_policy": "allow_global_power_scope",
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        result_json(&dry)
    }

    async fn apply_power_symbol_preview(path: &std::path::Path, net: &str) -> String {
        let preview = power_symbol_preview(path, net).await;
        let applied = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": net,
                "representation": "power_symbol",
                "scope_policy": "allow_global_power_scope",
                "dry_run": false,
                "plan_revision": preview["plan_revision"].as_str().unwrap()
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!applied.is_error, "{}", result_text(&applied));
        std::fs::read_to_string(path).unwrap()
    }

    fn materialize_obstacle_fixture(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obstacles.kicad_sch");
        std::fs::write(
            &path,
            format!(
                r#"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "root")
  (paper "A4")
  (lib_symbols
    (symbol "Test:P"
      (symbol "P_1_1"
        (pin passive line (at 0 0 180) (length 0) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
      )
    )
    (symbol "Test:Block"
      (symbol "Block_0_1"
        (rectangle (start -10 -5) (end 10 5) (stroke (width 0.1524) (type solid)) (fill (type none)))
      )
    )
  )
  (label "NODE" (at 50 50 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "label-a"))
  (label "NODE" (at 100 50 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "label-b"))
  (symbol (lib_id "Test:P") (at 50 50 0) (unit 1) (property "Reference" "J1" (at 50 48 0)) (property "Value" "P" (at 50 52 0)) (uuid "j1"))
  (symbol (lib_id "Test:P") (at 100 50 0) (unit 1) (property "Reference" "U1" (at 100 48 0)) (property "Value" "P" (at 100 52 0)) (uuid "u1"))
{body}
)"#
            ),
        )
        .unwrap();
        (dir, path)
    }

    fn materialize_and_preview(path: &std::path::Path) -> serde_json::Value {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let dry = handle_refactor_schematic_net_representation(
                &json!({
                    "schematic": path.display().to_string(),
                    "net": "NODE",
                    "representation": "wired_local",
                    "remove_redundant_local_labels": true
                }),
                &test_ctx(),
            )
            .await
            .unwrap();
            assert!(!dry.is_error, "{}", result_text(&dry));
            result_json(&dry)
        })
    }

    fn proposed_segments(preview: &serde_json::Value) -> Vec<Segment> {
        preview["proposed_wires"]
            .as_array()
            .unwrap()
            .iter()
            .map(|wire| Segment {
                x1: wire["x1"].as_f64().unwrap(),
                y1: wire["y1"].as_f64().unwrap(),
                x2: wire["x2"].as_f64().unwrap(),
                y2: wire["y2"].as_f64().unwrap(),
            })
            .collect()
    }

    fn test_ctx_with_kicad_cli(kicad_cli: &str) -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: kicad_cli.to_string(),
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

    fn project_power_fixture(
        rail: &str,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let other_rail = if rail == "+5V_MAIN" {
            "VCC_3V3_S0"
        } else {
            "+5V_MAIN"
        };
        let root = dir.path().join("fixture.kicad_sch");
        let power = dir.path().join("power.kicad_sch");
        let core = dir.path().join("core.kicad_sch");
        std::fs::write(dir.path().join("fixture.kicad_pro"), "{}\n").unwrap();
        std::fs::write(
            &root,
            format!(
                r#"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "root")
  (paper "A4")
  (lib_symbols)
  (sheet (at 20 20) (size 30 20) (uuid "sheet-power")
    (property "Sheetname" "Power" (at 20 19.2 0))
    (property "Sheetfile" "power.kicad_sch" (at 20 40.4 0))
    (pin "{rail}" output (at 50 25 0) (uuid "pin-power-rail"))
    (pin "{other_rail}" output (at 50 27.5 0) (uuid "pin-power-other"))
    (pin "USB_SIGNAL" passive (at 50 30 0) (uuid "pin-power-usb"))
  )
  (sheet (at 80 20) (size 30 20) (uuid "sheet-core")
    (property "Sheetname" "Core" (at 80 19.2 0))
    (property "Sheetfile" "core.kicad_sch" (at 80 40.4 0))
    (pin "{rail}" input (at 80 25 180) (uuid "pin-core-rail"))
    (pin "{other_rail}" input (at 80 27.5 180) (uuid "pin-core-other"))
    (pin "USB_SIGNAL" passive (at 80 30 180) (uuid "pin-core-usb"))
  )
  (wire (pts (xy 50 25) (xy 80 25)) (uuid "wire-rail"))
  (wire (pts (xy 50 27.5) (xy 80 27.5)) (uuid "wire-other"))
  (wire (pts (xy 50 30) (xy 80 30)) (uuid "wire-usb"))
  (sheet_instances (path "/" (page "1")))
)"#
            ),
        )
        .unwrap();
        let child = |name: &str, label_kind: &str, refs: &[&str]| -> String {
            let symbols = refs
                .iter()
                .enumerate()
                .map(|(index, reference)| {
                    let x = 40.0 + (index as f64 * 20.0);
                    one_pin(reference, "LOAD", x, 50.0, &format!("{name}-{reference}"))
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                r##"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "{name}-uuid")
  (paper "A4")
  (lib_symbols
    (symbol "Test:P"
      (symbol "P_1_1"
        (pin passive line (at 0 0 180) (length 0) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
      )
    )
    (symbol "power:+5V"
      (power)
      (property "Reference" "#PWR" (at 0 -3.81 0) (effects (font (size 1.27 1.27))) (hide yes))
      (property "Value" "+5V" (at 0 3.556 0) (effects (font (size 1.27 1.27))))
      (symbol "+5V_0_1"
        (pin power_in line (at 0 0 270) (length 0) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
      )
    )
    (symbol "power:VCC"
      (power)
      (property "Reference" "#PWR" (at 0 -3.81 0) (effects (font (size 1.27 1.27))) (hide yes))
      (property "Value" "VCC" (at 0 3.556 0) (effects (font (size 1.27 1.27))))
      (symbol "VCC_0_1"
        (pin power_in line (at 0 0 270) (length 0) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
      )
    )
  )
  ({label_kind} "{rail}" (shape passive) (at 40 50 0) (effects (font (size 1.27 1.27)) (justify left)) (uuid "{name}-rail-label"))
  (hierarchical_label "{other_rail}" (shape passive) (at 40 55 0) (effects (font (size 1.27 1.27)) (justify left)) (uuid "{name}-other-label"))
  (hierarchical_label "USB_SIGNAL" (shape passive) (at 40 60 0) (effects (font (size 1.27 1.27)) (justify left)) (uuid "{name}-usb-label"))
{symbols}
)"##
            )
        };
        std::fs::write(&power, child("power", "hierarchical_label", &["R14"])).unwrap();
        std::fs::write(&core, child("core", "hierarchical_label", &["U1", "U2"])).unwrap();
        (dir, root, power, core)
    }

    #[tokio::test]
    async fn noncanonical_legacy_power_symbol_is_replaced_with_canonical_template() {
        let net = "+5V_MAIN";
        let body = [
            label(net, 50.0, 50.0, "l1"),
            one_pin("U1", "IC", 50.0, 50.0, "u1"),
            legacy_power(net, "#PWR010", 50.0, 50.0, "legacy-pwr"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("legacy_replace", &body);
        let preview = power_symbol_preview(&path, net).await;
        assert_eq!(
            preview["noncanonical_replacements"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{preview}"
        );
        let after = apply_power_symbol_preview(&path, net).await;
        assert!(!after.contains("legacy-pwr"));
        assert!(after.contains("(lib_id \"power:+5V\")"));
        assert!(after.contains("(property \"Value\" \"+5V_MAIN\""));
    }

    #[tokio::test]
    async fn same_node_duplicate_power_symbols_remove_redundant_instance() {
        let net = "+5V_MAIN";
        let body = [
            label(net, 50.0, 50.0, "l1"),
            one_pin("U1", "IC", 50.0, 50.0, "u1"),
            canonical_custom_power(net, "#PWR001", 50.0, 50.0, "canon-a"),
            canonical_custom_power(net, "#PWR002", 50.0, 50.0, "canon-b"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("same_node_duplicate", &body);
        let preview = power_symbol_preview(&path, net).await;
        assert_eq!(
            preview["redundant_duplicates_removed"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{preview}"
        );
        let after = apply_power_symbol_preview(&path, net).await;
        assert_eq!(after.matches("(property \"Value\" \"+5V_MAIN\"").count(), 1);
    }

    #[tokio::test]
    async fn duplicate_legacy_power_symbols_leave_one_canonical_replacement() {
        let net = "+5V_MAIN";
        let body = [
            label(net, 50.0, 50.0, "l1"),
            one_pin("U1", "IC", 50.0, 50.0, "u1"),
            legacy_power(net, "#PWR010", 50.0, 50.0, "legacy-a"),
            legacy_power(net, "#PWR011", 50.0, 50.0, "legacy-b"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("legacy_duplicates", &body);
        let preview = power_symbol_preview(&path, net).await;
        assert_eq!(
            preview["noncanonical_replacements"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{preview}"
        );
        assert_eq!(
            preview["redundant_duplicates_removed"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{preview}"
        );
        let after = apply_power_symbol_preview(&path, net).await;
        assert!(!after.contains("legacy-a"));
        assert!(!after.contains("legacy-b"));
        assert_eq!(after.matches("(property \"Value\" \"+5V_MAIN\"").count(), 1);
    }

    #[tokio::test]
    async fn obsolete_single_segment_legacy_stub_is_removed() {
        let net = "+5V_MAIN";
        let body = [
            legacy_power(net, "#PWR010", 20.0, 20.0, "obsolete-pwr"),
            wire(20.0, 20.0, 25.0, 20.0, "stub-wire"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("single_stub", &body);
        let preview = power_symbol_preview(&path, net).await;
        assert_eq!(
            preview["obsolete_symbols_removed"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{preview}"
        );
        assert_eq!(
            preview["obsolete_stub_wires_removed"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{preview}"
        );
        let after = apply_power_symbol_preview(&path, net).await;
        assert!(!after.contains("obsolete-pwr"));
        assert!(!after.contains("stub-wire"));
    }

    #[tokio::test]
    async fn obsolete_multi_segment_legacy_stub_is_removed() {
        let net = "+5V_MAIN";
        let body = [
            legacy_power(net, "#PWR010", 20.0, 20.0, "obsolete-pwr"),
            wire(20.0, 20.0, 25.0, 20.0, "stub-a"),
            wire(25.0, 20.0, 25.0, 25.0, "stub-b"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("multi_stub", &body);
        let preview = power_symbol_preview(&path, net).await;
        assert_eq!(
            preview["obsolete_stub_wires_removed"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "{preview}"
        );
        let after = apply_power_symbol_preview(&path, net).await;
        assert!(!after.contains("stub-a"));
        assert!(!after.contains("stub-b"));
    }

    #[tokio::test]
    async fn short_meaningful_power_wire_is_preserved() {
        let net = "+5V_MAIN";
        let body = [
            label(net, 55.0, 20.0, "l1"),
            one_pin("U1", "IC", 55.0, 20.0, "u1"),
            legacy_power(net, "#PWR010", 20.0, 20.0, "legacy-pwr"),
            wire(20.0, 20.0, 55.0, 20.0, "meaningful-wire"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("meaningful_wire", &body);
        let preview = power_symbol_preview(&path, net).await;
        assert!(preview["obsolete_stub_wires_removed"]
            .as_array()
            .unwrap()
            .is_empty());
        let after = apply_power_symbol_preview(&path, net).await;
        assert!(after.contains("meaningful-wire"));
    }

    #[tokio::test]
    async fn branched_meaningful_power_wire_is_preserved() {
        let net = "+5V_MAIN";
        let body = [
            label(net, 55.0, 20.0, "l1"),
            one_pin("U1", "IC", 55.0, 20.0, "u1"),
            legacy_power(net, "#PWR010", 20.0, 20.0, "legacy-pwr"),
            wire(20.0, 20.0, 55.0, 20.0, "branch-a"),
            wire(55.0, 20.0, 55.0, 25.0, "branch-b"),
            wire(55.0, 20.0, 60.0, 20.0, "branch-c"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("branched_wire", &body);
        let preview = power_symbol_preview(&path, net).await;
        assert!(preview["obsolete_stub_wires_removed"]
            .as_array()
            .unwrap()
            .is_empty());
        let after = apply_power_symbol_preview(&path, net).await;
        assert!(after.contains("branch-a"));
        assert!(after.contains("branch-b"));
        assert!(after.contains("branch-c"));
    }

    #[tokio::test]
    async fn legitimate_separate_power_symbols_are_preserved() {
        let net = "+5V_MAIN";
        let body = [
            label(net, 50.8, 20.32, "l1"),
            label(net, 76.2, 20.32, "l2"),
            one_pin("U1", "IC", 50.8, 20.32, "u1"),
            one_pin("U2", "IC", 76.2, 20.32, "u2"),
            canonical_custom_power(net, "#PWR001", 50.8, 20.32, "canon-a"),
            canonical_custom_power(net, "#PWR002", 76.2, 20.32, "canon-b"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("legitimate_repeats", &body);
        let preview = power_symbol_preview(&path, net).await;
        assert_eq!(
            preview["canonical_unchanged"].as_array().unwrap().len(),
            2,
            "{preview}"
        );
        assert!(preview["redundant_duplicates_removed"]
            .as_array()
            .unwrap()
            .is_empty());
        let after = apply_power_symbol_preview(&path, net).await;
        assert_eq!(after.matches("(property \"Value\" \"+5V_MAIN\"").count(), 2);
    }

    #[tokio::test]
    async fn combined_power_symbol_debt_is_classified_in_one_pass() {
        let net = "+5V_MAIN";
        let body = [
            label(net, 50.0, 20.0, "l1"),
            one_pin("U1", "IC", 50.0, 20.0, "u1"),
            legacy_power(net, "#PWR010", 50.0, 20.0, "legacy-useful"),
            legacy_power(net, "#PWR011", 50.0, 20.0, "legacy-dup"),
            legacy_power(net, "#PWR012", 90.0, 20.0, "legacy-obsolete"),
            wire(90.0, 20.0, 95.0, 20.0, "obsolete-wire"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("combined_debt", &body);
        let preview = power_symbol_preview(&path, net).await;
        assert_eq!(
            preview["noncanonical_replacements"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{preview}"
        );
        assert_eq!(
            preview["redundant_duplicates_removed"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{preview}"
        );
        assert_eq!(
            preview["obsolete_symbols_removed"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{preview}"
        );
        assert_eq!(
            preview["obsolete_stub_wires_removed"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{preview}"
        );
    }

    #[tokio::test]
    async fn project_scope_power_refactor_includes_legacy_cleanup_on_participating_sheets() {
        let (_dir, root, power, core) = project_power_fixture("+5V_MAIN");
        let mut power_text = std::fs::read_to_string(&power).unwrap();
        power_text = insert_before_schematic_close(
            power_text,
            &format!(
                "\n{}\n{}\n",
                legacy_power("+5V_MAIN", "#PWR010", 40.0, 50.0, "power-legacy"),
                legacy_power("+5V_MAIN", "#PWR011", 90.0, 20.0, "power-obsolete")
            ),
        );
        power_text = insert_before_schematic_close(
            power_text,
            &format!(
                "\n{}\n",
                wire(90.0, 20.0, 95.0, 20.0, "power-obsolete-wire")
            ),
        );
        std::fs::write(&power, power_text).unwrap();
        let mut core_text = std::fs::read_to_string(&core).unwrap();
        core_text = insert_before_schematic_close(
            core_text,
            &format!(
                "\n{}\n",
                legacy_power("+5V_MAIN", "#PWR012", 40.0, 50.0, "core-legacy")
            ),
        );
        std::fs::write(&core, core_text).unwrap();

        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": root.display().to_string(),
                "net": "+5V_MAIN",
                "representation": "power_symbol",
                "scope": "project",
                "scope_policy": "allow_global_power_scope",
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let body = result_json(&dry);
        assert_eq!(
            body["noncanonical_replacements"].as_array().unwrap().len(),
            2,
            "{body}"
        );
        assert_eq!(
            body["obsolete_symbols_removed"].as_array().unwrap().len(),
            1,
            "{body}"
        );
        assert_eq!(
            body["obsolete_stub_wires_removed"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{body}"
        );
    }

    #[test]
    fn normalized_kicad_netlist_groups_preserves_singletons() {
        let netlist = r#"(export
  (nets
    (net (code "1") (name "GROUP_A")
      (node (ref "R1") (pin "1"))
      (node (ref "R2") (pin "1"))
    )
    (net (code "2") (name "GROUP_B")
      (node (ref "U1") (pin "6"))
    )
  )
)"#;
        let groups = normalized_kicad_netlist_groups(netlist).unwrap();
        assert!(groups.contains(&vec!["R1.1".to_string(), "R2.1".to_string()]));
        assert!(groups.contains(&vec!["U1.6".to_string()]));
    }

    #[tokio::test]
    async fn project_scope_power_refactor_plans_hierarchy_cleanup_without_root_decoration() {
        let (_dir, root, _power, _core) = project_power_fixture("+5V_MAIN");
        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": root.display().to_string(),
                "net": "+5V_MAIN",
                "representation": "power_symbol",
                "scope": "project",
                "scope_policy": "allow_global_power_scope",
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let body = result_json(&dry);
        assert_eq!(body["scope"], json!("project"));
        assert_eq!(body["kicad_validation_available"], json!(false));
        assert_eq!(
            body["power_symbols_to_add"].as_array().unwrap().len(),
            2,
            "{body}"
        );
        assert!(body["power_symbols_to_add"]
            .as_array()
            .unwrap()
            .iter()
            .all(|symbol| symbol["template_lib_id"] == "power:+5V"));
        assert!(
            body["sheet_pins_to_remove"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["count"].as_u64().unwrap())
                .sum::<u64>()
                >= 2
        );
        assert!(
            body["obsolete_parent_root_wires_to_remove"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["count"].as_u64().unwrap())
                .sum::<u64>()
                >= 1
        );
        let root_before = std::fs::read_to_string(&root).unwrap();
        assert!(root_before.contains("VCC_3V3_S0"));
        assert!(root_before.contains("USB_SIGNAL"));
    }

    #[tokio::test]
    async fn project_scope_power_refactor_rejects_stale_child_revision_before_writing() {
        let (_dir, root, _power, core) = project_power_fixture("VCC_3V3_S0");
        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": root.display().to_string(),
                "net": "VCC_3V3_S0",
                "representation": "power_symbol",
                "scope": "project",
                "scope_policy": "allow_global_power_scope",
                "dry_run": true
            }),
            &test_ctx_with_kicad_cli("/bin/false"),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let body = result_json(&dry);
        let root_before = std::fs::read_to_string(&root).unwrap();
        let core_before = std::fs::read_to_string(&core).unwrap();
        std::fs::write(&core, core_before.replace("USB_SIGNAL", "USB_SIGNAL_STALE")).unwrap();

        let applied = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": root.display().to_string(),
                "net": "VCC_3V3_S0",
                "representation": "power_symbol",
                "scope": "project",
                "scope_policy": "allow_global_power_scope",
                "dry_run": false,
                "plan_revision": body["plan_revision"].as_str().unwrap()
            }),
            &test_ctx_with_kicad_cli("/bin/false"),
        )
        .await
        .unwrap();
        assert!(applied.is_error);
        assert!(result_text(&applied).contains("plan_revision mismatch"));
        assert_eq!(std::fs::read_to_string(&root).unwrap(), root_before);
        assert!(std::fs::read_to_string(&core)
            .unwrap()
            .contains("USB_SIGNAL_STALE"));
    }

    #[tokio::test]
    async fn project_scope_power_refactor_apply_requires_kicad_cli() {
        let (_dir, root, _power, _core) = project_power_fixture("+5V_MAIN");
        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": root.display().to_string(),
                "net": "+5V_MAIN",
                "representation": "power_symbol",
                "scope": "project",
                "scope_policy": "allow_global_power_scope",
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let body = result_json(&dry);

        let applied = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": root.display().to_string(),
                "net": "+5V_MAIN",
                "representation": "power_symbol",
                "scope": "project",
                "scope_policy": "allow_global_power_scope",
                "dry_run": false,
                "plan_revision": body["plan_revision"].as_str().unwrap()
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(applied.is_error);
        assert_eq!(
            result_text(&applied),
            "project-scope power migration requires kicad-cli validation"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn project_scope_power_refactor_applies_with_real_kicad_cli() {
        let kicad_cli = "/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli";
        if !std::path::Path::new(kicad_cli).exists() {
            eprintln!("skipping: {kicad_cli} not found");
            return;
        }
        for rail in ["+5V_MAIN", "VCC_3V3_S0"] {
            let (_dir, root, power, core) = project_power_fixture(rail);
            let ctx = test_ctx_with_kicad_cli(kicad_cli);
            let dry = handle_refactor_schematic_net_representation(
                &json!({
                    "schematic": root.display().to_string(),
                    "net": rail,
                    "representation": "power_symbol",
                    "scope": "project",
                    "scope_policy": "allow_global_power_scope",
                    "dry_run": true
                }),
                &ctx,
            )
            .await
            .unwrap();
            assert!(!dry.is_error, "{rail}: {}", result_text(&dry));
            let body = result_json(&dry);
            let applied = handle_refactor_schematic_net_representation(
                &json!({
                    "schematic": root.display().to_string(),
                    "net": rail,
                    "representation": "power_symbol",
                    "scope": "project",
                    "scope_policy": "allow_global_power_scope",
                    "dry_run": false,
                    "plan_revision": body["plan_revision"].as_str().unwrap()
                }),
                &ctx,
            )
            .await
            .unwrap();
            assert!(!applied.is_error, "{rail}: {}", result_text(&applied));
            let root_after = std::fs::read_to_string(&root).unwrap();
            assert!(!root_after.contains(&format!("\"{rail}\" output")));
            assert!(!root_after.contains(&format!("\"{rail}\" input")));
            assert!(!root_after.contains("wire-rail"));
            assert!(root_after.contains("USB_SIGNAL"));
            let other_rail = if rail == "+5V_MAIN" {
                "VCC_3V3_S0"
            } else {
                "+5V_MAIN"
            };
            assert!(root_after.contains(other_rail));
            assert!(root_after.contains("wire-other"));
            assert!(root_after.contains("wire-usb"));
            assert!(!root_after.contains(&format!("(property \"Value\" \"{rail}\"")));
            for child in [power, core] {
                let child_after = std::fs::read_to_string(child).unwrap();
                assert!(!child_after.contains(&format!("hierarchical_label \"{rail}\"")));
                assert!(child_after.contains(&format!("(property \"Value\" \"{rail}\"")));
                assert!(!child_after.contains(&format!("power:{rail}")));
                assert!(child_after.contains(other_rail));
                assert!(child_after.contains("USB_SIGNAL"));
            }
        }
    }

    #[tokio::test]
    async fn materialize_net_connects_only_existing_same_net_endpoints_and_cleans_labels() {
        let (_dir, path) = ltc_fixture();
        let ctx = test_ctx();

        let dry = handle_materialize_schematic_net(
            &json!({
                "schematic": path.display().to_string(),
                "net": "LTC_SHDN",
                "remove_redundant_local_labels": true,
                "dry_run": true
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let body = result_json(&dry);
        assert_eq!(body["endpoint_count"], json!(2));
        assert!(
            body["candidate_wire_count"].as_u64().unwrap() >= 1,
            "{body}"
        );
        assert_eq!(
            body["labels_that_would_be_removed"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let applied = handle_materialize_schematic_net(
            &json!({
                "schematic": path.display().to_string(),
                "net": "LTC_SHDN",
                "remove_redundant_local_labels": true,
                "dry_run": false,
                "plan_revision": body["plan_revision"].as_str().unwrap()
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!applied.is_error, "{}", result_text(&applied));

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("(wire"));
        assert_eq!(after.matches("\"LTC_SHDN\"").count(), 1);
        let snapshot = semantic_snapshot(&after).unwrap();
        assert_eq!(
            snapshot
                .pin_nets
                .get("R16:1:1")
                .and_then(|net| net.as_deref()),
            Some("AUX_5V")
        );
        assert_eq!(
            snapshot
                .pin_nets
                .get("R16:1:2")
                .and_then(|net| net.as_deref()),
            Some("LTC_SHDN")
        );
        assert_eq!(
            snapshot
                .pin_nets
                .get("U2:1:6")
                .and_then(|net| net.as_deref()),
            Some("LTC_SHDN")
        );
        assert!(snapshot.shorts.is_empty());
    }

    #[tokio::test]
    async fn materialize_net_rejects_explicit_endpoint_on_the_wrong_net() {
        let (_dir, path) = ltc_fixture();
        let result = handle_materialize_schematic_net(
            &json!({
                "schematic": path.display().to_string(),
                "net": "LTC_SHDN",
                "pins": [
                    { "reference": "R16", "pin_number": "1" },
                    { "reference": "U2", "pin_number": "6" }
                ],
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert!(result_text(&result).contains("R16.1"));
        assert!(result_text(&result).contains("AUX_5V"));
    }

    #[tokio::test]
    async fn materialize_net_rejects_sense_resistor_bypass() {
        let (_dir, path) = ltc_fixture();
        let before = std::fs::read_to_string(&path).unwrap();
        let sense = before
            .replace("R16", "R14")
            .replace("AUX_5V", "SENSE_HI")
            .replace("LTC_SHDN", "+5V_MAIN")
            .replace("U2", "TP1");
        std::fs::write(&path, sense).unwrap();

        let result = handle_materialize_schematic_net(
            &json!({
                "schematic": path.display().to_string(),
                "net": "+5V_MAIN",
                "pins": [
                    { "reference": "R14", "pin_number": "1" },
                    { "reference": "TP1", "pin_number": "6" }
                ],
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert!(result_text(&result).contains("R14.1"));
        assert!(result_text(&result).contains("SENSE_HI"));
    }

    #[tokio::test]
    async fn refactor_wired_local_previews_final_transaction_and_preserves_membership() {
        let (_dir, path) = ltc_fixture();
        let before = semantic_snapshot(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "LTC_SHDN",
                "representation": "wired_local",
                "remove_redundant_local_labels": true,
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let body = result_json(&dry);
        assert_eq!(
            body["semantic_connectivity_comparison"]["preserved"],
            json!(true)
        );

        let applied = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "LTC_SHDN",
                "representation": "wired_local",
                "remove_redundant_local_labels": true,
                "dry_run": false,
                "plan_revision": body["plan_revision"].as_str().unwrap()
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!applied.is_error, "{}", result_text(&applied));
        let after = semantic_snapshot(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(before.pin_nets, after.pin_nets);
        assert_eq!(before.shorts, after.shorts);
    }

    #[tokio::test]
    async fn refactor_branched_label_net_uses_tree_not_pairwise_mesh() {
        let body = [
            label("NODE", 50.0, 50.0, "l1"),
            label("NODE", 80.0, 60.0, "l2"),
            label("NODE", 110.0, 50.0, "l3"),
            label("NODE", 95.0, 75.0, "l4"),
            one_pin("J1", "CONN", 50.0, 50.0, "j1"),
            one_pin("D1", "ESD", 80.0, 60.0, "d1"),
            one_pin("R1", "0R", 110.0, 50.0, "r1"),
            one_pin("U1", "IC", 95.0, 75.0, "u1"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("branch", &body);
        let before = semantic_snapshot(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "NODE",
                "representation": "wired_local",
                "strategy": { "preferred_orientation": "horizontal", "trunk_y": 50.0 },
                "remove_redundant_local_labels": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let preview = result_json(&dry);
        let wire_count = preview["proposed_wires"].as_array().unwrap().len();
        assert!(
            wire_count <= 8,
            "four endpoints should use a trunk plus pin escapes, not a pairwise full mesh: {preview}"
        );
        assert!(wire_count >= 3, "expected trunk plus branches: {preview}");

        let applied = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "NODE",
                "representation": "wired_local",
                "strategy": { "preferred_orientation": "horizontal", "trunk_y": 50.0 },
                "remove_redundant_local_labels": true,
                "dry_run": false,
                "plan_revision": preview["plan_revision"].as_str().unwrap()
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!applied.is_error, "{}", result_text(&applied));
        let after = semantic_snapshot(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(before.pin_nets, after.pin_nets);
        assert_eq!(before.shorts, after.shorts);
    }

    #[test]
    fn materialize_routes_around_foreign_symbol_body() {
        let body = r#"  (symbol (lib_id "Test:Block") (at 75 50 0) (unit 1) (property "Reference" "B1" (at 75 43 0)) (property "Value" "BLOCK" (at 75 57 0)) (uuid "b1"))"#;
        let (_dir, path) = materialize_obstacle_fixture(body);
        let preview = materialize_and_preview(&path);
        let proposed = proposed_segments(&preview);
        let obstacle = RectObstacle {
            min_x: 65.0,
            min_y: 45.0,
            max_x: 85.0,
            max_y: 55.0,
        };
        assert!(
            proposed
                .iter()
                .all(|segment| !segment_crosses_rect_interior(*segment, obstacle)),
            "{preview}"
        );
        assert!(
            proposed
                .iter()
                .any(|segment| (segment.y1 - 50.0).abs() > 0.01),
            "route should detour off the blocked row: {preview}"
        );
    }

    #[test]
    fn materialize_routes_around_foreign_pin() {
        let body = r#"  (label "OTHER" (at 75 50 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "other"))
  (symbol (lib_id "Test:P") (at 75 50 0) (unit 1) (property "Reference" "F1" (at 75 48 0)) (property "Value" "P" (at 75 52 0)) (uuid "f1"))"#;
        let (_dir, path) = materialize_obstacle_fixture(body);
        let preview = materialize_and_preview(&path);
        let proposed = proposed_segments(&preview);
        assert!(
            proposed
                .iter()
                .all(|segment| !point_strictly_on_segment(75.0, 50.0, *segment)),
            "{preview}"
        );
    }

    #[test]
    fn materialize_routes_around_foreign_conductive_wire() {
        let body = r#"  (label "OTHER" (at 75 45 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "other"))
  (wire (pts (xy 75 45) (xy 75 55)) (uuid "foreign-wire"))"#;
        let (_dir, path) = materialize_obstacle_fixture(body);
        let preview = materialize_and_preview(&path);
        let proposed = proposed_segments(&preview);
        let foreign = Segment {
            x1: 75.0,
            y1: 45.0,
            x2: 75.0,
            y2: 55.0,
        };
        assert!(
            proposed
                .iter()
                .all(|segment| !conductive_segments_contact(*segment, foreign)),
            "{preview}"
        );
    }

    #[test]
    fn foreign_conductive_endpoint_and_t_contacts_are_hard_invalid() {
        let target_endpoint_touch = Segment {
            x1: 50.0,
            y1: 50.0,
            x2: 75.0,
            y2: 50.0,
        };
        let foreign_endpoint = Segment {
            x1: 75.0,
            y1: 50.0,
            x2: 75.0,
            y2: 60.0,
        };
        let target_t = Segment {
            x1: 50.0,
            y1: 55.0,
            x2: 100.0,
            y2: 55.0,
        };
        assert!(conductive_segments_contact(
            target_endpoint_touch,
            foreign_endpoint
        ));
        assert!(conductive_segments_contact(target_t, foreign_endpoint));
        let obstacles = MaterializeObstacles {
            bodies: Vec::new(),
            foreign_pins: Vec::new(),
            foreign_wires: vec![foreign_endpoint],
        };
        assert!(!obstacles.allows(target_endpoint_touch));
        assert!(!obstacles.allows(target_t));
    }

    #[tokio::test]
    async fn optimize_schematic_layout_returns_deterministic_top_k_candidates() {
        let body = [
            label("NODE", 50.0, 50.0, "l1"),
            label("NODE", 100.0, 50.0, "l2"),
            one_pin("J1", "CONN", 50.0, 50.0, "j1"),
            one_pin("U1", "IC", 100.0, 50.0, "u1"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("topk", &body);
        let args = json!({
            "schematic": path.display().to_string(),
            "net_names": ["NODE"],
            "candidate_count": 3,
            "dry_run": true
        });
        let first = handle_optimize_schematic_layout(&args, &test_ctx())
            .await
            .unwrap();
        let second = handle_optimize_schematic_layout(&args, &test_ctx())
            .await
            .unwrap();
        assert!(!first.is_error, "{}", result_text(&first));
        assert!(!second.is_error, "{}", result_text(&second));
        let first = result_json(&first);
        let second = result_json(&second);
        assert_eq!(first["plan_revision"], second["plan_revision"]);
        assert_eq!(first["candidates"], second["candidates"]);
        assert_eq!(first["algorithm"]["default_top_k"], json!(3));
        assert_eq!(first["algorithm"]["maximum_top_k"], json!(5));
        assert!(first["candidates"].as_array().unwrap().len() <= 3);
        assert_eq!(first["candidates"][0]["rank"], json!(1));
        assert_eq!(
            first["candidates"][0]["validation"]["foreign_conductive_contacts"],
            json!(0)
        );
    }

    #[tokio::test]
    async fn optimize_schematic_layout_returns_complete_multi_net_candidates() {
        let (_dir, path) = two_net_component_fixture("composite_topk", false);
        let args = json!({
            "schematic": path.display().to_string(),
            "net_names": ["NET_A", "NET_B"],
            "candidate_count": 3,
            "dry_run": true
        });
        let result = handle_optimize_schematic_layout(&args, &test_ctx())
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result_text(&result));
        let body = result_json(&result);
        for candidate in body["candidates"].as_array().unwrap() {
            assert_eq!(
                candidate["planned_changes"]["changed_or_routed_nets"],
                json!(["NET_A", "NET_B"])
            );
            assert!(candidate["routes_by_net"]["NET_A"].is_array());
            assert!(candidate["routes_by_net"]["NET_B"].is_array());
            assert!(candidate["candidate_wire_topology"].is_null());
        }
    }

    #[tokio::test]
    async fn composite_candidates_reject_internal_foreign_net_contacts() {
        let (_dir, path) = two_net_component_fixture("composite_avoid", false);
        let args = json!({
            "schematic": path.display().to_string(),
            "net_names": ["NET_A", "NET_B"],
            "candidate_count": 5,
            "dry_run": true
        });
        let result = handle_optimize_schematic_layout(&args, &test_ctx())
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result_text(&result));
        let body = result_json(&result);
        for candidate in body["candidates"].as_array().unwrap() {
            let a = candidate["routes_by_net"]["NET_A"].as_array().unwrap();
            let b = candidate["routes_by_net"]["NET_B"].as_array().unwrap();
            assert!(!routes_contact(a, b), "{candidate}");
        }
    }

    #[tokio::test]
    async fn impossible_multi_net_layout_does_not_return_partial_candidate() {
        let (_dir, path) = two_net_component_fixture("composite_partial", true);
        let args = json!({
            "schematic": path.display().to_string(),
            "net_names": ["NET_A", "NET_B"],
            "candidate_count": 3,
            "dry_run": true
        });
        let result = handle_optimize_schematic_layout(&args, &test_ctx())
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(!result_text(&result).contains("routes_by_net"));
    }

    #[tokio::test]
    async fn composite_multi_net_planning_is_deterministic() {
        let (_dir, path) = two_net_component_fixture("composite_deterministic", false);
        let args = json!({
            "schematic": path.display().to_string(),
            "net_names": ["NET_A", "NET_B"],
            "candidate_count": 5,
            "dry_run": true
        });
        let first = handle_optimize_schematic_layout(&args, &test_ctx())
            .await
            .unwrap();
        let second = handle_optimize_schematic_layout(&args, &test_ctx())
            .await
            .unwrap();
        assert!(!first.is_error, "{}", result_text(&first));
        assert!(!second.is_error, "{}", result_text(&second));
        assert_eq!(result_json(&first), result_json(&second));
    }

    #[tokio::test]
    async fn optimize_schematic_layout_applies_composite_candidate() {
        let (_dir, path) = horizontal_sheet_fixture("composite_apply", false);
        let dry_args = json!({
            "schematic": path.display().to_string(),
            "net_names": ["SIG_A", "SIG_B", "SIG_C"],
            "candidate_count": 5,
            "dry_run": true
        });
        let dry = handle_optimize_schematic_layout(&dry_args, &test_ctx())
            .await
            .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let body = result_json(&dry);
        for candidate in body["candidates"].as_array().unwrap() {
            assert_eq!(
                candidate["planned_changes"]["changed_or_routed_nets"],
                json!(["SIG_A", "SIG_B", "SIG_C"])
            );
        }
        let candidate = body["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["sheet_pin_changes"].as_array().unwrap().len() == 3)
            .expect("expected coherent three-pin repair candidate");
        let apply_args = json!({
            "schematic": path.display().to_string(),
            "net_names": ["SIG_A", "SIG_B", "SIG_C"],
            "candidate_count": 5,
            "candidate_id": candidate["candidate_id"],
            "plan_revision": body["plan_revision"],
            "dry_run": false
        });
        let applied = handle_optimize_schematic_layout(&apply_args, &test_ctx())
            .await
            .unwrap();
        assert!(!applied.is_error, "{}", result_text(&applied));
        let content = std::fs::read_to_string(path).unwrap();
        assert_eq!(sheet_pin_at(&content, "right-a"), (90.0, 25.4, 180.0));
        assert_eq!(sheet_pin_at(&content, "right-b"), (90.0, 30.48, 180.0));
        assert_eq!(sheet_pin_at(&content, "right-c"), (90.0, 35.56, 180.0));
        assert_eq!(sheet_pin_at(&content, "right-keep"), (120.0, 45.0, 0.0));
    }

    #[tokio::test]
    async fn optimize_schematic_layout_offers_horizontal_peer_facing_sheet_pin_state() {
        let (_dir, path) = horizontal_sheet_fixture("sheet_horizontal", false);
        let args = json!({
            "schematic": path.display().to_string(),
            "net_names": ["SIG_A"],
            "candidate_count": 5,
            "dry_run": true
        });
        let result = handle_optimize_schematic_layout(&args, &test_ctx())
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result_text(&result));
        let body = result_json(&result);
        let candidate = body["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| {
                !candidate["sheet_pin_changes"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            })
            .expect("expected a sheet-pin side candidate");
        let changes = candidate["sheet_pin_changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["pin_uuid"], json!("right-a"));
        assert_eq!(changes[0]["from_side"], json!("RIGHT"));
        assert_eq!(changes[0]["to_side"], json!("LEFT"));
        assert_eq!(changes[0]["to"], json!({"x": 90.0, "y": 25.4}));
        assert_eq!(
            candidate["score_breakdown"]["sheet_pin_side_changes"],
            json!(1)
        );
        assert!(candidate["structural_signature"]
            .as_str()
            .unwrap()
            .contains("sheet-pin:right-sheet:right-a:RIGHT:LEFT"));
    }

    #[tokio::test]
    async fn optimize_schematic_layout_applies_selected_sheet_pin_state() {
        let (_dir, path) = horizontal_sheet_fixture("sheet_apply", false);
        let dry_args = json!({
            "schematic": path.display().to_string(),
            "net_names": ["SIG_A"],
            "candidate_count": 5,
            "dry_run": true
        });
        let dry = handle_optimize_schematic_layout(&dry_args, &test_ctx())
            .await
            .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let dry_body = result_json(&dry);
        let candidate = dry_body["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| {
                !candidate["sheet_pin_changes"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            })
            .unwrap();
        let apply_args = json!({
            "schematic": path.display().to_string(),
            "net_names": ["SIG_A"],
            "candidate_count": 5,
            "candidate_id": candidate["candidate_id"],
            "plan_revision": dry_body["plan_revision"],
            "dry_run": false
        });
        let applied = handle_optimize_schematic_layout(&apply_args, &test_ctx())
            .await
            .unwrap();
        assert!(!applied.is_error, "{}", result_text(&applied));
        let content = std::fs::read_to_string(path).unwrap();
        assert_eq!(sheet_pin_at(&content, "right-a"), (90.0, 25.4, 180.0));
        assert_eq!(sheet_pin_at(&content, "right-keep"), (120.0, 45.0, 0.0));
    }

    #[tokio::test]
    async fn materialize_schematic_net_uses_sheet_pin_side_candidate() {
        let (_dir, path) = horizontal_sheet_fixture("sheet_materialize", false);
        let result = handle_materialize_schematic_net(
            &json!({
                "schematic": path.display().to_string(),
                "net": "SIG_A",
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", result_text(&result));
        let body = result_json(&result);
        let changes = body["candidate_sheet_pin_changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["pin_uuid"], json!("right-a"));
        assert_eq!(changes[0]["to_side"], json!("LEFT"));
        assert!(body["plan_revision"]
            .as_str()
            .unwrap()
            .starts_with("materialize-schematic-net:"));
    }

    #[tokio::test]
    async fn optimize_schematic_layout_offers_vertical_peer_facing_sheet_pin_state() {
        let (_dir, path) = vertical_sheet_fixture("sheet_vertical");
        let args = json!({
            "schematic": path.display().to_string(),
            "net_names": ["SIG_V"],
            "candidate_count": 5,
            "dry_run": true
        });
        let result = handle_optimize_schematic_layout(&args, &test_ctx())
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result_text(&result));
        let body = result_json(&result);
        let candidate = body["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| {
                !candidate["sheet_pin_changes"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            })
            .expect("expected a sheet-pin side candidate");
        let changes = candidate["sheet_pin_changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["pin_uuid"], json!("bottom-v"));
        assert_eq!(changes[0]["from_side"], json!("BOTTOM"));
        assert_eq!(changes[0]["to_side"], json!("TOP"));
        assert_eq!(changes[0]["to"], json!({"x": 44.45, "y": 90.0}));
    }

    #[tokio::test]
    async fn already_correct_sheet_pins_do_not_create_side_state() {
        let (_dir, path) = horizontal_sheet_fixture("sheet_correct", true);
        let args = json!({
            "schematic": path.display().to_string(),
            "net_names": ["SIG_A"],
            "candidate_count": 5,
            "dry_run": true
        });
        let result = handle_optimize_schematic_layout(&args, &test_ctx())
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result_text(&result));
        let body = result_json(&result);
        assert!(body["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|candidate| candidate["sheet_pin_changes"]
                .as_array()
                .unwrap()
                .is_empty()));
    }

    #[test]
    fn multi_signal_sheet_peer_interface_moves_coherently() {
        let (_dir, path) = horizontal_sheet_fixture("sheet_multi", false);
        let (before, tree) = read_schematic(&path).unwrap();
        let wires = extract_wires(&tree);
        let labels = konnect_sexp::schematic::extract_all_net_labels(&tree);
        let grid = 1.27;
        let mut endpoints_by_net = BTreeMap::new();
        for net in ["SIG_A", "SIG_B", "SIG_C"] {
            endpoints_by_net.insert(
                net.to_string(),
                net_endpoints_for(&tree, net, &json!({}), grid).unwrap(),
            );
        }
        let changes =
            composite_peer_facing_sheet_pin_changes(&before, &path, &endpoints_by_net, grid)
                .unwrap();
        assert_eq!(
            changes
                .iter()
                .map(|change| change.pin_uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["right-a", "right-b", "right-c"]
        );
        assert!(changes
            .iter()
            .all(|change| change.to_side == SheetPinSide::Left));
        assert_eq!(
            changes.iter().map(|change| change.to_y).collect::<Vec<_>>(),
            vec![25.4, 30.48, 35.56]
        );
        assert!(!changes.iter().any(|change| change.pin_uuid == "right-keep"));
        assert!(!wires.is_empty());
        assert!(!labels.is_empty());
    }

    #[test]
    fn composite_sheet_peer_selection_is_net_aware_for_three_sheets() {
        let (_dir, path) = three_sheet_peer_fixture("three_peer");
        let (before, tree) = read_schematic(&path).unwrap();
        let grid = 1.27;
        let mut endpoints_by_net = BTreeMap::new();
        for net in ["NET_AB", "NET_AC"] {
            endpoints_by_net.insert(
                net.to_string(),
                net_endpoints_for(&tree, net, &json!({}), grid).unwrap(),
            );
        }
        let changes =
            composite_peer_facing_sheet_pin_changes(&before, &path, &endpoints_by_net, grid)
                .unwrap();
        let by_uuid = changes
            .iter()
            .map(|change| (change.pin_uuid.as_str(), change.to_side))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(by_uuid.get("a-ab"), Some(&SheetPinSide::Left));
        assert_eq!(by_uuid.get("a-ac"), Some(&SheetPinSide::Bottom));
        assert_eq!(by_uuid.get("c-ac"), Some(&SheetPinSide::Top));
    }

    #[test]
    fn shared_sheet_can_face_multiple_actual_peers() {
        let (_dir, path) = shared_sheet_peer_fixture("shared_peer");
        let (before, tree) = read_schematic(&path).unwrap();
        let grid = 1.27;
        let mut endpoints_by_net = BTreeMap::new();
        for net in ["NET_L", "NET_R"] {
            endpoints_by_net.insert(
                net.to_string(),
                net_endpoints_for(&tree, net, &json!({}), grid).unwrap(),
            );
        }
        let changes =
            composite_peer_facing_sheet_pin_changes(&before, &path, &endpoints_by_net, grid)
                .unwrap();
        let by_uuid = changes
            .iter()
            .map(|change| (change.pin_uuid.as_str(), change.to_side))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(by_uuid.get("center-l"), Some(&SheetPinSide::Left));
        assert_eq!(by_uuid.get("center-r"), Some(&SheetPinSide::Right));
    }

    #[test]
    fn conflicting_sheet_pin_state_is_rejected_by_uuid() {
        let (_dir, path) = three_sheet_peer_fixture("conflicting_peer");
        let before = std::fs::read_to_string(&path).unwrap();
        let mut endpoints_by_net = BTreeMap::new();
        endpoints_by_net.insert(
            "NET_AB".to_string(),
            vec![
                sheet_endpoint("sheet-a", "A", "a-ab", "NET_AB", 80.0, 45.0, 0.0),
                sheet_endpoint("sheet-b", "B", "b-ab", "NET_AB", 40.0, 45.0, 0.0),
            ],
        );
        endpoints_by_net.insert(
            "NET_AC".to_string(),
            vec![
                sheet_endpoint("sheet-a", "A", "a-ab", "NET_AB", 80.0, 45.0, 0.0),
                sheet_endpoint("sheet-c", "C", "c-ac", "NET_AC", 85.0, 85.0, 0.0),
            ],
        );
        let err = composite_peer_facing_sheet_pin_changes(&before, &path, &endpoints_by_net, 1.27)
            .unwrap_err();
        assert!(result_text(&err).contains("conflicting_sheet_pin_state"));
        assert!(result_text(&err).contains("a-ab"));
    }

    #[tokio::test]
    async fn optimize_schematic_layout_rejects_stale_candidate_apply() {
        let body = [
            label("NODE", 50.0, 50.0, "l1"),
            label("NODE", 100.0, 50.0, "l2"),
            one_pin("J1", "CONN", 50.0, 50.0, "j1"),
            one_pin("U1", "IC", 100.0, 50.0, "u1"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("stale_topk", &body);
        let dry = handle_optimize_schematic_layout(
            &json!({
                "schematic": path.display().to_string(),
                "net_names": ["NODE"],
                "candidate_count": 3,
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let preview = result_json(&dry);
        let before = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, before.replace("\"CONN\"", "\"CONN_STALE\"")).unwrap();
        let changed = std::fs::read_to_string(&path).unwrap();
        let applied = handle_optimize_schematic_layout(
            &json!({
                "schematic": path.display().to_string(),
                "net_names": ["NODE"],
                "candidate_count": 3,
                "dry_run": false,
                "candidate_id": preview["candidates"][0]["candidate_id"].as_str().unwrap(),
                "plan_revision": preview["plan_revision"].as_str().unwrap()
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(applied.is_error);
        assert!(result_text(&applied).contains("stale_plan_revision"));
        assert_eq!(changed, std::fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn bounded_router_finds_route_requiring_more_than_two_bends() {
        let obstacles = MaterializeObstacles {
            bodies: vec![
                RectObstacle {
                    min_x: 1.5,
                    min_y: -1.0,
                    max_x: 2.5,
                    max_y: 3.0,
                },
                RectObstacle {
                    min_x: 3.5,
                    min_y: -3.0,
                    max_x: 4.5,
                    max_y: 1.0,
                },
            ],
            foreign_pins: Vec::new(),
            foreign_wires: Vec::new(),
        };
        let route = obstacle_free_manhattan_route(&[(0.0, 0.0), (6.0, 0.0)], 1.0, &obstacles)
            .expect("search should route through the bounded maze");
        assert!(
            route.len() >= 3,
            "fixture should require a routed detour through the bounded search: {route:?}"
        );
        assert!(
            route.iter().all(|segment| obstacles.allows(*segment)),
            "{route:?}"
        );
    }

    #[test]
    fn bounded_router_reports_no_route_when_target_is_sealed() {
        let obstacles = MaterializeObstacles {
            bodies: vec![
                RectObstacle {
                    min_x: 3.4,
                    min_y: -0.4,
                    max_x: 3.6,
                    max_y: 0.4,
                },
                RectObstacle {
                    min_x: 4.4,
                    min_y: -0.4,
                    max_x: 4.6,
                    max_y: 0.4,
                },
                RectObstacle {
                    min_x: 3.6,
                    min_y: 0.4,
                    max_x: 4.4,
                    max_y: 0.6,
                },
                RectObstacle {
                    min_x: 3.6,
                    min_y: -0.6,
                    max_x: 4.4,
                    max_y: -0.4,
                },
            ],
            foreign_pins: Vec::new(),
            foreign_wires: Vec::new(),
        };
        assert!(
            obstacle_free_manhattan_route(&[(0.0, 0.0), (4.0, 0.0)], 1.0, &obstacles).is_none()
        );
    }

    #[tokio::test]
    async fn refactor_passive_boundary_remains_absolute() {
        let (_dir, path) = ltc_fixture();
        let result = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "LTC_SHDN",
                "representation": "wired_local",
                "pins": [
                    { "reference": "R16", "pin_number": "1" },
                    { "reference": "U2", "pin_number": "6" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert!(result_text(&result).contains("R16.1"));
        assert!(result_text(&result).contains("AUX_5V"));
    }

    #[tokio::test]
    async fn refactor_expands_coincident_endpoint_with_move_and_wire() {
        let (_dir, path) = ltc_fixture();
        let mut content = std::fs::read_to_string(&path).unwrap();
        content = content
            .replace("(at 90 50 0)\n    (unit 1)", "(at 55 50 0)\n    (unit 1)")
            .replace("(at 90 48 0)", "(at 55 48 0)")
            .replace("(at 90 52 0)", "(at 55 52 0)")
            .replace(
                "(label \"LTC_SHDN\" (at 90 50 0)",
                "(label \"LTC_SHDN\" (at 55 50 0)",
            );
        std::fs::write(&path, content).unwrap();
        let before = semantic_snapshot(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "LTC_SHDN",
                "representation": "expand_coincident_connection",
                "expand": { "move_reference": "U2", "dx": 20.32, "dy": 0.0 },
                "remove_redundant_local_labels": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let preview = result_json(&dry);
        assert_eq!(preview["proposed_moves"][0]["reference"], json!("U2"));
        assert_eq!(preview["proposed_wires"].as_array().unwrap().len(), 1);

        let applied = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "LTC_SHDN",
                "representation": "expand_coincident_connection",
                "expand": { "move_reference": "U2", "dx": 20.32, "dy": 0.0 },
                "remove_redundant_local_labels": true,
                "dry_run": false,
                "plan_revision": preview["plan_revision"].as_str().unwrap()
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!applied.is_error, "{}", result_text(&applied));
        let after_text = std::fs::read_to_string(&path).unwrap();
        assert!(after_text.contains("(at 75.32 50 0)"));
        assert!(after_text.contains("(wire"));
        let after = semantic_snapshot(&after_text).unwrap();
        assert_eq!(before.pin_nets, after.pin_nets);
        assert_eq!(before.shorts, after.shorts);
    }

    #[tokio::test]
    async fn refactor_reports_off_grid_endpoint_dependency() {
        let body = [
            label("OFFGRID", 50.13, 50.0, "l1"),
            label("OFFGRID", 80.0, 50.0, "l2"),
            one_pin("TP1", "TP", 50.13, 50.0, "tp1"),
            one_pin("U1", "IC", 80.0, 50.0, "u1"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("offgrid", &body);
        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "OFFGRID",
                "representation": "wired_local",
                "grid": 1.27
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let preview = result_json(&dry);
        let endpoints = preview["endpoints"].as_array().unwrap();
        assert!(endpoints.iter().any(|endpoint| {
            endpoint["reference"] == "TP1"
                && endpoint["on_grid"] == false
                && endpoint["attachment_depends_on_off_grid_position"] == true
        }));
    }

    #[tokio::test]
    async fn power_symbol_conversion_requires_explicit_global_scope_policy() {
        let body = [
            label("+5V_MAIN", 50.0, 50.0, "l1"),
            label("+5V_MAIN", 80.0, 50.0, "l2"),
            one_pin("U1", "IC", 50.0, 50.0, "u1"),
            one_pin("J1", "CONN", 80.0, 50.0, "j1"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("powerpolicy", &body);
        let before = std::fs::read_to_string(&path).unwrap();
        let result = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "+5V_MAIN",
                "representation": "power_symbol"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert!(result_text(&result).contains("allow_global_power_scope"));
        assert_eq!(before, std::fs::read_to_string(&path).unwrap());
    }

    #[tokio::test]
    async fn arbitrary_power_rail_converts_with_embedded_symbol_and_preserved_membership() {
        let body = [
            label("+5V_MAIN", 50.0, 50.0, "l1"),
            label("+5V_MAIN", 80.0, 50.0, "l2"),
            one_pin("U1", "IC", 50.0, 50.0, "u1"),
            one_pin("J1", "CONN", 80.0, 50.0, "j1"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("custom_power", &body);
        let before = semantic_snapshot(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "+5V_MAIN",
                "representation": "power_symbol",
                "scope_policy": "allow_global_power_scope"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let preview = result_json(&dry);
        let applied = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "+5V_MAIN",
                "representation": "power_symbol",
                "scope_policy": "allow_global_power_scope",
                "dry_run": false,
                "plan_revision": preview["plan_revision"].as_str().unwrap()
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!applied.is_error, "{}", result_text(&applied));
        let after_text = std::fs::read_to_string(&path).unwrap();
        assert!(!after_text.contains("(symbol \"power:+5V_MAIN\""));
        assert!(after_text.contains("(symbol \"power:+5V\""));
        assert!(after_text.contains("(lib_id \"power:+5V\")"));
        assert!(after_text.contains("(property \"Value\" \"+5V_MAIN\""));
        assert_eq!(after_text.matches("(label \"+5V_MAIN\"").count(), 0);
        let after = semantic_snapshot(&after_text).unwrap();
        assert_eq!(before.pin_nets, after.pin_nets);
        assert_eq!(before.shorts, after.shorts);
    }

    #[tokio::test]
    async fn custom_power_rail_values_use_resolvable_stock_templates() {
        for (net, template) in [
            ("+5V_MAIN", "power:+5V"),
            ("VCC_3V3_S0", "power:VCC"),
            ("TEST_CUSTOM_RAIL", "power:VCC"),
        ] {
            let body = [
                label(net, 50.0, 50.0, "l1"),
                label(net, 80.0, 50.0, "l2"),
                one_pin("U1", "IC", 50.0, 50.0, "u1"),
                one_pin("J1", "CONN", 80.0, 50.0, "j1"),
            ]
            .join("\n");
            let (_dir, path) = one_pin_net_fixture(net, &body);
            let before = semantic_snapshot(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let dry = handle_refactor_schematic_net_representation(
                &json!({
                    "schematic": path.display().to_string(),
                    "net": net,
                    "representation": "power_symbol",
                    "scope_policy": "allow_global_power_scope"
                }),
                &test_ctx(),
            )
            .await
            .unwrap();
            assert!(!dry.is_error, "{}: {}", net, result_text(&dry));
            let preview = result_json(&dry);
            let applied = handle_refactor_schematic_net_representation(
                &json!({
                    "schematic": path.display().to_string(),
                    "net": net,
                    "representation": "power_symbol",
                    "scope_policy": "allow_global_power_scope",
                    "dry_run": false,
                    "plan_revision": preview["plan_revision"].as_str().unwrap()
                }),
                &test_ctx(),
            )
            .await
            .unwrap();
            assert!(!applied.is_error, "{}: {}", net, result_text(&applied));
            let after_text = std::fs::read_to_string(&path).unwrap();
            assert!(
                !after_text.contains(&format!("(symbol \"power:{net}\"")),
                "custom rail must not invent a custom library symbol:\n{after_text}"
            );
            assert!(after_text.contains(&format!("(lib_id \"{template}\")")));
            assert!(after_text.contains(&format!("(property \"Value\" \"{net}\"")));
            let after = semantic_snapshot(&after_text).unwrap();
            assert_eq!(before.pin_nets, after.pin_nets, "{net}");
            assert_eq!(before.shorts, after.shorts, "{net}");
        }
    }

    #[tokio::test]
    async fn off_grid_arbitrary_power_rails_attach_to_grid_symbol_with_wire() {
        for (net, template) in [("+5V_MAIN", "power:+5V"), ("VCC_3V3_S0", "power:VCC")] {
            let body = [
                label(net, 51.0, 50.8, "l1"),
                one_pin("U1", "IC", 51.0, 50.8, "u1"),
            ]
            .join("\n");
            let (_dir, path) = one_pin_net_fixture(net, &body);
            let before = semantic_snapshot(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let dry = handle_refactor_schematic_net_representation(
                &json!({
                    "schematic": path.display().to_string(),
                    "net": net,
                    "representation": "power_symbol",
                    "scope_policy": "allow_global_power_scope",
                    "grid": 1.27
                }),
                &test_ctx(),
            )
            .await
            .unwrap();
            assert!(!dry.is_error, "{}: {}", net, result_text(&dry));
            let preview = result_json(&dry);
            let proposed = preview["proposed_power_symbols"].as_array().unwrap();
            assert_eq!(proposed.len(), 1, "{preview}");
            assert_eq!(
                proposed[0]["normalized_from"],
                json!({"x": 51.0, "y": 50.8})
            );
            assert!(point_on_grid(
                proposed[0]["x"].as_f64().unwrap(),
                proposed[0]["y"].as_f64().unwrap(),
                1.27
            ));
            assert_eq!(proposed[0]["template_lib_id"], json!(template));
            assert!(
                proposed[0]["attachment_wires"].as_array().unwrap().len() >= 1,
                "{preview}"
            );

            let applied = handle_refactor_schematic_net_representation(
                &json!({
                    "schematic": path.display().to_string(),
                    "net": net,
                    "representation": "power_symbol",
                    "scope_policy": "allow_global_power_scope",
                    "grid": 1.27,
                    "dry_run": false,
                    "plan_revision": preview["plan_revision"].as_str().unwrap()
                }),
                &test_ctx(),
            )
            .await
            .unwrap();
            assert!(!applied.is_error, "{}: {}", net, result_text(&applied));
            let after_text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(after_text.matches(&format!("(label \"{net}\"")).count(), 0);
            assert!(after_text.contains("(wire"));
            assert!(after_text.contains(&format!("(lib_id \"{template}\")")));
            let after = semantic_snapshot(&after_text).unwrap();
            assert_eq!(before.pin_nets, after.pin_nets, "{net}");
            assert_eq!(before.shorts, after.shorts, "{net}");
        }
    }

    #[tokio::test]
    async fn gnd_label_normalizes_to_power_symbol_with_membership_preserved() {
        let body = [
            label("GND", 50.8, 50.8, "l1"),
            label("GND", 80.01, 50.8, "l2"),
            one_pin("U1", "IC", 50.8, 50.8, "u1"),
            one_pin("J1", "CONN", 80.01, 50.8, "j1"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("gnd", &body);
        let before = semantic_snapshot(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "GND",
                "representation": "power_symbol",
                "scope_policy": "allow_global_power_scope"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let preview = result_json(&dry);
        assert_eq!(
            preview["proposed_power_symbols"].as_array().unwrap().len(),
            2
        );
        let applied = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "GND",
                "representation": "power_symbol",
                "scope_policy": "allow_global_power_scope",
                "dry_run": false,
                "plan_revision": preview["plan_revision"].as_str().unwrap()
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!applied.is_error, "{}", result_text(&applied));
        let after_text = std::fs::read_to_string(&path).unwrap();
        assert!(after_text.contains("(lib_id \"power:GND\")"));
        assert_eq!(after_text.matches("(label \"GND\"").count(), 0);
        let after = semantic_snapshot(&after_text).unwrap();
        assert_eq!(before.pin_nets, after.pin_nets);
        assert_eq!(before.shorts, after.shorts);
    }

    #[tokio::test]
    async fn off_grid_gnd_label_normalizes_to_grid_point_on_same_net() {
        let body = [
            label("GND", 51.0, 50.8, "l1"),
            label("GND", 80.01, 50.8, "l2"),
            one_pin("U1", "IC", 50.8, 50.8, "u1"),
            one_pin("J1", "CONN", 80.01, 50.8, "j1"),
            r#"  (wire (pts (xy 50.8 50.8) (xy 80.01 50.8)) (uuid "w1"))"#.to_string(),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("gnd_offgrid", &body);
        let before = semantic_snapshot(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "GND",
                "representation": "power_symbol",
                "scope_policy": "allow_global_power_scope",
                "grid": 1.27
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let preview = result_json(&dry);
        let proposed = preview["proposed_power_symbols"].as_array().unwrap();
        assert!(
            proposed
                .iter()
                .any(|p| p["normalized_from"] == json!({"x": 51.0, "y": 50.8})),
            "{preview}"
        );
        assert!(
            proposed.iter().all(|p| point_on_grid(
                p["x"].as_f64().unwrap(),
                p["y"].as_f64().unwrap(),
                1.27
            )),
            "{preview}"
        );
        let applied = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "GND",
                "representation": "power_symbol",
                "scope_policy": "allow_global_power_scope",
                "grid": 1.27,
                "dry_run": false,
                "plan_revision": preview["plan_revision"].as_str().unwrap()
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!applied.is_error, "{}", result_text(&applied));
        let after_text = std::fs::read_to_string(&path).unwrap();
        assert!(!after_text.contains("(at 51 50.8"));
        let after = semantic_snapshot(&after_text).unwrap();
        assert_eq!(before.pin_nets, after.pin_nets);
        assert_eq!(before.shorts, after.shorts);
    }

    #[tokio::test]
    async fn scope_diagnostic_reports_mixed_label_and_power_mechanisms() {
        let body = [
            label("GND", 50.0, 50.0, "l1"),
            global_label("GND", 80.0, 50.0, "g1"),
            placed_power("GND", "#PWR001", 110.0, 50.0, "pwr1"),
            one_pin("U1", "IC", 50.0, 50.0, "u1"),
        ]
        .join("\n");
        let (_dir, path) = one_pin_net_fixture("mixedscope", &body);
        let result = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "GND",
                "representation": "scope_diagnostic"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", result_text(&result));
        let diagnostic = result_json(&result);
        assert_eq!(diagnostic["ordinary_labels"], json!(1));
        assert_eq!(diagnostic["global_labels"], json!(1));
        assert_eq!(diagnostic["power_symbols"], json!(1));
        assert_eq!(diagnostic["mixed_connection_mechanisms"], json!(true));
    }

    #[tokio::test]
    async fn refactor_apply_is_transactional_on_stale_revision() {
        let (_dir, path) = ltc_fixture();
        let dry = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "LTC_SHDN",
                "representation": "wired_local",
                "remove_redundant_local_labels": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let preview = result_json(&dry);
        let before = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, before.replace("\"10k\"", "\"11k\"")).unwrap();
        let changed = std::fs::read_to_string(&path).unwrap();

        let applied = handle_refactor_schematic_net_representation(
            &json!({
                "schematic": path.display().to_string(),
                "net": "LTC_SHDN",
                "representation": "wired_local",
                "remove_redundant_local_labels": true,
                "dry_run": false,
                "plan_revision": preview["plan_revision"].as_str().unwrap()
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(applied.is_error);
        assert_eq!(changed, std::fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn materialize_schematic_net_tool_is_registered() {
        let tool = tools()
            .into_iter()
            .find(|tool| tool.name == "materialize_schematic_net")
            .expect("tool is registered");
        assert!(tool.description.contains("net-aware"));
        assert!(tool.input_schema["properties"]["remove_redundant_local_labels"].is_object());
        assert!(tools()
            .into_iter()
            .any(|tool| tool.name == "refactor_schematic_net_representation"));
        let optimize = crate::tools::sch_layout::tools()
            .into_iter()
            .find(|tool| tool.name == "optimize_schematic_layout")
            .expect("generic Top-K tool is registered");
        assert_eq!(
            optimize.input_schema["properties"]["candidate_count"]["maximum"],
            json!(5)
        );
    }
}
