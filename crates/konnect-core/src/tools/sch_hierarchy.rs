//! `sch_hierarchy` toolset — sheet object lifecycle (PR-A) plus sheet pin
//! lifecycle (PR-B): add, edit, move, delete, duplicate a sheet; recursive
//! hierarchy/page-numbering queries; import/add/edit/delete sheet pins and a
//! read-only pin/label sync check.
//!
//! Every handler here is file-editing only — KiCAD's own IPC API has no
//! schematic-editing commands upstream (`schematic_commands.proto` is empty),
//! so there's no dual IPC/file path to maintain, unlike the PCB toolsets.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    get_path, opt_f64, opt_str, project_name_for, require_f64, require_str, ToolContext, ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_schematic_editor::types::fmt_f64;
use konnect_sexp::schematic::{format_hierarchical_sheet, HierarchicalSheetSpec};
use konnect_sexp::{
    commit_command, commit_file_transaction, parse_sexp, prepare_command, read_consistent,
    DocumentRevision, FileTransition, ItemAnchor, ItemChange, ItemId, SchematicCommand,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "add_hierarchical_sheet",
            "Insert a hierarchical sheet into a parent schematic, linking it to a child \
             .kicad_sch file. Creates the child file (blank) if it doesn't exist yet, or \
             links to it as-is if it does — reusing an existing file places the *same* \
             sub-circuit at a second location (KiCAD's multi-instance sheet pattern) rather \
             than duplicating it. If the linked file already has symbols in it, their \
             hierarchical instance paths are patched immediately so ERC resolves them.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to the parent .kicad_sch file" },
                    "sheet_file": { "type": "string", "description": "Filename of the child .kicad_sch, resolved relative to the parent's directory" },
                    "sheet_name": { "type": "string", "description": "Display name (Sheetname property). Default: 'Sheet'" },
                    "x": { "type": "number", "description": "Top-left X in mm. Default: 50" },
                    "y": { "type": "number", "description": "Top-left Y in mm. Default: 50" },
                    "width": { "type": "number", "description": "Sheet box width in mm. Default: 80" },
                    "height": { "type": "number", "description": "Sheet box height in mm. Default: 50" },
                    "project_name": { "type": "string", "description": "Project name key for the page-number instance entry. Default: the schematic file's stem (matching eeschema)" }
                },
                "required": ["schematic", "sheet_file"]
            }),
            |args, ctx| async move { handle_add_hierarchical_sheet(args, ctx).await }
        ),
        tool!(
            "edit_sheet",
            "Rename, resize, reposition, or repoint (Sheetfile) an existing sheet. Provide \
             at least one of: new_name, new_file, or both x+y, or both width+height. Does \
             NOT rename the child file on disk when new_file is given — it only repoints \
             the reference; the file itself must already exist at that path.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string", "description": "Current Sheetname to look up" },
                    "new_name": { "type": "string" },
                    "new_file": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "width": { "type": "number" }, "height": { "type": "number" },
                    "project_name": { "type": "string", "description": PROJECT_NAME_DESC }
                },
                "required": ["schematic", "sheet_name"]
            }),
            |args, ctx| async move { handle_edit_sheet(args, ctx).await }
        ),
        tool!(
            "move_sheet",
            "Reposition a sheet on the parent canvas without touching any other field.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "sheet_name", "x", "y"]
            }),
            |args, ctx| async move { handle_move_sheet(args, ctx).await }
        ),
        tool!(
            "delete_sheet",
            "Remove a sheet reference from the parent schematic. Does NOT delete the child \
             .kicad_sch file on disk. Remaining sheets' page numbers may now have a gap — \
             call renumber_sheet_pages afterward if that matters.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string" }
                },
                "required": ["schematic", "sheet_name"]
            }),
            |args, ctx| async move { handle_delete_sheet(args, ctx).await }
        ),
        tool!(
            "delete_unlinked_child_schematic",
            "Dry-run or apply safe cleanup of an unused child .kicad_sch file. The target \
             must be a regular .kicad_sch under the root schematic's project directory, must \
             not be reachable from the root hierarchy, and must contain no symbols, wires, \
             labels, graphics, no-connects, or child sheets. Apply mode requires dry_run=false \
             plus the exact plan_revision returned by dry-run.",
            json!({
                "type": "object",
                "properties": {
                    "root_schematic": { "type": "string", "description": "Root .kicad_sch used to prove the target file is not linked anywhere in the active hierarchy" },
                    "target_schematic": { "type": "string", "description": "Unlinked empty child .kicad_sch file to delete" },
                    "dry_run": { "type": "boolean", "description": "Default true. false deletes the already-reviewed file.", "default": true },
                    "plan_revision": { "type": "string", "description": "Exact dry-run revision required when dry_run=false" }
                },
                "required": ["root_schematic", "target_schematic"]
            }),
            |args, ctx| async move { handle_delete_unlinked_child_schematic(args, ctx).await }
        ),
        tool!(
            "duplicate_sheet",
            "Copy an existing sheet and its child .kicad_sch file under a new name/file, \
             offset slightly so the new sheet box doesn't overlap the source. The copy gets \
             its own internal schematic UUID and its symbols' hierarchical instance paths \
             are patched for the new sheet — it is a fully independent sub-circuit, not a \
             live-linked reuse (for that, use add_hierarchical_sheet pointed at the existing file).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "source_sheet_name": { "type": "string" },
                    "new_sheet_name": { "type": "string" },
                    "new_file": { "type": "string", "description": "Filename for the copy, resolved relative to the parent's directory. Must not already exist." },
                    "project_name": { "type": "string", "description": PROJECT_NAME_DESC }
                },
                "required": ["schematic", "source_sheet_name", "new_sheet_name", "new_file"]
            }),
            |args, ctx| async move { handle_duplicate_sheet(args, ctx).await }
        ),
        tool!(
            "move_schematic_items_to_sheet",
            "Dry-run or atomically apply a hierarchy migration by moving existing schematic \
             objects between sheets: move existing objects between sheets, migrate hierarchy, \
             hierarchical restructure. Select by component references, item UUIDs, or bounding \
             box; optionally include connected local wires, junctions, labels, power symbols, \
             no-connect markers, text, and graphics. Moving preserves exact item blocks, DNP/BOM \
             metadata, symbol units, UUIDs where safe, embedded symbol definitions, and patches \
             moved symbols' hierarchical instance paths for the target child sheet. Apply mode \
             requires dry_run=false plus the exact plan_revision returned by dry-run.",
            json!({
                "type": "object",
                "properties": {
                    "source_schematic": { "type": "string", "description": "Path to the source .kicad_sch file containing the existing objects" },
                    "target_schematic": { "type": "string", "description": "Path to the linked child .kicad_sch file that will receive the objects" },
                    "selection": {
                        "type": "object",
                        "description": "Objects to move. references selects every placed unit of each component reference; uuids selects explicit top-level schematic item UUIDs; bbox selects items intersecting a region in schematic millimeters.",
                        "properties": {
                            "references": { "type": "array", "items": { "type": "string" } },
                            "uuids": { "type": "array", "items": { "type": "string" } },
                            "bbox": { "description": "Either [min_x,min_y,max_x,max_y] or {min_x,min_y,max_x,max_y}; coordinates are schematic millimeters." }
                        },
                        "additionalProperties": false
                    },
                    "include_connected": {
                        "type": "object",
                        "description": "Optional local-neighborhood expansion for objects intersecting the selected region or selected component bodies.",
                        "properties": {
                            "wires": { "type": "boolean", "default": false },
                            "junctions": { "type": "boolean", "default": false },
                            "labels": { "type": "boolean", "default": false },
                            "power_symbols": { "type": "boolean", "default": false },
                            "no_connects": { "type": "boolean", "default": false },
                            "text": { "type": "boolean", "default": false },
                            "graphics": { "type": "boolean", "default": false }
                        },
                        "additionalProperties": false
                    },
                    "placement": {
                        "type": "object",
                        "description": "Where moved objects land in the target. preserve_coordinates keeps coordinates unchanged; offset adds dx/dy; normalize_origin moves the selected region's minimum corner to dx/dy.",
                        "properties": {
                            "mode": { "type": "string", "enum": ["preserve_coordinates", "offset", "normalize_origin"], "default": "preserve_coordinates" },
                            "dx": { "type": "number", "default": 0 },
                            "dy": { "type": "number", "default": 0 }
                        },
                        "additionalProperties": false
                    },
                    "allow_partial_multi_unit": {
                        "type": "boolean",
                        "description": "When false, selecting only some placed units of a multi-unit reference is refused.",
                        "default": false
                    },
                    "dry_run": { "type": "boolean", "description": "Default true. false applies the already-reviewed plan.", "default": true },
                    "plan_revision": { "type": "string", "description": "Exact dry-run revision required when dry_run=false" },
                    "project_name": { "type": "string", "description": PROJECT_NAME_DESC }
                },
                "required": ["source_schematic", "target_schematic", "selection"]
            }),
            |args, ctx| async move { handle_move_schematic_items_to_sheet(args, ctx).await }
        ),
        tool!(
            "get_sheet_hierarchy",
            "Recursively walk the sheet tree starting from a schematic file, returning \
             nested JSON: each sheet's name/file/uuid/position/size/page/pins plus its own \
             children. Handles missing child files and reference cycles gracefully instead \
             of failing.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Root schematic to start from" },
                    "project_name": { "type": "string", "description": PROJECT_NAME_DESC }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_get_sheet_hierarchy(args, ctx).await }
        ),
        tool!(
            "renumber_sheet_pages",
            "Walk the whole sheet tree from a root schematic and reassign sequential page \
             numbers (2, 3, 4, ... — page 1 is always the root and is left untouched) in \
             depth-first order. Fixes gaps left by delete_sheet/duplicate_sheet. Only \
             touches files whose page numbers actually changed.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Root schematic to start from" },
                    "project_name": { "type": "string", "description": PROJECT_NAME_DESC }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_renumber_sheet_pages(args, ctx).await }
        ),
        tool!(
            "import_sheet_pins",
            "Scan the child sheet's hierarchical_labels and auto-generate matching pins on \
             the parent sheet block, skipping names that already have a pin. This is the \
             primary, expected way sheet pins get created — mirrors KiCAD's own 'Import Sheet \
             Pins' command rather than pairing every pin to a label by hand. New pins are \
             placed along one edge of the sheet box, stacked below any existing pins.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to the parent .kicad_sch file" },
                    "sheet_name": { "type": "string" },
                    "side": { "type": "string", "enum": ["right", "left"], "description": "Which edge to place new pins on. Default: 'right'" }
                },
                "required": ["schematic", "sheet_name"]
            }),
            |args, ctx| async move { handle_import_sheet_pins(args, ctx).await }
        ),
        tool!(
            "add_sheet_pin",
            "Manually add a single pin to an existing sheet block. Prefer import_sheet_pins \
             for the common case; use this when a hierarchical_label hasn't been written yet \
             or a pin needs to exist ahead of the label.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string" },
                    "pin_name": { "type": "string" },
                    "pin_type": { "type": "string", "enum": ALLOWED_PIN_TYPES },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "sheet_name", "pin_name", "pin_type", "x", "y"]
            }),
            |args, ctx| async move { handle_add_sheet_pin(args, ctx).await }
        ),
        tool!(
            "edit_sheet_pin",
            "Rename a sheet pin, change its electrical type, or reposition it along the \
             sheet border. Provide at least one of: new_name, pin_type, or both x+y.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string" },
                    "pin_name": { "type": "string", "description": "Current pin name to look up" },
                    "new_name": { "type": "string" },
                    "pin_type": { "type": "string", "enum": ALLOWED_PIN_TYPES },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "sheet_name", "pin_name"]
            }),
            |args, ctx| async move { handle_edit_sheet_pin(args, ctx).await }
        ),
        tool!(
            "delete_sheet_pin",
            "Remove a single pin from a sheet without touching the rest of the sheet.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string" },
                    "pin_name": { "type": "string" }
                },
                "required": ["schematic", "sheet_name", "pin_name"]
            }),
            |args, ctx| async move { handle_delete_sheet_pin(args, ctx).await }
        ),
        tool!(
            "validate_sheet_pins",
            "Read-only. Walk the whole sheet tree from a root schematic and report \
             hierarchical_labels with no matching parent sheet pin, and sheet pins with no \
             matching child hierarchical_label. Does not modify anything — use as a pre-ERC \
             sanity check or to catch drift after manual edits.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Root schematic to start from" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_validate_sheet_pins(args, ctx).await }
        ),
    ]
}

// ─── Shared helpers ─────────────────────────────────────────────────────────

pub(crate) const MAX_HIERARCHY_DEPTH: usize = 20;
const ALLOWED_PIN_TYPES: &[&str] = &["input", "output", "bidirectional", "tri_state", "passive"];
const SHEET_PIN_SPACING_MM: f64 = 2.54;
const PROJECT_NAME_DESC: &str =
    "Project name key for instance entries. Default: the schematic file's stem (matching eeschema)";

fn validate_pin_type(pin_type: &str) -> Result<(), CallToolResult> {
    if ALLOWED_PIN_TYPES.contains(&pin_type) {
        Ok(())
    } else {
        Err(CallToolResult::error(format!(
            "Invalid pin_type '{}' — must be one of: {}",
            pin_type,
            ALLOWED_PIN_TYPES.join(", ")
        )))
    }
}

