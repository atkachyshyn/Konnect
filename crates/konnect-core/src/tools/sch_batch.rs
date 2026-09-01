//! `sch_batch` toolset — bulk/batch operations on schematic elements.
//!
//! **Critical invariant**: every write handler performs a single file read,
//! collects ALL mutations as `SexpEdit` values against the original content,
//! then calls `write_atomic` exactly once. This fixes the Python bug where
//! `batch_connect_to_net` did N separate read/write cycles.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    find_all_symbol_instance_blocks, get_path, opt_str, project_name_for, require_array,
    require_f64, require_str, ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    geometry::{points_coincident, snap_point},
    schematic::{
        extract_all_net_labels, extract_labels, extract_symbol_instances, extract_wires,
        find_lib_symbol, format_net_label, format_wire, pin_endpoint, pin_label_rotation,
        read_schematic, symbol_bounds_for_instance, SymbolBounds,
    },
    writer::{
        apply_edits, find_balanced_block, find_block_starts, find_block_with_leading_whitespace,
        find_direct_child_blocks, find_enclosing_direct_child_block, new_uuid, read_consistent,
        write_atomic_if_unchanged, SexpEdit,
    },
    DocumentRevision,
};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};

use super::sch_connectivity::{ConnectivityIndex, COINCIDENT_TOLERANCE};
// Re-use the single-item component placer and pin-to-pin router.
use super::sch_components::place_one_component;
use super::sch_components::{component_metadata_edits, component_metadata_from_args};
use super::sch_wiring::{resolve_pin_endpoint, resolve_placed_pin, route_between};

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "batch_connect_to_net",
            "Connect multiple component pins to a named net by adding net labels at each pin \
             endpoint. Single file read → all labels inserted → single file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "net_name": { "type": "string", "description": "Name of the net to connect pins to" },
                    "pins": {
                        "type": "array",
                        "description": "List of {reference, pin_number} objects to connect",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "pin_number": { "type": "string" }
                            },
                            "required": ["reference", "pin_number"]
                        }
                    }
                },
                "required": ["schematic", "net_name", "pins"]
            }),
            |args, ctx| async move { handle_batch_connect_to_net(args, ctx).await }
        ),
        tool!(
            "batch_place_components",
            "Place multiple symbols from KiCAD libraries in a single file read/write cycle. \
             Pass explicit references -- there is no auto-numbering; an omitted reference \
             becomes '?' like an eeschema-unannotated symbol, same as add_schematic_component.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "components": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "lib_id": { "type": "string" },
                                "x": { "type": "number" }, "y": { "type": "number" },
                                "rotation": { "type": "number", "default": 0 },
                                "reference": { "type": "string" },
                                "value": { "type": "string" },
                                "unit": { "type": "integer", "default": 1 }
                            },
                            "required": ["lib_id", "x", "y"]
                        }
                    }
                },
                "required": ["schematic", "components"]
            }),
            |args, ctx| async move { handle_batch_place_components(args, ctx).await }
        ),
        tool!(
            "batch_connect_pins",
            "Connect multiple component pin pairs by reference and pin number, in a single \
             file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "connections": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "ref1": { "type": "string" }, "pin1": { "type": "string" },
                                "ref2": { "type": "string" }, "pin2": { "type": "string" }
                            },
                            "required": ["ref1", "pin1", "ref2", "pin2"]
                        }
                    }
                },
                "required": ["schematic", "connections"]
            }),
            |args, ctx| async move { handle_batch_connect_pins(args, ctx).await }
        ),
        tool!(
            "batch_delete",
            "Delete multiple schematic items (wires, labels, junctions, components) by UUID \
             or component reference designator — single file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "uuids": {
                        "type": "array",
                        "description": "UUIDs of items to delete. Each entry may be a string UUID or an object with uuid and expected_type.",
                        "items": {
                            "oneOf": [
                                { "type": "string" },
                                {
                                    "type": "object",
                                    "properties": {
                                        "uuid": { "type": "string" },
                                        "expected_type": {
                                            "type": "string",
                                            "enum": ["symbol", "local_label", "hierarchical_label", "global_label", "wire", "junction", "no_connect", "text"]
                                        }
                                    },
                                    "required": ["uuid"]
                                }
                            ]
                        }
                    },
                    "references": {
                        "type": "array",
                        "description": "Component reference designators to delete",
                        "items": { "type": "string" }
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_batch_delete(args, ctx).await }
        ),
        tool!(
            "bulk_move_schematic_components",
            "Move multiple components by a uniform dx/dy offset in a single atomic file \
             write. Junction dots are re-judged where the pins moved: a dot the pins \
             leave unjustified is removed and a pin landing mid-span on a wire gains \
             one, reported as junctions_pruned_count and junctions_added_count.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "description": "Reference designators to move",
                        "items": { "type": "string" }
                    },
                    "dx": { "type": "number", "description": "X offset in mm" },
                    "dy": { "type": "number", "description": "Y offset in mm" }
                },
                "required": ["schematic", "references", "dx", "dy"]
            }),
            |args, ctx| async move { handle_bulk_move(args, ctx).await }
        ),
        tool!(
            "batch_edit_schematic_components",
            "Apply field updates (Value, Footprint, custom properties) and population metadata \
             to multiple components in a single atomic file write. Optional booleans preserve \
             existing state when omitted: dnp writes KiCad (dnp yes/no), exclude_from_bom writes \
             (in_bom no/yes), and exclude_from_board writes (on_board no/yes).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "edits": {
                        "type": "array",
                        "description": "List of {reference, value?, footprint?, fields?, dnp?, exclude_from_bom?, exclude_from_board?} edit objects",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "value": { "type": "string" },
                                "footprint": { "type": "string" },
                                "fields": {
                                    "type": "object",
                                    "description": "Additional property fields as key:value pairs"
                                },
                                "dnp": {
                                    "type": "boolean",
                                    "description": "Set or clear KiCad's Do Not Populate state for every placed unit. Omit to preserve existing state."
                                },
                                "exclude_from_bom": {
                                    "type": "boolean",
                                    "description": "Set true to write KiCad (in_bom no); false writes (in_bom yes). Omit to preserve existing state."
                                },
                                "exclude_from_board": {
                                    "type": "boolean",
                                    "description": "Set true to write KiCad (on_board no); false writes (on_board yes). Omit to preserve existing state."
                                }
                            },
                            "required": ["reference"]
                        }
                    }
                },
                "required": ["schematic", "edits"]
            }),
            |args, ctx| async move { handle_batch_edit(args, ctx).await }
        ),
        tool!(
            "batch_delete_schematic_components",
            "Delete multiple components by reference designator in a single atomic file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "description": "Reference designators to delete",
                        "items": { "type": "string" }
                    }
                },
                "required": ["schematic", "references"]
            }),
            |args, ctx| async move { handle_batch_delete_components(args, ctx).await }
        ),
        tool!(
            "connect_passthrough",
            "Add a wire stub and matching net label at a point to route a signal through \
             a region without drawing a full wire path. Direction controls stub orientation.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "net_name": { "type": "string", "description": "Net name for the passthrough label" },
                    "x": { "type": "number", "description": "X position of the stub root in mm" },
                    "y": { "type": "number", "description": "Y position of the stub root in mm" },
                    "direction": {
                        "type": "string",
                        "description": "Stub direction. 'auto' (default) points it away from \
                                        the symbol body when a pin sits at (x, y), so the label \
                                        text does not run back across the symbol; it falls back \
                                        to 'right' on a bare point.",
                        "enum": ["auto", "right", "left", "up", "down"],
                        "default": "auto"
                    }
                },
                "required": ["schematic", "net_name", "x", "y"]
            }),
            |args, ctx| async move { handle_connect_passthrough(args, ctx).await }
        ),
        tool!(
            "add_schematic_text",
            "Add a text annotation (non-net label) to the schematic at a given position.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "text": { "type": "string", "description": "Text content to add" },
                    "x": { "type": "number", "description": "X position in mm" },
                    "y": { "type": "number", "description": "Y position in mm" },
                    "size": { "type": "number", "description": "Font size in mm", "default": 1.27 },
                    "rotation": { "type": "number", "description": "Rotation in degrees", "default": 0 },
                    "justify": {
                        "type": "string",
                        "description": "Alignment of the text against x/y: at most one horizontal token (left, right) and one vertical token (top, bottom), space separated. An axis you leave out is centred - KiCad has no 'center' keyword and encodes centring by omission, so 'bottom' means horizontally centred and bottom-aligned. 'center' is shorthand for centring both axes. Defaults to 'left bottom', what KiCad itself writes for a placed annotation; a centred horizontal axis can carry a long line off the page.",
                        "default": "left bottom"
                    }
                },
                "required": ["schematic", "text", "x", "y"]
            }),
            |args, ctx| async move { handle_add_schematic_text(args, ctx).await }
        ),
        tool!(
            "get_schematic_layout",
            "Return a compact spatial summary of the schematic: component positions, \
             transformed drawing/pin bounds (excluding free text), and optionally wire segments \
             and label locations. Reports any component whose embedded library geometry could \
             not be resolved.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "include_wires": { "type": "boolean", "description": "Include wire data", "default": true },
                    "include_labels": { "type": "boolean", "description": "Include label data", "default": true }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_get_layout(args, ctx).await }
        ),
        tool!(
            "inspect_schematic_selection_layout",
            "Return bounds for selected schematic items plus page, drawing-frame, safe \
             printable region, title-block reserved area, margin, and out-of-bounds diagnostics. \
             Select by component references, item UUIDs, or a bounding box.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "selection": {
                        "type": "object",
                        "properties": {
                            "references": { "type": "array", "items": { "type": "string" } },
                            "uuids": { "type": "array", "items": { "type": "string" } },
                            "bbox": { "description": "Either [min_x,min_y,max_x,max_y] or {min_x,min_y,max_x,max_y}" }
                        },
                        "additionalProperties": false
                    },
                    "safe_margin": { "type": "number", "description": "Margin inside page frame in mm", "default": 10.0 }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_inspect_selection_layout(args, ctx).await }
        ),
        tool!(
            "arrange_schematic_selection",
            "Preview or apply a generic schematic layout operation to selected items. Supports \
             translate, move_to_anchor, align, and distribute while preserving each item's internal \
             geometry. Apply mode revalidates component-pin endpoint membership and shorts before \
             writing, so moving only a component away from its labels/wires is refused.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "selection": {
                        "type": "object",
                        "properties": {
                            "references": { "type": "array", "items": { "type": "string" } },
                            "uuids": { "type": "array", "items": { "type": "string" } },
                            "bbox": { "description": "Either [min_x,min_y,max_x,max_y] or {min_x,min_y,max_x,max_y}" }
                        },
                        "additionalProperties": false
                    },
                    "operation": {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["translate", "move_to_anchor", "align", "distribute"] },
                            "dx": { "type": "number" },
                            "dy": { "type": "number" },
                            "anchor": { "type": "string", "enum": ["top_left", "center", "bottom_right"], "default": "top_left" },
                            "x": { "type": "number" },
                            "y": { "type": "number" },
                            "edge": { "type": "string", "enum": ["left", "right", "top", "bottom", "horizontal_center", "vertical_center"] },
                            "axis": { "type": "string", "enum": ["horizontal", "vertical"] },
                            "spacing": { "type": "number", "description": "Optional edge-to-edge spacing for distribution. Defaults to preserving the current span." }
                        },
                        "required": ["type"],
                        "additionalProperties": false
                    },
                    "dry_run": { "type": "boolean", "description": "Default true. false applies the already-reviewed plan.", "default": true },
                    "plan_revision": { "type": "string", "description": "Exact dry-run revision required when dry_run=false" }
                },
                "required": ["schematic", "selection", "operation"]
            }),
            |args, ctx| async move { handle_arrange_schematic_selection(args, ctx).await }
        ),
        tool!(
            "validate_wire_connections",
            "Check all wire endpoints for floating ends (not connected to a pin, label, \
             or another wire). Reports each floating endpoint with its coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "tolerance": { "type": "number", "description": "Snap tolerance in mm", "default": 0.01 }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_validate_wire_connections(args, ctx).await }
        ),
        tool!(
            "validate_component_connections",
            "Check that every connectable pin on every component has at least one wire \
             or label connected. Symbol pins typed no_connect and pins carrying a \
             no-connect marker are exempt. Reports unconnected pins with reference, \
             pin number, electrical type, and schematic position.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "ignore_power_pins": {
                        "type": "boolean",
                        "description": "Skip power-type pins in the check",
                        "default": false
                    },
                    "references": {
                        "type": "array",
                        "description": "Limit check to these reference designators (empty = all)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_validate_component_connections(args, ctx).await }
        ),
    ]
}

// ─── Private helpers ──────────────────────────────────────────────────────────

/// Find every `(symbol ...)` block for a reference designator, each with its
/// leading whitespace so deletion leaves clean formatting.
///
/// One entry per unit: deleting a multi-unit part means deleting all of them.
/// Returns `(block_start, block_end)` byte offsets in `content`.
fn find_symbol_blocks(content: &str, reference: &str) -> Vec<(usize, usize)> {
    find_all_symbol_instance_blocks(content, reference)
        .into_iter()
        .filter_map(|(sym_start, _)| find_block_with_leading_whitespace(content, sym_start))
        .collect()
}

