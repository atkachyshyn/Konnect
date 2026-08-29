//! Graphical primitive editing inside an existing `.kicad_sym` symbol definition.
//!
//! `create_symbol` writes a whole symbol; there was no way to add a caption to
//! one already on disk without editing the file by hand, which is exactly what
//! Konnect exists to prevent. These tools close that gap for project-local
//! text, line/polyline, rectangle, circle and arc edits.
//!
//! Everything here is a byte-range edit against untouched source, never a
//! re-serialization of the symbol. That is what makes the preservation
//! guarantee structural rather than best-effort: pins, pin names and numbers,
//! electrical types, unit membership, properties and existing graphics are not
//! rewritten, so they cannot be lost. Only the bytes of the items the selector
//! names are touched.
//!
//! Format reference, confirmed against KiCad 10's own libraries
//! (`(version 20251024)`, tab-indented, expanded):
//!
//! ```text
//! (text "P"
//!     (at 0 6.35 0)
//!     (effects
//!         (font
//!             (size 1.524 1.524)
//!         )
//!     )
//! )
//! ```
//!
//! Note there is no `uuid` on a symbol graphic — unlike a `.kicad_sch` item.
//! We do not invent one.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{ToolContext, ToolDef};
use konnect_schematic_editor::types::fmt_f64;
use konnect_sexp::parser::{parse_sexp, SexpNode};
use konnect_sexp::writer::{
    apply_edits, find_direct_child_blocks, read_consistent, write_atomic_if_unchanged, SexpEdit,
};
use serde_json::json;
use std::path::{Path, PathBuf};

// ─── Model ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Append,
    Replace,
    Delete,
}

impl Mode {
    fn parse(value: &serde_json::Value) -> Result<Self, Error> {
        match value.as_str() {
            Some("append") => Ok(Self::Append),
            Some("replace") => Ok(Self::Replace),
            Some("delete") => Ok(Self::Delete),
            _ => Err(invalid("mode", "must be 'append', 'replace' or 'delete'")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Replace => "replace",
            Self::Delete => "delete",
        }
    }
}

/// Which existing graphics a selector matches.
///
/// `line` is accepted as a friendlier spelling of KiCad's `polyline`, because
/// that is what a caller reading the schema will reach for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectorKind {
    Text,
    Rectangle,
    Line,
    Circle,
    Arc,
    Any,
}

impl SelectorKind {
    fn parse(value: Option<&str>) -> Result<Self, Error> {
        match value {
            None | Some("any") => Ok(Self::Any),
            Some("text") => Ok(Self::Text),
            Some("rectangle") => Ok(Self::Rectangle),
            Some("line") | Some("polyline") => Ok(Self::Line),
            Some("circle") => Ok(Self::Circle),
            Some("arc") => Ok(Self::Arc),
            Some(other) => Err(invalid(
                "selector.kind",
                format!(
                    "unknown kind '{other}'; expected text, rectangle, line, circle, arc or any"
                ),
            )),
        }
    }

    /// Whether an S-expression tag is a graphic this kind selects.
    fn matches_tag(self, tag: &str) -> bool {
        match self {
            Self::Text => tag == "text",
            Self::Rectangle => tag == "rectangle",
            Self::Line => tag == "polyline",
            Self::Circle => tag == "circle",
            Self::Arc => tag == "arc",
            Self::Any => is_graphic_tag(tag),
        }
    }
}

/// The graphic tags that may appear inside a symbol or unit body.
///
/// Deliberately a closed list: anything else in the body — `pin`, `property`,
/// a nested unit `symbol` — must never be selectable, or a careless `any`
/// selector with `delete` would take pins out.
fn is_graphic_tag(tag: &str) -> bool {
    matches!(
        tag,
        "text" | "text_box" | "rectangle" | "polyline" | "circle" | "arc" | "bezier"
    )
}

#[derive(Debug, Clone)]
struct Selector {
    kind: SelectorKind,
    text: Option<String>,
    uuid: Option<String>,
}

impl Selector {
    fn parse(value: Option<&serde_json::Value>) -> Result<Self, Error> {
        let Some(value) = value else {
            return Ok(Self {
                kind: SelectorKind::Any,
                text: None,
                uuid: None,
            });
        };
        if !value.is_object() {
            return Err(invalid("selector", "must be an object"));
        }
        Ok(Self {
            kind: SelectorKind::parse(value["kind"].as_str())?,
            text: value["text"].as_str().map(str::to_string),
            uuid: value["uuid"].as_str().map(str::to_string),
        })
    }

    fn matches(&self, node: &SexpNode, tag: &str) -> bool {
        if !self.kind.matches_tag(tag) {
            return false;
        }
        if let Some(wanted) = &self.text {
            // The literal is the first value after the tag for `text`; for any
            // other graphic there is no text to compare, so a text selector
            // simply does not match it.
            if node.get(1).and_then(SexpNode::as_str) != Some(wanted.as_str()) {
                return false;
            }
        }
        if let Some(wanted) = &self.uuid {
            if node.find_str("uuid") != Some(wanted.as_str()) {
                return false;
            }
        }
        true
    }
}

/// One text primitive to write.
#[derive(Debug, Clone)]
struct SymbolText {
    text: String,
    x: f64,
    y: f64,
    angle: f64,
    font_size: f64,
    font_thickness: Option<f64>,
    justify: Vec<String>,
    hide: bool,
}

impl SymbolText {
    fn parse(value: &serde_json::Value, index: usize) -> Result<Self, Error> {
        let at = |field: &str| format!("graphics[{index}].{field}");

        match value["kind"].as_str() {
            None | Some("text") => {}
            Some(other) => {
                return Err(invalid(
                    at("kind"),
                    format!("only 'text' can be written; got '{other}'"),
                ))
            }
        }

        let Some(text) = value["text"].as_str() else {
            return Err(invalid(at("text"), "missing or not a string"));
        };
        if text.is_empty() {
            return Err(invalid(at("text"), "must not be empty"));
        }

        let number = |field: &str, default: Option<f64>| -> Result<f64, Error> {
            match value.get(field) {
                None | Some(serde_json::Value::Null) => {
                    default.ok_or_else(|| invalid(at(field), "missing"))
                }
                Some(v) => v
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .ok_or_else(|| invalid(at(field), "must be a finite number")),
            }
        };

        let font_size = number("font_size", Some(1.27))?;
        if font_size <= 0.0 {
            return Err(invalid(at("font_size"), "must be greater than zero"));
        }
        let font_thickness = match value.get("font_thickness") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => {
                let t = v
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .ok_or_else(|| invalid(at("font_thickness"), "must be a finite number"))?;
                if t <= 0.0 {
                    return Err(invalid(at("font_thickness"), "must be greater than zero"));
                }
                Some(t)
            }
        };

        let mut justify = Vec::new();
        match value.get("justify") {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Array(items)) => {
                for item in items {
                    let Some(token) = item.as_str() else {
                        return Err(invalid(at("justify"), "entries must be strings"));
                    };
                    if !matches!(
                        token,
                        "left" | "right" | "center" | "top" | "bottom" | "mirror"
                    ) {
                        return Err(invalid(
                            at("justify"),
                            format!(
                                "unknown justification '{token}'; expected left, right, center, \
                                 top, bottom or mirror"
                            ),
                        ));
                    }
                    if !justify.iter().any(|existing| existing == token) {
                        justify.push(token.to_string());
                    }
                }
            }
            Some(_) => return Err(invalid(at("justify"), "must be an array of strings")),
        }

        let hide = match value.get("effects").and_then(|e| e.get("hide")) {
            None | Some(serde_json::Value::Null) => false,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| invalid(at("effects.hide"), "must be a boolean"))?,
        };

        Ok(Self {
            text: text.to_string(),
            x: number("x", Some(0.0))?,
            y: number("y", Some(0.0))?,
            angle: number("angle", Some(0.0))?,
            font_size,
            font_thickness,
            justify,
            hide,
        })
    }
}

#[derive(Debug, Clone)]
struct StrokeFill {
    stroke_width_mm: f64,
    stroke_type: String,
    fill: String,
}