fn parent_dir(sch_path: &Path) -> PathBuf {
    sch_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
fn create_blank_schematic(path: &Path) -> anyhow::Result<()> {
    let template = crate::tools::blank_schematic_template();
    konnect_sexp::writer::write_new_atomic(path, &template)?;
    // Round-trip through cse so the file is normalised to its writer's format,
    // matching the existing `create_schematic` tool's behavior.
    let sch = cse::Schematic::load(path)?;
    sch.overwrite()?;
    Ok(())
}

fn next_free_page(parent: &cse::Schematic, project_name: &str) -> u32 {
    let mut max_page: u32 = 1; // page 1 is always the root sheet
    for sheet in parent.sheets.iter() {
        if let Some(p) = sheet.page(project_name) {
            if let Ok(n) = p.parse::<u32>() {
                max_page = max_page.max(n);
            }
        }
    }
    max_page + 1
}

fn sheet_json(sheet: &cse::Sheet, project_name: &str) -> Value {
    let (x, y) = sheet.position();
    json!({
        "name": sheet.name(),
        "file": sheet.file(),
        "uuid": sheet.uuid,
        "x": x,
        "y": y,
        "width": sheet.width,
        "height": sheet.height,
        "page": sheet.page(project_name),
        "pins": sheet.pins.iter().map(|p| {
            let (px, py) = p.position();
            json!({ "name": p.name, "pin_type": p.pin_type, "x": px, "y": py })
        }).collect::<Vec<_>>()
    })
}

fn ensure_source_root_uuid(source: &str) -> anyhow::Result<(String, String)> {
    let tree = parse_sexp(source)?;
    if let Some(uuid) = tree.find_str("uuid") {
        return Ok((source.to_owned(), uuid.to_owned()));
    }
    let uuid = konnect_sexp::writer::new_uuid();
    let children = konnect_sexp::writer::find_direct_child_blocks(source, "kicad_sch");
    let anchor = children
        .iter()
        .find_map(|(start, end)| {
            let node = parse_sexp(&source[*start..*end]).ok()?;
            (!matches!(
                node.head(),
                Some("version" | "generator" | "generator_version")
            ))
            .then_some(*start)
        })
        .ok_or_else(|| anyhow::anyhow!("parent schematic has no UUID insertion anchor"))?;
    let line_start = source[..anchor]
        .rfind('\n')
        .map_or(anchor, |newline| newline + 1);
    let indent = &source[line_start..anchor];
    if !indent.chars().all(char::is_whitespace) {
        anyhow::bail!("parent schematic metadata is not line-oriented");
    }
    let replacement = format!("{indent}(uuid \"{uuid}\")\n");
    let updated = konnect_sexp::writer::apply_edits(
        source.to_owned(),
        vec![konnect_sexp::writer::SexpEdit::insert(
            line_start,
            replacement,
        )],
    );
    Ok((updated, uuid))
}

/// Give every item in a duplicated document its own UUID.
///
/// `duplicate_sheet` rewrote only the root `(uuid ...)`. Every nested item —
/// text, symbols, wires, labels, sheet pins — arrived in the copy still
/// carrying the source's UUID, so two sheets claimed the same identities and
/// anything resolving by UUID picks one of them arbitrarily.
///
/// Replacements are applied per quoted string rather than by substring, and a
/// string is remapped segment by segment, so an instance `(path "/a/b")` that
/// names a renamed item follows it instead of dangling. Matching whole segments
/// also keeps short non-UUID identifiers, which fixtures and project names use,
/// from being rewritten where they merely occur inside another word.
fn regenerate_item_uuids(source: &str) -> String {
    const DECLARATION: &str = "(uuid \"";

    let mut mapping: HashMap<&str, String> = HashMap::new();
    let mut rest = source;
    while let Some(at) = rest.find(DECLARATION) {
        let body = &rest[at + DECLARATION.len()..];
        let Some(end) = body.find('"') else { break };
        let declared = &body[..end];
        if !declared.is_empty() {
            mapping
                .entry(declared)
                .or_insert_with(|| uuid::Uuid::new_v4().to_string());
        }
        rest = &body[end..];
    }
    if mapping.is_empty() {
        return source.to_owned();
    }

    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(at) = rest.find('"') {
        out.push_str(&rest[..=at]);
        let body = &rest[at + 1..];
        let mut end = None;
        let mut escaped = false;
        for (index, ch) in body.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => {
                    end = Some(index);
                    break;
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            rest = body;
            break;
        };
        out.push_str(&remap_uuid_string(&body[..end], &mapping));
        out.push('"');
        rest = &body[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Remap a quoted string: either the whole value, or each `/`-separated segment
/// of an instance path.
fn remap_uuid_string(value: &str, mapping: &HashMap<&str, String>) -> String {
    if let Some(replacement) = mapping.get(value) {
        return replacement.clone();
    }
    if !value.contains('/') {
        return value.to_owned();
    }
    value
        .split('/')
        .map(|segment| {
            mapping
                .get(segment)
                .map_or(segment, |replacement| replacement.as_str())
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn replace_source_root_uuid(source: &str, uuid: &str) -> anyhow::Result<String> {
    let children = konnect_sexp::writer::find_direct_child_blocks(source, "kicad_sch");
    let range = children.iter().find_map(|(start, end)| {
        parse_sexp(&source[*start..*end])
            .ok()
            .is_some_and(|node| node.head() == Some("uuid"))
            .then_some((*start, *end))
    });
    if let Some((start, end)) = range {
        return Ok(konnect_sexp::writer::apply_edits(
            source.to_owned(),
            vec![konnect_sexp::writer::SexpEdit::replace(
                start,
                end,
                format!("(uuid \"{uuid}\")"),
            )],
        ));
    }
    let (with_uuid, generated) = ensure_source_root_uuid(source)?;
    replace_source_root_uuid(&with_uuid, uuid)
        .or_else(|_| anyhow::bail!("could not replace newly inserted schematic UUID {generated}"))
}

/// Commit one edited sheet item and report whether the document changed.
///
/// A command that restates the block already on disk is valid and commits as a
/// no-op, so callers that set a value unconditionally get `false` here rather
/// than an error.
fn commit_edited_sheet_item(
    path: &Path,
    before: &str,
    edited: &cse::Schematic,
    uuid: &str,
    label: &str,
) -> anyhow::Result<bool> {
    let command = SchematicCommand::replace_item_from_document(
        before,
        &edited.to_source(),
        ItemId::new(uuid)?,
        label,
    )?;
    Ok(commit_command(path, &command)?.changed)
}

// ─── Cross-sheet item migration ──────────────────────────────────────────────

const MIGRATABLE_TAGS: &[&str] = &[
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
struct Bounds {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl Bounds {
    fn from_point(x: f64, y: f64) -> Self {
        Self {
            min_x: x,
            min_y: y,
            max_x: x,
            max_y: y,
        }
    }

    fn include(&mut self, x: f64, y: f64) {
        if !x.is_finite() || !y.is_finite() {
            return;
        }
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

    fn expanded(self, amount: f64) -> Self {
        Self {
            min_x: self.min_x - amount,
            min_y: self.min_y - amount,
            max_x: self.max_x + amount,
            max_y: self.max_y + amount,
        }
    }

    fn contains(self, x: f64, y: f64) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }

    fn intersects(self, other: Self) -> bool {
        self.min_x <= other.max_x
            && self.max_x >= other.min_x
            && self.min_y <= other.max_y
            && self.max_y >= other.min_y
    }

    fn json(self) -> Value {
        json!({
            "min_x": self.min_x,
            "min_y": self.min_y,
            "max_x": self.max_x,
            "max_y": self.max_y
        })
    }
}

#[derive(Debug, Clone)]
struct SchematicItem {
    uuid: String,
    kind: String,
    source: String,
    reference: Option<String>,
    value: Option<String>,
    footprint: Option<String>,
    lib_id: Option<String>,
    lib_name: Option<String>,
    unit: Option<u32>,
    bounds: Option<Bounds>,
}

impl SchematicItem {
    fn is_power_symbol(&self) -> bool {
        self.kind == "symbol"
            && self
                .lib_id
                .as_deref()
                .is_some_and(|lib_id| lib_id.starts_with("power:"))
    }

    fn lib_symbol_key(&self) -> Option<&str> {
        self.lib_name.as_deref().or(self.lib_id.as_deref())
    }
}

#[derive(Debug, Clone, Copy)]
struct IncludeConnected {
    wires: bool,
    junctions: bool,
    labels: bool,
    power_symbols: bool,
    no_connects: bool,
    text: bool,
    graphics: bool,
}

impl IncludeConnected {
    fn parse(args: &Value) -> Result<Self, CallToolResult> {
        let include = args.get("include_connected").unwrap_or(&Value::Null);
        if !include.is_null() && !include.is_object() {
            return Err(CallToolResult::error(
                "include_connected must be an object when supplied",
            ));
        }
        let flag = |name: &str| include.get(name).and_then(Value::as_bool).unwrap_or(false);
        Ok(Self {
            wires: flag("wires"),
            junctions: flag("junctions"),
            labels: flag("labels"),
            power_symbols: flag("power_symbols"),
            no_connects: flag("no_connects"),
            text: flag("text"),
            graphics: flag("graphics"),
        })
    }

    fn allows(self, item: &SchematicItem) -> bool {
        match item.kind.as_str() {
            "wire" | "bus" | "bus_entry" => self.wires,
            "junction" => self.junctions,
            "label" | "global_label" | "hierarchical_label" => self.labels,
            "no_connect" => self.no_connects,
            "text" | "text_box" => self.text,
            "polyline" | "rectangle" | "circle" | "arc" | "image" => self.graphics,
            "symbol" if item.is_power_symbol() => self.power_symbols,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlacementMode {
    PreserveCoordinates,
    Offset,
    NormalizeOrigin,
}

#[derive(Debug, Clone, Copy)]
struct Placement {
    mode: PlacementMode,
    dx: f64,
    dy: f64,
}

impl Placement {
    fn parse(args: &Value) -> Result<Self, CallToolResult> {
        let placement = args.get("placement").unwrap_or(&Value::Null);
        if !placement.is_null() && !placement.is_object() {
            return Err(CallToolResult::error(
                "placement must be an object when supplied",
            ));
        }
        let mode = match placement
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("preserve_coordinates")
        {
            "preserve_coordinates" => PlacementMode::PreserveCoordinates,
            "offset" => PlacementMode::Offset,
            "normalize_origin" => PlacementMode::NormalizeOrigin,
            other => {
                return Err(CallToolResult::error(format!(
                    "placement.mode '{other}' is invalid; expected preserve_coordinates, offset or normalize_origin"
                )))
            }
        };
        let number = |name: &str| -> Result<f64, CallToolResult> {
            let value = placement.get(name).and_then(Value::as_f64).unwrap_or(0.0);
            if value.is_finite() {
                Ok(value)
            } else {
                Err(CallToolResult::error(format!(
                    "placement.{name} must be a finite number"
                )))
            }
        };
        Ok(Self {
            mode,
            dx: number("dx")?,
            dy: number("dy")?,
        })
    }

    fn offset_for(self, bounds: Option<Bounds>) -> Result<(f64, f64), CallToolResult> {
        match self.mode {
            PlacementMode::PreserveCoordinates => Ok((0.0, 0.0)),
            PlacementMode::Offset => Ok((self.dx, self.dy)),
            PlacementMode::NormalizeOrigin => {
                let Some(bounds) = bounds else {
                    return Err(CallToolResult::error(
                        "normalize_origin requires at least one selected item with coordinates",
                    ));
                };
                Ok((self.dx - bounds.min_x, self.dy - bounds.min_y))
            }
        }
    }
}

#[derive(Debug, Clone)]
struct MovePlan {
    source_path: PathBuf,
    target_path: PathBuf,
    source_before: String,
    target_before: String,
    project_name: String,
    target_hierarchy_path: String,
    selected_ids: Vec<String>,
    selected_items: Vec<SchematicItem>,
    moved_blocks: HashMap<String, String>,
    required_lib_symbols: Vec<String>,
    selection_bounds: Option<Bounds>,
    offset: (f64, f64),
    crossing_warnings: Vec<String>,
    partial_multi_units: Vec<String>,
    plan_revision: String,
}

impl MovePlan {
    fn items_by_type(&self) -> Value {
        let mut grouped: HashMap<String, Vec<Value>> = HashMap::new();
        for item in &self.selected_items {
            grouped.entry(item.kind.clone()).or_default().push(json!({
                "uuid": item.uuid,
                "reference": item.reference,
                "value": item.value,
                "footprint": item.footprint,
                "lib_id": item.lib_id,
                "unit": item.unit,
                "bounds": item.bounds.map(Bounds::json)
            }));
        }
        json!(grouped)
    }

    fn response(&self, dry_run: bool, applied: bool, transaction_id: Option<&str>) -> Value {
        let labels = self
            .selected_items
            .iter()
            .filter(|item| {
                matches!(
                    item.kind.as_str(),
                    "label" | "global_label" | "hierarchical_label"
                )
            })
            .filter_map(|item| first_string_payload(&item.source))
            .collect::<Vec<_>>();
        let power_symbols = self
            .selected_items
            .iter()
            .filter(|item| item.is_power_symbol())
            .filter_map(|item| item.value.clone())
            .collect::<Vec<_>>();
        json!({
            "dry_run": dry_run,
            "applied": applied,
            "safe_to_apply": self.partial_multi_units.is_empty(),
            "plan_revision": self.plan_revision,
            "transaction_id": transaction_id,
            "source_schematic": self.source_path.display().to_string(),
            "target_schematic": self.target_path.display().to_string(),
            "target_hierarchy_path": self.target_hierarchy_path,
            "selection_bounds": self.selection_bounds.map(Bounds::json),
            "placement_offset": { "dx": self.offset.0, "dy": self.offset.1 },
            "items_to_move": self.items_by_type(),
            "item_count": self.selected_items.len(),
            "multi_unit_components_detected": multi_unit_summary(&self.selected_items),
            "local_nets_affected": affected_nets(&self.selected_items),
            "labels_included": labels,
            "power_symbols_included": power_symbols,
            "no_connects_included": self.selected_items.iter().filter(|item| item.kind == "no_connect").count(),
            "items_crossing_selection_boundary": self.crossing_warnings,
            "warnings": self.crossing_warnings,
            "required_embedded_symbols": self.required_lib_symbols,
        })
    }
}

async fn handle_move_schematic_items_to_sheet(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let source_path = get_path(args, "source_schematic")?;
    let target_path = get_path(args, "target_schematic")?;
    let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(true);

    let plan = match build_move_plan(args, source_path, target_path) {
        Ok(plan) => plan,
        Err(error) => return Ok(error),
    };

    if dry_run {
        return Ok(CallToolResult::json(&plan.response(true, false, None)));
    }

    let supplied_revision = args
        .get("plan_revision")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if supplied_revision != plan.plan_revision {
        return Ok(CallToolResult::error(format!(
            "plan_revision mismatch; rerun dry_run and apply exactly '{}'",
            plan.plan_revision
        )));
    }

    let source_delete = SchematicCommand::delete_items(
        &plan.source_before,
        plan.selected_ids
            .iter()
            .map(|id| ItemId::new(id.clone()))
            .collect::<Result<Vec<_>, _>>()?,
        "Move schematic items out of source sheet",
    )?
    .requiring_unchanged_document();
    let (source_after, _) =
        prepare_command(&plan.source_path, &plan.source_before, &source_delete)?;

    let insert_changes = plan
        .selected_ids
        .iter()
        .map(|id| {
            Ok(ItemChange {
                id: ItemId::new(id.clone())?,
                before: None,
                after: Some(
                    plan.moved_blocks
                        .get(id)
                        .ok_or_else(|| {
                            konnect_sexp::SexpError::MissingNode(format!(
                                "prepared moved block {id}"
                            ))
                        })?
                        .clone(),
                ),
                anchor: ItemAnchor::BeforeFooter,
            })
        })
        .collect::<Result<Vec<_>, konnect_sexp::SexpError>>()?;
    let target_insert = SchematicCommand::from_changes(
        &plan.target_before,
        "Move schematic items into target sheet",
        insert_changes,
    )?
    .requiring_unchanged_document();
    let (target_with_items, _) =
        prepare_command(&plan.target_path, &plan.target_before, &target_insert)?;
    let target_after = add_missing_lib_symbols(
        &target_with_items,
        &plan.source_before,
        &plan.required_lib_symbols,
    )?;

    validate_migration_result(&plan, &source_after, &target_after)?;

    let transaction = commit_file_transaction(
        transaction_root(&plan.source_path, &plan.target_path)?,
        vec![
            FileTransition::replace(&plan.source_path, &plan.source_before, source_after),
            FileTransition::replace(&plan.target_path, &plan.target_before, target_after),
        ],
    )?;

    Ok(CallToolResult::json(&plan.response(
        false,
        true,
        Some(&transaction.id),
    )))
}

fn build_move_plan(
    args: &Value,
    source_path: PathBuf,
    target_path: PathBuf,
) -> Result<MovePlan, CallToolResult> {
    if !source_path.is_file() {
        return Err(CallToolResult::error("source_schematic is not a file"));
    }
    if !target_path.is_file() {
        return Err(CallToolResult::error("target_schematic is not a file"));
    }
    if same_file(&source_path, &target_path) {
        return Err(CallToolResult::error(
            "source_schematic and target_schematic must be different files",
        ));
    }

    let source_before = read_consistent(&source_path).map_err(|error| {
        CallToolResult::error(format!("failed to read source_schematic: {error}"))
    })?;
    let target_before = read_consistent(&target_path).map_err(|error| {
        CallToolResult::error(format!("failed to read target_schematic: {error}"))
    })?;
    parse_sexp(&source_before).map_err(|error| {
        CallToolResult::error(format!("source_schematic does not parse: {error}"))
    })?;
    parse_sexp(&target_before).map_err(|error| {
        CallToolResult::error(format!("target_schematic does not parse: {error}"))
    })?;

    let project_name = opt_str(args, "project_name")
        .map(str::to_string)
        .unwrap_or_else(|| project_name_for(&source_path));
    let target_hierarchy_path = target_instance_path(&source_path, &target_path, &source_before)?;
    let source_items = extract_migratable_items(&source_before)?;
    let target_items = extract_migratable_items(&target_before)?;
    let selector = args
        .get("selection")
        .ok_or_else(|| CallToolResult::error("selection is required"))?;
    let include = IncludeConnected::parse(args)?;
    let placement = Placement::parse(args)?;
    let allow_partial_multi_unit = args
        .get("allow_partial_multi_unit")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut selected = select_initial_items(selector, &source_items)?;
    let base_bounds = selected_bounds(&source_items, &selected);
    loop {
        let Some(bounds) = selected_bounds(&source_items, &selected).map(|b| b.expanded(0.01))
        else {
            break;
        };
        let mut changed = false;
        for item in &source_items {
            if selected.contains(&item.uuid) || !include.allows(item) {
                continue;
            }
            if item
                .bounds
                .is_some_and(|item_bounds| item_bounds.intersects(bounds))
            {
                changed |= selected.insert(item.uuid.clone());
            }
        }
        if !changed {
            break;
        }
    }
    if selected.is_empty() {
        return Err(CallToolResult::error(
            "selection matched no movable schematic items",
        ));
    }

    let partial_multi_units = partial_multi_unit_refs(&source_items, &selected);
    if !allow_partial_multi_unit && !partial_multi_units.is_empty() {
        return Err(CallToolResult::error(format!(
            "selection would move only part of multi-unit component(s): {}; rerun with references selecting the whole component or set allow_partial_multi_unit=true",
            partial_multi_units.join(", ")
        )));
    }

    let mut selected_items = source_items
        .iter()
        .filter(|item| selected.contains(&item.uuid))
        .cloned()
        .collect::<Vec<_>>();
    selected_items.sort_by_key(|item| source_before.find(&item.source).unwrap_or(usize::MAX));
    let selected_ids = selected_items
        .iter()
        .map(|item| item.uuid.clone())
        .collect::<Vec<_>>();
    let selection_bounds =
        selected_bounds(&selected_items, &selected_ids.iter().cloned().collect());
    let offset = placement.offset_for(selection_bounds)?;

    for id in &selected_ids {
        if target_items.iter().any(|item| item.uuid == *id) {
            return Err(CallToolResult::error(format!(
                "target_schematic already contains item UUID {id}"
            )));
        }
    }
    let mut target_preview_items = target_items.clone();
    target_preview_items.extend(selected_items.clone());
    validate_component_references(&target_preview_items).map_err(|error| {
        CallToolResult::error(format!(
            "target_schematic would fail component reference validation after migration: {error}"
        ))
    })?;

    let mut moved_blocks = HashMap::new();
    for item in &selected_items {
        let mut block = item.source.clone();
        if item.kind == "symbol" {
            block = retarget_symbol_instance_path(&block, &project_name, &target_hierarchy_path)
                .map_err(|error| {
                    CallToolResult::error(format!(
                        "failed to patch instance path for {}: {error}",
                        item.uuid
                    ))
                })?;
        }
        if offset != (0.0, 0.0) {
            block = translate_coordinate_clauses(&block, offset.0, offset.1);
        }
        moved_blocks.insert(item.uuid.clone(), block);
    }

    let mut required_lib_symbols = selected_items
        .iter()
        .filter(|item| item.kind == "symbol")
        .filter_map(SchematicItem::lib_symbol_key)
        .map(str::to_string)
        .collect::<Vec<_>>();
    required_lib_symbols.sort();
    required_lib_symbols.dedup();

    let crossing_warnings = crossing_warnings(&selected_items, base_bounds);
    let plan_revision = migration_plan_revision(
        &source_before,
        &target_before,
        &selected_ids,
        &target_hierarchy_path,
        offset,
    );

    Ok(MovePlan {
        source_path,
        target_path,
        source_before,
        target_before,
        project_name,
        target_hierarchy_path,
        selected_ids,
        selected_items,
        moved_blocks,
        required_lib_symbols,
        selection_bounds,
        offset,
        crossing_warnings,
        partial_multi_units,
        plan_revision,
    })
}

fn extract_migratable_items(source: &str) -> Result<Vec<SchematicItem>, CallToolResult> {
    let tree = parse_sexp(source)
        .map_err(|error| CallToolResult::error(format!("schematic does not parse: {error}")))?;
    let lib_syms = tree
        .find("lib_symbols")
        .map(|lib_symbols| lib_symbols.find_all("symbol"))
        .unwrap_or_default();
    let instances = konnect_sexp::schematic::extract_symbol_instances(&tree);
    let mut items = Vec::new();
    for (start, end) in konnect_sexp::writer::find_direct_child_blocks(source, "kicad_sch") {
        let block = &source[start..end];
        let node = parse_sexp(block).map_err(|error| {
            CallToolResult::error(format!("failed to parse schematic item block: {error}"))
        })?;
        let Some(kind) = node.head() else { continue };
        if !MIGRATABLE_TAGS.contains(&kind) {
            continue;
        }
        let Some(uuid) = node.find_str("uuid") else {
            continue;
        };
        let reference = property_value(&node, "Reference").map(str::to_string);
        let value = property_value(&node, "Value").map(str::to_string);
        let footprint = property_value(&node, "Footprint").map(str::to_string);
        let lib_id = node.find_str("lib_id").map(str::to_string);
        let lib_name = node.find_str("lib_name").map(str::to_string);
        let unit = node.find_f64("unit").map(|unit| unit as u32);
        let bounds = if kind == "symbol" {
            instances
                .iter()
                .find(|instance| instance.uuid.as_deref() == Some(uuid))
                .and_then(|instance| {
                    konnect_sexp::schematic::find_lib_symbol(&lib_syms, instance).and_then(
                        |symbol| {
                            konnect_sexp::schematic::symbol_bounds_for_instance(symbol, instance)
                        },
                    )
                })
                .map(|bounds| Bounds {
                    min_x: bounds.min_x,
                    min_y: bounds.min_y,
                    max_x: bounds.max_x,
                    max_y: bounds.max_y,
                })
                .or_else(|| item_bounds(&node))
        } else {
            item_bounds(&node)
        };
        items.push(SchematicItem {
            uuid: uuid.to_string(),
            kind: kind.to_string(),
            source: block.to_string(),
            reference,
            value,
            footprint,
            lib_id,
            lib_name,
            unit,
            bounds,
        });
    }
    Ok(items)
}

fn property_value<'a>(node: &'a konnect_sexp::SexpNode, name: &str) -> Option<&'a str> {
    node.find_all("property")
        .into_iter()
        .find(|property| property.get(1).and_then(|n| n.as_str()) == Some(name))
        .and_then(|property| property.get(2))
        .and_then(|value| value.as_str())
}

fn item_bounds(node: &konnect_sexp::SexpNode) -> Option<Bounds> {
    let mut bounds: Option<Bounds> = None;
    let mut include = |x: f64, y: f64| match &mut bounds {
        Some(bounds) => bounds.include(x, y),
        None => bounds = Some(Bounds::from_point(x, y)),
    };

    if let Some(at) = node.find("at") {
        if let (Some(x), Some(y)) = (at.get_f64(1), at.get_f64(2)) {
            include(x, y);
        }
    }
    for tag in ["start", "end", "center", "mid"] {
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

fn select_initial_items(
    selector: &Value,
    items: &[SchematicItem],
) -> Result<HashSet<String>, CallToolResult> {
    if !selector.is_object() {
        return Err(CallToolResult::error("selection must be an object"));
    }
    let references = string_array(selector, "references")?;
    let uuids = string_array(selector, "uuids")?;
    let bbox = parse_selection_bbox(selector.get("bbox"))?;
    if references.is_empty() && uuids.is_empty() && bbox.is_none() {
        return Err(CallToolResult::error(
            "selection must include references, uuids, or bbox",
        ));
    }

    let known_refs = items
        .iter()
        .filter_map(|item| item.reference.as_deref())
        .collect::<HashSet<_>>();
    for reference in &references {
        if !known_refs.contains(reference.as_str()) {
            return Err(CallToolResult::error(format!(
                "component reference '{reference}' was not found in source_schematic"
            )));
        }
    }
    let known_uuids = items
        .iter()
        .map(|item| item.uuid.as_str())
        .collect::<HashSet<_>>();
    for uuid in &uuids {
        if !known_uuids.contains(uuid.as_str()) {
            return Err(CallToolResult::error(format!(
                "item UUID '{uuid}' was not found in source_schematic"
            )));
        }
    }

    let mut selected = HashSet::new();
    for item in items {
        if item
            .reference
            .as_ref()
            .is_some_and(|reference| references.contains(reference))
            || uuids.contains(&item.uuid)
            || bbox.is_some_and(|bbox| item.bounds.is_some_and(|bounds| bounds.intersects(bbox)))
        {
            selected.insert(item.uuid.clone());
        }
    }
    Ok(selected)
}

fn string_array(value: &Value, field: &str) -> Result<Vec<String>, CallToolResult> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .filter(|s| !s.trim().is_empty())
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

fn parse_selection_bbox(value: Option<&Value>) -> Result<Option<Bounds>, CallToolResult> {
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
            .map(|value| value.as_f64())
            .collect::<Option<Vec<_>>>()
    } else if let Some(object) = value.as_object() {
        let get = |names: &[&str]| {
            names
                .iter()
                .find_map(|name| object.get(*name))
                .and_then(Value::as_f64)
        };
        match (
            get(&["min_x", "x_min", "x1"]),
            get(&["min_y", "y_min", "y1"]),
            get(&["max_x", "x_max", "x2"]),
            get(&["max_y", "y_max", "y2"]),
        ) {
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
    if numbers.iter().any(|number| !number.is_finite()) {
        return Err(CallToolResult::error(
            "selection.bbox coordinates must be finite numbers",
        ));
    }
    let min_x = numbers[0].min(numbers[2]);
    let max_x = numbers[0].max(numbers[2]);
    let min_y = numbers[1].min(numbers[3]);
    let max_y = numbers[1].max(numbers[3]);
    Ok(Some(Bounds {
        min_x,
        min_y,
        max_x,
        max_y,
    }))
}

fn selected_bounds(items: &[SchematicItem], selected: &HashSet<String>) -> Option<Bounds> {
    items
        .iter()
        .filter(|item| selected.contains(&item.uuid))
        .filter_map(|item| item.bounds)
        .reduce(Bounds::union)
}

fn partial_multi_unit_refs(items: &[SchematicItem], selected: &HashSet<String>) -> Vec<String> {
    let mut by_reference: HashMap<&str, Vec<&SchematicItem>> = HashMap::new();
    for item in items
        .iter()
        .filter(|item| item.kind == "symbol")
        .filter(|item| {
            item.reference
                .as_deref()
                .is_some_and(|reference| !reference.is_empty())
        })
    {
        by_reference
            .entry(item.reference.as_deref().unwrap())
            .or_default()
            .push(item);
    }
    let mut partial = by_reference
        .into_iter()
        .filter_map(|(reference, symbols)| {
            let selected_count = symbols
                .iter()
                .filter(|item| selected.contains(&item.uuid))
                .count();
            (symbols.len() > 1 && selected_count > 0 && selected_count < symbols.len())
                .then(|| format!("{reference} ({selected_count}/{})", symbols.len()))
        })
        .collect::<Vec<_>>();
    partial.sort();
    partial
}

fn multi_unit_summary(items: &[SchematicItem]) -> Vec<Value> {
    let mut by_reference: HashMap<&str, Vec<&SchematicItem>> = HashMap::new();
    for item in items.iter().filter(|item| item.kind == "symbol") {
        if let Some(reference) = item
            .reference
            .as_deref()
            .filter(|reference| !reference.is_empty())
        {
            by_reference.entry(reference).or_default().push(item);
        }
    }
    let mut out = by_reference
        .into_iter()
        .filter(|(_, symbols)| symbols.len() > 1)
        .map(|(reference, symbols)| {
            json!({
                "reference": reference,
                "units": symbols.iter().filter_map(|item| item.unit).collect::<Vec<_>>(),
                "count": symbols.len()
            })
        })
        .collect::<Vec<_>>();
    out.sort_by(|a, b| a["reference"].as_str().cmp(&b["reference"].as_str()));
    out
}

fn affected_nets(items: &[SchematicItem]) -> Vec<String> {
    let mut nets = items
        .iter()
        .filter_map(|item| match item.kind.as_str() {
            "label" | "global_label" | "hierarchical_label" => first_string_payload(&item.source),
            "symbol" if item.is_power_symbol() => item.value.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    nets.sort();
    nets.dedup();
    nets
}

fn first_string_payload(block: &str) -> Option<String> {
    let node = parse_sexp(block).ok()?;
    node.get(1)
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn crossing_warnings(items: &[SchematicItem], base_bounds: Option<Bounds>) -> Vec<String> {
    let Some(bounds) = base_bounds else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| matches!(item.kind.as_str(), "wire" | "bus"))
        .filter_map(|item| {
            let node = parse_sexp(&item.source).ok()?;
            let points = point_list(&node);
            if points.len() < 2 {
                return None;
            }
            let inside = points
                .iter()
                .filter(|(x, y)| bounds.contains(*x, *y))
                .count();
            (inside > 0 && inside < points.len()).then(|| {
                format!(
                    "{} {} crosses the selected boundary; verify hierarchy pins/labels for the external net",
                    item.kind, item.uuid
                )
            })
        })
        .collect()
}

fn point_list(node: &konnect_sexp::SexpNode) -> Vec<(f64, f64)> {
    let mut points = Vec::new();
    if let Some(at) = node.find("at") {
        if let (Some(x), Some(y)) = (at.get_f64(1), at.get_f64(2)) {
            points.push((x, y));
        }
    }
    for tag in ["start", "mid", "end", "center"] {
        if let Some(point) = node.find(tag) {
            if let (Some(x), Some(y)) = (point.get_f64(1), point.get_f64(2)) {
                points.push((x, y));
            }
        }
    }
    if let Some(pts) = node.find("pts") {
        for point in pts.find_all("xy") {
            if let (Some(x), Some(y)) = (point.get_f64(1), point.get_f64(2)) {
                points.push((x, y));
            }
        }
    }
    points
}

fn target_instance_path(
    source_path: &Path,
    target_path: &Path,
    source: &str,
) -> Result<String, CallToolResult> {
    let source_root_uuid = parse_sexp(source)
        .ok()
        .and_then(|root| root.find_str("uuid").map(str::to_string))
        .ok_or_else(|| {
            CallToolResult::error("source_schematic must have a root UUID to compute sheet paths")
        })?;
    let source_dir = parent_dir(source_path);
    let target_canonical = target_path.canonicalize().map_err(|error| {
        CallToolResult::error(format!("failed to canonicalize target_schematic: {error}"))
    })?;
    let source_sheet = cse::Schematic::load(source_path).map_err(|error| {
        CallToolResult::error(format!("failed to load source_schematic: {error}"))
    })?;
    let sheet = source_sheet.sheets.iter().find(|sheet| {
        let sheet_path = source_dir.join(sheet.file());
        sheet_path
            .canonicalize()
            .is_ok_and(|path| path == target_canonical)
    });
    let Some(sheet) = sheet else {
        return Err(CallToolResult::error(
            "target_schematic must be linked by a hierarchical sheet in source_schematic",
        ));
    };
    Ok(format!("/{source_root_uuid}/{}", sheet.uuid))
}

fn retarget_symbol_instance_path(
    block: &str,
    project_name: &str,
    target_path: &str,
) -> Result<String, konnect_sexp::SexpError> {
    let node = parse_sexp(block)?;
    if node.head() != Some("symbol") || node.find("lib_id").is_none() {
        return Ok(block.to_string());
    }
    let replaced = replace_project_instance_paths(block, project_name, target_path)?;
    let fake = format!("(kicad_sch\n{replaced}\n)");
    let Some(command) = SchematicCommand::ensure_symbol_instance_path(
        &fake,
        project_name,
        target_path,
        "Patch moved symbol instance path",
    )?
    else {
        return Ok(replaced);
    };
    let (patched, _) = prepare_command(Path::new("moved-symbol.kicad_sch"), &fake, &command)?;
    let items = extract_migratable_items(&patched).map_err(|error| {
        konnect_sexp::SexpError::InvalidValue(format!(
            "patched symbol instance path produced invalid item: {:?}",
            error.content
        ))
    })?;
    items
        .into_iter()
        .find(|item| item.kind == "symbol")
        .map(|item| item.source)
        .ok_or_else(|| {
            konnect_sexp::SexpError::MissingNode("patched moved symbol block".to_string())
        })
}

fn replace_project_instance_paths(
    block: &str,
    project_name: &str,
    target_path: &str,
) -> Result<String, konnect_sexp::SexpError> {
    let mut edits = Vec::new();
    for (instances_start, instances_end) in
        konnect_sexp::writer::find_direct_child_blocks(block, "symbol")
    {
        let instances_block = &block[instances_start..instances_end];
        let instances_node = parse_sexp(instances_block)?;
        if instances_node.head() != Some("instances") {
            continue;
        }
        for (project_start, project_end) in
            konnect_sexp::writer::find_direct_child_blocks(instances_block, "instances")
        {
            let project_block = &instances_block[project_start..project_end];
            let project_node = parse_sexp(project_block)?;
            if project_node.head() != Some("project")
                || project_node.get(1).and_then(|value| value.as_str()) != Some(project_name)
            {
                continue;
            }
            for (path_start, path_end) in
                konnect_sexp::writer::find_direct_child_blocks(project_block, "project")
            {
                let path_block = &project_block[path_start..path_end];
                let path_node = parse_sexp(path_block)?;
                if path_node.head() != Some("path") {
                    continue;
                }
                if let Some((start, end)) = first_quoted_string(path_block) {
                    edits.push(konnect_sexp::writer::SexpEdit::replace(
                        instances_start + project_start + path_start + start,
                        instances_start + project_start + path_start + end,
                        escape_quoted(target_path),
                    ));
                }
            }
        }
    }
    Ok(konnect_sexp::writer::apply_edits(block.to_string(), edits))
}

fn first_quoted_string(source: &str) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    let start_quote = bytes.iter().position(|byte| *byte == b'"')?;
    let mut escaped = false;
    for (index, byte) in bytes[start_quote + 1..].iter().copied().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' => escaped = true,
            b'"' => return Some((start_quote + 1, start_quote + 1 + index)),
            _ => {}
        }
    }
    None
}

fn escape_quoted(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

fn translate_coordinate_clauses(block: &str, dx: f64, dy: f64) -> String {
    let mut edits = Vec::new();
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let bytes = block.as_bytes();
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if in_string {
            match byte {
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        if byte == b'(' {
            for tag in ["at", "start", "mid", "end", "center", "xy"] {
                if let Some(after_tag) = coordinate_tag_end(block, index, tag) {
                    if let Some((x_start, x_end, x)) = parse_number_at(block, after_tag) {
                        if let Some((y_start, y_end, y)) = parse_number_at(block, x_end) {
                            edits.push(konnect_sexp::writer::SexpEdit::replace(
                                x_start,
                                x_end,
                                fmt_f64(x + dx),
                            ));
                            edits.push(konnect_sexp::writer::SexpEdit::replace(
                                y_start,
                                y_end,
                                fmt_f64(y + dy),
                            ));
                        }
                    }
                    break;
                }
            }
        }
        index += 1;
    }
    konnect_sexp::writer::apply_edits(block.to_string(), edits)
}

fn coordinate_tag_end(source: &str, offset: usize, tag: &str) -> Option<usize> {
    let rest = source.get(offset..)?;
    let prefix = format!("({tag}");
    if !rest.starts_with(&prefix) {
        return None;
    }
    let after = offset + prefix.len();
    source
        .as_bytes()
        .get(after)
        .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b')')
        .then_some(after)
}

fn parse_number_at(source: &str, offset: usize) -> Option<(usize, usize, f64)> {
    let bytes = source.as_bytes();
    let mut start = offset;
    while bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
        start += 1;
    }
    let mut end = start;
    while bytes
        .get(end)
        .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(*byte, b')' | b'('))
    {
        end += 1;
    }
    (start < end)
        .then(|| {
            source[start..end]
                .parse::<f64>()
                .ok()
                .map(|number| (start, end, number))
        })
        .flatten()
}

fn add_missing_lib_symbols(
    target: &str,
    source: &str,
    required: &[String],
) -> Result<String, konnect_sexp::SexpError> {
    if required.is_empty() {
        return Ok(target.to_string());
    }
    let source_symbols = embedded_lib_symbols(source)?;
    let target_symbols = embedded_lib_symbols(target)?;
    let target_names = target_symbols.keys().cloned().collect::<HashSet<_>>();
    let missing = required
        .iter()
        .filter(|name| !target_names.contains(name.as_str()))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(target.to_string());
    }
    let mut insertion = String::new();
    for name in missing {
        let Some(block) = source_symbols.get(name.as_str()) else {
            return Err(konnect_sexp::SexpError::MissingNode(format!(
                "embedded lib_symbol {name}"
            )));
        };
        insertion.push('\n');
        insertion.push_str(&indent_block(block, "\t\t"));
    }
    insert_into_lib_symbols(target, &insertion)
}

fn embedded_lib_symbols(source: &str) -> Result<HashMap<String, String>, konnect_sexp::SexpError> {
    let mut out = HashMap::new();
    for (start, end) in konnect_sexp::writer::find_direct_child_blocks(source, "kicad_sch") {
        let block = &source[start..end];
        let node = parse_sexp(block)?;
        if node.head() != Some("lib_symbols") {
            continue;
        }
        for (symbol_start, symbol_end) in
            konnect_sexp::writer::find_direct_child_blocks(block, "lib_symbols")
        {
            let symbol_block = &block[symbol_start..symbol_end];
            let symbol_node = parse_sexp(symbol_block)?;
            if symbol_node.head() == Some("symbol") {
                if let Some(name) = symbol_node.get(1).and_then(|value| value.as_str()) {
                    out.insert(name.to_string(), symbol_block.to_string());
                }
            }
        }
    }
    Ok(out)
}

fn insert_into_lib_symbols(
    target: &str,
    insertion: &str,
) -> Result<String, konnect_sexp::SexpError> {
    for (start, end) in konnect_sexp::writer::find_direct_child_blocks(target, "kicad_sch") {
        let block = &target[start..end];
        let node = parse_sexp(block)?;
        if node.head() != Some("lib_symbols") {
            continue;
        }
        let close = block.rfind(')').ok_or_else(|| {
            konnect_sexp::SexpError::InvalidValue("lib_symbols block is malformed".to_string())
        })?;
        return Ok(konnect_sexp::writer::apply_edits(
            target.to_string(),
            vec![konnect_sexp::writer::SexpEdit::insert(
                start + close,
                insertion,
            )],
        ));
    }
    let root_children = konnect_sexp::writer::find_direct_child_blocks(target, "kicad_sch");
    let anchor = root_children
        .iter()
        .find_map(|(start, _)| {
            let block = &target[*start..];
            block
                .starts_with("(paper")
                .then_some(target[..*start].rfind('\n').map_or(*start, |line| line + 1))
        })
        .unwrap_or_else(|| {
            target
                .rfind(')')
                .map(|index| target[..index].rfind('\n').map_or(index, |line| line + 1))
                .unwrap_or(target.len())
        });
    let block = format!("\t(lib_symbols{insertion}\n\t)\n");
    Ok(konnect_sexp::writer::apply_edits(
        target.to_string(),
        vec![konnect_sexp::writer::SexpEdit::insert(anchor, block)],
    ))
}

fn indent_block(block: &str, indent: &str) -> String {
    block
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn validate_migration_result(
    plan: &MovePlan,
    source_after: &str,
    target_after: &str,
) -> anyhow::Result<()> {
    parse_sexp(source_after)?;
    parse_sexp(target_after)?;
    ensure_unique_uuids(source_after, "source_schematic")?;
    ensure_unique_uuids(target_after, "target_schematic")?;

    let source_items = extract_migratable_items(source_after)
        .map_err(|error| anyhow::anyhow!("source result validation failed: {:?}", error.content))?;
    let target_items = extract_migratable_items(target_after)
        .map_err(|error| anyhow::anyhow!("target result validation failed: {:?}", error.content))?;
    for item in &plan.selected_items {
        if source_items
            .iter()
            .any(|candidate| candidate.uuid == item.uuid)
        {
            anyhow::bail!(
                "selected item {} still exists in source after migration",
                item.uuid
            );
        }
        let Some(after) = target_items
            .iter()
            .find(|candidate| candidate.uuid == item.uuid)
        else {
            anyhow::bail!(
                "selected item {} was not found in target after migration",
                item.uuid
            );
        };
        if after.reference != item.reference
            || after.value != item.value
            || after.footprint != item.footprint
            || after.lib_id != item.lib_id
            || after.lib_name != item.lib_name
            || after.unit != item.unit
        {
            anyhow::bail!(
                "selected item {} changed identity metadata during migration",
                item.uuid
            );
        }
    }
    validate_component_references(&target_items)?;
    for item in plan
        .selected_items
        .iter()
        .filter(|item| item.kind == "symbol")
    {
        let Some(block) = plan.moved_blocks.get(&item.uuid) else {
            continue;
        };
        let node = parse_sexp(block)?;
        if !symbol_has_instance_path_local(&node, &plan.project_name, &plan.target_hierarchy_path) {
            anyhow::bail!(
                "moved symbol {} does not carry target hierarchy path {}",
                item.uuid,
                plan.target_hierarchy_path
            );
        }
    }
    Ok(())
}

fn validate_component_references(items: &[SchematicItem]) -> anyhow::Result<()> {
    let mut by_reference: HashMap<&str, Vec<&SchematicItem>> = HashMap::new();
    for item in items {
        if let Some(reference) = item
            .reference
            .as_deref()
            .filter(|reference| !reference.is_empty())
        {
            by_reference.entry(reference).or_default().push(item);
        }
    }

    for (reference, group) in by_reference {
        if group.len() <= 1 {
            continue;
        }
        if group.iter().any(|item| item.kind != "symbol") {
            anyhow::bail!("target_schematic contains duplicate non-symbol reference {reference}");
        }

        let first = group[0];
        let mut units = HashSet::new();
        for item in group {
            if item.lib_id != first.lib_id
                || item.lib_name != first.lib_name
                || item.value != first.value
                || item.footprint != first.footprint
            {
                anyhow::bail!(
                    "target_schematic contains duplicate reference {reference} with inconsistent component metadata"
                );
            }
            let unit = item.unit.unwrap_or(1);
            if !units.insert(unit) {
                anyhow::bail!(
                    "target_schematic contains duplicate reference {reference} unit {unit}"
                );
            }
        }
    }
    Ok(())
}

fn ensure_unique_uuids(source: &str, label: &str) -> anyhow::Result<()> {
    let mut seen = HashSet::new();
    let mut rest = source;
    const PREFIX: &str = "(uuid \"";
    while let Some(index) = rest.find(PREFIX) {
        let value = &rest[index + PREFIX.len()..];
        let Some(end) = value.find('"') else {
            break;
        };
        let uuid = &value[..end];
        if !seen.insert(uuid.to_string()) {
            anyhow::bail!("{label} contains duplicate UUID {uuid}");
        }
        rest = &value[end..];
    }
    Ok(())
}

fn symbol_has_instance_path_local(
    node: &konnect_sexp::SexpNode,
    project_name: &str,
    path: &str,
) -> bool {
    node.find("instances").is_some_and(|instances| {
        instances.find_all("project").iter().any(|project| {
            project.get(1).and_then(|value| value.as_str()) == Some(project_name)
                && project.find_all("path").iter().any(|instance_path| {
                    instance_path.get(1).and_then(|value| value.as_str()) == Some(path)
                })
        })
    })
}

fn migration_plan_revision(
    source: &str,
    target: &str,
    selected_ids: &[String],
    target_hierarchy_path: &str,
    offset: (f64, f64),
) -> String {
    let mut material = String::new();
    material.push_str(&DocumentRevision::of(source).to_string());
    material.push('\n');
    material.push_str(&DocumentRevision::of(target).to_string());
    material.push('\n');
    material.push_str(target_hierarchy_path);
    material.push('\n');
    material.push_str(&fmt_f64(offset.0));
    material.push(',');
    material.push_str(&fmt_f64(offset.1));
    for id in selected_ids {
        material.push('\n');
        material.push_str(id);
    }
    let revision = DocumentRevision::of(&material);
    format!("move-schematic-items:{revision}")
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn transaction_root(source: &Path, target: &Path) -> anyhow::Result<PathBuf> {
    let source_parent = source
        .parent()
        .ok_or_else(|| anyhow::anyhow!("source_schematic has no parent directory"))?
        .canonicalize()?;
    let target_parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("target_schematic has no parent directory"))?
        .canonicalize()?;
    if target_parent.starts_with(&source_parent) {
        Ok(source_parent)
    } else if source_parent.starts_with(&target_parent) {
        Ok(target_parent)
    } else {
        anyhow::bail!(
            "source_schematic and target_schematic must share a project directory for atomic migration"
        );
    }
}

#[derive(Debug)]
struct DeleteUnlinkedChildPlan {
    root_path: PathBuf,
    target_path: PathBuf,
    plan_revision: String,
}

impl DeleteUnlinkedChildPlan {
    fn response(&self, dry_run: bool, deleted: bool) -> Value {
        json!({
            "dry_run": dry_run,
            "deleted": deleted,
            "safe_to_apply": true,
            "plan_revision": self.plan_revision,
            "root_schematic": self.root_path.display().to_string(),
            "target_schematic": self.target_path.display().to_string(),
            "reason": "target schematic is unlinked from the root hierarchy and contains no movable schematic items or child sheets"
        })
    }
}

async fn handle_delete_unlinked_child_schematic(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let root_path = get_path(args, "root_schematic")?;
    let target_path = get_path(args, "target_schematic")?;
    let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(true);

    let plan = match build_delete_unlinked_child_plan(root_path, target_path) {
        Ok(plan) => plan,
        Err(error) => return Ok(error),
    };

    if dry_run {
        return Ok(CallToolResult::json(&plan.response(true, false)));
    }

    let supplied_revision = args
        .get("plan_revision")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if supplied_revision != plan.plan_revision {
        return Ok(CallToolResult::error(format!(
            "plan_revision mismatch; rerun dry_run and apply exactly '{}'",
            plan.plan_revision
        )));
    }

    fs::remove_file(&plan.target_path)?;
    Ok(CallToolResult::json(&plan.response(false, true)))
}

fn build_delete_unlinked_child_plan(
    root_path: PathBuf,
    target_path: PathBuf,
) -> Result<DeleteUnlinkedChildPlan, CallToolResult> {
    if !root_path.is_file() {
        return Err(CallToolResult::error("root_schematic is not a file"));
    }
    if same_file(&root_path, &target_path) {
        return Err(CallToolResult::error(
            "target_schematic must be different from root_schematic",
        ));
    }
    if target_path
        .extension()
        .is_none_or(|extension| extension != "kicad_sch")
    {
        return Err(CallToolResult::error(
            "target_schematic must be a .kicad_sch file",
        ));
    }

    let metadata = fs::symlink_metadata(&target_path).map_err(|error| {
        CallToolResult::error(format!("failed to inspect target_schematic: {error}"))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(CallToolResult::error(
            "target_schematic must not be a symlink",
        ));
    }
    if !metadata.is_file() {
        return Err(CallToolResult::error(
            "target_schematic must be a regular file",
        ));
    }

    let root_dir = root_path
        .parent()
        .ok_or_else(|| CallToolResult::error("root_schematic has no parent directory"))?
        .canonicalize()
        .map_err(|error| {
            CallToolResult::error(format!(
                "failed to canonicalize root project directory: {error}"
            ))
        })?;
    let target_canonical = target_path.canonicalize().map_err(|error| {
        CallToolResult::error(format!("failed to canonicalize target_schematic: {error}"))
    })?;
    if !target_canonical.starts_with(&root_dir) {
        return Err(CallToolResult::error(
            "target_schematic must be inside root_schematic's project directory",
        ));
    }

    let root_before = read_consistent(&root_path).map_err(|error| {
        CallToolResult::error(format!("failed to read root_schematic: {error}"))
    })?;
    parse_sexp(&root_before).map_err(|error| {
        CallToolResult::error(format!("root_schematic does not parse: {error}"))
    })?;

    let target_before = read_consistent(&target_path).map_err(|error| {
        CallToolResult::error(format!("failed to read target_schematic: {error}"))
    })?;
    parse_sexp(&target_before).map_err(|error| {
        CallToolResult::error(format!("target_schematic does not parse: {error}"))
    })?;

    let linked_paths = linked_child_schematic_paths(&root_path)?;
    if linked_paths.contains(&target_canonical) {
        return Err(CallToolResult::error(
            "target_schematic is still linked from the root hierarchy",
        ));
    }

    let target_items = extract_migratable_items(&target_before)?;
    if !target_items.is_empty() {
        return Err(CallToolResult::error(format!(
            "target_schematic is not empty; found {} movable schematic item(s)",
            target_items.len()
        )));
    }
    let child_sheet_count = direct_child_count(&target_before, "sheet")?;
    if child_sheet_count > 0 {
        return Err(CallToolResult::error(format!(
            "target_schematic is not empty; found {child_sheet_count} child sheet(s)"
        )));
    }

    let plan_revision =
        delete_unlinked_child_plan_revision(&root_before, &target_before, &target_canonical);
    Ok(DeleteUnlinkedChildPlan {
        root_path,
        target_path,
        plan_revision,
    })
}

fn linked_child_schematic_paths(root_path: &Path) -> Result<HashSet<PathBuf>, CallToolResult> {
    let mut visited = HashSet::new();
    let mut linked = HashSet::new();
    collect_linked_child_schematic_paths(root_path, &mut visited, &mut linked)?;
    Ok(linked)
}

fn collect_linked_child_schematic_paths(
    path: &Path,
    visited: &mut HashSet<PathBuf>,
    linked: &mut HashSet<PathBuf>,
) -> Result<(), CallToolResult> {
    let canonical = path.canonicalize().map_err(|error| {
        CallToolResult::error(format!(
            "failed to canonicalize schematic {}: {error}",
            path.display()
        ))
    })?;
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }

    let schematic = cse::Schematic::load(&canonical).map_err(|error| {
        CallToolResult::error(format!(
            "failed to load schematic {}: {error}",
            canonical.display()
        ))
    })?;
    let dir = parent_dir(&canonical);
    for sheet in &schematic.sheets {
        let child_path = dir.join(sheet.file());
        let Ok(child_canonical) = child_path.canonicalize() else {
            continue;
        };
        linked.insert(child_canonical.clone());
        if child_canonical.is_file() {
            collect_linked_child_schematic_paths(&child_canonical, visited, linked)?;
        }
    }
    Ok(())
}

fn direct_child_count(source: &str, tag: &str) -> Result<usize, CallToolResult> {
    let mut count = 0usize;
    for (start, end) in konnect_sexp::writer::find_direct_child_blocks(source, "kicad_sch") {
        let block = &source[start..end];
        let node = parse_sexp(block).map_err(|error| {
            CallToolResult::error(format!("failed to parse schematic child block: {error}"))
        })?;
        if node.head() == Some(tag) {
            count += 1;
        }
    }
    Ok(count)
}

fn delete_unlinked_child_plan_revision(root: &str, target: &str, target_path: &Path) -> String {
    let mut material = String::new();
    material.push_str(&DocumentRevision::of(root).to_string());
    material.push('\n');
    material.push_str(&DocumentRevision::of(target).to_string());
    material.push('\n');
    material.push_str(&target_path.display().to_string());
    let revision = DocumentRevision::of(&material);
    format!("delete-unlinked-child-schematic:{revision}")
}

// ─── Handlers ───────────────────────────────────────────────────────────────

async fn handle_add_hierarchical_sheet(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let parent_path = get_path(args, "schematic")?;
    let sheet_file = match require_str(args, "sheet_file") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let sheet_name = opt_str(args, "sheet_name").unwrap_or("Sheet").to_string();
    let x = opt_f64(args, "x").unwrap_or(50.0);
    let y = opt_f64(args, "y").unwrap_or(50.0);
    let width = opt_f64(args, "width").unwrap_or(80.0);
    let height = opt_f64(args, "height").unwrap_or(50.0);
    let project_name = opt_str(args, "project_name")
        .map(str::to_string)
        .unwrap_or_else(|| project_name_for(&parent_path));

    let dir = parent_dir(&parent_path);
    let child_path = dir.join(&sheet_file);

    let relative = Path::new(&sheet_file);
    let valid_relative = !relative.is_absolute()
        && relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && relative
            .extension()
            .is_some_and(|extension| extension == "kicad_sch");
    if !valid_relative {
        return Ok(CallToolResult::error(
            "sheet_file must be a relative .kicad_sch path without parent traversal",
        ));
    }
    if !child_path.parent().is_some_and(Path::is_dir) {
        return Ok(CallToolResult::error(
            "The child sheet directory does not exist",
        ));
    }
    if child_path == parent_path {
        return Ok(CallToolResult::error(
            "A hierarchical sheet cannot reference its parent file",
        ));
    }

    let parent_before = read_consistent(&parent_path)?;
    let parent = cse::Schematic::load(&parent_path)?;

    if parent.sheets.by_name(&sheet_name).is_some() {
        return Ok(CallToolResult::error(format!(
            "Sheet named '{}' already exists in this schematic — use edit_sheet to modify it \
             or pick a different name",
            sheet_name
        )));
    }

    let child_existed = child_path.is_file();
    if child_path.exists() && !child_existed {
        return Ok(CallToolResult::error(
            "The child schematic path exists but is not a regular file",
        ));
    }
    let page = next_free_page(&parent, &project_name).to_string();
    let (parent_base, root_uuid) = ensure_source_root_uuid(&parent_before)?;
    let root_path = format!("/{root_uuid}");
    let block = format_hierarchical_sheet(HierarchicalSheetSpec {
        name: &sheet_name,
        file: &sheet_file,
        x,
        y,
        width,
        height,
        project_name: &project_name,
        parent_instance_path: &root_path,
        page: &page,
    });
    let parent_command = SchematicCommand::insert_item(
        &parent_base,
        block,
        ItemAnchor::BeforeFooter,
        "Add hierarchical sheet",
    )?
    .requiring_unchanged_document();
    let sheet_uuid = parent_command
        .changes
        .first()
        .map(|change| change.id.to_string())
        .ok_or_else(|| anyhow::anyhow!("sheet insertion produced no item change"))?;
    let (parent_after, _) = prepare_command(&parent_path, &parent_base, &parent_command)?;

    let child_before = child_path
        .is_file()
        .then(|| read_consistent(&child_path))
        .transpose()?;
    let mut transitions = vec![FileTransition::replace(
        &parent_path,
        parent_before,
        parent_after,
    )];
    let mut patched = 0usize;
    if let Some(child_before) = child_before {
        let hierarchy_path = format!("{root_path}/{sheet_uuid}");
        if let Some(child_command) = SchematicCommand::ensure_symbol_instance_path(
            &child_before,
            &project_name,
            &hierarchy_path,
            "Link hierarchical child symbols",
        )? {
            patched = child_command.changes.len();
            let (child_after, _) = prepare_command(&child_path, &child_before, &child_command)?;
            transitions.push(FileTransition::replace(
                &child_path,
                child_before,
                child_after,
            ));
        }
    } else {
        transitions.push(FileTransition::create(
            &child_path,
            konnect_sexp::schematic::format_blank_schematic(),
        ));
    }
    commit_file_transaction(&dir, transitions)?;

    let committed = cse::Schematic::load(&parent_path)?;
    let sheet_ref = committed
        .sheets
        .by_name(&sheet_name)
        .ok_or_else(|| anyhow::anyhow!("committed sheet was not readable"))?;
    Ok(CallToolResult::json(&json!({
        "added": sheet_name,
        "sheet": sheet_json(sheet_ref, &project_name),
        "child_file": child_path.display().to_string(),
        "reused_existing_file": child_existed,
        "patched_symbol_instances": patched
    })))
}

async fn handle_edit_sheet(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let project_name = opt_str(args, "project_name")
        .map(str::to_string)
        .unwrap_or_else(|| project_name_for(&sch_path));

    let before = read_consistent(&sch_path)?;
    let mut sch = cse::Schematic::load(&sch_path)?;
    let sheet = match sch.sheets.by_name_mut(&sheet_name) {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                sheet_name
            )))
        }
    };
    let sheet_uuid = sheet.uuid.clone();

    // `requested` is what the caller asked to set, `changed` is what actually
    // differs. They diverge when a caller re-asserts the state that is already
    // there, which is a request the sheet can honor.
    let mut requested = Vec::new();
    let mut changed = Vec::new();
    if let Some(new_name) = opt_str(args, "new_name") {
        requested.push("name");
        if sheet.name() != new_name {
            sheet.set_name(new_name);
            changed.push("name");
        }
    }
    if let Some(new_file) = opt_str(args, "new_file") {
        requested.push("file");
        if sheet.file() != new_file {
            sheet.set_file(new_file);
            changed.push("file");
        }
    }
    if let (Some(x), Some(y)) = (opt_f64(args, "x"), opt_f64(args, "y")) {
        requested.push("position");
        if sheet.at.x != x || sheet.at.y != y {
            sheet.move_to(x, y);
            changed.push("position");
        }
    }
    if let (Some(w), Some(h)) = (opt_f64(args, "width"), opt_f64(args, "height")) {
        requested.push("size");
        if sheet.width != w || sheet.height != h {
            sheet.set_size(w, h);
            changed.push("size");
        }
    }

    if requested.is_empty() {
        return Ok(CallToolResult::error(
            "No fields to change — provide at least one of: new_name, new_file, x+y, width+height",
        ));
    }

    let summary = sheet_json(sheet, &project_name);
    // Skip the commit outright when nothing differs. Writing would reserialise
    // the whole sheet (#210) and produce a diff for a request that asked for
    // the state already on disk.
    if !changed.is_empty() {
        let _ = commit_edited_sheet_item(&sch_path, &before, &sch, &sheet_uuid, "Edit sheet")?;
    }
    Ok(CallToolResult::json(&json!({
        "edited": sheet_name,
        "changed": !changed.is_empty(),
        "changed_fields": changed,
        "requested_fields": requested,
        "sheet": summary
    })))
}

async fn handle_move_sheet(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
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

    let before = read_consistent(&sch_path)?;
    let mut sch = cse::Schematic::load(&sch_path)?;
    match sch.sheets.by_name_mut(&sheet_name) {
        Some(sheet) => {
            let sheet_uuid = sheet.uuid.clone();
            let changed = sheet.at.x != x || sheet.at.y != y;
            if changed {
                sheet.move_to(x, y);
                let _ =
                    commit_edited_sheet_item(&sch_path, &before, &sch, &sheet_uuid, "Move sheet")?;
            }
            Ok(CallToolResult::json(
                &json!({ "moved": sheet_name, "x": x, "y": y, "changed": changed }),
            ))
        }
        None => Ok(CallToolResult::error(format!(
            "Sheet '{}' not found",
            sheet_name
        ))),
    }
}

async fn handle_delete_sheet(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let before = read_consistent(&sch_path)?;
    let sch = cse::Schematic::load(&sch_path)?;
    match sch.sheets.by_name(&sheet_name) {
        Some(removed) => {
            let child_file = removed.file().to_owned();
            let command = SchematicCommand::delete_item(
                &before,
                ItemId::new(removed.uuid.clone())?,
                "Delete sheet",
            )?;
            commit_command(&sch_path, &command)?;
            Ok(CallToolResult::json(&json!({
                "deleted": sheet_name,
                "child_file_preserved": child_file,
                "note": "The child schematic file was not deleted. Remaining sheets' page \
                         numbers may now have a gap — call renumber_sheet_pages if needed."
            })))
        }
        None => Ok(CallToolResult::error(format!(
            "Sheet '{}' not found",
            sheet_name
        ))),
    }
}

async fn handle_duplicate_sheet(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let source_name = match require_str(args, "source_sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let new_name = match require_str(args, "new_sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let new_file = match require_str(args, "new_file") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let project_name = opt_str(args, "project_name")
        .map(str::to_string)
        .unwrap_or_else(|| project_name_for(&sch_path));

    let parent_before = read_consistent(&sch_path)?;
    let parent = cse::Schematic::load(&sch_path)?;

    if parent.sheets.by_name(&new_name).is_some() {
        return Ok(CallToolResult::error(format!(
            "Sheet named '{}' already exists",
            new_name
        )));
    }

    let (src_x, src_y, src_w, src_h, src_file) = match parent.sheets.by_name(&source_name) {
        Some(s) => {
            let (x, y) = s.position();
            (x, y, s.width, s.height, s.file().to_string())
        }
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                source_name
            )))
        }
    };

    let dir = parent_dir(&sch_path);
    let source_child = dir.join(&src_file);
    let new_child = dir.join(&new_file);

    let relative = Path::new(&new_file);
    let valid_relative = !relative.is_absolute()
        && relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && relative
            .extension()
            .is_some_and(|extension| extension == "kicad_sch");
    if !valid_relative || !new_child.parent().is_some_and(Path::is_dir) {
        return Ok(CallToolResult::error(
            "new_file must be a relative .kicad_sch path in an existing project directory",
        ));
    }

    if new_child.exists() {
        return Ok(CallToolResult::error(format!(
            "'{}' already exists — pick a different file name, or use add_hierarchical_sheet \
             to link the existing file instead of duplicating",
            new_file
        )));
    }
    if !source_child.exists() {
        return Ok(CallToolResult::error(format!(
            "Source sheet's file '{}' was not found on disk — cannot duplicate",
            src_file
        )));
    }

    const DUPLICATE_OFFSET_MM: f64 = 20.0;
    let page = next_free_page(&parent, &project_name).to_string();
    let (parent_base, root_uuid) = ensure_source_root_uuid(&parent_before)?;
    let root_path = format!("/{root_uuid}");
    let block = format_hierarchical_sheet(HierarchicalSheetSpec {
        name: &new_name,
        file: &new_file,
        x: src_x + DUPLICATE_OFFSET_MM,
        y: src_y + DUPLICATE_OFFSET_MM,
        width: src_w,
        height: src_h,
        project_name: &project_name,
        parent_instance_path: &root_path,
        page: &page,
    });
    let parent_command = SchematicCommand::insert_item(
        &parent_base,
        block,
        ItemAnchor::BeforeFooter,
        "Duplicate hierarchical sheet",
    )?
    .requiring_unchanged_document();
    let sheet_uuid = parent_command
        .changes
        .first()
        .map(|change| change.id.to_string())
        .ok_or_else(|| anyhow::anyhow!("sheet duplication produced no item change"))?;
    let (parent_after, _) = prepare_command(&sch_path, &parent_base, &parent_command)?;

    let source_child_content = read_consistent(&source_child)?;
    // Fresh identities for the copy's own items before the root is renamed;
    // otherwise the duplicate shares every nested UUID with its source.
    let refreshed_child = regenerate_item_uuids(&source_child_content);
    let duplicated_uuid = uuid::Uuid::new_v4().to_string();
    let duplicated_base = replace_source_root_uuid(&refreshed_child, &duplicated_uuid)?;
    let hierarchy_path = format!("{root_path}/{sheet_uuid}");
    let (duplicated_after, patched) = if let Some(command) =
        SchematicCommand::ensure_symbol_instance_path(
            &duplicated_base,
            &project_name,
            &hierarchy_path,
            "Link duplicated child symbols",
        )? {
        let count = command.changes.len();
        let (after, _) = prepare_command(&new_child, &duplicated_base, &command)?;
        (after, count)
    } else {
        (duplicated_base, 0)
    };
    commit_file_transaction(
        &dir,
        vec![
            FileTransition::replace(&sch_path, parent_before, parent_after),
            FileTransition::create(&new_child, duplicated_after),
        ],
    )?;

    let committed = cse::Schematic::load(&sch_path)?;
    let sheet_ref = committed
        .sheets
        .by_name(&new_name)
        .ok_or_else(|| anyhow::anyhow!("duplicated sheet was not readable"))?;
    Ok(CallToolResult::json(&json!({
        "duplicated_from": source_name,
        "sheet": sheet_json(sheet_ref, &project_name),
        "child_file": new_child.display().to_string(),
        "patched_symbol_instances": patched
    })))
}

async fn handle_get_sheet_hierarchy(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let root_path = get_path(args, "schematic")?;
    let project_name = opt_str(args, "project_name")
        .map(str::to_string)
        .unwrap_or_else(|| project_name_for(&root_path));

    if !root_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Schematic '{}' not found",
            root_path.display()
        )));
    }

    let mut visited = HashSet::new();
    let tree = build_hierarchy_node(&root_path, &project_name, 0, &mut visited)?;
    Ok(CallToolResult::json(&tree))
}