/// Return `(val_start, val_end)` byte offsets in `content` for the *value* portion
/// of a `(property "FieldName" "VALUE" ...)` node, once per placed instance of
/// `reference`. Only the bytes inside the opening quote are included (i.e. the
/// replacement does NOT need to include surrounding quotes).
///
/// Multi-unit parts repeat their fields in every unit's block and KiCad expects
/// those copies to agree, so a field edit has to rewrite all of them.
fn field_value_ranges(content: &str, reference: &str, field: &str) -> Vec<(usize, usize)> {
    find_all_symbol_instance_blocks(content, reference)
        .into_iter()
        .filter_map(|(sym_start, sym_end)| {
            let sym_block = &content[sym_start..sym_end];

            let field_search = format!(r#"(property "{field}" ""#);
            let field_rel = sym_block.find(&field_search)?;
            let val_start = sym_start + field_rel + field_search.len();
            // find the closing quote of the current value
            let val_end = val_start + content[val_start..].find('"')?;
            Some((val_start, val_end))
        })
        .collect()
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_batch_connect_to_net(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pins = match args["pins"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'pins' array")),
    };

    let (content, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let mut inserts = String::new();
    let mut added: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    // Endpoints already carrying this net's label, so a second never lands
    // on the first. Seeded from the file, extended as we go.
    let mut labelled: Vec<(f64, f64)> = extract_labels(&tree)
        .iter()
        .filter(|l| l.net == net_name)
        .map(|l| (l.x, l.y))
        .collect();

    for pin_spec in &pins {
        let reference = match pin_spec["reference"].as_str() {
            Some(r) => r,
            None => {
                errors.push("Missing 'reference' in pin spec".into());
                continue;
            }
        };
        let pin_number = match pin_spec["pin_number"].as_str() {
            Some(p) => p,
            None => {
                errors.push("Missing 'pin_number' in pin spec".into());
                continue;
            }
        };

        let (pin, t) = match resolve_placed_pin(&instances, &lib_syms, reference, pin_number) {
            Ok(p) => p,
            Err(e) => {
                errors.push(e.to_string());
                continue;
            }
        };
        let (px, py) = pin_endpoint(&pin, t);
        let rotation = pin_label_rotation(&pin, t);

        // Symbols stack several pins on one endpoint; a label each renders as
        // a smear. They stay connected by that endpoint.
        let duplicate = labelled
            .iter()
            .any(|(lx, ly)| points_coincident(*lx, *ly, px, py, 0.01));
        if !duplicate {
            inserts.push_str(&format_net_label(&net_name, px, py, rotation));
            labelled.push((px, py));
        }
        let mut entry = json!({
            "reference": reference,
            "pin": pin_number,
            "x": px,
            "y": py,
            "rotation": rotation
        });
        if duplicate {
            entry["deduplicated"] = json!(true);
        }
        added.push(entry);
    }

    if !inserts.is_empty() {
        let expected = content.clone();
        // Labels are element class 2; symbol instances MUST come last, so a
        // splice at the file's final `)` puts them after the instances and
        // KiCad refuses the whole file (#156, same bug as add_schematic_text).
        let new_content = crate::tools::sch_wiring::insert_before_close(&content, &inserts);
        write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    }

    Ok(CallToolResult::json(&json!({
        "net": net_name,
        "added": added,
        "added_count": added.len(),
        "errors": errors
    })))
}

/// Extract the message text out of a `CallToolResult` error, for folding a
/// single-item handler's structured error into a batch tool's `errors` list.
fn error_text(result: &CallToolResult) -> String {
    match result.content.first() {
        Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
        _ => "unknown error".to_string(),
    }
}

async fn handle_batch_place_components(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let components = match args["components"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'components' array")),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;
    let root_uuid = crate::tools::ensure_root_uuid(&mut sch);
    let project_name = project_name_for(&sch_path);
    // Built once: the lib-table parse is memoised across the whole batch.
    let src = crate::tools::library::KiCadSymbolSource::for_file(&sch_path);

    let mut placed: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for comp in &components {
        let Some(lib_id) = comp["lib_id"].as_str() else {
            errors.push("Missing 'lib_id' in component spec".into());
            continue;
        };
        let (Some(x), Some(y)) = (comp["x"].as_f64(), comp["y"].as_f64()) else {
            errors.push(format!("Missing 'x'/'y' for '{}'", lib_id));
            continue;
        };
        let rotation = comp["rotation"].as_f64().unwrap_or(0.0);
        let reference = comp["reference"].as_str().unwrap_or("?");
        let value = comp["value"].as_str();
        let unit = comp["unit"].as_f64().unwrap_or(1.0) as u32;

        match place_one_component(
            &mut sch,
            &root_uuid,
            &project_name,
            lib_id,
            x,
            y,
            rotation,
            reference,
            value,
            unit,
            &src,
        ) {
            Ok(v) => placed.push(v),
            Err(e) => errors.push(error_text(&e)),
        }
    }

    if !placed.is_empty() {
        sch.overwrite()?;
    }

    let mut result = CallToolResult::json(&json!({
        "placed": placed,
        "placed_count": placed.len(),
        "errors": errors
    }));
    result.is_error = placed.is_empty() && !errors.is_empty();
    Ok(result)
}

async fn handle_batch_connect_pins(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let connections = match args["connections"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'connections' array")),
    };

    let (content, tree) = read_schematic(&sch_path)?;
    let expected = content.clone();
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // Resolve every endpoint from the initial tree before any wire is
    // inserted -- symbols/lib_symbols never change as wires are added, so
    // this is safe to do up front instead of re-resolving per connection.
    let mut resolved: Vec<(f64, f64, f64, f64)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for conn in &connections {
        let (Some(ref1), Some(pin1), Some(ref2), Some(pin2)) = (
            conn["ref1"].as_str(),
            conn["pin1"].as_str(),
            conn["ref2"].as_str(),
            conn["pin2"].as_str(),
        ) else {
            errors.push("Missing ref1/pin1/ref2/pin2 in connection spec".into());
            continue;
        };
        match (
            resolve_pin_endpoint(&instances, &lib_syms, ref1, pin1),
            resolve_pin_endpoint(&instances, &lib_syms, ref2, pin2),
        ) {
            (Ok((x1, y1)), Ok((x2, y2))) => resolved.push((x1, y1, x2, y2)),
            (Err(e), _) | (_, Err(e)) => errors.push(e.to_string()),
        }
    }

    // ponytail: re-parses content per wire; incremental tree edits if batches get huge.
    let mut new_content = content;
    for (x1, y1, x2, y2) in &resolved {
        new_content = route_between(new_content, *x1, *y1, *x2, *y2);
    }

    if !resolved.is_empty() {
        write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    }

    let mut result = CallToolResult::json(&json!({
        "connected_count": resolved.len(),
        "errors": errors
    }));
    result.is_error = resolved.is_empty() && !errors.is_empty();
    Ok(result)
}

async fn handle_batch_delete(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let content = read_consistent(&sch_path)?;
    let expected = content.clone();

    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut delete_ranges: HashSet<(usize, usize)> = HashSet::new();

    // TODO(KICAD_IPC):
    // Replace this KiCad-10 compatibility deletion path when the minimum
    // supported KiCad release provides reliable typed DeleteItems for arbitrary
    // schematic items/sheets.
    //
    // Delete by UUID — walk back from uuid node to enclosing top-level block.
    // Typed requests are preflighted: an expected-type mismatch must fail
    // without applying any other mutation in the same batch.
    if let Some(uuids) = args["uuids"].as_array() {
        let mut requests = Vec::new();
        for (index, uuid_val) in uuids.iter().enumerate() {
            let Some((uuid, expected_type)) = parse_uuid_delete_request(uuid_val) else {
                errors.push(format!(
                    "uuids[{index}] must be a string UUID or an object with uuid"
                ));
                continue;
            };
            if let Some(expected_type) = expected_type {
                let pattern = format!(r#"(uuid "{}")"#, uuid);
                let Some(uuid_pos) = content.find(&pattern) else {
                    errors.push(format!("UUID '{}' not found", uuid));
                    continue;
                };
                let Some((block_start, block_end)) =
                    find_enclosing_direct_child_block(&content, "kicad_sch", uuid_pos)
                else {
                    errors.push(format!("Cannot locate block for UUID '{}'", uuid));
                    continue;
                };
                let item = &content[block_start..block_end];
                let actual_type = schematic_item_type(item);
                if Some(expected_type) != actual_type {
                    return Ok(CallToolResult::error(format!(
                        "UUID '{}' has type '{}', not expected_type '{}'; no items deleted",
                        uuid,
                        actual_type.unwrap_or("unknown"),
                        expected_type
                    )));
                }
            }
            requests.push((uuid.to_string(), expected_type.map(str::to_string)));
        }

        for (uuid, _expected_type) in requests {
            let pattern = format!(r#"(uuid "{}")"#, uuid);
            match content.find(&pattern) {
                Some(uuid_pos) => {
                    match find_enclosing_direct_child_block(&content, "kicad_sch", uuid_pos) {
                        Some((block_start, block_end)) => {
                            let item = &content[block_start..block_end];
                            if !is_deletable_schematic_item(item) {
                                errors.push(format!(
                                    "UUID '{}' belongs to protected schematic structure '{}'",
                                    uuid,
                                    sexp_tag(item)
                                ));
                                continue;
                            }
                            match find_block_with_leading_whitespace(&content, block_start) {
                                Some((del_start, del_end)) => {
                                    if delete_ranges.insert((del_start, del_end)) {
                                        edits.push(SexpEdit::delete(del_start, del_end));
                                        deleted.push(uuid.clone());
                                    }
                                }
                                None => {
                                    errors.push(format!("Cannot parse block for UUID '{}'", uuid))
                                }
                            }
                        }
                        None => errors.push(format!("Cannot locate block for UUID '{}'", uuid)),
                    }
                }
                None => errors.push(format!("UUID '{}' not found", uuid)),
            }
        }
    }

    // Delete by reference designator
    if let Some(refs) = args["references"].as_array() {
        for ref_val in refs {
            let reference = match ref_val.as_str() {
                Some(r) => r,
                None => continue,
            };
            let blocks = find_symbol_blocks(&content, reference);
            if blocks.is_empty() {
                errors.push(format!("Component '{}' not found", reference));
                continue;
            }
            // Every unit of a multi-unit part, or the whole component is not gone.
            let mut any = false;
            for (del_start, del_end) in blocks {
                if delete_ranges.insert((del_start, del_end)) {
                    edits.push(SexpEdit::delete(del_start, del_end));
                    any = true;
                }
            }
            if any {
                deleted.push(reference.to_string());
            }
        }
    }

    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "deleted_count": deleted.len(),
        "deleted": deleted,
        "errors": errors
    })))
}

fn sexp_tag(block: &str) -> &str {
    let Some(after_open) = block.strip_prefix('(') else {
        return "";
    };
    let end = after_open
        .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .unwrap_or(after_open.len());
    &after_open[..end]
}

fn parse_uuid_delete_request(value: &serde_json::Value) -> Option<(&str, Option<&str>)> {
    value.as_str().map(|uuid| (uuid, None)).or_else(|| {
        let object = value.as_object()?;
        let uuid = object.get("uuid")?.as_str()?;
        let expected_type = object.get("expected_type").and_then(|v| v.as_str());
        Some((uuid, expected_type))
    })
}

fn schematic_item_type(block: &str) -> Option<&'static str> {
    match sexp_tag(block) {
        "symbol" => Some("symbol"),
        "label" => Some("local_label"),
        "hierarchical_label" => Some("hierarchical_label"),
        "global_label" => Some("global_label"),
        "wire" => Some("wire"),
        "junction" => Some("junction"),
        "no_connect" => Some("no_connect"),
        "text" => Some("text"),
        _ => None,
    }
}

// Blocklist of structural forms, not an allowlist of item kinds: deleting a
// drawing item (text, bus, sheet, image, polyline, …) by UUID has always
// worked and must keep working — only the schematic's skeleton is protected.
fn is_deletable_schematic_item(block: &str) -> bool {
    !matches!(
        sexp_tag(block),
        "version"
            | "generator"
            | "generator_version"
            | "uuid"
            | "paper"
            | "title_block"
            | "lib_symbols"
            | "sheet_instances"
            | "symbol_instances"
            | "embedded_fonts"
    )
}

// ─── Schematic layout selection and arrangement ──────────────────────────────

const LAYOUT_ITEM_TAGS: &[&str] = &[
    "symbol",
    "wire",
    "bus",
    "bus_entry",
    "junction",
    "label",
    "global_label",
    "hierarchical_label",
    "no_connect",
    "text",
    "text_box",
    "polyline",
    "rectangle",
    "circle",
    "arc",
    "image",
];

#[derive(Debug, Clone, Copy, PartialEq)]
struct LayoutBounds {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl LayoutBounds {
    fn point(x: f64, y: f64) -> Self {
        Self {
            min_x: x,
            min_y: y,
            max_x: x,
            max_y: y,
        }
    }

    fn include(&mut self, x: f64, y: f64) {
        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(x);
        self.max_y = self.max_y.max(y);
    }

    fn union(self, other: Self) -> Self {
        Self {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }

    fn width(self) -> f64 {
        self.max_x - self.min_x
    }

    fn height(self) -> f64 {
        self.max_y - self.min_y
    }

    fn center_x(self) -> f64 {
        (self.min_x + self.max_x) / 2.0
    }

    fn center_y(self) -> f64 {
        (self.min_y + self.max_y) / 2.0
    }

    fn contains(self, other: Self) -> bool {
        other.min_x >= self.min_x
            && other.max_x <= self.max_x
            && other.min_y >= self.min_y
            && other.max_y <= self.max_y
    }

    fn margin_inside(self, other: Self) -> f64 {
        [
            other.min_x - self.min_x,
            self.max_x - other.max_x,
            other.min_y - self.min_y,
            self.max_y - other.max_y,
        ]
        .into_iter()
        .fold(f64::INFINITY, f64::min)
    }

    fn json(self) -> serde_json::Value {
        json!({
            "min_x": self.min_x,
            "min_y": self.min_y,
            "max_x": self.max_x,
            "max_y": self.max_y,
            "width": self.width(),
            "height": self.height()
        })
    }
}

#[derive(Debug, Clone)]
struct LayoutItem {
    uuid: String,
    kind: String,
    reference: Option<String>,
    range: (usize, usize),
    bounds: LayoutBounds,
}

impl LayoutItem {
    fn json(&self) -> serde_json::Value {
        json!({
            "uuid": self.uuid,
            "kind": self.kind,
            "reference": self.reference,
            "bounds": self.bounds.json()
        })
    }
}

#[derive(Debug, Clone)]
struct LayoutPlan {
    schematic: std::path::PathBuf,
    before: String,
    selected: Vec<LayoutItem>,
    deltas: BTreeMap<String, (f64, f64)>,
    before_signature: SemanticSignature,
    plan_revision: String,
}

impl LayoutPlan {
    fn response(&self, dry_run: bool, applied: bool) -> serde_json::Value {
        json!({
            "dry_run": dry_run,
            "applied": applied,
            "safe_to_apply": true,
            "plan_revision": self.plan_revision,
            "schematic": self.schematic.display().to_string(),
            "selected_count": self.selected.len(),
            "selection_bounds": union_layout_bounds(&self.selected).map(LayoutBounds::json),
            "items": self.selected.iter().map(LayoutItem::json).collect::<Vec<_>>(),
            "deltas": self.deltas.iter().map(|(uuid, (dx, dy))| json!({"uuid": uuid, "dx": dx, "dy": dy})).collect::<Vec<_>>()
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
struct SemanticSignature {
    pin_nets: BTreeMap<String, Option<String>>,
    shorts: Vec<Vec<String>>,
}

async fn handle_inspect_selection_layout(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let safe_margin = args["safe_margin"].as_f64().unwrap_or(10.0);
    let (content, tree) = read_schematic(&sch_path)?;
    let items = extract_layout_items(&content, &tree)?;
    let selected = match select_layout_items(&items, args.get("selection")) {
        Ok(selected) => selected,
        Err(error) => return Ok(error),
    };
    let page = schematic_page_geometry(&tree, safe_margin);
    let selection_bounds = union_layout_bounds(&selected);
    let safe = page.safe_region;
    let out_of_bounds = selected
        .iter()
        .filter(|item| !safe.contains(item.bounds))
        .map(LayoutItem::json)
        .collect::<Vec<_>>();
    let minimum_margin = selection_bounds.map(|bounds| safe.margin_inside(bounds));

    Ok(CallToolResult::json(&json!({
        "schematic": sch_path.display().to_string(),
        "selected_count": selected.len(),
        "selection_bounds": selection_bounds.map(LayoutBounds::json),
        "items": selected.iter().map(LayoutItem::json).collect::<Vec<_>>(),
        "page": page.json(),
        "fully_inside_safe_region": out_of_bounds.is_empty(),
        "minimum_margin": minimum_margin,
        "out_of_bounds_items": out_of_bounds
    })))
}

async fn handle_arrange_schematic_selection(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let plan = match build_layout_plan(&sch_path, args) {
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
    let after = apply_layout_deltas(&plan.before, &plan.selected, &plan.deltas)?;
    validate_semantic_signature(&plan.before_signature, &after, "after layout arrangement")
        .map_err(|error| anyhow::anyhow!(error))?;
    write_atomic_if_unchanged(&plan.schematic, &plan.before, &after)?;
    Ok(CallToolResult::json(&plan.response(false, true)))
}

fn build_layout_plan(
    sch_path: &std::path::PathBuf,
    args: &serde_json::Value,
) -> Result<LayoutPlan, CallToolResult> {
    let (before, tree) =
        read_schematic(sch_path).map_err(|error| CallToolResult::error(error.to_string()))?;
    let items = extract_layout_items(&before, &tree)
        .map_err(|error| CallToolResult::error(error.to_string()))?;
    let selected = select_layout_items(&items, args.get("selection"))?;
    if selected.is_empty() {
        return Err(CallToolResult::error("selection matched no layout items"));
    }
    let deltas = layout_deltas(&selected, &args["operation"])?;
    let after = apply_layout_deltas(&before, &selected, &deltas)
        .map_err(|error| CallToolResult::error(error.to_string()))?;
    let before_signature =
        semantic_signature(&before).map_err(|error| CallToolResult::error(error.to_string()))?;
    validate_semantic_signature(
        &before_signature,
        &after,
        "preview after layout arrangement",
    )
    .map_err(|error| CallToolResult::error(error.to_string()))?;
    let plan_revision = layout_plan_revision(&before, &selected, &deltas, &args["operation"]);
    Ok(LayoutPlan {
        schematic: sch_path.clone(),
        before,
        selected,
        deltas,
        before_signature,
        plan_revision,
    })
}

fn extract_layout_items(
    content: &str,
    tree: &konnect_sexp::SexpNode,
) -> anyhow::Result<Vec<LayoutItem>> {
    let instances = extract_symbol_instances(tree);
    let lib_symbols = tree
        .find("lib_symbols")
        .map(|node| node.find_all("symbol"))
        .unwrap_or_default();
    let mut out = Vec::new();
    for (start, end) in find_direct_child_blocks(content, "kicad_sch") {
        let block = &content[start..end];
        let node = konnect_sexp::parse_sexp(block)?;
        let Some(kind) = node.head() else { continue };
        if !LAYOUT_ITEM_TAGS.contains(&kind) {
            continue;
        }
        let Some(uuid) = node.find_str("uuid") else {
            continue;
        };
        let reference = property_value(&node, "Reference").map(str::to_string);
        let bounds = if kind == "symbol" {
            instances
                .iter()
                .find(|instance| instance.uuid.as_deref() == Some(uuid))
                .and_then(|instance| {
                    find_lib_symbol(&lib_symbols, instance)
                        .and_then(|symbol| symbol_bounds_for_instance(symbol, instance))
                })
                .map(|bounds| LayoutBounds {
                    min_x: bounds.min_x,
                    min_y: bounds.min_y,
                    max_x: bounds.max_x,
                    max_y: bounds.max_y,
                })
                .or_else(|| item_layout_bounds(&node))
        } else {
            item_layout_bounds(&node)
        };
        if let Some(bounds) = bounds {
            out.push(LayoutItem {
                uuid: uuid.to_string(),
                kind: kind.to_string(),
                reference,
                range: (start, end),
                bounds,
            });
        }
    }
    Ok(out)
}

fn property_value<'a>(node: &'a konnect_sexp::SexpNode, name: &str) -> Option<&'a str> {
    node.find_all("property")
        .into_iter()
        .find(|property| property.get(1).and_then(|n| n.as_str()) == Some(name))
        .and_then(|property| property.get(2))
        .and_then(|value| value.as_str())
}

fn item_layout_bounds(node: &konnect_sexp::SexpNode) -> Option<LayoutBounds> {
    let mut bounds: Option<LayoutBounds> = None;
    let mut include = |x: f64, y: f64| match &mut bounds {
        Some(bounds) => bounds.include(x, y),
        None => bounds = Some(LayoutBounds::point(x, y)),
    };
    if let Some(at) = node.find("at") {
        if let (Some(x), Some(y)) = (at.get_f64(1), at.get_f64(2)) {
            include(x, y);
        }
    }
    for tag in ["start", "mid", "end", "center"] {
        if let Some(point) = node.find(tag) {
            if let (Some(x), Some(y)) = (point.get_f64(1), point.get_f64(2)) {
                include(x, y);
            }
        }
    }
    if let Some(points) = node.find("pts") {
        for point in points.find_all("xy") {
            if let (Some(x), Some(y)) = (point.get_f64(1), point.get_f64(2)) {
                include(x, y);
            }
        }
    }
    if node.head() == Some("circle") {
        if let (Some(center), Some(radius)) = (node.find("center"), node.find_f64("radius")) {
            if let (Some(x), Some(y)) = (center.get_f64(1), center.get_f64(2)) {
                include(x - radius, y - radius);
                include(x + radius, y + radius);
            }
        }
    }
    bounds
}

fn select_layout_items(
    items: &[LayoutItem],
    selection: Option<&serde_json::Value>,
) -> Result<Vec<LayoutItem>, CallToolResult> {
    let Some(selection) = selection else {
        return Ok(items.to_vec());
    };
    if !selection.is_object() {
        return Err(CallToolResult::error("selection must be an object"));
    }
    let refs = string_array(selection, "references")?;
    let uuids = string_array(selection, "uuids")?;
    let bbox = parse_layout_bbox(selection.get("bbox"))?;
    if refs.is_empty() && uuids.is_empty() && bbox.is_none() {
        return Ok(items.to_vec());
    }
    let selected = items
        .iter()
        .filter(|item| {
            item.reference
                .as_ref()
                .is_some_and(|reference| refs.contains(reference))
                || uuids.contains(&item.uuid)
                || bbox.is_some_and(|bbox| {
                    item.bounds.min_x <= bbox.max_x
                        && item.bounds.max_x >= bbox.min_x
                        && item.bounds.min_y <= bbox.max_y
                        && item.bounds.max_y >= bbox.min_y
                })
        })
        .cloned()
        .collect();
    Ok(selected)
}

fn string_array(value: &serde_json::Value, field: &str) -> Result<Vec<String>, CallToolResult> {
    match value.get(field) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        CallToolResult::error(format!(
                            "selection.{field} must contain only non-empty strings"
                        ))
                    })
            })
            .collect(),
        Some(_) => Err(CallToolResult::error(format!(
            "selection.{field} must be an array"
        ))),
    }
}

fn parse_layout_bbox(
    value: Option<&serde_json::Value>,
) -> Result<Option<LayoutBounds>, CallToolResult> {
    let Some(value) = value else { return Ok(None) };
    if value.is_null() {
        return Ok(None);
    }
    let numbers = if let Some(items) = value.as_array() {
        if items.len() != 4 {
            return Err(CallToolResult::error(
                "selection.bbox array must be [min_x,min_y,max_x,max_y]",
            ));
        }
        items
            .iter()
            .map(serde_json::Value::as_f64)
            .collect::<Option<Vec<_>>>()
    } else if let Some(object) = value.as_object() {
        let parsed = (
            object.get("min_x").and_then(serde_json::Value::as_f64),
            object.get("min_y").and_then(serde_json::Value::as_f64),
            object.get("max_x").and_then(serde_json::Value::as_f64),
            object.get("max_y").and_then(serde_json::Value::as_f64),
        );
        match parsed {
            (Some(min_x), Some(min_y), Some(max_x), Some(max_y)) => {
                Some(vec![min_x, min_y, max_x, max_y])
            }
            _ => None,
        }
    } else {
        None
    };
    let Some(numbers) = numbers else {
        return Err(CallToolResult::error(
            "selection.bbox must be an array or object with finite numeric bounds",
        ));
    };
    Ok(Some(LayoutBounds {
        min_x: numbers[0].min(numbers[2]),
        min_y: numbers[1].min(numbers[3]),
        max_x: numbers[0].max(numbers[2]),
        max_y: numbers[1].max(numbers[3]),
    }))
}

fn union_layout_bounds(items: &[LayoutItem]) -> Option<LayoutBounds> {
    items
        .iter()
        .map(|item| item.bounds)
        .reduce(LayoutBounds::union)
}

#[derive(Debug, Clone, Copy)]
struct PageGeometry {
    page: LayoutBounds,
    drawing_frame: LayoutBounds,
    safe_region: LayoutBounds,
    title_block_reserved: LayoutBounds,
}

impl PageGeometry {
    fn json(self) -> serde_json::Value {
        json!({
            "page_bounds": self.page.json(),
            "drawing_frame": self.drawing_frame.json(),
            "safe_printable_region": self.safe_region.json(),
            "title_block_reserved_area": self.title_block_reserved.json()
        })
    }
}

fn schematic_page_geometry(tree: &konnect_sexp::SexpNode, safe_margin: f64) -> PageGeometry {
    let (mut width, mut height) = match tree.find("paper").and_then(|paper| {
        let name = paper
            .get(1)
            .and_then(|value| value.as_str())
            .unwrap_or("A4");
        if name == "User" {
            Some((paper.get_f64(2)?, paper.get_f64(3)?))
        } else {
            named_paper_dimensions(name)
        }
    }) {
        Some(size) => size,
        None => named_paper_dimensions("A4").unwrap(),
    };
    if tree.find("paper").is_some_and(|paper| {
        paper
            .children()
            .unwrap_or(&[])
            .iter()
            .any(|arg| arg.as_str() == Some("portrait"))
    }) {
        std::mem::swap(&mut width, &mut height);
    }
    let page = LayoutBounds {
        min_x: 0.0,
        min_y: 0.0,
        max_x: width,
        max_y: height,
    };
    let frame_margin = 5.0;
    let drawing_frame = LayoutBounds {
        min_x: frame_margin,
        min_y: frame_margin,
        max_x: width - frame_margin,
        max_y: height - frame_margin,
    };
    let safe_region = LayoutBounds {
        min_x: safe_margin,
        min_y: safe_margin,
        max_x: width - safe_margin,
        max_y: height - safe_margin,
    };
    let title_block_reserved = LayoutBounds {
        min_x: (width - 125.0).max(safe_margin),
        min_y: (height - 40.0).max(safe_margin),
        max_x: width - safe_margin,
        max_y: height - safe_margin,
    };
    PageGeometry {
        page,
        drawing_frame,
        safe_region,
        title_block_reserved,
    }
}

fn named_paper_dimensions(name: &str) -> Option<(f64, f64)> {
    Some(match name {
        "A0" => (1189.0, 841.0),
        "A1" => (841.0, 594.0),
        "A2" => (594.0, 420.0),
        "A3" => (420.0, 297.0),
        "A4" => (297.0, 210.0),
        "A5" => (210.0, 148.0),
        "A" | "USLetter" => (279.4, 215.9),
        "B" | "USLedger" => (431.8, 279.4),
        "C" => (558.8, 431.8),
        "D" => (863.6, 558.8),
        "E" => (1117.6, 863.6),
        "USLegal" => (355.6, 215.9),
        _ => return None,
    })
}

fn layout_deltas(
    selected: &[LayoutItem],
    operation: &serde_json::Value,
) -> Result<BTreeMap<String, (f64, f64)>, CallToolResult> {
    if !operation.is_object() {
        return Err(CallToolResult::error("operation must be an object"));
    }
    let Some(kind) = operation["type"].as_str() else {
        return Err(CallToolResult::error("operation.type is required"));
    };
    let mut out = BTreeMap::new();
    match kind {
        "translate" => {
            let dx = operation["dx"].as_f64().unwrap_or(0.0);
            let dy = operation["dy"].as_f64().unwrap_or(0.0);
            for item in selected {
                out.insert(item.uuid.clone(), (dx, dy));
            }
        }
        "move_to_anchor" => {
            let bounds = union_layout_bounds(selected)
                .ok_or_else(|| CallToolResult::error("selection has no bounds"))?;
            let x = operation["x"]
                .as_f64()
                .ok_or_else(|| CallToolResult::error("operation.x is required"))?;
            let y = operation["y"]
                .as_f64()
                .ok_or_else(|| CallToolResult::error("operation.y is required"))?;
            let anchor = operation["anchor"].as_str().unwrap_or("top_left");
            let (anchor_x, anchor_y) = match anchor {
                "top_left" => (bounds.min_x, bounds.min_y),
                "center" => (bounds.center_x(), bounds.center_y()),
                "bottom_right" => (bounds.max_x, bounds.max_y),
                other => {
                    return Err(CallToolResult::error(format!(
                        "operation.anchor '{other}' is invalid"
                    )))
                }
            };
            for item in selected {
                out.insert(item.uuid.clone(), (x - anchor_x, y - anchor_y));
            }
        }
        "align" => {
            let edge = operation["edge"]
                .as_str()
                .ok_or_else(|| CallToolResult::error("operation.edge is required for align"))?;
            let selection = union_layout_bounds(selected)
                .ok_or_else(|| CallToolResult::error("selection has no bounds"))?;
            for item in selected {
                let (dx, dy) = match edge {
                    "left" => (selection.min_x - item.bounds.min_x, 0.0),
                    "right" => (selection.max_x - item.bounds.max_x, 0.0),
                    "top" => (0.0, selection.min_y - item.bounds.min_y),
                    "bottom" => (0.0, selection.max_y - item.bounds.max_y),
                    "horizontal_center" => (selection.center_x() - item.bounds.center_x(), 0.0),
                    "vertical_center" => (0.0, selection.center_y() - item.bounds.center_y()),
                    other => {
                        return Err(CallToolResult::error(format!(
                            "operation.edge '{other}' is invalid"
                        )))
                    }
                };
                out.insert(item.uuid.clone(), (dx, dy));
            }
        }
        "distribute" => {
            let axis = operation["axis"].as_str().ok_or_else(|| {
                CallToolResult::error("operation.axis is required for distribute")
            })?;
            let mut ordered = selected.to_vec();
            match axis {
                "horizontal" => ordered.sort_by(|a, b| {
                    a.bounds
                        .min_x
                        .partial_cmp(&b.bounds.min_x)
                        .expect("finite bounds")
                }),
                "vertical" => ordered.sort_by(|a, b| {
                    a.bounds
                        .min_y
                        .partial_cmp(&b.bounds.min_y)
                        .expect("finite bounds")
                }),
                other => {
                    return Err(CallToolResult::error(format!(
                        "operation.axis '{other}' is invalid"
                    )))
                }
            }
            if ordered.len() < 3 {
                return Err(CallToolResult::error(
                    "distribute requires at least three selected items",
                ));
            }
            let first = ordered.first().unwrap().bounds;
            let last = ordered.last().unwrap().bounds;
            let spacing = operation["spacing"].as_f64();
            for (index, item) in ordered.iter().enumerate() {
                let (target_x, target_y) = if axis == "horizontal" {
                    let target = if let Some(spacing) = spacing {
                        first.min_x
                            + ordered[..index]
                                .iter()
                                .map(|item| item.bounds.width() + spacing)
                                .sum::<f64>()
                    } else {
                        first.min_x
                            + (last.min_x - first.min_x) * index as f64 / (ordered.len() - 1) as f64
                    };
                    (target, item.bounds.min_y)
                } else {
                    let target = if let Some(spacing) = spacing {
                        first.min_y
                            + ordered[..index]
                                .iter()
                                .map(|item| item.bounds.height() + spacing)
                                .sum::<f64>()
                    } else {
                        first.min_y
                            + (last.min_y - first.min_y) * index as f64 / (ordered.len() - 1) as f64
                    };
                    (item.bounds.min_x, target)
                };
                out.insert(
                    item.uuid.clone(),
                    (target_x - item.bounds.min_x, target_y - item.bounds.min_y),
                );
            }
        }
        other => {
            return Err(CallToolResult::error(format!(
                "operation.type '{other}' is invalid"
            )))
        }
    }
    Ok(out)
}

fn apply_layout_deltas(
    content: &str,
    selected: &[LayoutItem],
    deltas: &BTreeMap<String, (f64, f64)>,
) -> anyhow::Result<String> {
    let mut edits = Vec::new();
    for item in selected {
        let Some((dx, dy)) = deltas.get(&item.uuid).copied() else {
            continue;
        };
        if dx == 0.0 && dy == 0.0 {
            continue;
        }
        let block = &content[item.range.0..item.range.1];
        for (rel_start, rel_end, replacement) in coordinate_clause_replacements(block, dx, dy)? {
            edits.push(SexpEdit::replace(
                item.range.0 + rel_start,
                item.range.0 + rel_end,
                replacement,
            ));
        }
    }
    Ok(apply_edits(content.to_string(), edits))
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

fn semantic_signature(content: &str) -> anyhow::Result<SemanticSignature> {
    let tree = konnect_sexp::parse_sexp(content)?;
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let index = ConnectivityIndex::build(&tree, &wires, &labels, COINCIDENT_TOLERANCE);
    let mut graph = super::sch_connectivity::net_graph_for(&tree, &wires, &labels);
    let pin_nets = index
        .placed_pins()
        .iter()
        .map(|pin| {
            (
                format!("{}:{}:{}", pin.reference, pin.unit, pin.pin.number),
                graph.net_at(pin.at.0, pin.at.1),
            )
        })
        .collect();
    Ok(SemanticSignature {
        pin_nets,
        shorts: layout_shorted_label_sets(&tree),
    })
}

fn layout_shorted_label_sets(tree: &konnect_sexp::SexpNode) -> Vec<Vec<String>> {
    let wires = extract_wires(tree);
    let labels = extract_all_net_labels(tree);
    let mut graph = super::sch_connectivity::net_graph_for(tree, &wires, &labels);
    let mut root_nets: HashMap<(i64, i64), Vec<String>> = HashMap::new();
    for label in &labels {
        let root = graph.find(super::sch_connectivity::pt_key(label.x, label.y));
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

fn validate_semantic_signature(
    before: &SemanticSignature,
    after_content: &str,
    stage: &str,
) -> anyhow::Result<()> {
    let after = semantic_signature(after_content)?;
    if after != *before {
        anyhow::bail!("semantic endpoint membership or short set changed {stage}");
    }
    Ok(())
}

fn layout_plan_revision(
    before: &str,
    selected: &[LayoutItem],
    deltas: &BTreeMap<String, (f64, f64)>,
    operation: &serde_json::Value,
) -> String {
    let mut material = String::new();
    material.push_str(&DocumentRevision::of(before).to_string());
    material.push('\n');
    material.push_str(&operation.to_string());
    for item in selected {
        material.push('\n');
        material.push_str(&item.uuid);
    }
    for (uuid, (dx, dy)) in deltas {
        material.push('\n');
        material.push_str(uuid);
        material.push(':');
        material.push_str(&cse::types::fmt_f64(*dx));
        material.push(',');
        material.push_str(&cse::types::fmt_f64(*dy));
    }
    let revision = DocumentRevision::of(&material);
    format!("arrange-schematic-selection:{revision}")
}

/// Edits translating every `(property …)` anchor inside the symbol block at
/// `sym_start..sym_end` by `(ddx, ddy)`.
///
/// A property's own rotation is left untouched: a translation does not turn
/// text. Block starts come from `find_block_starts`, which is string-aware, so
/// a property *value* containing `(property` cannot be mistaken for one.
fn property_translation_edits(
    content: &str,
    sym_start: usize,
    sym_end: usize,
    ddx: f64,
    ddy: f64,
) -> Vec<SexpEdit> {
    if ddx == 0.0 && ddy == 0.0 {
        return Vec::new();
    }
    let mut edits = Vec::new();
    for prop_start in konnect_sexp::writer::find_block_starts(content, "property") {
        if prop_start < sym_start || prop_start >= sym_end {
            continue;
        }
        let Some((_, prop_end)) = konnect_sexp::writer::find_balanced_block(content, prop_start)
        else {
            continue;
        };
        let prop = &content[prop_start..prop_end];
        // The property's own (at …), not one nested deeper in (effects …).
        let Some(at_rel) = prop.find("(at ") else {
            continue;
        };
        let at_abs = prop_start + at_rel + "(at ".len();
        let Some(close_rel) = prop[at_rel..].find(')') else {
            continue;
        };
        let at_end = prop_start + at_rel + close_rel;
        let parts: Vec<&str> = content[at_abs..at_end].split_whitespace().collect();
        let (Some(px), Some(py)) = (
            parts.first().and_then(|s| s.parse::<f64>().ok()),
            parts.get(1).and_then(|s| s.parse::<f64>().ok()),
        ) else {
            continue;
        };
        let rot = parts.get(2).copied().unwrap_or("0");
        edits.push(SexpEdit::replace(
            at_abs,
            at_end,
            format!(
                "{} {} {rot}",
                cse::types::fmt_f64(px + ddx),
                cse::types::fmt_f64(py + ddy)
            ),
        ));
    }
    edits
}

async fn handle_bulk_move(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = match require_array(args, "references") {
        Ok(a) => a.clone(),
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
    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut moved: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r,
            None => continue,
        };

        // Every placement of this reference — a multi-unit part has one block
        // per unit, and shifting only the first would tear the part apart.
        let blocks = find_all_symbol_instance_blocks(&content, reference);
        if blocks.is_empty() {
            errors.push(format!("'{}' not found", reference));
            continue;
        }

        let mut placements: Vec<serde_json::Value> = Vec::new();
        for (sym_start, sym_end) in blocks {
            // Find first (at X Y [ROT]) inside this symbol block
            let sym_block = &content[sym_start..sym_end];
            let at_pat = "(at ";
            let at_rel = match sym_block.find(at_pat) {
                Some(r) => r,
                None => {
                    errors.push(format!("No (at) in symbol '{}'", reference));
                    continue;
                }
            };
            let at_abs = sym_start + at_rel + at_pat.len();
            let close_rel = sym_block[at_rel..].find(')').unwrap_or(0);
            let at_end = sym_start + at_rel + close_rel;

            let at_str = &content[at_abs..at_end];
            let parts: Vec<&str> = at_str.split_whitespace().collect();
            let x = parts
                .first()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let y = parts
                .get(1)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let rot = parts
                .get(2)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);

            let (new_x, new_y) = snap_point(x + dx, y + dy, 1.27);
            edits.push(SexpEdit::replace(
                at_abs,
                at_end,
                format!("{new_x} {new_y} {rot}"),
            ));
            // Property coordinates are ABSOLUTE in .kicad_sch, so the field
            // text does not follow the symbol on its own — moving only the
            // symbol's own (at …) strands Reference and Value at the old
            // location (#202). Shift them by the delta the symbol *actually*
            // moved, which is the snapped one, or they drift relative to the
            // part. `Symbol::translate` does the same on the typed path.
            edits.extend(property_translation_edits(
                &content,
                sym_start,
                sym_end,
                new_x - x,
                new_y - y,
            ));
            placements.push(json!({
                "old_x": x, "old_y": y,
                "new_x": new_x, "new_y": new_y
            }));
        }

        if !placements.is_empty() {
            moved.push(json!({
                "reference": reference,
                "units": placements.len(),
                "placements": placements
            }));
        }
    }

    // Pin positions before the shift, so a dot the pins vacate can be re-judged
    // and a pin landing mid-span gets one (#120). A move changes no wires.
    const TOL: f64 = 0.01;
    let pins_of = |src: &str| -> Vec<(f64, f64)> {
        konnect_sexp::parse_sexp(src)
            .ok()
            .map(|t| crate::tools::all_pin_endpoints(&t))
            .unwrap_or_default()
    };
    // No wires means nothing can be justified and nothing can be landed on, so
    // the whole pass — including two full symbol/lib_symbols walks — is skipped.
    let has_wires = expected.contains("(wire");
    let before_pins = if has_wires {
        pins_of(&expected)
    } else {
        Vec::new()
    };

    let new_content = apply_edits(content, edits);

    let after_pins = if has_wires {
        pins_of(&new_content)
    } else {
        Vec::new()
    };
    let differs = |a: &[(f64, f64)], b: &[(f64, f64)]| -> Vec<(f64, f64)> {
        a.iter()
            .copied()
            .filter(|&(x, y)| {
                !b.iter()
                    .any(|&(ox, oy)| konnect_sexp::geometry::points_coincident(x, y, ox, oy, TOL))
            })
            .collect()
    };
    let mut points = differs(&before_pins, &after_pins);
    points.extend(differs(&after_pins, &before_pins));
    let (new_content, junctions_added, junctions_pruned) =
        crate::tools::sch_wiring::reconcile_junctions_at(new_content, &points);

    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "moved_count": moved.len(),
        "moved": moved,
        "dx": dx, "dy": dy,
        "junctions_added_count": junctions_added,
        "junctions_pruned_count": junctions_pruned,
        "errors": errors
    })))
}