impl StrokeFill {
    fn parse(value: &serde_json::Value, index: usize) -> Result<Self, Error> {
        let at = |field: &str| format!("graphics[{index}].{field}");
        let stroke_width_mm = match value
            .get("stroke_width_mm")
            .or_else(|| value.get("stroke_width"))
        {
            None | Some(serde_json::Value::Null) => 0.254,
            Some(v) => v
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .ok_or_else(|| {
                    invalid(
                        at("stroke_width_mm"),
                        "must be a finite non-negative number",
                    )
                })?,
        };
        let stroke_type = value["stroke_type"].as_str().unwrap_or("default");
        if !matches!(
            stroke_type,
            "default" | "solid" | "dash" | "dot" | "dash_dot" | "dash_dot_dot"
        ) {
            return Err(invalid(
                at("stroke_type"),
                "must be default, solid, dash, dot, dash_dot or dash_dot_dot",
            ));
        }
        let fill = value["fill"].as_str().unwrap_or("none");
        if !matches!(fill, "none" | "background" | "outline") {
            return Err(invalid(
                at("fill"),
                "must be none, background or outline for KiCad symbol graphics",
            ));
        }
        if stroke_width_mm == 0.0 && fill == "none" {
            return Err(invalid(
                at("stroke_width_mm"),
                "must be positive when fill is none",
            ));
        }
        Ok(Self {
            stroke_width_mm,
            stroke_type: stroke_type.to_string(),
            fill: fill.to_string(),
        })
    }
}

#[derive(Debug, Clone)]
struct SymbolPolyline {
    points: Vec<(f64, f64)>,
    style: StrokeFill,
}

#[derive(Debug, Clone)]
struct SymbolRectangle {
    start: (f64, f64),
    end: (f64, f64),
    style: StrokeFill,
}

#[derive(Debug, Clone)]
struct SymbolCircle {
    center: (f64, f64),
    radius_mm: f64,
    style: StrokeFill,
}

#[derive(Debug, Clone)]
struct SymbolArc {
    start: (f64, f64),
    mid: (f64, f64),
    end: (f64, f64),
    style: StrokeFill,
}

#[derive(Debug, Clone)]
enum SymbolGraphic {
    Text(SymbolText),
    Polyline(SymbolPolyline),
    Rectangle(SymbolRectangle),
    Circle(SymbolCircle),
    Arc(SymbolArc),
}

impl SymbolGraphic {
    fn parse(value: &serde_json::Value, index: usize) -> Result<Self, Error> {
        let at = |field: &str| format!("graphics[{index}].{field}");
        match value["kind"].as_str().unwrap_or("text") {
            "text" => Ok(Self::Text(SymbolText::parse(value, index)?)),
            "line" | "polyline" => {
                let points = if let Some(items) = value["points"].as_array() {
                    let points: Result<Vec<_>, _> = items
                        .iter()
                        .enumerate()
                        .map(|(point_index, point)| {
                            parse_point(point, &format!("graphics[{index}].points[{point_index}]"))
                        })
                        .collect();
                    points?
                } else {
                    vec![
                        parse_point(&value["start"], &at("start"))?,
                        parse_point(&value["end"], &at("end"))?,
                    ]
                };
                if points.len() < 2 {
                    return Err(invalid(at("points"), "must contain at least two points"));
                }
                Ok(Self::Polyline(SymbolPolyline {
                    points,
                    style: StrokeFill::parse(value, index)?,
                }))
            }
            "rectangle" => Ok(Self::Rectangle(SymbolRectangle {
                start: parse_point(&value["start"], &at("start"))?,
                end: parse_point(&value["end"], &at("end"))?,
                style: StrokeFill::parse(value, index)?,
            })),
            "circle" => {
                let radius_mm = match value.get("radius_mm").or_else(|| value.get("radius")) {
                    Some(v) => v
                        .as_f64()
                        .filter(|n| n.is_finite() && *n > 0.0)
                        .ok_or_else(|| invalid(at("radius_mm"), "must be a finite number greater than zero"))?,
                    None => return Err(invalid(at("radius_mm"), "missing")),
                };
                Ok(Self::Circle(SymbolCircle {
                    center: parse_point(&value["center"], &at("center"))?,
                    radius_mm,
                    style: StrokeFill::parse(value, index)?,
                }))
            }
            "arc" => Ok(Self::Arc(SymbolArc {
                start: parse_point(&value["start"], &at("start"))?,
                mid: parse_point(&value["mid"], &at("mid"))?,
                end: parse_point(&value["end"], &at("end"))?,
                style: StrokeFill::parse(value, index)?,
            })),
            other => Err(invalid(
                at("kind"),
                format!("unsupported primitive '{other}'; expected text, line, polyline, rectangle, circle or arc"),
            )),
        }
    }
}

fn parse_point(value: &serde_json::Value, field: &str) -> Result<(f64, f64), Error> {
    let finite = |v: &serde_json::Value, axis: &str| {
        v.as_f64()
            .filter(|n| n.is_finite())
            .ok_or_else(|| invalid(format!("{field}.{axis}"), "must be a finite number"))
    };
    if let Some(items) = value.as_array() {
        if items.len() != 2 {
            return Err(invalid(field, "point arrays must be [x, y]"));
        }
        return Ok((finite(&items[0], "x")?, finite(&items[1], "y")?));
    }
    if let Some(obj) = value.as_object() {
        let Some(x) = obj.get("x") else {
            return Err(invalid(format!("{field}.x"), "missing"));
        };
        let Some(y) = obj.get("y") else {
            return Err(invalid(format!("{field}.y"), "missing"));
        };
        return Ok((finite(x, "x")?, finite(y, "y")?));
    }
    Err(invalid(
        field,
        "must be a point object {x,y} or [x,y] array",
    ))
}

// ─── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum Error {
    Invalid { field: String, reason: String },
    Conflict(String),
    NotFound(String),
}

fn invalid(field: impl Into<String>, reason: impl Into<String>) -> Error {
    Error::Invalid {
        field: field.into(),
        reason: reason.into(),
    }
}

impl Error {
    fn into_result(self, path: &Path) -> CallToolResult {
        match self {
            Error::Invalid { field, reason } => CallToolResult::error_kind(
                ToolErrorKind::InvalidArgument {
                    field: field.clone(),
                    reason: reason.clone(),
                },
                format!("Argument '{field}' is invalid: {reason}"),
            ),
            Error::Conflict(reason) => CallToolResult::error_kind(
                ToolErrorKind::Conflict {
                    paths: vec![path.display().to_string()],
                },
                format!("Symbol graphics were not changed: {reason}"),
            ),
            Error::NotFound(reason) => CallToolResult::error_kind(
                ToolErrorKind::InvalidArgument {
                    field: "symbol_name".to_string(),
                    reason: reason.clone(),
                },
                format!("Symbol graphics were not changed: {reason}"),
            ),
        }
    }
}

// ─── Mutation core ───────────────────────────────────────────────────────────

#[derive(Debug)]
struct Prepared {
    replacement: String,
    matched: usize,
    added: usize,
    deleted: usize,
    warnings: Vec<String>,
}

/// Locate `(symbol "name" …)` among the direct children of a block.
///
/// Two traps here, both from `find_direct_child_blocks` anchoring on the
/// *first* block carrying `parent_tag`:
///
/// 1. `parent_tag` must be the enclosing tag, not the child's. Passing
///    "symbol" at library level anchors on the first top-level symbol and
///    returns *its* units — so the library's own symbols are never seen.
/// 2. A unit sub-symbol carries the same `symbol` tag as its parent, so the
///    search is scoped by slicing the parent's byte range and re-basing the
///    offsets rather than searching the whole document.
fn find_named_symbol(
    source: &str,
    range: (usize, usize),
    parent_tag: &str,
    name: &str,
) -> Option<((usize, usize), Vec<String>)> {
    let (base, end) = range;
    let slice = &source[base..end];
    let mut available = Vec::new();
    let mut found = None;
    for (start, stop) in find_direct_child_blocks(slice, parent_tag) {
        let Ok(node) = parse_sexp(&slice[start..stop]) else {
            continue;
        };
        if node.head() != Some("symbol") {
            continue;
        }
        let Some(child_name) = node.get(1).and_then(SexpNode::as_str) else {
            continue;
        };
        available.push(child_name.to_string());
        if child_name == name {
            found = Some((base + start, base + stop));
        }
    }
    found.map(|range| (range, available))
}