pub(crate) fn build_hierarchy_node(
    path: &Path,
    project_name: &str,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
) -> anyhow::Result<Value> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    if depth > MAX_HIERARCHY_DEPTH {
        return Ok(json!({
            "file": path.display().to_string(),
            "error": "max hierarchy depth exceeded — possible reference cycle",
            "children": []
        }));
    }
    if !visited.insert(canon.clone()) {
        return Ok(json!({
            "file": path.display().to_string(),
            "error": "cycle detected — this file is already an ancestor in this tree",
            "children": []
        }));
    }

    let sch = match cse::Schematic::load(path) {
        Ok(s) => s,
        Err(e) => {
            visited.remove(&canon);
            return Ok(json!({
                "file": path.display().to_string(),
                "error": format!("failed to load: {}", e),
                "children": []
            }));
        }
    };

    let dir = parent_dir(path);
    let mut children = Vec::new();
    for sheet in sch.sheets.iter() {
        let child_path = dir.join(sheet.file());
        let mut node = sheet_json(sheet, project_name);
        if child_path.exists() {
            let sub = build_hierarchy_node(&child_path, project_name, depth + 1, visited)?;
            node["children"] = sub["children"].clone();
            if let Some(err) = sub.get("error") {
                node["error"] = err.clone();
            }
        } else {
            node["children"] = json!([]);
            node["error"] = json!("child file not found on disk");
        }
        children.push(node);
    }
    visited.remove(&canon);

    Ok(json!({
        "file": path.display().to_string(),
        "children": children
    }))
}