async fn handle_batch_edit(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let edits_arr = match args["edits"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'edits' array")),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut file_edits: Vec<SexpEdit> = Vec::new();
    let mut changed: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for edit_spec in &edits_arr {
        let reference = match edit_spec["reference"].as_str() {
            Some(r) => r,
            None => {
                errors.push("Missing 'reference' in edit spec".into());
                continue;
            }
        };

        let mut component_changes: Vec<String> = Vec::new();

        // Standard fields, then arbitrary extra fields from the "fields" object.
        // Each is rewritten in every unit's block, which is where a multi-unit
        // part keeps its copies of the value.
        let extra = edit_spec["fields"].as_object();
        let specs = [("Value", "value"), ("Footprint", "footprint")]
            .into_iter()
            .filter_map(|(field, key)| Some((field.to_string(), edit_spec[key].as_str()?)))
            .chain(
                extra
                    .into_iter()
                    .flatten()
                    .filter_map(|(name, val)| Some((name.clone(), val.as_str()?))),
            );

        for (field, new_val) in specs {
            let ranges = field_value_ranges(&content, reference, &field);
            if ranges.is_empty() {
                errors.push(format!("Field '{}' not found on '{}'", field, reference));
                continue;
            }
            let units = ranges.len();
            for (start, end) in ranges {
                file_edits.push(SexpEdit::replace(start, end, new_val.to_string()));
            }
            component_changes.push(if units > 1 {
                format!("{} → {} ({} units)", field, new_val, units)
            } else {
                format!("{} → {}", field, new_val)
            });
        }

        match component_metadata_from_args(edit_spec) {
            Ok(update) => match component_metadata_edits(&content, reference, update) {
                Ok((edits, metadata_changes)) => {
                    file_edits.extend(edits);
                    component_changes.extend(metadata_changes);
                }
                Err(why) => errors.push(format!("{reference}: metadata: {why}")),
            },
            Err(result) => errors.push(format!("{reference}: {}", error_text(&result))),
        }

        if !component_changes.is_empty() {
            changed.push(json!({
                "reference": reference,
                "changes": component_changes
            }));
        }
    }

    let new_content = apply_edits(content, file_edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "updated_count": changed.len(),
        "updated": changed,
        "errors": errors
    })))
}