/// Leading indentation of the line `offset` sits on.
fn indent_at(source: &str, offset: usize) -> String {
    let line_start = source[..offset].rfind('\n').map_or(0, |i| i + 1);
    source[line_start..offset]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

fn escape_sexp_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render one `(text …)` block in KiCad 10's expanded style, at `indent`, with
/// `unit` as one nesting level and `nl` as the file's line ending.
fn serialize_text(item: &SymbolText, indent: &str, unit: &str, nl: &str) -> String {
    let i1 = format!("{indent}{unit}");
    let i2 = format!("{i1}{unit}");
    let i3 = format!("{i2}{unit}");
    let mut out = String::new();
    out.push_str(nl);
    out.push_str(indent);
    out.push_str(&format!("(text \"{}\"", escape_sexp_string(&item.text)));
    out.push_str(nl);
    out.push_str(&format!(
        "{i1}(at {} {} {})",
        fmt_f64(item.x),
        fmt_f64(item.y),
        fmt_f64(item.angle)
    ));
    out.push_str(nl);
    out.push_str(&format!("{i1}(effects"));
    out.push_str(nl);
    out.push_str(&format!("{i2}(font"));
    out.push_str(nl);
    out.push_str(&format!(
        "{i3}(size {} {})",
        fmt_f64(item.font_size),
        fmt_f64(item.font_size)
    ));
    if let Some(thickness) = item.font_thickness {
        out.push_str(nl);
        out.push_str(&format!("{i3}(thickness {})", fmt_f64(thickness)));
    }
    out.push_str(nl);
    out.push_str(&format!("{i2})"));
    if !item.justify.is_empty() {
        out.push_str(nl);
        out.push_str(&format!("{i2}(justify {})", item.justify.join(" ")));
    }
    if item.hide {
        out.push_str(nl);
        out.push_str(&format!("{i2}(hide yes)"));
    }
    out.push_str(nl);
    out.push_str(&format!("{i1})"));
    out.push_str(nl);
    out.push_str(indent);
    out.push(')');
    out
}

fn serialize_style(style: &StrokeFill, i1: &str, i2: &str, nl: &str, out: &mut String) {
    out.push_str(nl);
    out.push_str(&format!("{i1}(stroke"));
    out.push_str(nl);
    out.push_str(&format!("{i2}(width {})", fmt_f64(style.stroke_width_mm)));
    out.push_str(nl);
    out.push_str(&format!("{i2}(type {})", style.stroke_type));
    out.push_str(nl);
    out.push_str(&format!("{i1})"));
    out.push_str(nl);
    out.push_str(&format!("{i1}(fill"));
    out.push_str(nl);
    out.push_str(&format!("{i2}(type {})", style.fill));
    out.push_str(nl);
    out.push_str(&format!("{i1})"));
}

fn serialize_graphic(item: &SymbolGraphic, indent: &str, unit: &str, nl: &str) -> String {
    if let SymbolGraphic::Text(text) = item {
        return serialize_text(text, indent, unit, nl);
    }

    let i1 = format!("{indent}{unit}");
    let i2 = format!("{i1}{unit}");
    let mut out = String::new();
    out.push_str(nl);
    out.push_str(indent);
    match item {
        SymbolGraphic::Text(_) => unreachable!(),
        SymbolGraphic::Polyline(polyline) => {
            out.push_str("(polyline");
            out.push_str(nl);
            out.push_str(&format!("{i1}(pts"));
            for (x, y) in &polyline.points {
                out.push_str(nl);
                out.push_str(&format!("{i2}(xy {} {})", fmt_f64(*x), fmt_f64(*y)));
            }
            out.push_str(nl);
            out.push_str(&format!("{i1})"));
            serialize_style(&polyline.style, &i1, &i2, nl, &mut out);
        }
        SymbolGraphic::Rectangle(rectangle) => {
            out.push_str("(rectangle");
            out.push_str(nl);
            out.push_str(&format!(
                "{i1}(start {} {})",
                fmt_f64(rectangle.start.0),
                fmt_f64(rectangle.start.1)
            ));
            out.push_str(nl);
            out.push_str(&format!(
                "{i1}(end {} {})",
                fmt_f64(rectangle.end.0),
                fmt_f64(rectangle.end.1)
            ));
            serialize_style(&rectangle.style, &i1, &i2, nl, &mut out);
        }
        SymbolGraphic::Circle(circle) => {
            out.push_str("(circle");
            out.push_str(nl);
            out.push_str(&format!(
                "{i1}(center {} {})",
                fmt_f64(circle.center.0),
                fmt_f64(circle.center.1)
            ));
            out.push_str(nl);
            out.push_str(&format!("{i1}(radius {})", fmt_f64(circle.radius_mm)));
            serialize_style(&circle.style, &i1, &i2, nl, &mut out);
        }
        SymbolGraphic::Arc(arc) => {
            out.push_str("(arc");
            out.push_str(nl);
            out.push_str(&format!(
                "{i1}(start {} {})",
                fmt_f64(arc.start.0),
                fmt_f64(arc.start.1)
            ));
            out.push_str(nl);
            out.push_str(&format!(
                "{i1}(mid {} {})",
                fmt_f64(arc.mid.0),
                fmt_f64(arc.mid.1)
            ));
            out.push_str(nl);
            out.push_str(&format!(
                "{i1}(end {} {})",
                fmt_f64(arc.end.0),
                fmt_f64(arc.end.1)
            ));
            serialize_style(&arc.style, &i1, &i2, nl, &mut out);
        }
    }
    out.push_str(nl);
    out.push_str(indent);
    out.push(')');
    out
}

#[allow(clippy::too_many_arguments)]
fn prepare(
    source: &str,
    symbol_name: &str,
    unit_symbol: Option<&str>,
    selector: &Selector,
    mode: Mode,
    graphics: &[SymbolGraphic],
    allow_empty: bool,
) -> Result<Prepared, Error> {
    let root = parse_sexp(source)
        .map_err(|e| invalid("library_path", format!("invalid S-expression: {e}")))?;
    if root.head() != Some("kicad_symbol_lib") {
        return Err(invalid(
            "library_path",
            format!(
                "root must be kicad_symbol_lib, found {}",
                root.head().unwrap_or("nothing")
            ),
        ));
    }

    let root_range = (0, source.len());
    let (symbol_range, _) = find_named_symbol(source, root_range, "kicad_symbol_lib", symbol_name)
        .ok_or_else(|| {
            Error::NotFound(format!(
                "no symbol named '{symbol_name}' in this library; it defines: {}",
                summarize(&list_top_level(source))
            ))
        })?;

    let (target_range, target_label) = match unit_symbol {
        Some(unit) => {
            let (range, _) =
                find_named_symbol(source, symbol_range, "symbol", unit).ok_or_else(|| {
                    Error::NotFound(format!(
                        "symbol '{symbol_name}' has no unit '{unit}'; its units are: {}",
                        summarize(&available_units(source, symbol_range))
                    ))
                })?;
            (range, format!("{symbol_name} / {unit}"))
        }
        None => (symbol_range, symbol_name.to_string()),
    };

    // Direct children of the target, so a nested unit's graphics are never
    // touched when operating on the parent symbol.
    let (base, end) = target_range;
    let slice = &source[base..end];
    let mut children: Vec<(usize, usize, String)> = Vec::new();
    for (start, stop) in find_direct_child_blocks(slice, "symbol") {
        let block = &slice[start..stop];
        let Ok(node) = parse_sexp(block) else {
            return Err(invalid(
                "library_path",
                "symbol body contains an invalid item",
            ));
        };
        let Some(tag) = node.head() else { continue };
        children.push((base + start, base + stop, tag.to_string()));
    }

    let mut selected = Vec::new();
    for (start, stop, tag) in &children {
        if !is_graphic_tag(tag) {
            continue;
        }
        let Ok(node) = parse_sexp(&source[*start..*stop]) else {
            continue;
        };
        if selector.matches(&node, tag) {
            selected.push((*start, *stop, tag.clone()));
        }
    }

    let mut warnings = Vec::new();

    if matches!(mode, Mode::Replace | Mode::Delete) {
        if selected.is_empty() {
            if !allow_empty {
                return Err(Error::NotFound(format!(
                    "selector matched no graphics in {target_label}; pass allow_empty to treat \
                     this as a no-op"
                )));
            }
            warnings.push(format!("selector matched no graphics in {target_label}"));
        }
        // A text_box or bezier can carry structure this tool cannot rebuild, so
        // refuse rather than silently drop something the caller cannot restore.
        if let Some((_, _, tag)) = selected
            .iter()
            .find(|(_, _, tag)| matches!(tag.as_str(), "text_box" | "bezier"))
        {
            return Err(Error::Conflict(format!(
                "selector matched a '{tag}' graphic, which this tool cannot safely rewrite; \
                 narrow the selector"
            )));
        }
    }

    // Indentation: copy the first child's, so the edit matches the file rather
    // than a house style. Fall back to one level in from the target block.
    let target_indent = indent_at(source, base);
    let child_indent = children
        .first()
        .map(|(start, _, _)| indent_at(source, *start))
        .unwrap_or_else(|| format!("{target_indent}\t"));
    let unit_indent = child_indent
        .strip_prefix(target_indent.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("\t")
        .to_string();
    let nl = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };

    let serialized: String = graphics
        .iter()
        .map(|item| serialize_graphic(item, &child_indent, &unit_indent, nl))
        .collect();

    let matched = selected.len();
    let mut edits = Vec::new();
    let (added, deleted) = match mode {
        Mode::Append => {
            let anchor = children
                .last()
                .map(|(_, stop, _)| *stop)
                .unwrap_or(end.saturating_sub(1));
            edits.push(SexpEdit::insert(anchor, serialized));
            (graphics.len(), 0)
        }
        Mode::Replace => {
            if let Some(((first_start, first_stop, _), rest)) = selected.split_first() {
                edits.push(SexpEdit::replace(
                    with_leading_whitespace(source, *first_start),
                    *first_stop,
                    serialized,
                ));
                for (start, stop, _) in rest {
                    edits.push(SexpEdit::delete(
                        with_leading_whitespace(source, *start),
                        *stop,
                    ));
                }
            } else {
                let anchor = children
                    .last()
                    .map(|(_, stop, _)| *stop)
                    .unwrap_or(end.saturating_sub(1));
                edits.push(SexpEdit::insert(anchor, serialized));
            }
            (graphics.len(), matched.saturating_sub(1).min(matched))
        }
        Mode::Delete => {
            for (start, stop, _) in &selected {
                edits.push(SexpEdit::delete(
                    with_leading_whitespace(source, *start),
                    *stop,
                ));
            }
            (0, matched)
        }
    };

    let replacement = apply_edits(source.to_string(), edits);

    // Never hand a broken library to the writer: reparse before it reaches disk.
    let reparsed = parse_sexp(&replacement)
        .map_err(|e| Error::Conflict(format!("edit would produce an unparsable library: {e}")))?;
    if reparsed.head() != Some("kicad_symbol_lib") {
        return Err(Error::Conflict(
            "edit would change the library root element".to_string(),
        ));
    }

    Ok(Prepared {
        replacement,
        matched,
        added,
        deleted,
        warnings,
    })
}

/// Start offset of the block at `start`, extended back over the indentation and
/// newline in front of it, so deleting leaves no blank indented line.
fn with_leading_whitespace(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut at = start;
    while at > 0 && (bytes[at - 1] == b' ' || bytes[at - 1] == b'\t') {
        at -= 1;
    }
    if at > 0 && bytes[at - 1] == b'\n' {
        at -= 1;
        if at > 0 && bytes[at - 1] == b'\r' {
            at -= 1;
        }
    }
    at
}

fn list_top_level(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    for (start, stop) in find_direct_child_blocks(source, "kicad_symbol_lib") {
        if let Ok(node) = parse_sexp(&source[start..stop]) {
            if node.head() == Some("symbol") {
                if let Some(name) = node.get(1).and_then(SexpNode::as_str) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

fn available_units(source: &str, symbol_range: (usize, usize)) -> Vec<String> {
    let (base, end) = symbol_range;
    let slice = &source[base..end];
    let mut names = Vec::new();
    for (start, stop) in find_direct_child_blocks(slice, "symbol") {
        if let Ok(node) = parse_sexp(&slice[start..stop]) {
            if node.head() == Some("symbol") {
                if let Some(name) = node.get(1).and_then(SexpNode::as_str) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

fn summarize(names: &[String]) -> String {
    if names.is_empty() {
        return "none".to_string();
    }
    let shown: Vec<&str> = names.iter().take(12).map(String::as_str).collect();
    if names.len() > shown.len() {
        format!("{} … ({} total)", shown.join(", "), names.len())
    } else {
        shown.join(", ")
    }
}

// ─── Tool definitions ────────────────────────────────────────────────────────

fn graphic_item_schema() -> serde_json::Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "description": "KiCad symbol graphical text. Coordinates and sizes are in millimeters.",
                "properties": {
                    "kind": { "type": "string", "enum": ["text"], "default": "text" },
                    "text": { "type": "string", "description": "The literal to display" },
                    "x": { "type": "number", "description": "X position in mm", "default": 0 },
                    "y": { "type": "number", "description": "Y position in mm", "default": 0 },
                    "angle": { "type": "number", "description": "Rotation in degrees", "default": 0 },
                    "font_size": { "type": "number", "exclusiveMinimum": 0, "default": 1.27 },
                    "font_thickness": { "type": "number", "exclusiveMinimum": 0, "description": "Stroke thickness in mm; omitted if not given" },
                    "justify": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["left", "right", "center", "top", "bottom", "mirror"] }
                    },
                    "effects": {
                        "type": "object",
                        "properties": { "hide": { "type": "boolean", "default": false } },
                        "additionalProperties": false
                    }
                },
                "required": ["text"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "description": "KiCad symbol polyline/line primitive. Coordinates are in millimeters.",
                "properties": {
                    "kind": { "type": "string", "enum": ["line", "polyline"] },
                    "points": {
                        "type": "array",
                        "description": "Polyline vertices as {x,y} objects or [x,y] arrays; at least two points."
                    },
                    "start": { "description": "Start point for a two-point line, as {x,y} or [x,y]." },
                    "end": { "description": "End point for a two-point line, as {x,y} or [x,y]." },
                    "stroke_width_mm": { "type": "number", "minimum": 0, "default": 0.254 },
                    "stroke_type": { "type": "string", "enum": ["default", "solid", "dash", "dot", "dash_dot", "dash_dot_dot"], "default": "default" },
                    "fill": { "type": "string", "enum": ["none", "background", "outline"], "default": "none" }
                },
                "required": ["kind"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "description": "KiCad symbol rectangle primitive. Coordinates are in millimeters.",
                "properties": {
                    "kind": { "type": "string", "enum": ["rectangle"] },
                    "start": { "description": "First corner as {x,y} or [x,y]." },
                    "end": { "description": "Opposite corner as {x,y} or [x,y]." },
                    "stroke_width_mm": { "type": "number", "minimum": 0, "default": 0.254 },
                    "stroke_type": { "type": "string", "enum": ["default", "solid", "dash", "dot", "dash_dot", "dash_dot_dot"], "default": "default" },
                    "fill": { "type": "string", "enum": ["none", "background", "outline"], "default": "none" }
                },
                "required": ["kind", "start", "end"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "description": "KiCad symbol circle primitive. Coordinates and radius are in millimeters.",
                "properties": {
                    "kind": { "type": "string", "enum": ["circle"] },
                    "center": { "description": "Center point as {x,y} or [x,y]." },
                    "radius_mm": { "type": "number", "exclusiveMinimum": 0 },
                    "stroke_width_mm": { "type": "number", "minimum": 0, "default": 0.254 },
                    "stroke_type": { "type": "string", "enum": ["default", "solid", "dash", "dot", "dash_dot", "dash_dot_dot"], "default": "default" },
                    "fill": { "type": "string", "enum": ["none", "background", "outline"], "default": "none" }
                },
                "required": ["kind", "center", "radius_mm"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "description": "KiCad symbol arc primitive. Coordinates are in millimeters.",
                "properties": {
                    "kind": { "type": "string", "enum": ["arc"] },
                    "start": { "description": "Arc start point as {x,y} or [x,y]." },
                    "mid": { "description": "Arc midpoint as {x,y} or [x,y]." },
                    "end": { "description": "Arc end point as {x,y} or [x,y]." },
                    "stroke_width_mm": { "type": "number", "minimum": 0, "default": 0.254 },
                    "stroke_type": { "type": "string", "enum": ["default", "solid", "dash", "dot", "dash_dot", "dash_dot_dot"], "default": "default" },
                    "fill": { "type": "string", "enum": ["none", "background", "outline"], "default": "none" }
                },
                "required": ["kind", "start", "mid", "end"],
                "additionalProperties": false
            }
        ]
    })
}

pub(super) fn set_symbol_graphics_tool() -> ToolDef {
    tool!(
        "set_symbol_graphics",
        "Append, replace or delete graphical primitives inside an existing .kicad_sym symbol or one of \
         its unit sub-symbols (e.g. 'Core3576_2_1'). Edits are byte-range operations on the \
         parsed S-expression, so pins, pin names and numbers, electrical types, unit membership, \
         properties and untouched graphics are preserved structurally rather than re-serialized. \
         Writable primitives are text, line/polyline, rectangle, circle and arc. Coordinates are \
         in millimeters. Omit unit_symbol to operate on the top-level symbol. A replace or \
         delete that matches nothing is a non-mutating error unless allow_empty is set.",
        json!({
            "type": "object",
            "properties": {
                "library_path": {
                    "type": "string",
                    "description": "Absolute path to the .kicad_sym library"
                },
                "symbol_name": {
                    "type": "string",
                    "description": "Top-level symbol name, e.g. 'Core3576'"
                },
                "unit_symbol": {
                    "type": "string",
                    "description": "Optional unit sub-symbol, e.g. 'Core3576_2_1'. Omit to target the top-level symbol."
                },
                "selector": {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["text", "rectangle", "line", "circle", "arc", "any"],
                            "default": "any"
                        },
                        "text": { "type": "string", "description": "Exact literal match, for text items" },
                        "uuid": { "type": "string", "description": "Match an item by uuid, where one is present" }
                    },
                    "additionalProperties": false
                },
                "mode": { "type": "string", "enum": ["append", "replace", "delete"] },
                "graphics": {
                    "type": "array",
                    "items": graphic_item_schema(),
                    "description": "Graphics to write. Required for append and replace; must be omitted or empty for delete. Omitted fields preserve nothing; each supplied primitive is serialized as a complete KiCad symbol graphic."
                },
                "allow_empty": {
                    "type": "boolean",
                    "default": false,
                    "description": "Treat a zero-match replace/delete as a no-op success instead of an error"
                }
            },
            "required": ["library_path", "symbol_name", "mode"],
            "additionalProperties": false
        }),
        |args, ctx| async move { handle_set_symbol_graphics(args, ctx).await }
    )
}

pub(super) fn add_symbol_text_tool() -> ToolDef {
    tool!(
        "add_symbol_text",
        "Add one line of graphical text to an existing .kicad_sym symbol or unit sub-symbol — the \
         common case of set_symbol_graphics(mode='append'), without the selector and graphics \
         array. Use it to caption a unit, e.g. 'Unit A — Display / Video' on 'Core3576_1_1'. \
         Preserves pins, properties and existing graphics exactly as set_symbol_graphics does.",
        json!({
            "type": "object",
            "properties": {
                "library_path": { "type": "string", "description": "Absolute path to the .kicad_sym library" },
                "symbol_name": { "type": "string", "description": "Top-level symbol name, e.g. 'Core3576'" },
                "unit_symbol": { "type": "string", "description": "Optional unit sub-symbol, e.g. 'Core3576_1_1'" },
                "text": { "type": "string", "description": "The literal to display" },
                "x": { "type": "number", "default": 0 },
                "y": { "type": "number", "default": 0 },
                "angle": { "type": "number", "default": 0 },
                "font_size": { "type": "number", "exclusiveMinimum": 0, "default": 1.27 },
                "font_thickness": { "type": "number", "exclusiveMinimum": 0 },
                "justify": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["left", "right", "center", "top", "bottom", "mirror"] }
                }
            },
            "required": ["library_path", "symbol_name", "text"],
            "additionalProperties": false
        }),
        |args, ctx| async move { handle_add_symbol_text(args, ctx).await }
    )
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn handle_add_symbol_text(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    // Rebuild the general call, so there is exactly one implementation of the
    // edit and the convenience form cannot drift from it.
    let mut item = serde_json::Map::new();
    item.insert("kind".into(), json!("text"));
    for field in [
        "text",
        "x",
        "y",
        "angle",
        "font_size",
        "font_thickness",
        "justify",
    ] {
        if let Some(value) = args.get(field) {
            if !value.is_null() {
                item.insert(field.to_string(), value.clone());
            }
        }
    }
    let mut forwarded = serde_json::Map::new();
    for field in ["library_path", "symbol_name", "unit_symbol"] {
        if let Some(value) = args.get(field) {
            if !value.is_null() {
                forwarded.insert(field.to_string(), value.clone());
            }
        }
    }
    forwarded.insert("mode".into(), json!("append"));
    forwarded.insert("graphics".into(), json!([serde_json::Value::Object(item)]));

    handle_set_symbol_graphics(&serde_json::Value::Object(forwarded), ctx).await
}

async fn handle_set_symbol_graphics(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let Some(path) = args["library_path"].as_str().map(PathBuf::from) else {
        return Ok(invalid("library_path", "missing or not a string").into_result(Path::new("")));
    };
    if path.extension().and_then(|e| e.to_str()) != Some("kicad_sym") {
        return Ok(invalid("library_path", "must end in .kicad_sym").into_result(&path));
    }
    if is_installed_kicad_symbol_library(&path) {
        return Ok(invalid(
            "library_path",
            "refuses to modify a KiCad installed/system symbol library; copy the symbol into a project-local library first",
        )
        .into_result(&path));
    }
    if std::fs::metadata(&path)
        .map(|metadata| metadata.permissions().readonly())
        .unwrap_or(false)
    {
        return Ok(invalid("library_path", "symbol library is read-only").into_result(&path));
    }
    let Some(symbol_name) = args["symbol_name"].as_str() else {
        return Ok(invalid("symbol_name", "missing or not a string").into_result(&path));
    };
    let unit_symbol = args["unit_symbol"].as_str();

    let mode = match Mode::parse(&args["mode"]) {
        Ok(mode) => mode,
        Err(error) => return Ok(error.into_result(&path)),
    };
    let selector = match Selector::parse(args.get("selector")) {
        Ok(selector) => selector,
        Err(error) => return Ok(error.into_result(&path)),
    };
    let allow_empty = match args.get("allow_empty") {
        None | Some(serde_json::Value::Null) => false,
        Some(value) => match value.as_bool() {
            Some(value) => value,
            None => {
                return Ok(invalid("allow_empty", "must be a boolean").into_result(&path));
            }
        },
    };

    let graphics = match parse_graphics(args, mode) {
        Ok(graphics) => graphics,
        Err(error) => return Ok(error.into_result(&path)),
    };

    let source = match read_consistent(&path) {
        Ok(source) => source,
        Err(konnect_sexp::SexpError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CallToolResult::error_kind(
                ToolErrorKind::FileNotFound {
                    path: path.display().to_string(),
                },
                format!("Symbol library not found: {}", path.display()),
            ));
        }
        Err(error) => return Err(error.into()),
    };

    let pins_before = pin_fingerprint(&source, symbol_name);

    let prepared = match prepare(
        &source,
        symbol_name,
        unit_symbol,
        &selector,
        mode,
        &graphics,
        allow_empty,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return Ok(error.into_result(&path)),
    };

    if let (Some(before), Some(after)) = (
        pins_before.as_ref(),
        pin_fingerprint(&prepared.replacement, symbol_name).as_ref(),
    ) {
        if before != after {
            return Ok(Error::Conflict(
                "edit would change symbol pins; graphics edits may not alter pin number, name, type, style, position, orientation or length".to_string(),
            )
            .into_result(&path));
        }
    }

    let changed = prepared.replacement != source;
    if changed {
        if let Err(error) = write_atomic_if_unchanged(&path, &source, &prepared.replacement) {
            return match error {
                konnect_sexp::SexpError::Conflict { .. } => Ok(Error::Conflict(
                    "library changed after it was read".to_string(),
                )
                .into_result(&path)),
                other => Err(other.into()),
            };
        }
    }

    // Re-read from disk and reparse: proves what actually landed is valid,
    // not merely what we intended to write.
    let mut warnings = prepared.warnings;
    let pins_after = match read_consistent(&path) {
        Ok(after) => match parse_sexp(&after) {
            Ok(_) => pin_fingerprint(&after, symbol_name),
            Err(e) => {
                warnings.push(format!("library did not reparse after write: {e}"));
                None
            }
        },
        Err(e) => {
            warnings.push(format!("library could not be re-read after write: {e}"));
            None
        }
    };
    if let (Some(before), Some(after)) = (pins_before.as_ref(), pins_after.as_ref()) {
        if before != after {
            warnings.push(format!(
                "pin fingerprint changed during write/reload — this should not happen; \
                 inspect the symbol"
            ));
        }
    }

    Ok(CallToolResult::json(&json!({
        "success": true,
        "file_changed": changed,
        "library_path": path.display().to_string(),
        "symbol_name": symbol_name,
        "unit_symbol": unit_symbol,
        "mode": mode.as_str(),
        "matched": prepared.matched,
        "added": prepared.added,
        "deleted": prepared.deleted,
        "pin_count_before": pins_before.as_ref().map(Vec::len),
        "pin_count_after": pins_after.as_ref().map(Vec::len),
        "warnings": warnings
    })))
}

fn is_installed_kicad_symbol_library(path: &Path) -> bool {
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    crate::tools::find_kicad_library_dirs("symbols")
        .into_iter()
        .filter_map(|dir| dir.canonicalize().ok())
        .any(|dir| path.starts_with(dir))
}

fn parse_graphics(args: &serde_json::Value, mode: Mode) -> Result<Vec<SymbolGraphic>, Error> {
    match mode {
        Mode::Delete => match args.get("graphics") {
            None | Some(serde_json::Value::Null) => Ok(Vec::new()),
            Some(value) if value.as_array().is_some_and(Vec::is_empty) => Ok(Vec::new()),
            Some(_) => Err(invalid(
                "graphics",
                "must be omitted or empty in delete mode",
            )),
        },
        Mode::Append | Mode::Replace => {
            let Some(items) = args.get("graphics").and_then(|v| v.as_array()) else {
                return Err(invalid(
                    "graphics",
                    "is required for append and replace modes",
                ));
            };
            if items.is_empty() {
                return Err(invalid("graphics", "must contain at least one item"));
            }
            items
                .iter()
                .enumerate()
                .map(|(index, item)| SymbolGraphic::parse(item, index))
                .collect()
        }
    }
}

/// Pins beneath the named top-level symbol, or `None` if it cannot be read.
fn pin_fingerprint(source: &str, symbol_name: &str) -> Option<Vec<String>> {
    let root = parse_sexp(source).ok()?;
    let symbol = root
        .find_all("symbol")
        .into_iter()
        .find(|s| s.get(1).and_then(SexpNode::as_str) == Some(symbol_name))?;
    let mut pins = Vec::new();
    fn walk(node: &SexpNode, unit: &str, out: &mut Vec<String>) {
        for child in node.children().unwrap_or(&[]) {
            match child.head() {
                Some("symbol") => {
                    let name = child.get(1).and_then(SexpNode::as_str).unwrap_or(unit);
                    walk(child, name, out);
                }
                Some("pin") => {
                    let pin_type = child.get(1).and_then(SexpNode::as_str).unwrap_or("");
                    let pin_style = child.get(2).and_then(SexpNode::as_str).unwrap_or("");
                    let name = child.find_str("name").unwrap_or("");
                    let number = child.find_str("number").unwrap_or("");
                    let (x, y, angle) = child
                        .find("at")
                        .map(|at| {
                            (
                                at.get_f64(1).unwrap_or(0.0),
                                at.get_f64(2).unwrap_or(0.0),
                                at.get_f64(3).unwrap_or(0.0),
                            )
                        })
                        .unwrap_or((0.0, 0.0, 0.0));
                    let length = child
                        .find("length")
                        .and_then(|length| length.get_f64(1))
                        .unwrap_or(0.0);
                    out.push(format!(
                        "{unit}|{number}|{name}|{pin_type}|{pin_style}|{}|{}|{}|{}",
                        fmt_f64(x),
                        fmt_f64(y),
                        fmt_f64(angle),
                        fmt_f64(length)
                    ));
                }
                _ => walk(child, unit, out),
            }
        }
    }
    walk(symbol, symbol_name, &mut pins);
    pins.sort();
    Some(pins)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A five-unit symbol in KiCad 10's own shape: tab indentation, expanded
    /// tokens, unit sub-symbols one level below the parent, pins carrying
    /// numbers, names and electrical types. Built to match the real library
    /// format rather than this crate's compact writer, because the files these
    /// tools edit come from KiCad.
    fn core3576() -> String {
        let mut s = String::from(
            "(kicad_symbol_lib\n\t(version 20251024)\n\t(generator \"kicad_symbol_editor\")\n\
             \t(symbol \"Core3576\"\n\
             \t\t(exclude_from_sim no)\n\
             \t\t(in_bom yes)\n\
             \t\t(on_board yes)\n\
             \t\t(property \"Reference\" \"U\"\n\t\t\t(at 0 20 0)\n\t\t\t(effects\n\t\t\t\t(font\n\t\t\t\t\t(size 1.27 1.27)\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\
             \t\t(property \"Value\" \"Core3576\"\n\t\t\t(at 0 18 0)\n\t\t\t(effects\n\t\t\t\t(font\n\t\t\t\t\t(size 1.27 1.27)\n\t\t\t\t)\n\t\t\t)\n\t\t)\n",
        );
        let types = ["input", "output", "bidirectional", "passive", "power_in"];
        for unit in 1..=5u32 {
            s.push_str(&format!("\t\t(symbol \"Core3576_{unit}_1\"\n"));
            s.push_str(
                "\t\t\t(rectangle\n\t\t\t\t(start -10 10)\n\t\t\t\t(end 10 -10)\n\
                 \t\t\t\t(stroke\n\t\t\t\t\t(width 0.254)\n\t\t\t\t\t(type default)\n\t\t\t\t)\n\
                 \t\t\t\t(fill\n\t\t\t\t\t(type background)\n\t\t\t\t)\n\t\t\t)\n",
            );
            for pin in 1..=3u32 {
                let number = (unit - 1) * 3 + pin;
                s.push_str(&format!(
                    "\t\t\t(pin {} line\n\t\t\t\t(at -12.7 {} 0)\n\t\t\t\t(length 2.54)\n\
                     \t\t\t\t(name \"SIG{}\"\n\t\t\t\t\t(effects\n\t\t\t\t\t\t(font\n\t\t\t\t\t\t\t(size 1.27 1.27)\n\t\t\t\t\t\t)\n\t\t\t\t\t)\n\t\t\t\t)\n\
                     \t\t\t\t(number \"{}\"\n\t\t\t\t\t(effects\n\t\t\t\t\t\t(font\n\t\t\t\t\t\t\t(size 1.27 1.27)\n\t\t\t\t\t\t)\n\t\t\t\t\t)\n\t\t\t\t)\n\t\t\t)\n",
                    types[(unit - 1) as usize],
                    5.08 - (pin as f64) * 2.54,
                    number,
                    number
                ));
            }
            s.push_str("\t\t)\n");
        }
        s.push_str("\t\t(embedded_fonts no)\n\t)\n)\n");
        s
    }

    /// Every (pin …) as (number, name, electrical type, owning unit) — the
    /// tuple the acceptance criteria say must survive untouched.
    fn pin_fingerprint(source: &str) -> Vec<(String, String, String, String)> {
        let root = parse_sexp(source).expect("library parses");
        let mut out = Vec::new();
        fn walk(node: &SexpNode, unit: &str, out: &mut Vec<(String, String, String, String)>) {
            for child in node.children().unwrap_or(&[]) {
                match child.head() {
                    Some("symbol") => {
                        let name = child.get(1).and_then(SexpNode::as_str).unwrap_or("");
                        walk(child, name, out);
                    }
                    Some("pin") => {
                        let etype = child
                            .get(1)
                            .and_then(SexpNode::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = child
                            .find("name")
                            .and_then(|n| n.get(1))
                            .and_then(SexpNode::as_str)
                            .unwrap_or("")
                            .to_string();
                        let number = child
                            .find("number")
                            .and_then(|n| n.get(1))
                            .and_then(SexpNode::as_str)
                            .unwrap_or("")
                            .to_string();
                        out.push((number, name, etype, unit.to_string()));
                    }
                    _ => walk(child, unit, out),
                }
            }
        }
        walk(&root, "", &mut out);
        out.sort();
        out
    }

    fn texts_in_unit(source: &str, unit: &str) -> Vec<String> {
        let root = parse_sexp(source).expect("library parses");
        fn find<'a>(node: &'a SexpNode, unit: &str) -> Option<&'a SexpNode> {
            for child in node.children().unwrap_or(&[]) {
                if child.head() == Some("symbol")
                    && child.get(1).and_then(SexpNode::as_str) == Some(unit)
                {
                    return Some(child);
                }
                if let Some(found) = find(child, unit) {
                    return Some(found);
                }
            }
            None
        }
        find(&root, unit)
            .map(|node| {
                node.children()
                    .unwrap_or(&[])
                    .iter()
                    .filter(|c| c.head() == Some("text"))
                    .filter_map(|c| c.get(1).and_then(SexpNode::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Count `(tag …)` nodes at any depth.
    ///
    /// `SexpNode::find_all` is direct-children only — which is deliberate
    /// elsewhere (it is how `top_level_symbol_names` excludes nested units) but
    /// wrong for counting graphics that live inside unit sub-symbols.
    fn count_tag(source: &str, tag: &str) -> usize {
        fn walk(node: &SexpNode, tag: &str, count: &mut usize) {
            for child in node.children().unwrap_or(&[]) {
                if child.head() == Some(tag) {
                    *count += 1;
                }
                walk(child, tag, count);
            }
        }
        let root = parse_sexp(source).expect("parses");
        let mut count = 0;
        walk(&root, tag, &mut count);
        count
    }

    fn text_item(text: &str, y: f64) -> SymbolGraphic {
        SymbolGraphic::Text(SymbolText {
            text: text.to_string(),
            x: 0.0,
            y,
            angle: 0.0,
            font_size: 1.27,
            font_thickness: Some(0.15),
            justify: vec!["center".to_string()],
            hide: false,
        })
    }

    fn append(source: &str, unit: Option<&str>, text: &str) -> String {
        prepare(
            source,
            "Core3576",
            unit,
            &Selector {
                kind: SelectorKind::Any,
                text: None,
                uuid: None,
            },
            Mode::Append,
            &[text_item(text, -35.0)],
            false,
        )
        .expect("append succeeds")
        .replacement
    }

    fn style(fill: &str) -> StrokeFill {
        StrokeFill {
            stroke_width_mm: 0.254,
            stroke_type: "default".to_string(),
            fill: fill.to_string(),
        }
    }

    fn primitive_set() -> Vec<SymbolGraphic> {
        vec![
            SymbolGraphic::Polyline(SymbolPolyline {
                points: vec![(-5.08, 0.0), (0.0, 5.08), (5.08, 0.0)],
                style: style("none"),
            }),
            SymbolGraphic::Rectangle(SymbolRectangle {
                start: (-6.35, 6.35),
                end: (6.35, -6.35),
                style: style("background"),
            }),
            SymbolGraphic::Circle(SymbolCircle {
                center: (0.0, 0.0),
                radius_mm: 1.27,
                style: style("none"),
            }),
            SymbolGraphic::Arc(SymbolArc {
                start: (-2.54, -2.54),
                mid: (0.0, -3.81),
                end: (2.54, -2.54),
                style: style("none"),
            }),
        ]
    }

    /// The acceptance scenario: caption all five units, then prove nothing else
    /// moved. This is the whole point of the tool — a caption must not cost you
    /// a pin.
    #[test]
    fn captions_five_units_without_disturbing_pins() {
        let original = core3576();
        let before = pin_fingerprint(&original);
        assert_eq!(before.len(), 15, "fixture should have 15 pins");

        let captions = [
            ("Core3576_1_1", "Unit A — Display / Video"),
            ("Core3576_2_1", "Unit B — Camera / High-Speed Interfaces"),
            ("Core3576_3_1", "Unit C — System / Storage / USB"),
            ("Core3576_4_1", "Unit D — GPIO / Peripheral I/O / Ethernet"),
            ("Core3576_5_1", "Unit E — Power / GND"),
        ];

        let mut current = original.clone();
        for (unit, caption) in captions {
            current = append(&current, Some(unit), caption);
        }

        // Pins: numbers, names, electrical types and unit membership all intact.
        assert_eq!(
            pin_fingerprint(&current),
            before,
            "pins must be untouched by a graphics edit"
        );

        // Exactly one new text in each intended unit, and nowhere else.
        for (unit, caption) in captions {
            assert_eq!(
                texts_in_unit(&current, unit),
                vec![caption.to_string()],
                "unit {unit} should carry exactly its own caption"
            );
        }
        assert_eq!(
            count_tag(&current, "text"),
            5,
            "exactly five text primitives should exist across the whole library"
        );

        // The library still parses, and every unit's rectangle is still there.
        let reparsed = parse_sexp(&current).expect("edited library must parse");
        assert_eq!(reparsed.head(), Some("kicad_symbol_lib"));
        assert_eq!(count_tag(&current, "rectangle"), 5);
        assert!(current.contains("(property \"Value\" \"Core3576\""));
    }

    /// The emitted text must match KiCad 10's own shape, verified against
    /// libraries shipped with KiCad (`(version 20251024)`). In particular there
    /// is no uuid on a symbol graphic — inventing one would be a format guess.
    #[test]
    fn emitted_text_matches_the_kicad_format() {
        let out = append(&core3576(), Some("Core3576_1_1"), "Unit A");
        let block = {
            let start = out.find("(text \"Unit A\"").expect("text written");
            let end = out[start..].find("\n\t\t\t)").expect("block closes") + start + 5;
            &out[start..end]
        };
        assert!(block.contains("(at 0 -35 0)"), "got: {block}");
        assert!(block.contains("(size 1.27 1.27)"), "got: {block}");
        assert!(block.contains("(thickness 0.15)"), "got: {block}");
        assert!(block.contains("(justify center)"), "got: {block}");
        assert!(
            !block.contains("uuid"),
            "symbol graphics carry no uuid in .kicad_sym: {block}"
        );
        // Tab indentation is inherited from the file, not imposed.
        assert!(
            out.contains("\n\t\t\t(text \"Unit A\""),
            "indent should match the unit's children"
        );
    }

    #[test]
    fn omitting_the_unit_targets_the_top_level_symbol() {
        let out = append(&core3576(), None, "Top level caption");
        // Present at the parent, absent from every unit.
        assert!(out.contains("(text \"Top level caption\""));
        for unit in 1..=5 {
            assert!(
                texts_in_unit(&out, &format!("Core3576_{unit}_1")).is_empty(),
                "unit {unit} must not receive the parent's text"
            );
        }
    }

    #[test]
    fn replace_swaps_only_the_selected_text() {
        let staged = append(&core3576(), Some("Core3576_1_1"), "old caption");
        let staged = {
            // A second graphic that must survive the replace.
            prepare(
                &staged,
                "Core3576",
                Some("Core3576_1_1"),
                &Selector {
                    kind: SelectorKind::Any,
                    text: None,
                    uuid: None,
                },
                Mode::Append,
                &[text_item("keep me", -40.0)],
                false,
            )
            .unwrap()
            .replacement
        };

        let out = prepare(
            &staged,
            "Core3576",
            Some("Core3576_1_1"),
            &Selector {
                kind: SelectorKind::Text,
                text: Some("old caption".to_string()),
                uuid: None,
            },
            Mode::Replace,
            &[text_item("new caption", -35.0)],
            false,
        )
        .expect("replace succeeds");

        assert_eq!(out.matched, 1);
        let texts = texts_in_unit(&out.replacement, "Core3576_1_1");
        assert!(texts.contains(&"new caption".to_string()));
        assert!(
            texts.contains(&"keep me".to_string()),
            "unselected text must survive"
        );
        assert!(!texts.contains(&"old caption".to_string()));
        assert_eq!(
            pin_fingerprint(&out.replacement),
            pin_fingerprint(&core3576())
        );
    }

    #[test]
    fn delete_removes_only_the_selected_text() {
        let staged = append(&core3576(), Some("Core3576_2_1"), "remove me");
        let staged = prepare(
            &staged,
            "Core3576",
            Some("Core3576_2_1"),
            &Selector {
                kind: SelectorKind::Any,
                text: None,
                uuid: None,
            },
            Mode::Append,
            &[text_item("keep me", -40.0)],
            false,
        )
        .unwrap()
        .replacement;

        let out = prepare(
            &staged,
            "Core3576",
            Some("Core3576_2_1"),
            &Selector {
                kind: SelectorKind::Text,
                text: Some("remove me".to_string()),
                uuid: None,
            },
            Mode::Delete,
            &[],
            false,
        )
        .expect("delete succeeds");

        assert_eq!(out.deleted, 1);
        assert_eq!(
            texts_in_unit(&out.replacement, "Core3576_2_1"),
            vec!["keep me".to_string()]
        );
        // The rectangle in that unit is a graphic too — a text selector must
        // not have touched it.
        assert_eq!(count_tag(&out.replacement, "rectangle"), 5);
        assert_eq!(
            pin_fingerprint(&out.replacement),
            pin_fingerprint(&core3576())
        );
    }

    /// An `any` selector must never reach a pin or a nested unit symbol, or a
    /// careless delete would strip the symbol.
    #[test]
    fn any_selector_never_matches_pins_or_units() {
        for tag in ["pin", "property", "symbol", "embedded_fonts"] {
            assert!(!is_graphic_tag(tag), "{tag} must not be selectable");
        }
        for tag in ["text", "rectangle", "polyline", "circle", "arc"] {
            assert!(is_graphic_tag(tag), "{tag} should be selectable");
        }

        // Deleting "any" in a unit removes its rectangle but leaves all pins.
        let out = prepare(
            &core3576(),
            "Core3576",
            Some("Core3576_3_1"),
            &Selector {
                kind: SelectorKind::Any,
                text: None,
                uuid: None,
            },
            Mode::Delete,
            &[],
            false,
        )
        .expect("delete succeeds");
        assert_eq!(out.deleted, 1, "only the rectangle matches");
        assert_eq!(
            pin_fingerprint(&out.replacement),
            pin_fingerprint(&core3576())
        );
    }

    #[test]
    fn append_writes_all_supported_geometry_without_disturbing_pins() {
        let original = core3576();
        let out = prepare(
            &original,
            "Core3576",
            Some("Core3576_1_1"),
            &Selector {
                kind: SelectorKind::Any,
                text: None,
                uuid: None,
            },
            Mode::Append,
            &primitive_set(),
            false,
        )
        .expect("append succeeds")
        .replacement;

        assert!(out.contains("(polyline"), "{out}");
        assert!(out.contains("(rectangle"), "{out}");
        assert!(out.contains("(circle"), "{out}");
        assert!(out.contains("(arc"), "{out}");
        assert!(out.contains("(radius 1.27)"), "{out}");
        assert_eq!(
            super::pin_fingerprint(&out, "Core3576"),
            super::pin_fingerprint(&original, "Core3576")
        );
        parse_sexp(&out).expect("edited library parses");
    }

    #[test]
    fn replace_can_swap_a_rectangle_for_line_circle_and_arc_primitives() {
        let original = core3576();
        let out = prepare(
            &original,
            "Core3576",
            Some("Core3576_1_1"),
            &Selector {
                kind: SelectorKind::Rectangle,
                text: None,
                uuid: None,
            },
            Mode::Replace,
            &primitive_set(),
            false,
        )
        .expect("replace succeeds");

        assert_eq!(out.matched, 1);
        assert_eq!(count_tag(&out.replacement, "polyline"), 1);
        assert_eq!(count_tag(&out.replacement, "circle"), 1);
        assert_eq!(count_tag(&out.replacement, "arc"), 1);
        assert_eq!(
            super::pin_fingerprint(&out.replacement, "Core3576"),
            super::pin_fingerprint(&original, "Core3576")
        );
    }

    #[test]
    fn json_parsing_accepts_line_rectangle_circle_and_arc() {
        let parsed = parse_graphics(
            &json!({
                "graphics": [
                    { "kind": "line", "start": [0, 0], "end": [2.54, 0], "stroke_width_mm": 0.15 },
                    { "kind": "rectangle", "start": {"x": -1.27, "y": 1.27}, "end": {"x": 1.27, "y": -1.27}, "fill": "background" },
                    { "kind": "circle", "center": [0, 0], "radius_mm": 1.27 },
                    { "kind": "arc", "start": [-1, 0], "mid": [0, 1], "end": [1, 0] }
                ]
            }),
            Mode::Append,
        )
        .expect("graphics parse");
        assert_eq!(parsed.len(), 4);
    }

    #[test]
    fn zero_match_replace_is_a_non_mutating_error_unless_allowed() {
        let source = core3576();
        let selector = Selector {
            kind: SelectorKind::Text,
            text: Some("nothing here".to_string()),
            uuid: None,
        };
        let err = prepare(
            &source,
            "Core3576",
            Some("Core3576_1_1"),
            &selector,
            Mode::Delete,
            &[],
            false,
        )
        .expect_err("zero matches must be refused");
        assert!(matches!(err, Error::NotFound(_)));

        let ok = prepare(
            &source,
            "Core3576",
            Some("Core3576_1_1"),
            &selector,
            Mode::Delete,
            &[],
            true,
        )
        .expect("allow_empty downgrades it to a no-op");
        assert_eq!(ok.deleted, 0);
        assert_eq!(ok.replacement, source, "a no-op must not change a byte");
        assert!(!ok.warnings.is_empty(), "the no-op should be reported");
    }

    #[test]
    fn unknown_symbol_and_unit_are_named_helpfully() {
        let source = core3576();
        let err = prepare(
            &source,
            "Nope",
            None,
            &Selector {
                kind: SelectorKind::Any,
                text: None,
                uuid: None,
            },
            Mode::Append,
            &[text_item("x", 0.0)],
            false,
        )
        .expect_err("unknown symbol must be refused");
        match err {
            Error::NotFound(reason) => assert!(reason.contains("Core3576"), "{reason}"),
            other => panic!("expected NotFound, got {other:?}"),
        }

        let err = prepare(
            &source,
            "Core3576",
            Some("Core3576_9_1"),
            &Selector {
                kind: SelectorKind::Any,
                text: None,
                uuid: None,
            },
            Mode::Append,
            &[text_item("x", 0.0)],
            false,
        )
        .expect_err("unknown unit must be refused");
        match err {
            Error::NotFound(reason) => {
                assert!(
                    reason.contains("Core3576_1_1"),
                    "should list real units: {reason}"
                )
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn text_with_quotes_is_escaped() {
        let out = append(&core3576(), Some("Core3576_1_1"), "3\" \\ display");
        assert!(out.contains(r#"(text "3\" \\ display""#), "escaping failed");
        let reparsed = parse_sexp(&out).expect("escaped text must still parse");
        assert_eq!(reparsed.head(), Some("kicad_symbol_lib"));
        assert_eq!(
            texts_in_unit(&out, "Core3576_1_1"),
            vec!["3\" \\ display".to_string()]
        );
    }

    #[test]
    fn crlf_libraries_keep_their_line_endings() {
        let source = core3576().replace('\n', "\r\n");
        let out = append(&source, Some("Core3576_1_1"), "CRLF caption");
        assert!(out.contains("\r\n\t\t\t(text \"CRLF caption\""));
        assert!(
            !out.contains("\n\t\t\t(text \"CRLF caption\"\n"),
            "must not introduce bare LF into a CRLF file"
        );
        assert_eq!(pin_fingerprint(&out), pin_fingerprint(&source));
    }

    #[test]
    fn a_non_library_root_is_refused() {
        let err = prepare(
            "(kicad_pcb (version 20251024))",
            "Core3576",
            None,
            &Selector {
                kind: SelectorKind::Any,
                text: None,
                uuid: None,
            },
            Mode::Append,
            &[text_item("x", 0.0)],
            false,
        )
        .expect_err("a board file must be refused");
        assert!(matches!(err, Error::Invalid { .. }));
    }

    #[test]
    fn both_tools_are_registered_with_usable_schemas() {
        let tools = crate::tools::library::tools();
        for name in ["set_symbol_graphics", "add_symbol_text"] {
            let tool = tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("library toolset must expose {name}"));
            assert_eq!(tool.input_schema["type"], "object");
            assert!(tool.input_schema["properties"]["library_path"].is_object());
            assert!(tool.input_schema["properties"]["symbol_name"].is_object());
            assert!(tool.input_schema["properties"]["unit_symbol"].is_object());
        }
    }
}