async fn handle_renumber_sheet_pages(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let root_path = get_path(args, "schematic")?;
    let project_name = opt_str(args, "project_name")
        .map(str::to_string)
        .unwrap_or_else(|| project_name_for(&root_path));

    if !root_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Schematic '{}' not found",
            root_path.display()
        )));
    }

    // Page paths are hierarchical instance paths rooted at the root sheet's
    // UUID ("/<root-uuid>", then "/<root-uuid>/<sheet-uuid>" one level down),
    // matching what eeschema writes.
    let root_before = read_consistent(&root_path)?;
    let (root_base, root_uuid) = ensure_source_root_uuid(&root_before)?;
    let root_prefix = format!("/{root_uuid}");

    let mut next_page = 2u32; // page 1 is always the root, left untouched
    let mut renumbered = Vec::new();
    let mut visited = HashSet::new();
    let mut transitions = Vec::new();
    collect_renumber_transitions(
        &root_path,
        &root_prefix,
        &project_name,
        &mut next_page,
        &mut renumbered,
        &mut visited,
        Some((&root_before, &root_base, &root_uuid)),
        &mut transitions,
    )?;
    if !transitions.is_empty() {
        commit_file_transaction(parent_dir(&root_path), transitions)?;
    }

    Ok(CallToolResult::json(&json!({
        "renumbered_count": renumbered.len(),
        "pages": renumbered
    })))
}