async fn handle_batch_delete_components(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = match args["references"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'references' array")),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r,
            None => continue,
        };
        let blocks = find_symbol_blocks(&content, reference);
        if blocks.is_empty() {
            errors.push(format!("Component '{}' not found", reference));
            continue;
        }
        // Every unit of a multi-unit part, or the whole component is not gone.
        for (del_start, del_end) in blocks {
            edits.push(SexpEdit::delete(del_start, del_end));
        }
        deleted.push(reference.to_string());
    }

    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "deleted_count": deleted.len(),
        "deleted": deleted,
        "errors": errors
    })))
}

async fn handle_connect_passthrough(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net_name = match require_str(args, "net_name") {
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
    let direction = opt_str(args, "direction").unwrap_or("auto");

    let (content, tree) = read_schematic(&sch_path)?;
    let dir = crate::tools::resolve_stub_direction(direction, (x, y), &tree);

    // Stub is 2.54mm (2×1.27 grid units)
    let stub = 2.54_f64;
    let (wire_end_x, wire_end_y) = (x + dir.dx * stub, y + dir.dy * stub);

    let wire_sexp = format_wire(x, y, wire_end_x, wire_end_y);
    let label_sexp = format_net_label(&net_name, wire_end_x, wire_end_y, dir.label_rotation);

    let expected = content.clone();
    // Wires and labels are element class 2; symbol instances MUST come last.
    let new_content = crate::tools::sch_wiring::insert_before_close(
        &content,
        &format!("{wire_sexp}{label_sexp}"),
    );
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "net": net_name,
        "stub_root": { "x": x, "y": y },
        "label_position": { "x": wire_end_x, "y": wire_end_y },
        "direction": dir.name,
        "label_rotation": dir.label_rotation
    })))
}