#[allow(clippy::too_many_arguments)]
fn collect_renumber_transitions(
    path: &Path,
    hier_prefix: &str,
    project_name: &str,
    next_page: &mut u32,
    renumbered: &mut Vec<Value>,
    visited: &mut HashSet<PathBuf>,
    source_override: Option<(&str, &str, &str)>,
    transitions: &mut Vec<FileTransition>,
) -> anyhow::Result<()> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canon.clone()) {
        return Ok(()); // cycle guard — already on this DFS path, skip
    }

    let loaded_before = source_override
        .map(|(before, _, _)| before.to_owned())
        .unwrap_or(read_consistent(path)?);
    let command_source = source_override
        .map(|(_, base, _)| base)
        .unwrap_or(loaded_before.as_str());
    let mut sch = cse::Schematic::load(path)?;
    if let Some((_, _, root_uuid)) = source_override {
        sch.uuid = Some(root_uuid.to_owned());
    }
    let dir = parent_dir(path);
    let mut changed_ids = Vec::new();

    // Snapshot the sheet order first: recursing below needs `sch` unborrowed.
    let sheet_order: Vec<(String, String, String)> = sch
        .sheets
        .iter()
        .map(|s| (s.name().to_string(), s.file().to_string(), s.uuid.clone()))
        .collect();

    for (name, file, sheet_uuid) in &sheet_order {
        let page = next_page.to_string();
        *next_page += 1;
        if let Some(sheet) = sch.sheets.by_name_mut(name) {
            if sheet.page(project_name) != Some(page.as_str()) {
                sheet.set_page(project_name, hier_prefix, &page);
                changed_ids.push(ItemId::new(sheet.uuid.clone())?);
            }
        }
        renumbered.push(json!({ "sheet_name": name, "file": file, "page": page }));

        let child_path = dir.join(file);
        if child_path.exists() {
            let child_prefix = format!("{}/{}", hier_prefix, sheet_uuid);
            collect_renumber_transitions(
                &child_path,
                &child_prefix,
                project_name,
                next_page,
                renumbered,
                visited,
                None,
                transitions,
            )?;
        }
    }

    let replacement = if changed_ids.is_empty() {
        command_source.to_owned()
    } else {
        let command = SchematicCommand::replace_items_from_document(
            command_source,
            &sch.to_source(),
            changed_ids,
            "Renumber hierarchical sheets",
        )?;
        prepare_command(path, command_source, &command)?.0
    };
    if replacement != loaded_before {
        transitions.push(FileTransition::replace(path, loaded_before, replacement));
    }
    visited.remove(&canon);
    Ok(())
}

async fn handle_import_sheet_pins(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let side = opt_str(args, "side").unwrap_or("right").to_string();
    if side != "right" && side != "left" {
        return Ok(CallToolResult::error(format!(
            "Invalid side '{}' — must be 'right' or 'left'",
            side
        )));
    }

    let before = read_consistent(&sch_path)?;
    let mut parent = cse::Schematic::load(&sch_path)?;
    let dir = parent_dir(&sch_path);

    let (child_path, sheet_x, sheet_y, sheet_w, existing_pin_count) =
        match parent.sheets.by_name(&sheet_name) {
            Some(s) => {
                let (x, y) = s.position();
                (dir.join(s.file()), x, y, s.width, s.pins.len())
            }
            None => {
                return Ok(CallToolResult::error(format!(
                    "Sheet '{}' not found",
                    sheet_name
                )))
            }
        };

    if !child_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Child file '{}' not found on disk — cannot read its hierarchical labels",
            child_path.display()
        )));
    }
    let child = cse::Schematic::load(&child_path)?;
    let label_names: Vec<(String, String)> = child
        .hierarchical_labels
        .iter()
        .map(|l| {
            (
                l.text.clone(),
                l.shape.clone().unwrap_or_else(|| "passive".to_string()),
            )
        })
        .collect();

    let sheet = parent
        .sheets
        .by_name_mut(&sheet_name)
        .expect("looked up above");
    let sheet_uuid = sheet.uuid.clone();

    let edge_x = if side == "right" {
        sheet_x + sheet_w
    } else {
        sheet_x
    };
    let rotation = if side == "right" { 0.0 } else { 180.0 };

    let mut imported = Vec::new();
    let mut skipped_existing = Vec::new();
    let mut slot = existing_pin_count;
    for (name, shape) in label_names {
        if sheet.pin_by_name(&name).is_some() {
            skipped_existing.push(name);
            continue;
        }
        let pin_type = if ALLOWED_PIN_TYPES.contains(&shape.as_str()) {
            shape
        } else {
            "passive".to_string()
        };
        slot += 1;
        let y = sheet_y + SHEET_PIN_SPACING_MM * slot as f64;
        let mut pin = cse::SheetPin::new(name.as_str(), pin_type.as_str(), edge_x, y);
        pin.at.rotation = Some(rotation);
        imported.push(pin.name.clone());
        sheet.add_pin(pin);
    }

    if !imported.is_empty() {
        let _ = commit_edited_sheet_item(
            &sch_path,
            &before,
            &parent,
            &sheet_uuid,
            "Import sheet pins",
        )?;
    }

    Ok(CallToolResult::json(&json!({
        "sheet": sheet_name,
        "imported_pins": imported,
        "skipped_existing": skipped_existing
    })))
}

async fn handle_add_sheet_pin(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin_name = match require_str(args, "pin_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin_type = match require_str(args, "pin_type") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    if let Err(e) = validate_pin_type(&pin_type) {
        return Ok(e);
    }
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let before = read_consistent(&sch_path)?;
    let mut sch = cse::Schematic::load(&sch_path)?;
    let sheet = match sch.sheets.by_name_mut(&sheet_name) {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                sheet_name
            )))
        }
    };
    let sheet_uuid = sheet.uuid.clone();

    if sheet.pin_by_name(&pin_name).is_some() {
        return Ok(CallToolResult::error(format!(
            "Sheet '{}' already has a pin named '{}'",
            sheet_name, pin_name
        )));
    }

    sheet.add_pin(cse::SheetPin::new(
        pin_name.as_str(),
        pin_type.as_str(),
        x,
        y,
    ));
    let _ = commit_edited_sheet_item(&sch_path, &before, &sch, &sheet_uuid, "Add sheet pin")?;

    Ok(CallToolResult::json(&json!({
        "added_pin": pin_name,
        "sheet": sheet_name,
        "pin_type": pin_type,
        "x": x,
        "y": y
    })))
}

async fn handle_edit_sheet_pin(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin_name = match require_str(args, "pin_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    if let Some(pt) = opt_str(args, "pin_type") {
        if let Err(e) = validate_pin_type(pt) {
            return Ok(e);
        }
    }

    let before = read_consistent(&sch_path)?;
    let mut sch = cse::Schematic::load(&sch_path)?;
    let sheet = match sch.sheets.by_name_mut(&sheet_name) {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                sheet_name
            )))
        }
    };
    let sheet_uuid = sheet.uuid.clone();
    let pin = match sheet.pin_by_name_mut(&pin_name) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(format!(
                "Pin '{}' not found on sheet '{}'",
                pin_name, sheet_name
            )))
        }
    };

    let mut changed = Vec::new();
    if let Some(new_name) = opt_str(args, "new_name") {
        pin.name = new_name.to_string();
        changed.push("name");
    }
    if let Some(pt) = opt_str(args, "pin_type") {
        pin.pin_type = pt.to_string();
        changed.push("pin_type");
    }
    if let (Some(x), Some(y)) = (opt_f64(args, "x"), opt_f64(args, "y")) {
        pin.at.x = x;
        pin.at.y = y;
        changed.push("position");
    }

    if changed.is_empty() {
        return Ok(CallToolResult::error(
            "No fields to change — provide at least one of: new_name, pin_type, x+y",
        ));
    }

    let summary = json!({
        "name": pin.name, "pin_type": pin.pin_type, "x": pin.at.x, "y": pin.at.y
    });
    let _ = commit_edited_sheet_item(&sch_path, &before, &sch, &sheet_uuid, "Edit sheet pin")?;

    Ok(CallToolResult::json(&json!({
        "edited_pin": pin_name,
        "sheet": sheet_name,
        "changed_fields": changed,
        "pin": summary
    })))
}

async fn handle_delete_sheet_pin(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin_name = match require_str(args, "pin_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let before = read_consistent(&sch_path)?;
    let mut sch = cse::Schematic::load(&sch_path)?;
    let sheet = match sch.sheets.by_name_mut(&sheet_name) {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                sheet_name
            )))
        }
    };
    let sheet_uuid = sheet.uuid.clone();

    if !sheet.remove_pin(&pin_name) {
        return Ok(CallToolResult::error(format!(
            "Pin '{}' not found on sheet '{}'",
            pin_name, sheet_name
        )));
    }
    let _ = commit_edited_sheet_item(&sch_path, &before, &sch, &sheet_uuid, "Delete sheet pin")?;

    Ok(CallToolResult::json(&json!({
        "deleted_pin": pin_name,
        "sheet": sheet_name
    })))
}

async fn handle_validate_sheet_pins(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let root_path = get_path(args, "schematic")?;

    if !root_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Schematic '{}' not found",
            root_path.display()
        )));
    }

    let mut issues = Vec::new();
    let mut visited = HashSet::new();
    collect_pin_mismatches(&root_path, 0, &mut visited, &mut issues)?;

    Ok(CallToolResult::json(&json!({
        "issue_count": issues.len(),
        "issues": issues
    })))
}