/// KiCad's default alignment for a placed text annotation.
///
/// Measured against KiCad 10's own demo projects: every text block in the
/// current file format carries `(justify left bottom)`.
const DEFAULT_TEXT_JUSTIFY: &str = "left bottom";

/// Translate a caller's alignment into the `(justify ...)` clause KiCad writes.
///
/// Alignment is per axis, and centring an axis means leaving its token out:
/// `left` is left-aligned and vertically centred, `bottom` is horizontally
/// centred and bottom-aligned. `"center"` centres both and so returns an empty
/// string. There is no token for it — `(justify center)` makes KiCad refuse the
/// whole file, the same way a misplaced item does.
fn schematic_text_justify(value: &str) -> Result<String, String> {
    fn claim(
        slot: &mut Option<&'static str>,
        value: &'static str,
        token: &str,
    ) -> Result<(), String> {
        if let Some(existing) = slot {
            return Err(format!(
                "justify names '{existing}' and '{token}' on the same axis - use at most one horizontal (left, right) and one vertical (top, bottom) token"
            ));
        }
        *slot = Some(value);
        Ok(())
    }

    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("center") {
        return Ok(String::new());
    }

    let mut horizontal: Option<&'static str> = None;
    let mut vertical: Option<&'static str> = None;
    for token in trimmed.split_whitespace() {
        match token.to_ascii_lowercase().as_str() {
            "left" => claim(&mut horizontal, "left", token)?,
            "right" => claim(&mut horizontal, "right", token)?,
            "top" => claim(&mut vertical, "top", token)?,
            "bottom" => claim(&mut vertical, "bottom", token)?,
            _ => {
                return Err(format!(
                    "unknown justify token '{token}' - use left, right, top, bottom, or center"
                ))
            }
        }
    }

    // KiCad writes the horizontal token first.
    let mut parts = Vec::with_capacity(2);
    parts.extend(horizontal);
    parts.extend(vertical);
    Ok(format!(" (justify {})", parts.join(" ")))
}

async fn handle_add_schematic_text(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let text = match require_str(args, "text") {
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
    let size = args["size"].as_f64().unwrap_or(1.27);
    let rotation = args["rotation"].as_f64().unwrap_or(0.0);
    let justify = args["justify"].as_str().unwrap_or(DEFAULT_TEXT_JUSTIFY);
    // Without a justify token KiCad centres the text on `x`, so a long line
    // crosses the page edge and is dropped from the PDF export while the
    // .kicad_sch still looks complete.
    let justify_sexp = match schematic_text_justify(justify) {
        Ok(v) => v,
        Err(e) => return Ok(CallToolResult::error(e)),
    };
    let uuid = new_uuid();

    // Escape for a KiCad quoted string. Newlines and tabs must become their
    // two-character escapes: KiCad's reader rejects a literal newline inside
    // quotes, and it fails at the *file* level — a multi-line annotation makes
    // the whole schematic unloadable with only "Failed to load schematic".
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "")
        .replace('\n', "\\n")
        .replace('\t', "\\t");

    let text_sexp = format!(
        "\n  (text \"{escaped}\"\n    (at {x} {y} {rotation})\n    \
         (effects (font (size {size} {size})){justify_sexp})\n    (uuid \"{uuid}\")\n  )"
    );

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    // Before the first symbol instance, not at the end of the file: KiCad 10
    // requires symbol instances to come last and refuses to load a schematic
    // with a `(text …)` after them.
    let new_content = crate::tools::sch_wiring::insert_before_close(&content, &text_sexp);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "added": text,
        "x": x, "y": y,
        "size": size,
        "rotation": rotation,
        "justify": justify,
        "uuid": uuid
    })))
}

async fn handle_get_layout(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let include_wires = args["include_wires"].as_bool().unwrap_or(true);
    let include_labels = args["include_labels"].as_bool().unwrap_or(true);

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);

    let lib_symbols = tree
        .find("lib_symbols")
        .map(|node| node.find_all("symbol"))
        .unwrap_or_default();
    let placements = instances
        .iter()
        .map(|instance| {
            let bounds = find_lib_symbol(&lib_symbols, instance)
                .and_then(|symbol| symbol_bounds_for_instance(symbol, instance));
            (instance, bounds)
        })
        .collect::<Vec<_>>();
    let bounds_json = |bounds: SymbolBounds| {
        json!({
            "x_min": bounds.min_x,
            "y_min": bounds.min_y,
            "x_max": bounds.max_x,
            "y_max": bounds.max_y,
            "width": bounds.width(),
            "height": bounds.height()
        })
    };
    let components: Vec<serde_json::Value> = placements
        .iter()
        .map(|(instance, bounds)| {
            json!({
                "reference": instance.reference,
                "value": instance.value,
                "lib_id": instance.lib_id,
                "unit": instance.unit,
                "x": instance.x, "y": instance.y,
                "rotation": instance.rotation,
                "mirror_x": instance.mirror_x,
                "mirror_y": instance.mirror_y,
                "bounds": bounds.map(&bounds_json)
            })
        })
        .collect();

    // Enclose actual placed graphics and pin extents. If a library definition
    // is missing, preserve the old origin coverage for that one instance and
    // report the unresolved reference instead of silently understating it.
    let mut overall: Option<SymbolBounds> = None;
    let mut unresolved_bounds = Vec::new();
    for (instance, bounds) in &placements {
        let bounds = bounds.unwrap_or_else(|| {
            unresolved_bounds.push(instance.reference.clone());
            SymbolBounds {
                min_x: instance.x,
                min_y: instance.y,
                max_x: instance.x,
                max_y: instance.y,
            }
        });
        match &mut overall {
            Some(overall) => {
                overall.min_x = overall.min_x.min(bounds.min_x);
                overall.min_y = overall.min_y.min(bounds.min_y);
                overall.max_x = overall.max_x.max(bounds.max_x);
                overall.max_y = overall.max_y.max(bounds.max_y);
            }
            None => overall = Some(bounds),
        }
    }
    let bbox = overall.map_or_else(
        || json!({ "x_min": 0, "y_min": 0, "x_max": 0, "y_max": 0, "width": 0, "height": 0 }),
        bounds_json,
    );

    let mut result = json!({
        "component_count": components.len(),
        "components": components,
        "bounding_box": bbox,
        "bounds_resolved": placements.len() - unresolved_bounds.len(),
        "bounds_unresolved": unresolved_bounds
    });

    if include_wires {
        let wires = extract_wires(&tree);
        let wire_data: Vec<serde_json::Value> = wires
            .iter()
            .map(|w| json!({ "x1": w.x1, "y1": w.y1, "x2": w.x2, "y2": w.y2, "uuid": w.uuid }))
            .collect();
        result["wire_count"] = json!(wire_data.len());
        result["wires"] = json!(wire_data);
    }

    if include_labels {
        let labels = extract_labels(&tree);
        let label_data: Vec<serde_json::Value> = labels
            .iter()
            .map(|l| json!({ "net": l.net, "type": format!("{:?}", l.kind), "x": l.x, "y": l.y }))
            .collect();
        result["label_count"] = json!(label_data.len());
        result["labels"] = json!(label_data);
    }

    Ok(CallToolResult::json(&result))
}