fn collect_pin_mismatches(
    path: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    issues: &mut Vec<Value>,
) -> anyhow::Result<()> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if depth > MAX_HIERARCHY_DEPTH || !visited.insert(canon.clone()) {
        return Ok(());
    }

    let sch = match cse::Schematic::load(path) {
        Ok(s) => s,
        Err(_) => {
            visited.remove(&canon);
            return Ok(());
        }
    };
    let dir = parent_dir(path);

    for sheet in sch.sheets.iter() {
        let child_path = dir.join(sheet.file());
        if !child_path.exists() {
            issues.push(json!({
                "sheet": sheet.name(),
                "file": sheet.file(),
                "error": "child file not found on disk"
            }));
            continue;
        }
        let child = cse::Schematic::load(&child_path)?;
        let label_names: HashSet<String> = child
            .hierarchical_labels
            .iter()
            .map(|l| l.text.clone())
            .collect();
        let pin_names: HashSet<String> = sheet.pins.iter().map(|p| p.name.clone()).collect();

        let labels_without_pins: Vec<&String> = label_names.difference(&pin_names).collect();
        let pins_without_labels: Vec<&String> = pin_names.difference(&label_names).collect();

        if !labels_without_pins.is_empty() || !pins_without_labels.is_empty() {
            issues.push(json!({
                "sheet": sheet.name(),
                "file": sheet.file(),
                "labels_without_pins": labels_without_pins,
                "pins_without_labels": pins_without_labels
            }));
        }

        collect_pin_mismatches(&child_path, depth + 1, visited, issues)?;
    }
    visited.remove(&canon);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn test_ctx() -> ToolContext {
        let config = ServerConfig {
            kicad_cli: "kicad-cli".into(),
            kicad_binary: "kicad".into(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
            dispatcher_tools: false,
        };
        ToolContext::new(config, Arc::new(crate::router::ToolRouter::new()))
    }

    fn blank_schematic(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        create_blank_schematic(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn add_hierarchical_sheet_creates_child_file_and_links_it() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();

        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_file": "power.kicad_sch",
            "sheet_name": "Power Supply",
            "x": 20.0, "y": 20.0
        });
        let result = handle_add_hierarchical_sheet(&args, &ctx).await.unwrap();
        assert!(!result.is_error);

        assert!(tmp.path().join("power.kicad_sch").exists());
        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.len(), 1);
        assert_eq!(
            parent.sheets.by_name("Power Supply").unwrap().file(),
            "power.kicad_sch"
        );
        // Pages are stored under the default project name (the file stem) at
        // the parent's "/<root-uuid>" instance path.
        assert_eq!(
            parent.sheets.by_name("Power Supply").unwrap().page("root"),
            Some("2")
        );
    }

    fn result_json(result: &CallToolResult) -> Value {
        serde_json::from_str(&result_text(result)).unwrap()
    }

    fn result_text(result: &CallToolResult) -> String {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        }
    }

    fn write_migration_fixture(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let root = tmp.path().join("root.kicad_sch");
        let child = tmp.path().join("child.kicad_sch");
        std::fs::write(
            &root,
            r##"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "root-uuid")
  (paper "A4")
  (lib_symbols
    (symbol "Device:C"
      (pin passive line (at 0 2.54 270) (length 1.27) (name "" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
      (pin passive line (at 0 -2.54 90) (length 1.27) (name "" (effects (font (size 1.27 1.27)))) (number "2" (effects (font (size 1.27 1.27)))))
    )
    (symbol "Device:R"
      (pin passive line (at 0 2.54 270) (length 1.27) (name "" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
      (pin passive line (at 0 -2.54 90) (length 1.27) (name "" (effects (font (size 1.27 1.27)))) (number "2" (effects (font (size 1.27 1.27)))))
    )
    (symbol "power:GND"
      (power)
      (pin power_in line (at 0 0 0) (length 0) (name "GND" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
    )
  )
  (symbol
    (lib_id "Device:C")
    (at 20 20 0)
    (unit 1)
    (dnp yes)
    (in_bom no)
    (on_board yes)
    (property "Reference" "C1" (at 20 16 0))
    (property "Value" "100n" (at 20 24 0))
    (property "Footprint" "Capacitor_SMD:C_0402" (at 20 28 0))
    (uuid "sym-c1")
    (instances
      (project "root"
        (path "/root-uuid" (reference "C1") (unit 1))
      )
    )
  )
  (symbol
    (lib_id "Device:R")
    (at 40 20 0)
    (unit 1)
    (property "Reference" "R1" (at 40 16 0))
    (property "Value" "10k" (at 40 24 0))
    (uuid "sym-r1")
    (instances
      (project "root"
        (path "/root-uuid" (reference "R1") (unit 1))
      )
    )
  )
  (symbol
    (lib_id "power:GND")
    (at 20 25.08 0)
    (unit 1)
    (property "Reference" "#PWR01" (at 20 25.08 0))
    (property "Value" "GND" (at 20 27.62 0))
    (uuid "sym-gnd")
  )
  (wire
    (pts
      (xy 20 22.54)
      (xy 20 25.08)
    )
    (stroke (width 0) (type default))
    (uuid "wire-c1-gnd")
  )
  (junction (at 20 25.08) (diameter 0) (uuid "junction-c1"))
  (label "LOCAL_SIG" (at 20 17.46 0) (effects (font (size 1.27 1.27))) (uuid "label-c1"))
  (no_connect (at 20 17.46) (uuid "nc-c1"))
  (text "move me" (at 18 12 0) (effects (font (size 1.27 1.27))) (uuid "text-c1"))
  (sheet
    (at 100 40)
    (size 60 40)
    (uuid "sheet-child")
    (property "Sheetname" "Child" (at 100 39.365 0))
    (property "Sheetfile" "child.kicad_sch" (at 100 80.635 0))
    (instances
      (project "root"
        (path "/root-uuid" (page "2"))
      )
    )
  )
  (sheet_instances
    (path "/" (page "1"))
  )
)"##,
        )
        .unwrap();
        std::fs::write(
            &child,
            r#"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "child-uuid")
  (paper "A4")
  (lib_symbols
  )
  (sheet_instances
    (path "/" (page "2"))
  )
)"#,
        )
        .unwrap();
        (root, child)
    }

    async fn migration_dry_run(root: &Path, child: &Path) -> Value {
        let result = handle_move_schematic_items_to_sheet(
            &json!({
                "source_schematic": root.display().to_string(),
                "target_schematic": child.display().to_string(),
                "selection": { "references": ["C1"] },
                "include_connected": {
                    "wires": true,
                    "junctions": true,
                    "labels": true,
                    "power_symbols": true,
                    "no_connects": true,
                    "text": true
                },
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", result_text(&result));
        result_json(&result)
    }

    #[tokio::test]
    async fn move_schematic_items_dry_run_reports_revision_and_connected_items() {
        let tmp = TempDir::new().unwrap();
        let (root, child) = write_migration_fixture(&tmp);

        let body = migration_dry_run(&root, &child).await;

        assert_eq!(body["dry_run"], json!(true));
        assert_eq!(body["applied"], json!(false));
        assert!(body["plan_revision"]
            .as_str()
            .unwrap()
            .starts_with("move-schematic-items:"));
        assert_eq!(
            body["target_hierarchy_path"],
            json!("/root-uuid/sheet-child")
        );
        assert_eq!(body["items_to_move"]["symbol"].as_array().unwrap().len(), 2);
        assert_eq!(body["items_to_move"]["wire"].as_array().unwrap().len(), 1);
        assert_eq!(
            body["items_to_move"]["junction"].as_array().unwrap().len(),
            1
        );
        assert_eq!(body["items_to_move"]["label"].as_array().unwrap().len(), 1);
        assert_eq!(body["no_connects_included"], json!(1));
        assert!(body["local_nets_affected"]
            .as_array()
            .unwrap()
            .contains(&json!("LOCAL_SIG")));
    }

    #[tokio::test]
    async fn move_schematic_items_apply_moves_exact_blocks_and_patches_instances() {
        let tmp = TempDir::new().unwrap();
        let (root, child) = write_migration_fixture(&tmp);
        let plan = migration_dry_run(&root, &child).await;
        let revision = plan["plan_revision"].as_str().unwrap();

        let result = handle_move_schematic_items_to_sheet(
            &json!({
                "source_schematic": root.display().to_string(),
                "target_schematic": child.display().to_string(),
                "selection": { "references": ["C1"] },
                "include_connected": {
                    "wires": true,
                    "junctions": true,
                    "labels": true,
                    "power_symbols": true,
                    "no_connects": true,
                    "text": true
                },
                "dry_run": false,
                "plan_revision": revision
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", result_text(&result));
        let body = result_json(&result);
        assert_eq!(body["applied"], json!(true));

        let root_after = std::fs::read_to_string(&root).unwrap();
        let child_after = std::fs::read_to_string(&child).unwrap();
        parse_sexp(&root_after).unwrap();
        parse_sexp(&child_after).unwrap();
        assert!(!root_after.contains("sym-c1"));
        assert!(root_after.contains("sym-r1"));
        assert!(child_after.contains("sym-c1"));
        assert!(child_after.contains("(dnp yes)"));
        assert!(child_after.contains("(in_bom no)"));
        assert!(child_after.contains("(on_board yes)"));
        assert!(child_after.contains("(path \"/root-uuid/sheet-child\""));
        assert!(child_after.contains("(symbol \"Device:C\""));
        assert!(child_after.contains("wire-c1-gnd"));
        assert!(child_after.contains("label-c1"));
        assert!(child_after.contains("sym-gnd"));
    }

    #[tokio::test]
    async fn move_schematic_items_apply_requires_exact_plan_revision() {
        let tmp = TempDir::new().unwrap();
        let (root, child) = write_migration_fixture(&tmp);

        let result = handle_move_schematic_items_to_sheet(
            &json!({
                "source_schematic": root.display().to_string(),
                "target_schematic": child.display().to_string(),
                "selection": { "references": ["C1"] },
                "dry_run": false,
                "plan_revision": "stale"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert!(result_text(&result).contains("plan_revision mismatch"));
        assert!(std::fs::read_to_string(&root).unwrap().contains("sym-c1"));
        assert!(!std::fs::read_to_string(&child).unwrap().contains("sym-c1"));
    }

    #[tokio::test]
    async fn move_schematic_items_refuses_partial_multi_unit_by_default() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root.kicad_sch");
        let child = tmp.path().join("child.kicad_sch");
        std::fs::write(
            &root,
            r#"(kicad_sch
  (version 20250610)
  (uuid "root-uuid")
  (paper "A4")
  (lib_symbols)
  (symbol (lib_id "Amplifier_Operational:LM358") (at 10 10 0) (unit 1) (property "Reference" "U1" (at 10 10 0)) (property "Value" "LM358" (at 10 12 0)) (uuid "u1-a"))
  (symbol (lib_id "Amplifier_Operational:LM358") (at 30 10 0) (unit 2) (property "Reference" "U1" (at 30 10 0)) (property "Value" "LM358" (at 30 12 0)) (uuid "u1-b"))
  (sheet
    (at 100 40)
    (size 60 40)
    (uuid "sheet-child")
    (property "Sheetname" "Child" (at 100 39 0))
    (property "Sheetfile" "child.kicad_sch" (at 100 81 0))
    (instances (project "root" (path "/root-uuid" (page "2"))))
  )
)"#,
        )
        .unwrap();
        std::fs::write(
            &child,
            r#"(kicad_sch
  (version 20250610)
  (uuid "child-uuid")
  (paper "A4")
)"#,
        )
        .unwrap();

        let result = handle_move_schematic_items_to_sheet(
            &json!({
                "source_schematic": root.display().to_string(),
                "target_schematic": child.display().to_string(),
                "selection": { "uuids": ["u1-a"] },
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert!(result_text(&result).contains("multi-unit"));
    }

    fn write_multi_unit_migration_fixture(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let root = tmp.path().join("root.kicad_sch");
        let child = tmp.path().join("child.kicad_sch");
        let mut symbols = String::new();
        for unit in 1..=5 {
            symbols.push_str(&format!(
                r#"
  (symbol
    (lib_id "Device:U")
    (at {} 10 0)
    (unit {unit})
    (property "Reference" "U1" (at {} 8 0))
    (property "Value" "RK3576" (at {} 12 0))
    (property "Footprint" "Package_BGA:BGA" (at {} 14 0))
    (uuid "u1-{unit}")
    (instances
      (project "root"
        (path "/root-uuid" (reference "U1") (unit {unit}))
      )
    )
  )"#,
                10 * unit,
                10 * unit,
                10 * unit,
                10 * unit
            ));
        }
        std::fs::write(
            &root,
            format!(
                r#"(kicad_sch
  (version 20250610)
  (generator "konnect-test")
  (uuid "root-uuid")
  (paper "A4")
  (lib_symbols
    (symbol "Device:U")
  )
{symbols}
  (sheet
    (at 100 40)
    (size 60 40)
    (uuid "sheet-child")
    (property "Sheetname" "Child" (at 100 39 0))
    (property "Sheetfile" "child.kicad_sch" (at 100 81 0))
    (instances (project "root" (path "/root-uuid" (page "2"))))
  )
)"#
            ),
        )
        .unwrap();
        std::fs::write(
            &child,
            r#"(kicad_sch
  (version 20250610)
  (uuid "child-uuid")
  (paper "A4")
  (lib_symbols)
)"#,
        )
        .unwrap();
        (root, child)
    }

    #[tokio::test]
    async fn move_schematic_items_apply_allows_legal_multi_unit_reference() {
        let tmp = TempDir::new().unwrap();
        let (root, child) = write_multi_unit_migration_fixture(&tmp);

        let dry = handle_move_schematic_items_to_sheet(
            &json!({
                "source_schematic": root.display().to_string(),
                "target_schematic": child.display().to_string(),
                "selection": { "references": ["U1"] },
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        let body = result_json(&dry);
        assert_eq!(body["safe_to_apply"], json!(true));
        assert_eq!(
            body["multi_unit_components_detected"],
            json!([{ "reference": "U1", "units": [1, 2, 3, 4, 5], "count": 5 }])
        );

        let result = handle_move_schematic_items_to_sheet(
            &json!({
                "source_schematic": root.display().to_string(),
                "target_schematic": child.display().to_string(),
                "selection": { "references": ["U1"] },
                "dry_run": false,
                "plan_revision": body["plan_revision"].as_str().unwrap()
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", result_text(&result));

        let root_after = std::fs::read_to_string(&root).unwrap();
        let child_after = std::fs::read_to_string(&child).unwrap();
        assert!(!root_after.contains("(property \"Reference\" \"U1\""));
        for unit in 1..=5 {
            assert!(child_after.contains(&format!("(uuid \"u1-{unit}\")")));
            assert!(child_after.contains(&format!("(unit {unit})")));
        }
        assert!(child_after.contains("(path \"/root-uuid/sheet-child\""));
    }

    fn test_symbol_item(uuid: &str, reference: &str, unit: u32) -> SchematicItem {
        SchematicItem {
            uuid: uuid.to_string(),
            kind: "symbol".to_string(),
            source: String::new(),
            reference: Some(reference.to_string()),
            value: Some("RK3576".to_string()),
            footprint: Some("Package_BGA:BGA".to_string()),
            lib_id: Some("Device:U".to_string()),
            lib_name: None,
            unit: Some(unit),
            bounds: None,
        }
    }

    #[test]
    fn component_reference_validation_rejects_duplicate_same_unit() {
        let items = vec![
            test_symbol_item("u1-a", "U1", 1),
            test_symbol_item("u1-b", "U1", 1),
        ];

        let error = validate_component_references(&items).unwrap_err();
        assert!(error.to_string().contains("duplicate reference U1 unit 1"));
    }

    #[test]
    fn component_reference_validation_allows_legal_multi_unit_symbols() {
        let items = vec![
            test_symbol_item("u1-a", "U1", 1),
            test_symbol_item("u1-b", "U1", 2),
            test_symbol_item("u1-c", "U1", 3),
            test_symbol_item("u1-d", "U1", 4),
            test_symbol_item("u1-e", "U1", 5),
        ];

        validate_component_references(&items).unwrap();
    }

    #[tokio::test]
    async fn delete_unlinked_child_schematic_dry_run_and_apply_delete_empty_unlinked_file() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let orphan = blank_schematic(tmp.path(), "orphan.kicad_sch");

        let dry = handle_delete_unlinked_child_schematic(
            &json!({
                "root_schematic": root.display().to_string(),
                "target_schematic": orphan.display().to_string(),
                "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!dry.is_error, "{}", result_text(&dry));
        assert!(orphan.exists());
        let body = result_json(&dry);
        assert_eq!(body["safe_to_apply"], json!(true));
        assert!(body["plan_revision"]
            .as_str()
            .unwrap()
            .starts_with("delete-unlinked-child-schematic:"));

        let applied = handle_delete_unlinked_child_schematic(
            &json!({
                "root_schematic": root.display().to_string(),
                "target_schematic": orphan.display().to_string(),
                "dry_run": false,
                "plan_revision": body["plan_revision"].as_str().unwrap()
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!applied.is_error, "{}", result_text(&applied));
        assert!(!orphan.exists());
    }

    #[tokio::test]
    async fn delete_unlinked_child_schematic_refuses_linked_or_nonempty_files() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        handle_add_hierarchical_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "sheet_file": "linked.kicad_sch",
                "sheet_name": "Linked"
            }),
            &ctx,
        )
        .await
        .unwrap();
        let linked = tmp.path().join("linked.kicad_sch");

        let linked_result = handle_delete_unlinked_child_schematic(
            &json!({
                "root_schematic": root.display().to_string(),
                "target_schematic": linked.display().to_string()
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(linked_result.is_error);
        assert!(result_text(&linked_result).contains("still linked"));

        let nonempty = tmp.path().join("nonempty.kicad_sch");
        std::fs::write(
            &nonempty,
            r#"(kicad_sch
  (version 20250610)
  (uuid "nonempty")
  (paper "A4")
  (symbol (lib_id "Device:R") (at 10 10 0) (unit 1) (property "Reference" "R1" (at 10 10 0)) (uuid "r1"))
)"#,
        )
        .unwrap();

        let nonempty_result = handle_delete_unlinked_child_schematic(
            &json!({
                "root_schematic": root.display().to_string(),
                "target_schematic": nonempty.display().to_string()
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(nonempty_result.is_error);
        assert!(result_text(&nonempty_result).contains("not empty"));
        assert!(nonempty.exists());
    }

    #[test]
    fn move_schematic_items_tool_is_registered_with_discoverable_schema() {
        let tool = tools()
            .into_iter()
            .find(|tool| tool.name == "move_schematic_items_to_sheet")
            .expect("tool is registered");
        assert!(tool
            .description
            .contains("move existing objects between sheets"));
        assert!(tool.description.contains("migrate hierarchy"));
        assert!(tool.description.contains("hierarchical restructure"));
        assert!(tool.input_schema["properties"]["selection"]["description"]
            .as_str()
            .unwrap()
            .contains("references"));
        assert!(
            tool.input_schema["properties"]["include_connected"]["properties"]["wires"].is_object()
        );
    }

    #[test]
    fn delete_unlinked_child_schematic_tool_is_registered_with_discoverable_schema() {
        let tool = tools()
            .into_iter()
            .find(|tool| tool.name == "delete_unlinked_child_schematic")
            .expect("tool is registered");
        assert!(tool.description.contains("safe cleanup"));
        assert!(tool.description.contains("must not be reachable"));
        assert!(tool.input_schema["properties"]["root_schematic"].is_object());
        assert!(tool.input_schema["properties"]["plan_revision"].is_object());
    }

    async fn sheet_at(tmp: &TempDir, ctx: &ToolContext, x: f64, y: f64) -> PathBuf {
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_file": "power.kicad_sch",
            "sheet_name": "Power",
            "x": x, "y": y
        });
        handle_add_hierarchical_sheet(&args, ctx).await.unwrap();
        root
    }

    #[tokio::test]
    async fn edit_sheet_accepts_the_position_the_sheet_already_has() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_at(&tmp, &ctx, 20.0, 20.0).await;
        let before = std::fs::read_to_string(&root).unwrap();

        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power",
            "x": 20.0, "y": 20.0
        });
        let result = handle_edit_sheet(&args, &ctx).await.unwrap();

        assert!(!result.is_error, "an idempotent edit is not an error");
        let body = result_json(&result);
        assert_eq!(body["changed"], json!(false));
        assert_eq!(body["changed_fields"], json!([]));
        assert_eq!(body["requested_fields"], json!(["position"]));
        assert_eq!(
            std::fs::read_to_string(&root).unwrap(),
            before,
            "a no-op edit leaves the file alone"
        );
    }

    #[tokio::test]
    async fn edit_sheet_reports_only_the_fields_that_differ() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_at(&tmp, &ctx, 20.0, 20.0).await;

        // Position is restated, the name is genuinely new.
        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power",
            "new_name": "Power Supply",
            "x": 20.0, "y": 20.0
        });
        let result = handle_edit_sheet(&args, &ctx).await.unwrap();

        assert!(!result.is_error);
        let body = result_json(&result);
        assert_eq!(body["changed"], json!(true));
        assert_eq!(body["changed_fields"], json!(["name"]));
        assert_eq!(body["requested_fields"], json!(["name", "position"]));
    }

    #[tokio::test]
    async fn move_sheet_accepts_the_position_the_sheet_already_has() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_at(&tmp, &ctx, 20.0, 20.0).await;

        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power",
            "x": 20.0, "y": 20.0
        });
        let result = handle_move_sheet(&args, &ctx).await.unwrap();

        assert!(!result.is_error, "an idempotent move is not an error");
        assert_eq!(result_json(&result)["changed"], json!(false));
    }

    #[tokio::test]
    async fn edit_sheet_is_idempotent_on_a_sheet_konnect_already_wrote() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_at(&tmp, &ctx, 20.0, 20.0).await;
        let move_it = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power",
            "x": 30.0, "y": 30.0
        });

        // The first edit rewrites the sheet in Konnect's own serialisation, so
        // the block round-trips byte-for-byte from here on. That is the state
        // in which the reported error appeared.
        let first = handle_edit_sheet(&move_it, &ctx).await.unwrap();
        assert!(!first.is_error);
        assert_eq!(result_json(&first)["changed"], json!(true));
        let settled = std::fs::read_to_string(&root).unwrap();

        let second = handle_edit_sheet(&move_it, &ctx).await.unwrap();

        assert!(
            !second.is_error,
            "re-asserting the current position must not error"
        );
        assert_eq!(result_json(&second)["changed"], json!(false));
        assert_eq!(std::fs::read_to_string(&root).unwrap(), settled);
    }

    #[tokio::test]
    async fn edit_sheet_pin_accepts_the_position_the_pin_already_has() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_at(&tmp, &ctx, 20.0, 20.0).await;
        let add = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power",
            "pin_name": "VCC",
            "pin_type": "input",
            "x": 20.0, "y": 25.0
        });
        handle_add_sheet_pin(&add, &ctx).await.unwrap();

        let restate = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power",
            "pin_name": "VCC",
            "x": 20.0, "y": 25.0
        });
        handle_edit_sheet_pin(&restate, &ctx).await.unwrap();
        let result = handle_edit_sheet_pin(&restate, &ctx).await.unwrap();

        // This handler does no field pre-comparison; it reaches the command
        // layer with an identical block and relies on the no-op being legal.
        assert!(
            !result.is_error,
            "the relaxed command layer covers callers that do not pre-compare"
        );
    }

    #[tokio::test]
    async fn edit_sheet_still_rejects_a_call_with_no_fields() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_at(&tmp, &ctx, 20.0, 20.0).await;

        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power"
        });
        let result = handle_edit_sheet(&args, &ctx).await.unwrap();

        assert!(result.is_error, "asking for nothing is still an error");
    }

    #[tokio::test]
    async fn add_hierarchical_sheet_rejects_duplicate_name() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();

        let args = json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" });
        handle_add_hierarchical_sheet(&args, &ctx).await.unwrap();

        let args2 = json!({ "schematic": root.display().to_string(), "sheet_file": "b.kicad_sch", "sheet_name": "A" });
        let result = handle_add_hierarchical_sheet(&args2, &ctx).await.unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn second_sheet_gets_next_free_page() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();

        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "b.kicad_sch", "sheet_name": "B" }),
            &ctx,
        )
        .await
        .unwrap();

        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.by_name("A").unwrap().page("root"), Some("2"));
        assert_eq!(parent.sheets.by_name("B").unwrap().page("root"), Some("3"));
    }

    #[tokio::test]
    async fn edit_sheet_renames_and_resizes() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "new_name": "Renamed", "width": 100.0, "height": 60.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert!(parent.sheets.by_name("A").is_none());
        let renamed = parent.sheets.by_name("Renamed").unwrap();
        assert_eq!(renamed.width, 100.0);
        assert_eq!(renamed.height, 60.0);
    }

    #[tokio::test]
    async fn edit_sheet_with_no_fields_errors() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn move_sheet_updates_position_only() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A", "x": 10.0, "y": 10.0 }),
            &ctx,
        )
        .await
        .unwrap();

        handle_move_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "x": 99.0, "y": 88.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let parent = cse::Schematic::load(&root).unwrap();
        let sheet = parent.sheets.by_name("A").unwrap();
        assert_eq!(sheet.position(), (99.0, 88.0));
    }

    #[tokio::test]
    async fn delete_sheet_removes_reference_but_keeps_child_file() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_delete_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert!(parent.sheets.is_empty());
        assert!(tmp.path().join("a.kicad_sch").exists());
    }

    #[tokio::test]
    async fn delete_sheet_not_found_errors() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        let result = handle_delete_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Nope" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn duplicate_sheet_copies_file_independently() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "amp.kicad_sch", "sheet_name": "Amp1", "x": 10.0, "y": 10.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_duplicate_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "source_sheet_name": "Amp1",
                "new_sheet_name": "Amp2",
                "new_file": "amp2.kicad_sch"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);
        assert!(tmp.path().join("amp2.kicad_sch").exists());

        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.len(), 2);
        let amp2 = parent.sheets.by_name("Amp2").unwrap();
        assert_eq!(amp2.file(), "amp2.kicad_sch");
        assert_eq!(amp2.position(), (30.0, 30.0)); // offset from source (10,10)

        // Independent files: the two schematics have different internal UUIDs.
        let sch1 = cse::Schematic::load(tmp.path().join("amp.kicad_sch")).unwrap();
        let sch2 = cse::Schematic::load(tmp.path().join("amp2.kicad_sch")).unwrap();
        assert_ne!(sch1.uuid, sch2.uuid);
    }

    fn declared_uuids(source: &str) -> HashSet<String> {
        const DECLARATION: &str = "(uuid \"";
        let mut found = HashSet::new();
        let mut rest = source;
        while let Some(at) = rest.find(DECLARATION) {
            let body = &rest[at + DECLARATION.len()..];
            let Some(end) = body.find('"') else { break };
            found.insert(body[..end].to_owned());
            rest = &body[end + 1..];
        }
        found
    }

    #[test]
    fn regenerating_uuids_replaces_declarations_and_the_paths_that_name_them() {
        let source = r#"(kicad_sch
  (symbol (lib_id "Device:R") (uuid "sym-a")
    (instances (project "demo" (path "/root-a/sym-a" (reference "R1"))))
  )
  (text "see sym-a in the notes" (uuid "text-a"))
  (sheet_instances (path "/root-a" (page "2")))
)
"#;

        let out = regenerate_item_uuids(source);

        let before = declared_uuids(source);
        let after = declared_uuids(&out);
        assert_eq!(before.len(), 2, "fixture declares two UUIDs");
        assert_eq!(after.len(), 2, "the copy declares two UUIDs");
        assert!(
            before.is_disjoint(&after),
            "every declaration must change: {before:?} vs {after:?}"
        );

        // The instance path naming the renamed symbol follows it.
        let new_symbol = out
            .split_once("(uuid \"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(id, _)| id.to_owned())
            .expect("symbol uuid present");
        assert!(
            out.contains(&format!("(path \"/root-a/{new_symbol}\"")),
            "the path must follow the renamed item:\n{out}"
        );

        // Strings that are not declared UUIDs are left alone — including a
        // sentence that merely contains one, and "root-a", which is a path
        // segment but was never declared here.
        assert!(out.contains("(project \"demo\""), "{out}");
        assert!(out.contains("(reference \"R1\")"), "{out}");
        assert!(out.contains("(lib_id \"Device:R\")"), "{out}");
        assert!(
            out.contains("\"see sym-a in the notes\""),
            "text content must survive verbatim:\n{out}"
        );
        assert!(out.contains("(path \"/root-a\" (page \"2\"))"), "{out}");
    }

    #[test]
    fn regenerating_uuids_leaves_a_document_without_any_alone() {
        let source = "(kicad_sch\n  (lib_symbols)\n)\n";
        assert_eq!(regenerate_item_uuids(source), source);
    }

    /// The scan walks every quoted string in the file, so an escaped quote
    /// inside text content must not shift it out of step — a bug there
    /// corrupts the whole document, not just the annotation.
    #[test]
    fn regenerating_uuids_survives_escaped_quotes_in_text() {
        let source = r#"(kicad_sch
  (text "a \"b\" c" (uuid "text-a"))
  (generator "konnect")
)
"#;

        let out = regenerate_item_uuids(source);

        assert!(
            out.contains("(generator \"konnect\")"),
            "a later string was corrupted by the escape:\n{out}"
        );
        assert!(out.contains(r#""a \"b\" c""#), "{out}");
        assert!(!out.contains("text-a"), "{out}");
    }

    /// The report: `add_schematic_text` then `duplicate_sheet` leaves both
    /// sheets carrying the same text UUID.
    #[tokio::test]
    async fn duplicate_sheet_gives_the_copy_its_own_item_uuids() {
        const SOURCE_TEXT_UUID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "sheet_file": "amp.kicad_sch",
                "sheet_name": "Amp1",
                "x": 10.0, "y": 10.0
            }),
            &ctx,
        )
        .await
        .unwrap();

        // An annotation in the child, shaped as add_schematic_text writes one.
        let child = tmp.path().join("amp.kicad_sch");
        let content = std::fs::read_to_string(&child).unwrap();
        let cut = content.rfind(')').unwrap();
        let block = format!(
            "\n  (text \"NOTE\"\n    (at 10 10 0)\n    \
             (effects (font (size 1.27 1.27)) (justify left bottom))\n    \
             (uuid \"{SOURCE_TEXT_UUID}\")\n  )\n"
        );
        std::fs::write(
            &child,
            format!("{}{}{}", &content[..cut], block, &content[cut..]),
        )
        .unwrap();

        let result = handle_duplicate_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "source_sheet_name": "Amp1",
                "new_sheet_name": "Amp2",
                "new_file": "amp2.kicad_sch"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let source_ids = declared_uuids(&std::fs::read_to_string(&child).unwrap());
        let copy_ids =
            declared_uuids(&std::fs::read_to_string(tmp.path().join("amp2.kicad_sch")).unwrap());

        assert!(
            source_ids.contains(SOURCE_TEXT_UUID),
            "the source keeps its own annotation"
        );
        assert!(
            !copy_ids.contains(SOURCE_TEXT_UUID),
            "the copy kept the source's text UUID"
        );
        assert!(
            source_ids.is_disjoint(&copy_ids),
            "no UUID may be shared between a sheet and its copy:\n{source_ids:?}\n{copy_ids:?}"
        );
        assert_eq!(
            source_ids.len(),
            copy_ids.len(),
            "same items, new identities"
        );
    }

    #[tokio::test]
    async fn duplicate_sheet_refuses_to_overwrite_existing_file() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        // A second, unrelated sheet already occupies "b.kicad_sch".
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "b.kicad_sch", "sheet_name": "B" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_duplicate_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "source_sheet_name": "A",
                "new_sheet_name": "A-copy",
                "new_file": "b.kicad_sch"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn get_sheet_hierarchy_returns_nested_tree() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "mid.kicad_sch", "sheet_name": "Mid" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": tmp.path().join("mid.kicad_sch").display().to_string(), "sheet_file": "leaf.kicad_sch", "sheet_name": "Leaf" }),
            &ctx,
        )
        .await
        .unwrap();

        let result =
            handle_get_sheet_hierarchy(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        assert!(!result.is_error);

        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let tree: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(tree["children"][0]["name"], "Mid");
        assert_eq!(tree["children"][0]["children"][0]["name"], "Leaf");
    }

    #[tokio::test]
    async fn get_sheet_hierarchy_reports_missing_child_file() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "gone.kicad_sch", "sheet_name": "Gone" }),
            &ctx,
        )
        .await
        .unwrap();
        std::fs::remove_file(tmp.path().join("gone.kicad_sch")).unwrap();

        let result =
            handle_get_sheet_hierarchy(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let tree: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(tree["children"][0]["error"], "child file not found on disk");
    }

    #[tokio::test]
    async fn renumber_sheet_pages_closes_gap_after_delete() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        for (file, name) in [
            ("a.kicad_sch", "A"),
            ("b.kicad_sch", "B"),
            ("c.kicad_sch", "C"),
        ] {
            handle_add_hierarchical_sheet(
                &json!({ "schematic": root.display().to_string(), "sheet_file": file, "sheet_name": name }),
                &ctx,
            )
            .await
            .unwrap();
        }
        // A=2, B=3, C=4. Delete B, leaving a gap at page 3.
        handle_delete_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "B" }),
            &ctx,
        )
        .await
        .unwrap();

        let result =
            handle_renumber_sheet_pages(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.by_name("A").unwrap().page("root"), Some("2"));
        assert_eq!(parent.sheets.by_name("C").unwrap().page("root"), Some("3"));
    }

    #[tokio::test]
    async fn linking_existing_file_with_symbols_patches_instance_paths() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let child_path = tmp.path().join("reused.kicad_sch");
        create_blank_schematic(&child_path).unwrap();

        // Put a symbol in the child file before it's ever linked.
        {
            let mut child = cse::Schematic::load(&child_path).unwrap();
            let mut sym = cse::Symbol::new("Device:R", 10.0, 10.0);
            sym.set_reference("R1");
            child.add_symbol(sym);
            child.overwrite().unwrap();
        }

        let ctx = test_ctx();
        let result = handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "reused.kicad_sch", "sheet_name": "Reused" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let child = cse::Schematic::load(&child_path).unwrap();
        let sym = child.symbols.by_reference("R1").unwrap();
        // eeschema path format: "/<root-uuid>/<sheet-symbol-uuid>", keyed
        // under the default project name (the parent file's stem).
        let parent = cse::Schematic::load(&root).unwrap();
        let hier_path = format!(
            "/{}/{}",
            parent.uuid.as_deref().expect("root uuid must exist"),
            parent.sheets.by_name("Reused").unwrap().uuid
        );
        assert!(sym.has_instance_path("root", &hier_path));
    }

    // ─── PR-B: sheet pin lifecycle ─────────────────────────────────────────

    fn add_label(sch_path: &Path, text: &str, shape: &str, x: f64, y: f64) {
        let mut sch = cse::Schematic::load(sch_path).unwrap();
        sch.add_hierarchical_label(text, shape, x, y);
        sch.overwrite().unwrap();
    }

    #[tokio::test]
    async fn import_sheet_pins_creates_matching_pins_from_labels() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "power.kicad_sch", "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        let child_path = tmp.path().join("power.kicad_sch");
        add_label(&child_path, "VIN", "input", 5.0, 5.0);
        add_label(&child_path, "GND", "passive", 5.0, 10.0);

        let result = handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        let sheet = parent.sheets.by_name("Power").unwrap();
        assert_eq!(sheet.pins.len(), 2);
        assert_eq!(sheet.pin_by_name("VIN").unwrap().pin_type, "input");
        assert_eq!(sheet.pin_by_name("GND").unwrap().pin_type, "passive");
    }

    #[tokio::test]
    async fn import_sheet_pins_skips_already_imported_names() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "power.kicad_sch", "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        let child_path = tmp.path().join("power.kicad_sch");
        add_label(&child_path, "VIN", "input", 5.0, 5.0);

        handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        let result = handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.by_name("Power").unwrap().pins.len(), 1); // not duplicated
    }

    #[tokio::test]
    async fn add_sheet_pin_writes_a_rotation_kicad_can_load() {
        // Regression for #303: the pin used to be written as `(at x y)` with no
        // rotation, and KiCAD then refused to load the whole schematic.
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "TESTNET", "pin_type": "input", "x": 100.0, "y": 105.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let written = std::fs::read_to_string(&root).unwrap();
        assert!(
            written.contains("(at 100 105 0)"),
            "sheet pin must be written with a rotation, got: {}",
            written
                .lines()
                .skip_while(|l| !l.contains("(pin \"TESTNET\""))
                .take(3)
                .collect::<Vec<_>>()
                .join("\n")
        );

        // And it must survive a reload through the same parser.
        let parent = cse::Schematic::load(&root).unwrap();
        let pin_rotation = parent.sheets.by_name("A").unwrap().pins[0].at.rotation;
        assert_eq!(pin_rotation, Some(0.0));
    }

    #[tokio::test]
    async fn add_sheet_pin_rejects_duplicate_name() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let args = json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC", "pin_type": "input", "x": 90.0, "y": 55.0 });
        let result = handle_add_sheet_pin(&args, &ctx).await.unwrap();
        assert!(!result.is_error);

        let result2 = handle_add_sheet_pin(&args, &ctx).await.unwrap();
        assert!(result2.is_error);
    }

    #[tokio::test]
    async fn add_sheet_pin_rejects_invalid_pin_type() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC", "pin_type": "not_a_type", "x": 90.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn edit_sheet_pin_renames_and_retypes() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC", "pin_type": "input", "x": 90.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC", "new_name": "VDD", "pin_type": "output" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        let sheet = parent.sheets.by_name("A").unwrap();
        assert!(sheet.pin_by_name("VCC").is_none());
        let renamed = sheet.pin_by_name("VDD").unwrap();
        assert_eq!(renamed.pin_type, "output");
    }

    #[tokio::test]
    async fn edit_sheet_pin_with_no_fields_errors() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC", "pin_type": "input", "x": 90.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn delete_sheet_pin_removes_it() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC", "pin_type": "input", "x": 90.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_delete_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert!(parent
            .sheets
            .by_name("A")
            .unwrap()
            .pin_by_name("VCC")
            .is_none());
    }

    #[tokio::test]
    async fn delete_sheet_pin_not_found_errors() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_delete_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "Nope" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn validate_sheet_pins_reports_mismatches() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "power.kicad_sch", "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        let child_path = tmp.path().join("power.kicad_sch");
        // Label with no pin, and (below) a pin with no label — deliberate mismatch.
        add_label(&child_path, "VIN", "input", 5.0, 5.0);
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Power", "pin_name": "GND", "pin_type": "passive", "x": 90.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result =
            handle_validate_sheet_pins(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        assert!(!result.is_error);

        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let report: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["issue_count"], 1);
        let issue = &report["issues"][0];
        assert_eq!(issue["sheet"], "Power");
        assert!(issue["labels_without_pins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "VIN"));
        assert!(issue["pins_without_labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "GND"));
    }

    #[tokio::test]
    async fn validate_sheet_pins_reports_no_issues_when_synced() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "power.kicad_sch", "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        let child_path = tmp.path().join("power.kicad_sch");
        add_label(&child_path, "VIN", "input", 5.0, 5.0);
        handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();

        let result =
            handle_validate_sheet_pins(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let report: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["issue_count"], 0);
    }
}