async fn handle_validate_wire_connections(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let tol = args["tolerance"].as_f64().unwrap_or(0.01);

    let (_, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let index = ConnectivityIndex::build(&tree, &wires, &labels, tol);

    let floating: Vec<serde_json::Value> = index
        .floating_wire_ends()
        .into_iter()
        .map(|(x, y, wire_uuid)| json!({ "x": x, "y": y, "wire_uuid": wire_uuid }))
        .collect();

    Ok(CallToolResult::json(&json!({
        "valid": floating.is_empty(),
        "floating_count": floating.len(),
        "floating_endpoints": floating
    })))
}

async fn handle_validate_component_connections(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let filter_refs: HashSet<String> = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let ignore_power_pins = args["ignore_power_pins"].as_bool().unwrap_or(false);
    let (_, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let index = ConnectivityIndex::build(&tree, &wires, &labels, COINCIDENT_TOLERANCE);

    let mut unconnected: Vec<serde_json::Value> = Vec::new();

    for placed in index.placed_pins() {
        if !filter_refs.is_empty() && !filter_refs.contains(&placed.reference) {
            continue;
        }
        let (px, py) = placed.at;

        // A library-declared no-connect pin is intentional by definition and
        // does not need a placed X marker (#267).
        if placed.pin.electrical_type == "no_connect" {
            continue;
        }
        if ignore_power_pins
            && matches!(
                placed.pin.electrical_type.as_str(),
                "power_in" | "power_out"
            )
        {
            continue;
        }

        // Skip intentional no-connects.
        if index.has_no_connect(px, py) {
            continue;
        }

        if !index.attaches_pin(px, py) {
            unconnected.push(json!({
                "reference": placed.reference,
                "value": placed.value,
                "pin": placed.pin.number,
                "pin_name": placed.pin.name,
                "pin_type": placed.pin.electrical_type,
                "x": px,
                "y": py
            }));
        }
    }

    Ok(CallToolResult::json(&json!({
        "valid": unconnected.is_empty(),
        "unconnected_count": unconnected.len(),
        "unconnected_pins": unconnected
    })))
}

#[cfg(test)]
mod batch_delete_tests {
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

    #[tokio::test]
    async fn batch_delete_uuid_is_tab_indentation_safe_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch-delete.kicad_sch");
        let uuid = "11111111-1111-1111-1111-111111111111";
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(wire\n\t\t(pts (xy 0 0) (xy 10 0))\n\t\t(uuid \"{uuid}\")\n\t)\n\t(text \"keep me\" (at 5 5 0) (uuid \"text\"))\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();

        let result = handle_batch_delete(
            &json!({
                "schematic": path.display().to_string(),
                "uuids": [uuid, "root", uuid]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains(uuid));
        assert!(after.contains("(uuid \"root\")"));
        assert!(after.contains("keep me"));
        assert!(after.contains("(sheet_instances"));
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn batch_delete_uuid_removes_top_level_text_but_preserves_structure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch-delete-text.kicad_sch");
        let text_uuid = "22222222-2222-2222-2222-222222222222";
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20260306)\n  (generator \"eeschema\")\n  (uuid \"root\")\n  (text \"obsolete caption\"\n    (at 5 5 0)\n    (effects (font (size 1.27 1.27)))\n    (uuid \"{text_uuid}\")\n  )\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();

        let result = handle_batch_delete(
            &json!({
                "schematic": path.display().to_string(),
                "uuids": [text_uuid]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("obsolete caption"));
        assert!(after.contains("(uuid \"root\")"));
        assert!(after.contains("(sheet_instances"));
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn batch_delete_uuid_expected_type_removes_only_coincident_local_label() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coincident-label-delete.kicad_sch");
        let local_uuid = "33333333-3333-3333-3333-333333333333";
        let hier_uuid = "44444444-4444-4444-4444-444444444444";
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20260306)\n  (generator \"eeschema\")\n  (uuid \"root\")\n  (label \"SIGNAL\" (at 10 20 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid \"{local_uuid}\"))\n  (hierarchical_label \"SIGNAL\" (shape bidirectional) (at 10 20 0) (effects (font (size 1.27 1.27)) (justify left)) (uuid \"{hier_uuid}\"))\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();

        let result = handle_batch_delete(
            &json!({
                "schematic": path.display().to_string(),
                "uuids": [{ "uuid": local_uuid, "expected_type": "local_label" }]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains(local_uuid));
        assert!(after.contains(hier_uuid));
        assert!(after.contains("(hierarchical_label \"SIGNAL\""));
        assert_eq!(after.matches("\"SIGNAL\"").count(), 1);
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn batch_delete_uuid_expected_type_mismatch_fails_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("typed-delete-mismatch.kicad_sch");
        let uuid = "55555555-5555-5555-5555-555555555555";
        let content = format!(
            "(kicad_sch\n  (version 20260306)\n  (generator \"eeschema\")\n  (uuid \"root\")\n  (label \"SIGNAL\" (at 10 20 0) (uuid \"{uuid}\"))\n  (text \"keep\" (at 5 5 0) (uuid \"text\"))\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n"
        );
        std::fs::write(&path, &content).unwrap();

        let result = handle_batch_delete(
            &json!({
                "schematic": path.display().to_string(),
                "uuids": [
                    { "uuid": uuid, "expected_type": "hierarchical_label" },
                    "text"
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert_eq!(content, std::fs::read_to_string(&path).unwrap());
    }
}

#[cfg(test)]
mod batch_place_and_connect_tests {
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

    // Pre-seed lib_symbols so ensure_lib_symbol short-circuits without KiCad
    // (precedent: sch_components.rs add_schematic_component_hides_power_reference).
    const DEVICE_R: &str = "    (symbol \"Device:R\"\n      (property \"Reference\" \"R\" (at 0 0 0))\n      (property \"Value\" \"R\" (at 0 0 0))\n    )\n";

    fn seeded_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("place.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n{DEVICE_R}  )\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn batch_place_components_dedupes_lib_symbols() {
        let (_d, path) = seeded_schematic();
        let result = handle_batch_place_components(
            &json!({
                "schematic": path.display().to_string(),
                "components": [
                    { "lib_id": "Device:R", "x": 100.0, "y": 100.0, "reference": "R1" },
                    { "lib_id": "Device:R", "x": 110.0, "y": 100.0, "reference": "R2" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let sch = cse::Schematic::load(&path).unwrap();
        assert!(sch.symbols.by_reference("R1").is_some());
        assert!(sch.symbols.by_reference("R2").is_some());

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after.matches("(symbol \"Device:R\"").count(),
            1,
            "lib_symbols entry must not be duplicated: {after}"
        );
        assert!(
            !after
                .lines()
                .any(|line| line.ends_with(' ') || line.ends_with('\t')),
            "batch placement must not leave trailing whitespace: {after:?}"
        );
    }

    #[tokio::test]
    async fn batch_place_components_collects_per_item_errors() {
        let (_d, path) = seeded_schematic();
        let result = handle_batch_place_components(
            &json!({
                "schematic": path.display().to_string(),
                "components": [
                    { "lib_id": "Device:R", "x": 100.0, "y": 100.0, "reference": "R1" },
                    { "lib_id": "Nonexistent_xyzzy:Foo", "x": 110.0, "y": 100.0, "reference": "R2" },
                    { "lib_id": "Device:R", "x": 120.0, "y": 100.0, "reference": "R3" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["placed_count"], 2);
        assert_eq!(parsed["errors"].as_array().unwrap().len(), 1);

        let sch = cse::Schematic::load(&path).unwrap();
        assert!(sch.symbols.by_reference("R1").is_some());
        assert!(sch.symbols.by_reference("R3").is_some());
        assert!(sch.symbols.by_reference("R2").is_none());
    }

    #[tokio::test]
    async fn batch_place_components_total_failure_sets_is_error() {
        let (_d, path) = seeded_schematic();
        let result = handle_batch_place_components(
            &json!({
                "schematic": path.display().to_string(),
                "components": [
                    { "lib_id": "Nonexistent_xyzzy:Foo", "x": 100.0, "y": 100.0, "reference": "R1" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "{result:?}");
    }

    /// Six single-pin instances of a synthetic part, positioned so that
    /// connecting them by pin pairs produces a T-junction on the second pair.
    fn multi_point_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let pin_def = "\t\t\t(pin passive line (at 0 0 0) (length 0)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n";
        let lib_sym = format!("\t\t(symbol \"Test:PT\"\n{pin_def}\t\t)\n");
        let inst = |reference: &str, x: f64, y: f64, uuid: &str| {
            format!(
                "\t(symbol\n\t\t(lib_id \"Test:PT\")\n\t\t(at {x} {y} 0)\n\t\t(uuid \"{uuid}\")\n\t\t(property \"Reference\" \"{reference}\"\n\t\t\t(at {x} {y} 0)\n\t\t)\n\t)\n"
            )
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("points.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(uuid \"3af69a4c-1faa-40bd-91dc-c4fc245c4cbd\")\n\t(lib_symbols\n{}\t)\n{}{}{}{}{}{})\n",
                lib_sym,
                inst("R1", 100.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000001"),
                inst("R2", 120.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000002"),
                inst("R3", 110.0, 80.0, "aaaaaaaa-0000-0000-0000-000000000003"),
                inst("R4", 110.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000004"),
                inst("R5", 200.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000005"),
                inst("R6", 220.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000006"),
            ),
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn batch_connect_pins_dedupes_junction_and_collects_errors() {
        // R3-R4's wire T-lands on R1-R2's wire at (110, 100) -- without the
        // STEP 1 fix, processing the third connection re-detects that same
        // T-junction from the raw wire list and inserts a second dot.
        let (_d, path) = multi_point_schematic();
        let result = handle_batch_connect_pins(
            &json!({
                "schematic": path.display().to_string(),
                "connections": [
                    { "ref1": "R1", "pin1": "1", "ref2": "R2", "pin2": "1" },
                    { "ref1": "R3", "pin1": "1", "ref2": "R4", "pin2": "1" },
                    { "ref1": "R5", "pin1": "1", "ref2": "R6", "pin2": "1" },
                    { "ref1": "Rbad", "pin1": "1", "ref2": "R6", "pin2": "1" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after.matches("(junction").count(),
            1,
            "the T-junction at (110, 100) must not be re-inserted: {after}"
        );

        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["connected_count"], 3);
        assert_eq!(parsed["errors"].as_array().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod schematic_layout_primitive_tests {
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

    fn layout_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.kicad_sch");
        std::fs::write(
            &path,
            r#"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "root")
  (paper "A4")
  (lib_symbols
    (symbol "Test:PT"
      (symbol "PT_1_1"
        (pin passive line (at 0 0 0) (length 0) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
      )
    )
  )
  (label "SIG" (at 250 30 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "label-a"))
  (label "SIG" (at 292 30 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid "label-b"))
  (symbol
    (lib_id "Test:PT")
    (at 250 30 0)
    (unit 1)
    (property "Reference" "TP1" (at 250 28 0))
    (property "Value" "TP" (at 250 32 0))
    (uuid "tp1")
  )
  (symbol
    (lib_id "Test:PT")
    (at 292 30 0)
    (unit 1)
    (property "Reference" "TP2" (at 292 28 0))
    (property "Value" "TP" (at 292 32 0))
    (uuid "tp2")
  )
)"#,
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn inspect_selection_layout_reports_page_safe_region_and_out_of_bounds_items() {
        let (_dir, path) = layout_fixture();
        let result = handle_inspect_selection_layout(
            &json!({
                "schematic": path.display().to_string(),
                "selection": { "bbox": [240, 20, 300, 40] }
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", result_text(&result));
        let body = result_json(&result);
        assert_eq!(body["page"]["page_bounds"]["max_x"], json!(297.0));
        assert_eq!(body["selected_count"], json!(4));
        assert_eq!(body["fully_inside_safe_region"], json!(false));
        assert!(!body["out_of_bounds_items"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn arrange_selection_moves_connected_group_to_target_anchor() {
        let (_dir, path) = layout_fixture();
        let ctx = test_ctx();

        let dry = handle_arrange_schematic_selection(
            &json!({
                "schematic": path.display().to_string(),
                "selection": { "bbox": [240, 20, 300, 40] },
                "operation": { "type": "move_to_anchor", "anchor": "top_left", "x": 20.0, "y": 20.0 },
                "dry_run": true
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let body = result_json(&dry);

        let applied = handle_arrange_schematic_selection(
            &json!({
                "schematic": path.display().to_string(),
                "selection": { "bbox": [240, 20, 300, 40] },
                "operation": { "type": "move_to_anchor", "anchor": "top_left", "x": 20.0, "y": 20.0 },
                "dry_run": false,
                "plan_revision": body["plan_revision"].as_str().unwrap()
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!applied.is_error, "{}", result_text(&applied));

        let inspect = handle_inspect_selection_layout(
            &json!({
                "schematic": path.display().to_string(),
                "selection": { "bbox": [15, 15, 60, 35] }
            }),
            &ctx,
        )
        .await
        .unwrap();
        let inspected = result_json(&inspect);
        assert_eq!(inspected["fully_inside_safe_region"], json!(true));
        assert_eq!(
            semantic_signature(&std::fs::read_to_string(&path).unwrap())
                .unwrap()
                .pin_nets
                .values()
                .filter(|net| net.as_deref() == Some("SIG"))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn arrange_selection_refuses_to_move_symbols_away_from_labels() {
        let (_dir, path) = layout_fixture();
        let result = handle_arrange_schematic_selection(
            &json!({
                "schematic": path.display().to_string(),
                "selection": { "references": ["TP1", "TP2"] },
                "operation": { "type": "translate", "dx": -100.0, "dy": 0.0 },
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert!(result_text(&result).contains("semantic endpoint membership"));
    }

    #[test]
    fn schematic_layout_primitive_tools_are_registered() {
        let tools = tools();
        assert!(tools
            .iter()
            .any(|tool| tool.name == "inspect_schematic_selection_layout"));
        assert!(tools
            .iter()
            .any(|tool| tool.name == "arrange_schematic_selection"));
    }
}

#[cfg(test)]
mod midwire_pin_tests {
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

    /// U1 has a single pin at (100,80), sitting strictly mid-segment on a wire
    /// from (90,80) to (110,80).
    fn midwire_schematic(with_junction: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        let junction = if with_junction {
            "\t(junction (at 100 80) (diameter 0) (color 0 0 0 0) (uuid \"j1\"))\n"
        } else {
            ""
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("midwire.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Test:P1\"\n\t\t\t(symbol \"P1_1_1\"\n\t\t\t\t(pin passive line (at 0 0 0) (length 2.54)\n\t\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n\t(wire\n\t\t(pts (xy 90 80) (xy 110 80))\n\t\t(uuid \"w1\")\n\t)\n{junction}\t(symbol\n\t\t(lib_id \"Test:P1\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(uuid \"u1\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 100 75 0)\n\t\t)\n\t)\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    /// KiCad connects a pin mid-wire only through a junction dot; the
    /// validator must mirror that instead of demanding a wire endpoint.
    #[tokio::test]
    async fn midwire_pin_connects_with_junction_only() {
        for (with_junction, expect_valid) in [(true, true), (false, false)] {
            let (_d, path) = midwire_schematic(with_junction);
            let result = handle_validate_component_connections(
                &json!({ "schematic": path.display().to_string() }),
                &test_ctx(),
            )
            .await
            .unwrap();
            let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
                panic!("expected text content");
            };
            let body: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(
                body["valid"].as_bool(),
                Some(expect_valid),
                "with_junction={with_junction}: {body}"
            );
        }
    }

    fn typed_pin_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("typed-pins.kicad_sch");
        std::fs::write(
            &path,
            r#"(kicad_sch
  (version 20260306)
  (generator "eeschema")
  (uuid "root")
  (lib_symbols
    (symbol "Test:Typed"
      (symbol "Typed_1_1"
        (pin no_connect line (at 0 0 0) (length 0)
          (name "NC" (effects (font (size 1.27 1.27))))
          (number "1" (effects (font (size 1.27 1.27)))))
        (pin power_in line (at 0 2.54 0) (length 2.54)
          (name "VDD" (effects (font (size 1.27 1.27))))
          (number "2" (effects (font (size 1.27 1.27)))))
        (pin output line (at 0 5.08 0) (length 2.54)
          (name "OUT" (effects (font (size 1.27 1.27))))
          (number "3" (effects (font (size 1.27 1.27))))))
      (symbol "Typed_2_1"
        (pin input line (at 0 7.62 0) (length 2.54)
          (name "OTHER_UNIT" (effects (font (size 1.27 1.27))))
          (number "4" (effects (font (size 1.27 1.27))))))))
  (symbol
    (lib_id "Test:Typed")
    (at 100 80 0)
    (unit 1)
    (uuid "u1")
    (property "Reference" "U1" (at 100 75 0))
    (property "Value" "Typed" (at 100 77 0)))
  (sheet_instances (path "/" (page "1"))))
"#,
        )
        .unwrap();
        (dir, path)
    }

    async fn validate_components_json(
        schematic: &std::path::Path,
        ignore_power_pins: bool,
    ) -> serde_json::Value {
        let result = handle_validate_component_connections(
            &json!({
                "schematic": schematic.display().to_string(),
                "ignore_power_pins": ignore_power_pins
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn declared_no_connect_and_other_unit_pins_are_not_reported() {
        let (_dir, path) = typed_pin_schematic();
        let body = validate_components_json(&path, false).await;
        let pins = body["unconnected_pins"].as_array().unwrap();

        assert_eq!(body["unconnected_count"], 2);
        assert_eq!(pins[0]["pin"], "2");
        assert_eq!(pins[0]["pin_type"], "power_in");
        assert_eq!(pins[1]["pin"], "3");
        assert_eq!(pins[1]["pin_type"], "output");
        assert!(pins.iter().all(|pin| pin["pin"] != "1"));
        assert!(pins.iter().all(|pin| pin["pin"] != "4"));
    }

    #[tokio::test]
    async fn ignore_power_pins_option_is_effective() {
        let (_dir, path) = typed_pin_schematic();
        let body = validate_components_json(&path, true).await;
        let pins = body["unconnected_pins"].as_array().unwrap();

        assert_eq!(body["unconnected_count"], 1);
        assert_eq!(pins[0]["pin"], "3");
        assert_eq!(pins[0]["pin_type"], "output");
    }
}

#[cfg(test)]
mod connect_to_net_orientation_tests {
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

    /// One pin per edge, plus two pins stacked on one endpoint. Placed at
    /// (100, 100): west tip (89.84, 100), east (110.16, 100), north
    /// (100, 89.84), south (100, 110.16), stack (89.84, 94.92).
    fn quad_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let pin = |x: f64, y: f64, angle: f64, name: &str, number: &str| {
            format!(
                "        (pin passive line (at {x} {y} {angle}) (length 2.54)\n\
                 \x20         (name \"{name}\") (number \"{number}\"))\n"
            )
        };
        let body = format!(
            "{}{}{}{}{}{}",
            pin(-10.16, 0.0, 0.0, "WEST", "1"),
            pin(10.16, 0.0, 180.0, "EAST", "2"),
            pin(0.0, 10.16, 270.0, "NORTH", "3"),
            pin(0.0, -10.16, 90.0, "SOUTH", "4"),
            pin(-10.16, 5.08, 0.0, "GND", "5"),
            pin(-10.16, 5.08, 0.0, "GND", "6"),
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quad.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  \
                 (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  \
                 (lib_symbols\n    (symbol \"Test:QUAD\"\n      (symbol \"QUAD_1_1\"\n\
                 {body}      )\n    )\n  )\n  (symbol\n    (lib_id \"Test:QUAD\")\n    \
                 (at 100 100 0)\n    (unit 1)\n    \
                 (property \"Reference\" \"U1\" (at 100 90 0))\n    \
                 (property \"Value\" \"QUAD\" (at 100 110 0))\n  )\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    /// The `(at x y ROT)` and justify of the label for `net`.
    fn label_of(body: &str, net: &str) -> (String, String) {
        let start = body
            .find(&format!("(label \"{net}\""))
            .expect("label present");
        let block = &body[start..];
        let end = block.find("(uuid").unwrap_or(block.len());
        let block = &block[..end];
        let at = {
            let i = block.find("(at ").expect("at present") + 4;
            block[i..][..block[i..].find(')').unwrap()]
                .trim()
                .to_string()
        };
        let justify = match block.find("(justify ") {
            Some(j) => {
                let rest = &block[j + "(justify ".len()..];
                rest[..rest.find(')').unwrap()].trim().to_string()
            }
            None => "<none>".to_string(),
        };
        (at, justify)
    }

    async fn connect(path: &std::path::Path, net: &str, pin_number: &str) -> String {
        let result = handle_batch_connect_to_net(
            &json!({
                "schematic": path.display().to_string(),
                "net_name": net,
                "pins": [{ "reference": "U1", "pin_number": pin_number }]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        std::fs::read_to_string(path).unwrap()
    }

    /// The reported bug: a left-edge pin's label was written at rotation 0,
    /// so its text ran east across the body, over the pin names.
    #[tokio::test]
    async fn a_left_edge_pin_gets_a_label_reading_away_from_the_body() {
        let (_d, path) = quad_schematic();
        let after = connect(&path, "SWDIO", "1").await;
        assert_eq!(
            label_of(&after, "SWDIO"),
            ("89.84 100 180".into(), "right bottom".into())
        );
        assert!(konnect_sexp::parse_sexp(&after).is_ok(), "{after}");
        assert!(
            !after
                .lines()
                .any(|line| line.ends_with(' ') || line.ends_with('\t')),
            "label insertion must not leave the symbol line's indent behind: {after:?}"
        );
    }

    #[tokio::test]
    async fn a_right_edge_pin_keeps_reading_east() {
        let (_d, path) = quad_schematic();
        let after = connect(&path, "XTAL", "2").await;
        assert_eq!(
            label_of(&after, "XTAL"),
            ("110.16 100 0".into(), "left bottom".into())
        );
    }

    /// eeschema never turns a pin-anchored label sideways, whichever way a
    /// vertical pin faces — see `pin_label_rotation`.
    #[tokio::test]
    async fn vertical_pins_keep_their_label_horizontal() {
        let (_d, path) = quad_schematic();
        let after = connect(&path, "TOP", "3").await;
        assert_eq!(label_of(&after, "TOP").0, "100 89.84 0");
        let after = connect(&path, "BOTTOM", "4").await;
        assert_eq!(label_of(&after, "BOTTOM").0, "100 110.16 0");
    }

    /// Pins on one endpoint are already connected, so one label serves them
    /// all; superimposed copies render as a smear.
    #[tokio::test]
    async fn stacked_pins_share_a_single_label() {
        let (_d, path) = quad_schematic();
        let result = handle_batch_connect_to_net(
            &json!({
                "schematic": path.display().to_string(),
                "net_name": "GND",
                "pins": [
                    { "reference": "U1", "pin_number": "5" },
                    { "reference": "U1", "pin_number": "6" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        // Both pins are reported connected — the second is not an error.
        assert_eq!(parsed["added_count"], 2);
        assert_eq!(parsed["errors"].as_array().unwrap().len(), 0);
        assert_eq!(parsed["added"][1]["deduplicated"], json!(true));

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after.matches("(label \"GND\"").count(), 1, "{after}");
    }

    /// Re-running must not stack a second label on the first.
    #[tokio::test]
    async fn re_connecting_the_same_pin_adds_no_second_label() {
        let (_d, path) = quad_schematic();
        connect(&path, "SWDIO", "1").await;
        let after = connect(&path, "SWDIO", "1").await;
        assert_eq!(after.matches("(label \"SWDIO\"").count(), 1, "{after}");
    }
}

#[cfg(test)]
mod multi_unit_pin_tests {
    use crate::tools::sch_batch::tools;
    use konnect_sexp::schematic::{
        extract_lib_pins_for_unit, extract_symbol_instances, pin_endpoint, read_schematic,
    };
    use std::io::Write;

    /// Two units of one symbol, placed 15.24mm apart. Unit 1 owns pin 1, unit 2
    /// owns pin 3; both sit at local x = -7.62 in their own unit's drawing.
    const SCH: &str = r#"(kicad_sch
	(version 20241209)
	(lib_symbols
		(symbol "74xx:74HC14"
			(symbol "74HC14_1_1"
				(pin input line (at -7.62 0 0) (length 2.54)
					(name "A" (effects (font (size 1.27 1.27))))
					(number "1" (effects (font (size 1.27 1.27))))
				)
			)
			(symbol "74HC14_2_1"
				(pin input line (at -7.62 0 0) (length 2.54)
					(name "A" (effects (font (size 1.27 1.27))))
					(number "3" (effects (font (size 1.27 1.27))))
				)
			)
		)
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 100 0)
		(unit 1)
		(property "Reference" "U1" (at 100 100 0))
		(property "Value" "74HC14" (at 100 100 0))
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 115.24 0)
		(unit 2)
		(property "Reference" "U1" (at 100 115.24 0))
		(property "Value" "74HC14" (at 100 115.24 0))
	)
)
"#;

    /// The regression: resolving a pin used the FIRST instance with a matching
    /// reference, so every pin of a multi-unit part was transformed by unit 1's
    /// placement. Two nets then landed on one coordinate and were silently
    /// shorted — no error, no warning.
    #[test]
    fn each_unit_resolves_its_own_pin_position() {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();

        let (_c, tree) = read_schematic(f.path()).unwrap();
        let instances = extract_symbol_instances(&tree);
        let lib_syms = tree
            .find("lib_symbols")
            .map(|n| n.find_all("symbol"))
            .unwrap_or_default();

        let resolve = |number: &str| -> Option<(f64, f64)> {
            instances
                .iter()
                .filter(|i| i.reference == "U1")
                .find_map(|inst| {
                    let sym = lib_syms
                        .iter()
                        .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id))?;
                    extract_lib_pins_for_unit(sym, inst.unit)
                        .into_iter()
                        .find(|p| p.number == number)
                        .map(|p| pin_endpoint(&p, inst.pin_transform()))
                })
        };

        let p1 = resolve("1").expect("unit 1 pin 1");
        let p3 = resolve("3").expect("unit 2 pin 3");

        assert!(
            (p1.1 - p3.1).abs() > 1.0,
            "unit 1 and unit 2 pins must not land on the same point \
             (got {p1:?} and {p3:?}) — that is the short this guards against"
        );
        assert!(
            (p1.1 - 100.0).abs() < 0.01,
            "unit 1 pin should sit at y=100, got {p1:?}"
        );
        assert!(
            (p3.1 - 115.24).abs() < 0.01,
            "unit 2 pin should sit at y=115.24, got {p3:?}"
        );
    }

    #[test]
    fn batch_connect_to_net_is_registered() {
        assert!(tools().iter().any(|t| t.name == "batch_connect_to_net"));
    }
}

#[cfg(test)]
mod multi_unit_field_tests {
    use super::{field_value_ranges, find_symbol_blocks};
    use konnect_sexp::writer::{apply_edits, SexpEdit};

    /// A 3-unit part plus an unrelated single-unit part. Every unit repeats the
    /// reference and carries its own copy of the shared fields, which is how
    /// eeschema writes them.
    const SCH: &str = r#"(kicad_sch
	(version 20241209)
	(lib_symbols
		(symbol "74xx:74HC14"
			(property "Reference" "U")
			(property "Footprint" "")
		)
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 100 0)
		(unit 1)
		(property "Reference" "U6" (at 100 100 0))
		(property "Value" "74HC14" (at 100 100 0))
		(property "Footprint" "" (at 100 100 0))
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 115.24 0)
		(unit 2)
		(property "Reference" "U6" (at 100 115.24 0))
		(property "Value" "74HC14" (at 100 115.24 0))
		(property "Footprint" "" (at 100 115.24 0))
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 130.48 0)
		(unit 7)
		(property "Reference" "U6" (at 100 130.48 0))
		(property "Value" "74HC14" (at 100 130.48 0))
		(property "Footprint" "" (at 100 130.48 0))
	)
	(symbol
		(lib_id "Device:R")
		(at 200 100 0)
		(unit 1)
		(property "Reference" "R1" (at 200 100 0))
		(property "Value" "10k" (at 200 100 0))
		(property "Footprint" "" (at 200 100 0))
	)
)
"#;

    /// The regression: field lookup stopped at the first instance, so assigning
    /// a footprint to a multi-unit part left units 2..n blank. KiCad then had
    /// one part claiming two different footprints.
    #[test]
    fn field_edit_reaches_every_unit() {
        let ranges = field_value_ranges(SCH, "U6", "Footprint");
        assert_eq!(
            ranges.len(),
            3,
            "expected one Footprint per unit: {ranges:?}"
        );

        let edits = ranges
            .iter()
            .map(|&(s, e)| SexpEdit::replace(s, e, "Package_SO:SOIC-14".to_string()))
            .collect();
        let out = apply_edits(SCH.to_string(), edits);
        assert_eq!(
            out.matches(r#"(property "Footprint" "Package_SO:SOIC-14""#)
                .count(),
            3
        );
        // The neighbouring single-unit part must be untouched.
        assert!(out.contains(r#"(property "Reference" "R1" (at 200 100 0))"#));
        assert_eq!(
            out.matches(r#"(property "Footprint" "" (at 200"#).count(),
            1
        );
    }

    #[test]
    fn single_unit_part_still_edits_once() {
        let ranges = field_value_ranges(SCH, "R1", "Value");
        assert_eq!(ranges.len(), 1);
    }

    #[test]
    fn missing_field_yields_no_ranges() {
        assert!(field_value_ranges(SCH, "U6", "Datasheet").is_empty());
        assert!(field_value_ranges(SCH, "U99", "Value").is_empty());
    }

    /// Deleting one unit's block used to leave the other six behind as orphans
    /// referencing a component the caller believes is gone.
    #[test]
    fn delete_removes_every_unit() {
        let blocks = find_symbol_blocks(SCH, "U6");
        assert_eq!(blocks.len(), 3, "expected one block per unit: {blocks:?}");

        let edits = blocks
            .iter()
            .map(|&(s, e)| SexpEdit::delete(s, e))
            .collect();
        let out = apply_edits(SCH.to_string(), edits);
        assert!(
            !out.contains(r#""Reference" "U6""#),
            "no U6 unit should survive:\n{out}"
        );
        assert!(out.contains(r#""Reference" "R1""#), "R1 must survive");
        // The lib_symbols definition is not an instance and must stay.
        assert!(out.contains(r#"(symbol "74xx:74HC14""#));
    }

    /// The blocks must not overlap, or apply_edits would splice the file wrong.
    #[test]
    fn unit_blocks_are_disjoint_and_ordered() {
        let blocks = find_symbol_blocks(SCH, "U6");
        for w in blocks.windows(2) {
            assert!(w[0].1 <= w[1].0, "blocks overlap: {:?} {:?}", w[0], w[1]);
        }
    }
}

#[cfg(test)]
mod add_text_placement_tests {
    use super::{schematic_text_justify, tools};
    use crate::mcp::protocol::CallToolResult;
    use crate::tools::ToolContext;
    use serde_json::json;
    use std::io::Write;
    use std::sync::Arc;

    const SCH: &str = "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\")\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(uuid \"u1\")\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 100 75 0)\n\t\t)\n\t)\n\t(sheet_instances\n\t\t(path \"/\" (page \"1\"))\n\t)\n)\n";

    async fn add_text_inner(text: &str, justify: Option<&str>) -> (String, CallToolResult) {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();

        let def = tools()
            .into_iter()
            .find(|t| t.name == "add_schematic_text")
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
        let router = Arc::new(crate::router::ToolRouter::new());
        let ctx = Arc::new(ToolContext::new(cfg, router));
        let mut args = json!({
            "schematic": f.path().to_str().unwrap(),
            "text": text, "x": 30.0, "y": 114.3
        });
        if let Some(j) = justify {
            args["justify"] = json!(j);
        }
        let result = (def.handler)(&args, ctx).await.unwrap();
        (std::fs::read_to_string(f.path()).unwrap(), result)
    }

    async fn add_text(text: &str) -> String {
        add_text_inner(text, None).await.0
    }

    async fn add_text_justified(text: &str, justify: &str) -> String {
        add_text_inner(text, Some(justify)).await.0
    }

    async fn add_text_result(text: &str, justify: &str) -> CallToolResult {
        add_text_inner(text, Some(justify)).await.1
    }

    /// The regression: the text was spliced in at the file's last `)`, which
    /// puts it *after* the symbol instances and `sheet_instances`. KiCad 10
    /// requires instances last and rejects the whole file — "Failed to load
    /// schematic", with no hint as to which element is misplaced.
    #[tokio::test]
    async fn text_goes_before_the_symbol_instances() {
        let out = add_text("hello").await;
        let text_at = out.find("(text \"hello\"").expect("text written");
        let sym_at = out.find("(symbol\n\t\t(lib_id").expect("instance present");
        let sheets_at = out
            .find("(sheet_instances")
            .expect("sheet_instances present");
        assert!(
            text_at < sym_at && text_at < sheets_at,
            "text must precede symbol instances (text {text_at}, symbol {sym_at})"
        );
        // and it must land after lib_symbols, not inside it
        assert!(text_at > out.find("(lib_symbols").unwrap());
    }

    /// The other half of the same incident: the content was written with the
    /// newline as a literal byte inside the quoted string. KiCad wants the
    /// two-character escape and refuses the file otherwise.
    #[tokio::test]
    async fn multiline_text_escapes_its_newlines() {
        let out = add_text("line one\nline two").await;
        let text_at = out
            .find(r#"(text "line one\nline two""#)
            .expect("newline must be written as an escape, not a raw byte");
        assert!(text_at < out.find("(symbol\n\t\t(lib_id").unwrap());
    }

    #[tokio::test]
    async fn quotes_backslashes_and_tabs_are_escaped() {
        let out = add_text("a \"b\" c\\d\te").await;
        assert!(out.contains(r#"(text "a \"b\" c\\d\te""#), "got:\n{out}");
    }

    /// The reported defect: with no `(justify ...)` KiCad centres the text on
    /// `x`, so a long line crosses the left page edge and vanishes from the PDF
    /// export while the .kicad_sch still reads as complete.
    #[tokio::test]
    async fn text_is_left_aligned_by_default() {
        let out = add_text("hello").await;
        assert!(
            out.contains("(effects (font (size 1.27 1.27)) (justify left bottom))"),
            "got:\n{out}"
        );
    }

    #[tokio::test]
    async fn justify_is_written_as_kicad_orders_it() {
        let out = add_text_justified("hello", "top right").await;
        assert!(
            out.contains("(justify right top)"),
            "horizontal token comes first, got:\n{out}"
        );
    }

    /// KiCad has no `center` token: centred text is text with no justify at
    /// all. This is the pre-change behaviour, still reachable on request.
    #[tokio::test]
    async fn center_writes_no_justify_token() {
        let out = add_text_justified("hello", "center").await;
        assert!(!out.contains("(justify"), "got:\n{out}");
        assert!(
            out.contains("(effects (font (size 1.27 1.27)))"),
            "got:\n{out}"
        );
    }

    #[tokio::test]
    async fn an_unknown_token_is_refused() {
        let result = add_text_result("hello", "middle").await;
        assert!(
            result.is_error,
            "an unknown alignment must not be guessed at"
        );
    }

    #[tokio::test]
    async fn two_tokens_on_one_axis_are_refused() {
        let result = add_text_result("hello", "left right").await;
        assert!(result.is_error);
    }

    #[test]
    fn justify_parsing_covers_the_shapes_kicad_writes() {
        // The five forms present in KiCad 10's own demo projects.
        for (input, expected) in [
            ("left bottom", " (justify left bottom)"),
            ("right bottom", " (justify right bottom)"),
            ("left", " (justify left)"),
            // One axis alone: the other is centred by omission.
            ("bottom", " (justify bottom)"),
            ("left top", " (justify left top)"),
            ("right", " (justify right)"),
        ] {
            assert_eq!(schematic_text_justify(input).unwrap(), expected, "{input}");
        }
        assert_eq!(schematic_text_justify("center").unwrap(), "");
        assert_eq!(
            schematic_text_justify("  BOTTOM  Left ").unwrap(),
            " (justify left bottom)"
        );
        assert!(schematic_text_justify("sideways").is_err());
        assert!(schematic_text_justify("top bottom").is_err());
    }
}

/// `add_schematic_text` was not the only handler splicing at the file's last
/// `)`. `batch_connect_to_net` and `connect_to_net` did the same, and a label
/// or wire written after the symbol instances breaks the file exactly as #156
/// described — KiCad reports only "Failed to load schematic", and because the
/// file no longer loads, `kicad-cli erc` leaves a stale report in place.
#[cfg(test)]
mod insert_order_tests {
    use crate::tools::sch_wiring::insert_before_close;

    const SCH: &str = "(kicad_sch\n\t(lib_symbols\n\t\t(symbol \"Device:R\")\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(uuid \"u1\")\n\t)\n\t(sheet_instances\n\t\t(path \"/\" (page \"1\"))\n\t)\n)\n";

    #[test]
    fn labels_land_before_the_symbol_instances() {
        let out = insert_before_close(SCH, "\n  (label \"NET\" (at 10 10 0))");
        let label = out.find("(label \"NET\"").expect("label written");
        let inst = out.find("(symbol\n\t\t(lib_id").expect("instance present");
        assert!(
            label < inst,
            "a label after the instances makes the file unloadable:\n{out}"
        );
        assert!(
            !out.contains(")(symbol"),
            "elements must not be glued: {out}"
        );
        assert!(
            !out.lines()
                .any(|line| line.ends_with(' ') || line.ends_with('\t')),
            "insertion must consume the target line's indent: {out:?}"
        );
    }

    /// The old splice point, for contrast: the file's final `)` sits after
    /// everything, so anything inserted there lands last.
    #[test]
    fn the_old_final_paren_splice_would_land_after_the_instances() {
        let close = SCH.rfind(')').unwrap();
        let inst = SCH.find("(symbol\n\t\t(lib_id").unwrap();
        assert!(
            close > inst,
            "this test is meaningless if the last paren precedes the instances"
        );
    }
}

#[cfg(test)]
mod batch_edit_metadata_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use serde_json::json;
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

    const SCH: &str = "(kicad_sch\n\t(version 20250610)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Device:C\"\n\t\t\t(property \"Reference\" \"C\" (at 0 0 0))\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:C\")\n\t\t(at 10 10 0)\n\t\t(unit 1)\n\t\t(in_bom yes)\n\t\t(on_board yes)\n\t\t(dnp no)\n\t\t(uuid \"c1\")\n\t\t(property \"Reference\" \"C1\" (at 10 8 0))\n\t\t(property \"Value\" \"100n\" (at 10 12 0))\n\t)\n\t(symbol\n\t\t(lib_id \"Device:C\")\n\t\t(at 20 10 0)\n\t\t(unit 1)\n\t\t(in_bom yes)\n\t\t(on_board yes)\n\t\t(dnp no)\n\t\t(uuid \"c2\")\n\t\t(property \"Reference\" \"C2\" (at 20 8 0))\n\t\t(property \"Value\" \"1u\" (at 20 12 0))\n\t)\n)\n";

    #[tokio::test]
    async fn batch_edit_writes_population_metadata_per_component() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch.kicad_sch");
        std::fs::write(&path, SCH).unwrap();

        let result = handle_batch_edit(
            &json!({
                "schematic": path.to_str().unwrap(),
                "edits": [
                    { "reference": "C1", "dnp": true, "exclude_from_bom": true },
                    { "reference": "C2", "dnp": false, "exclude_from_board": true }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        let out = std::fs::read_to_string(&path).unwrap();
        let c1_start = out.find("(property \"Reference\" \"C1\"").unwrap();
        let c2_start = out.find("(property \"Reference\" \"C2\"").unwrap();
        let c1 = &out[..c2_start];
        let c2 = &out[c1_start..];
        assert!(c1.contains("(dnp yes)"), "{out}");
        assert!(c1.contains("(in_bom no)"), "{out}");
        assert!(c2.contains("(dnp no)"), "{out}");
        assert!(c2.contains("(on_board no)"), "{out}");
    }
}

/// #202: `bulk_move` shifted only the symbol's own `(at …)`. Property `(at …)`
/// coordinates are absolute in `.kicad_sch`, so Reference and Value text
/// stayed at the old location while the symbol moved away. The typed path
/// (`move_schematic_component` → `Symbol::translate`) always translated the
/// properties too — this was the second, text-based implementation that never
/// got the fix.
#[cfg(test)]
mod bulk_move_field_tests {
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

    /// One symbol with Reference and Value at eeschema-style offsets beside
    /// it. Reference carries a rotation, which must survive the move.
    const SCH: &str = "(kicad_sch\n\t(version 20250610)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\" (at 0 0 0))\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 101.6 101.6 0)\n\t\t(unit 1)\n\t\t(uuid \"sym-1\")\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 105.232 100.33 90)\n\t\t\t(effects (font (size 1.27 1.27)))\n\t\t)\n\t\t(property \"Value\" \"10k\"\n\t\t\t(at 105.232 102.87 0)\n\t\t)\n\t\t(instances\n\t\t\t(project \"p\"\n\t\t\t\t(path \"/root\" (reference \"R1\") (unit 1))\n\t\t\t)\n\t\t)\n\t)\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n";

    /// The placed symbol's `(at …)` and each property's, read back from the
    /// written file. Numeric, so a float-formatting change can't break the
    /// test and a wrong coordinate can't hide behind one.
    fn positions(sch: &str) -> (Vec<f64>, Vec<(String, Vec<f64>)>) {
        let tree = konnect_sexp::parse_sexp(sch).expect("parses");
        let symbol = tree
            .children()
            .unwrap()
            .iter()
            .find(|n| n.head() == Some("symbol") && n.find("lib_id").is_some())
            .expect("placed symbol");
        let at_of = |n: &konnect_sexp::SexpNode| -> Vec<f64> {
            let at = n.find("at").expect("(at …)");
            (1..at.children().unwrap().len())
                .filter_map(|i| at.get_f64(i))
                .collect()
        };
        let props = symbol
            .find_all("property")
            .into_iter()
            .map(|p| {
                (
                    p.get(1).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    at_of(p),
                )
            })
            .collect();
        (at_of(symbol), props)
    }

    async fn bulk_move(dx: f64, dy: f64) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("move.kicad_sch");
        std::fs::write(&path, SCH).unwrap();
        let result = handle_bulk_move(
            &json!({ "schematic": path.to_str().unwrap(),
                     "references": ["R1"], "dx": dx, "dy": dy }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        std::fs::read_to_string(&path).unwrap()
    }

    /// #120 end to end: a dot the pin vacates is pruned, and the response says
    /// so. #315's wire-carrying move is gated on exactly this judgement.
    ///
    /// Fixture is eeschema's own output (`kicad-cli sch upgrade`), not a
    /// hand-written sheet — R1's pin sits mid-span on a wire at
    /// (120.65, 139.7) and earns the dot there. Moving R1 away must strand it.
    #[tokio::test]
    async fn bulk_move_prunes_the_junction_its_pin_vacates() {
        const SHEET: &str = include_str!("../../tests/fixtures/junction_reconcile.kicad_sch");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.kicad_sch");
        std::fs::write(&path, SHEET).unwrap();
        let dots = |p: &std::path::Path| -> usize {
            std::fs::read_to_string(p)
                .unwrap()
                .matches("(junction")
                .count()
        };
        assert_eq!(dots(&path), 3, "fixture starts with three dots");

        let result = handle_bulk_move(
            &json!({ "schematic": path.to_str().unwrap(),
                     "references": ["R1"], "dx": 0.0, "dy": -20.32 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            !after.contains("(at 120.65 139.7)"),
            "the dot R1's pin left must be pruned: {after}"
        );
        // The T and the bus tee are untouched — only R1's point was in scope.
        assert_eq!(dots(&path), 2, "exactly one dot removed");
        assert!(after.contains("(at 120.65 170.18)"), "the T survives");
        assert!(after.contains("(at 260.35 140)"), "the bus tee survives");

        let body = format!("{:?}", result.content);
        assert!(
            body.contains("junctions_pruned_count"),
            "the response must report what it did: {body}"
        );
    }

    /// Every property keeps its offset from the symbol — which is the same as
    /// saying it moved by whatever the symbol actually moved.
    async fn assert_fields_follow(dx: f64, dy: f64) {
        let (before_sym, before_props) = positions(SCH);
        let after_src = bulk_move(dx, dy).await;
        let (after_sym, after_props) = positions(&after_src);

        // The handler snaps to the 1.27 grid, so the effective delta is not
        // necessarily the requested one — the fields must follow the real one.
        let (mdx, mdy) = (after_sym[0] - before_sym[0], after_sym[1] - before_sym[1]);
        assert_eq!(before_props.len(), after_props.len());
        for ((name, before), (after_name, after)) in before_props.iter().zip(&after_props) {
            assert_eq!(name, after_name, "property order preserved");
            assert!(
                (after[0] - (before[0] + mdx)).abs() < 1e-6
                    && (after[1] - (before[1] + mdy)).abs() < 1e-6,
                "'{name}' must move with the symbol (delta {mdx}, {mdy}): \
                 {before:?} -> {after:?}\n{after_src}"
            );
            // A property's own rotation is independent of a translation.
            assert_eq!(
                before.get(2),
                after.get(2),
                "'{name}' rotation must not change"
            );
        }
        assert!(konnect_sexp::parse_sexp(&after_src).is_ok());
    }

    #[tokio::test]
    async fn field_text_moves_with_the_symbol() {
        // On-grid delta: symbol lands exactly where asked.
        assert_fields_follow(12.7, 2.54).await;
    }

    #[tokio::test]
    async fn fields_follow_the_snapped_delta_not_the_requested_one() {
        // Off-grid delta: the symbol snaps, so the fields must move by the
        // snapped amount or they drift relative to the part.
        assert_fields_follow(1.0, 0.0).await;
    }

    /// A negative move exercises the same path in the other direction.
    #[tokio::test]
    async fn field_text_follows_a_negative_move() {
        assert_fields_follow(-25.4, -12.7).await;
    }
}

#[cfg(test)]
mod power_symbol_connection_tests {
    use super::*;
    use crate::tools::{ServerConfig, ToolContext};
    use std::io::Write;
    use std::sync::Arc;

    /// A `power:GND` symbol dropped straight onto R1's pin 2 — no wire between
    /// them, which is how KiCad itself draws a decoupling ground. Pin 1 is
    /// genuinely unconnected.
    const SCH: &str = include_str!("../../tests/fixtures/power_symbol_on_pin.kicad_sch");

    /// The regression: the graph knew only labels, so a pin whose entire
    /// connection is a power symbol was reported unconnected.
    #[tokio::test]
    async fn a_pin_under_a_power_symbol_is_connected() {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();

        let ctx = ToolContext::new(
            ServerConfig::default(),
            Arc::new(crate::router::ToolRouter::new()),
        );
        let result = handle_validate_component_connections(
            &json!({ "schematic": f.path().to_str().unwrap() }),
            &ctx,
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        let s: serde_json::Value = serde_json::from_str(text).unwrap();

        let unconnected: Vec<(&str, &str)> = s["unconnected_pins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["reference"].as_str().unwrap(), p["pin"].as_str().unwrap()))
            .collect();
        assert_eq!(unconnected, vec![("R1", "1")], "only pin 1 floats: {s}");
    }
}

#[cfg(test)]
mod layout_bounds_tests {
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

    /// The retained nodes are from KiCad 10's stock Device:R definition; the
    /// surrounding schematic is reduced to what this read-only query needs.
    fn schematic() -> &'static str {
        r#"(kicad_sch
	(version 20260206)
	(generator "eeschema")
	(lib_symbols
		(symbol "Device:R"
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
	)
	(symbol
		(lib_id "Device:R")
		(at 100 50 0)
		(unit 1)
		(uuid "r1")
		(property "Reference" "R1" (at 102 50 90))
		(property "Value" "10k" (at 100 50 90))
	)
)
"#
    }

    fn response_json(result: &CallToolResult) -> serde_json::Value {
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn schematic_layout_bounds_enclose_graphics_and_pin_tips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("layout.kicad_sch");
        std::fs::write(&path, schematic()).unwrap();

        let result = handle_get_layout(
            &json!({
                "schematic": path.to_string_lossy(),
                "include_wires": false,
                "include_labels": false
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let result = response_json(&result);

        assert_eq!(result["bounds_resolved"], 1);
        assert_eq!(result["bounds_unresolved"], json!([]));
        assert_eq!(result["bounding_box"]["x_min"], 98.984);
        assert_eq!(result["bounding_box"]["x_max"], 101.016);
        assert_eq!(result["bounding_box"]["y_min"], 46.19);
        assert_eq!(result["bounding_box"]["y_max"], 53.81);
        assert_eq!(result["components"][0]["bounds"], result["bounding_box"]);
        assert_ne!(
            result["bounding_box"]["x_min"], 100.0,
            "an origin-only box reproduces the old false result"
        );
    }

    /// A real eeschema save (KiCad's ecc83 demo): U1 is placed as three units
    /// of the embedded dual triode — two identical triode units and one
    /// heater unit with different library geometry. Every placement must get
    /// its own resolved bounds from its OWN unit's drawing, or a multi-unit
    /// component reports one unit's box three times.
    #[tokio::test]
    async fn every_placed_unit_gets_bounds_from_its_own_geometry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ecc83.kicad_sch");
        std::fs::write(
            &path,
            include_str!("../../tests/fixtures/ecc83_multiunit.kicad_sch"),
        )
        .unwrap();

        let result =
            handle_get_layout(&json!({ "schematic": path.to_string_lossy() }), &test_ctx())
                .await
                .unwrap();
        let result = response_json(&result);

        assert_eq!(result["bounds_unresolved"], json!([]), "{result}");
        let u1_boxes: Vec<(f64, f64)> = result["components"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|component| component["reference"] == "U1")
            .map(|component| {
                let bounds = &component["bounds"];
                assert!(
                    !bounds.is_null(),
                    "every U1 placement resolves bounds: {component}"
                );
                (
                    bounds["width"].as_f64().unwrap(),
                    bounds["height"].as_f64().unwrap(),
                )
            })
            .collect();
        assert_eq!(u1_boxes.len(), 3, "three placed units of U1");
        let distinct: std::collections::BTreeSet<String> = u1_boxes
            .iter()
            .map(|(w, h)| format!("{w:.3}x{h:.3}"))
            .collect();
        assert!(
            distinct.len() >= 2,
            "the heater unit's geometry differs from the triodes', so one \
             shared box means unit selection is broken: {u1_boxes:?}"
        );
    }

    #[tokio::test]
    async fn unresolved_geometry_is_named_and_its_origin_remains_covered() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("unresolved.kicad_sch");
        let source = schematic().replace(
            "\n)\n",
            "\n\t(symbol\n\t\t(lib_id \"Missing:Part\")\n\t\t(at 120 60 0)\n\t\t(unit 1)\n\t\t(uuid \"u1\")\n\t\t(property \"Reference\" \"U1\")\n\t)\n)\n",
        );
        std::fs::write(&path, source).unwrap();

        let result = handle_get_layout(&json!({"schematic": path.to_string_lossy()}), &test_ctx())
            .await
            .unwrap();
        let result = response_json(&result);

        assert_eq!(result["bounds_resolved"], 1);
        assert_eq!(result["bounds_unresolved"], json!(["U1"]));
        assert_eq!(result["components"][1]["bounds"], serde_json::Value::Null);
        assert_eq!(result["bounding_box"]["x_max"], 120.0);
        assert_eq!(result["bounding_box"]["y_max"], 60.0);
    }
}
