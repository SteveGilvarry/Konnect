//! `sch_components` toolset — add, edit, move, rotate, delete schematic symbols.
//!
//! Simple CRUD operations use `konnect_schematic_editor` (cse) for structured
//! round-trip parsing.  Pin coordinate math still delegates to
//! `konnect_sexp::geometry::transform_pin`.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, opt_f64, opt_str, require_f64, require_str, ToolContext, ToolDef};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    geometry::snap_point,
    schematic::{extract_symbol_instances, pin_endpoint, read_schematic},
    writer::{apply_edits, find_block_with_leading_whitespace, new_uuid, write_atomic, SexpEdit},
};
use serde_json::json;

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "create_schematic",
            "Create a new blank .kicad_sch schematic file.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Full path for the new .kicad_sch file" }
                },
                "required": ["path"]
            }),
            |args, ctx| async move { handle_create_schematic(args, ctx).await }
        ),
        tool!(
            "add_schematic_component",
            "Add a symbol from a KiCAD library to the schematic. The symbol is snapped \
             to the 1.27mm schematic grid. Specify position in schematic mm coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "lib_id": { "type": "string", "description": "Library:Symbol (e.g. 'Device:R')" },
                    "x": { "type": "number", "description": "X position in mm" },
                    "y": { "type": "number", "description": "Y position in mm" },
                    "rotation": { "type": "number", "description": "Rotation in degrees (0/90/180/270)", "default": 0 },
                    "reference": { "type": "string", "description": "Optional override for reference designator" },
                    "value": { "type": "string", "description": "Optional override for value field" },
                    "unit": { "type": "integer", "description": "Unit number for multi-unit symbols (e.g. gate B of a quad buffer = 2). Default 1. Place each unit as its own call with the same reference.", "default": 1 }
                },
                "required": ["schematic", "lib_id", "x", "y"]
            }),
            |args, ctx| async move { handle_add_schematic_component(args, ctx).await }
        ),
        tool!(
            "delete_schematic_component",
            "Remove a symbol instance from the schematic by its reference designator.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator (e.g. 'R1')" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_delete_schematic_component(args, ctx).await }
        ),
        tool!(
            "edit_schematic_component",
            "Update fields (Reference, Value, Footprint, custom properties) of a symbol instance.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string", "description": "Current reference designator" },
                    "new_reference": { "type": "string", "description": "New reference designator (optional)" },
                    "value": { "type": "string", "description": "New value (optional)" },
                    "footprint": { "type": "string", "description": "New footprint (optional)" },
                    "datasheet": { "type": "string", "description": "New datasheet URL (optional)" },
                    "fields": {
                        "type": "object",
                        "description": "Additional property fields to set as key:value pairs"
                    }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_edit_schematic_component(args, ctx).await }
        ),
        tool!(
            "edit_component_field_position",
            "Move a single property field (Reference, Value, or custom) of a symbol instance \
             to an absolute position, optionally rotating it.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator" },
                    "field": { "type": "string", "description": "Field name, e.g. 'Reference', 'Value'" },
                    "x": { "type": "number" },
                    "y": { "type": "number" },
                    "rotation": { "type": "number", "description": "Field text rotation (default: keep current)" }
                },
                "required": ["schematic", "reference", "field", "x", "y"]
            }),
            |args, ctx| async move { handle_edit_field_position(args, ctx).await }
        ),
        tool!(
            "autoplace_component_fields",
            "KiCad-style field autoplacement: put Reference above and Value below the symbol \
             body (bbox from pin endpoints) for one or more components. Fixes text-through-body \
             rendering after rotations.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "references": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Components to autoplace; omit for ALL non-power symbols"
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_autoplace_fields(args, ctx).await }
        ),
        tool!(
            "get_schematic_component",
            "Get all properties, position, and pin locations for a symbol instance.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_get_schematic_component(args, ctx).await }
        ),
        tool!(
            "list_schematic_components",
            "List all symbol instances in a schematic with their positions, values, \
             footprints, and pin locations.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_list_schematic_components(args, ctx).await }
        ),
        tool!(
            "move_schematic_component",
            "Move a symbol to a new position. Does NOT adjust connected wires.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "x": { "type": "number", "description": "New X position in mm" },
                    "y": { "type": "number", "description": "New Y position in mm" }
                },
                "required": ["schematic", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_schematic_component(args, ctx).await }
        ),
        tool!(
            "rotate_schematic_component",
            "Rotate a symbol by setting its absolute rotation angle (0/90/180/270).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "rotation": { "type": "number", "description": "Absolute rotation in degrees" }
                },
                "required": ["schematic", "reference", "rotation"]
            }),
            |args, ctx| async move { handle_rotate_schematic_component(args, ctx).await }
        ),
        tool!(
            "move_connected",
            "Move a symbol and stretch/shrink connected wire stubs to preserve connections.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "x": { "type": "number" },
                    "y": { "type": "number" }
                },
                "required": ["schematic", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_connected(args, ctx).await }
        ),
        tool!(
            "move_region",
            "Move all symbols within a bounding box by a given offset.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x1": { "type": "number", "description": "Region bounding box min X" },
                    "y1": { "type": "number", "description": "Region bounding box min Y" },
                    "x2": { "type": "number", "description": "Region bounding box max X" },
                    "y2": { "type": "number", "description": "Region bounding box max Y" },
                    "dx": { "type": "number", "description": "X offset to move by" },
                    "dy": { "type": "number", "description": "Y offset to move by" }
                },
                "required": ["schematic", "x1", "y1", "x2", "y2", "dx", "dy"]
            }),
            |args, ctx| async move { handle_move_region(args, ctx).await }
        ),
        tool!(
            "annotate_schematic",
            "Run kicad-cli to auto-assign reference designators (R? → R1, U? → U1, etc.).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_annotate_schematic(args, ctx).await }
        ),
        tool!(
            "get_schematic_pin_locations",
            "Get the exact schematic-space (X,Y) coordinates of every pin on a symbol, \
             accounting for rotation and mirroring. Uses the canonical pin transform.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_get_schematic_pin_locations(args, ctx).await }
        ),
        tool!(
            "batch_get_schematic_pin_locations",
            "Get pin locations for multiple components in a single file read.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "references": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of reference designators"
                    }
                },
                "required": ["schematic", "references"]
            }),
            |args, ctx| async move { handle_batch_get_pin_locations(args, ctx).await }
        ),
        tool!(
            "add_component_annotation",
            "Add a custom property (annotation) to a symbol instance in the schematic.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "reference": { "type": "string", "description": "Component reference designator (e.g. 'R1')" },
                    "key": { "type": "string", "description": "Property name" },
                    "value": { "type": "string", "description": "Property value" }
                },
                "required": ["schematic", "reference", "key", "value"]
            }),
            |args, ctx| async move { handle_add_component_annotation(args, ctx).await }
        ),
        tool!(
            "group_components",
            "Add a group property to multiple components in the schematic.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of reference designators to group"
                    },
                    "group_name": { "type": "string", "description": "Group name to assign" }
                },
                "required": ["schematic", "references", "group_name"]
            }),
            |args, ctx| async move { handle_group_components(args, ctx).await }
        ),
        tool!(
            "replace_component",
            "Replace a component's lib_id with a new library symbol (swap the component type).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "reference": { "type": "string", "description": "Component reference designator (e.g. 'U1')" },
                    "new_lib_id": { "type": "string", "description": "New Library:Symbol identifier (e.g. 'Device:C')" }
                },
                "required": ["schematic", "reference", "new_lib_id"]
            }),
            |args, ctx| async move { handle_replace_component(args, ctx).await }
        ),
        tool!(
            "get_schematic_view",
            "Render the schematic to a PNG image (base64-encoded) via kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_get_schematic_view(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_create_schematic(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = get_path(args, "path")?;
    // Build a minimal valid schematic and save via cse's atomic writer
    let template = "(kicad_sch\n\t(version 20260306)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n";
    // Write the template then immediately load/save through cse so the file
    // is normalised to cse's writer output format.
    write_atomic(&path, template)?;
    let sch = cse::Schematic::load(&path)?;
    sch.overwrite()?;
    Ok(CallToolResult::json(
        &json!({ "created": path.display().to_string() }),
    ))
}

async fn handle_add_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let lib_id = match require_str(args, "lib_id") {
        Ok(s) => s.to_string(),
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
    let reference = opt_str(args, "reference");
    let value = opt_str(args, "value");
    let unit = args["unit"].as_u64().unwrap_or(1) as u32;

    // Snap to 1.27mm grid
    let (x, y) = snap_point(x, y, 1.27);

    let ref_str = reference.unwrap_or("?");
    let val_str = value.unwrap_or(lib_id.split(':').next_back().unwrap_or("?"));

    // Load via konnect-schematic-editor
    let mut sch = cse::Schematic::load(&sch_path)?;

    // Embed the library symbol definition
    cse::library::ensure_lib_symbol(&mut sch, &lib_id);

    // Build the Symbol struct
    let mut sym = cse::Symbol::new(&lib_id, x, y);
    sym.at.rotation = Some(rotation);
    sym.unit = unit;

    // Helper: build an effects sub-node  (font (size 1.27 1.27))  with optional (hide yes)
    let effects_node = |hide: bool| -> cse::sexp::SexpNode {
        let font = cse::sexp::SexpNode::List(vec![
            cse::sexp::atom("font"),
            cse::sexp::SexpNode::List(vec![
                cse::sexp::atom("size"),
                cse::sexp::atom("1.27"),
                cse::sexp::atom("1.27"),
            ]),
        ]);
        let mut children = vec![cse::sexp::atom("effects"), font];
        if hide {
            children.push(cse::sexp::SexpNode::List(vec![
                cse::sexp::atom("hide"),
                cse::sexp::atom("yes"),
            ]));
        }
        cse::sexp::SexpNode::List(children)
    };

    // Helper: build an (at X Y ROT) sub-node
    let at_node = |px: f64, py: f64, rot: f64| -> cse::sexp::SexpNode {
        cse::sexp::SexpNode::List(vec![
            cse::sexp::atom("at"),
            cse::sexp::atom(cse::types::fmt_f64(px)),
            cse::sexp::atom(cse::types::fmt_f64(py)),
            cse::sexp::atom(cse::types::fmt_f64(rot)),
        ])
    };

    // Offset Reference above component, Value below
    let ref_y = y - 3.81;
    let val_y = y + 3.81;

    // Reference property
    let mut ref_prop = cse::Property::new("Reference", ref_str);
    ref_prop.sub_nodes.push(at_node(x, ref_y, 0.0));
    ref_prop.sub_nodes.push(effects_node(false));
    sym.properties.push(ref_prop);

    // Value property
    let mut val_prop = cse::Property::new("Value", val_str);
    val_prop.sub_nodes.push(at_node(x, val_y, 0.0));
    val_prop.sub_nodes.push(effects_node(false));
    sym.properties.push(val_prop);

    // Footprint property (hidden)
    let mut fp_prop = cse::Property::new("Footprint", "");
    fp_prop.sub_nodes.push(at_node(x, y, 0.0));
    fp_prop.sub_nodes.push(effects_node(true));
    sym.properties.push(fp_prop);

    // Datasheet property (hidden)
    let mut ds_prop = cse::Property::new("Datasheet", "");
    ds_prop.sub_nodes.push(at_node(x, y, 0.0));
    ds_prop.sub_nodes.push(effects_node(true));
    sym.properties.push(ds_prop);

    // Instances node
    let instances = cse::sexp::SexpNode::List(vec![
        cse::sexp::atom("instances"),
        cse::sexp::SexpNode::List(vec![
            cse::sexp::atom("project"),
            cse::sexp::qstr(""),
            cse::sexp::SexpNode::List(vec![
                cse::sexp::atom("path"),
                cse::sexp::qstr("/"),
                cse::sexp::SexpNode::List(vec![
                    cse::sexp::atom("reference"),
                    cse::sexp::qstr(ref_str),
                ]),
                cse::sexp::SexpNode::List(vec![
                    cse::sexp::atom("unit"),
                    cse::sexp::atom(unit.to_string()),
                ]),
            ]),
        ]),
    ]);
    sym.raw_sub_nodes.push(instances);

    let uuid = sym.uuid.clone();
    sch.add_symbol(sym);
    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "added": lib_id,
        "reference": ref_str,
        "value": val_str,
        "x": x, "y": y,
        "uuid": uuid
    })))
}

async fn handle_delete_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.remove_by_reference(&reference) {
        Some(_) => {
            sch.overwrite()?;
            Ok(CallToolResult::json(&json!({ "deleted": reference })))
        }
        None => Ok(CallToolResult::error(format!(
            "Component '{}' not found in schematic",
            reference
        ))),
    }
}

async fn handle_edit_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    use super::sch_batch::{field_value_range, insert_property_edit};

    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&sch_path)?;
    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut changed = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // Standard fields always exist on a symbol instance: replace in place.
    // Custom fields from "fields" may be new: replace-or-insert.
    let standard = [
        ("Reference", opt_str(args, "new_reference")),
        ("Value", opt_str(args, "value")),
        ("Footprint", opt_str(args, "footprint")),
        ("Datasheet", opt_str(args, "datasheet")),
    ];
    for (field, val) in standard {
        let Some(new_val) = val else { continue };
        match field_value_range(&content, &reference, field) {
            Some((start, end)) => {
                edits.push(SexpEdit::replace(start, end, new_val.to_string()));
                changed.push(format!("{} → {}", field, new_val));
            }
            None => errors.push(format!("Field '{}' not found on '{}'", field, reference)),
        }
    }

    if let Some(fields_obj) = args["fields"].as_object() {
        for (field_name, field_val) in fields_obj {
            let Some(new_val) = field_val.as_str() else {
                errors.push(format!("Field '{}' value must be a string", field_name));
                continue;
            };
            match field_value_range(&content, &reference, field_name) {
                Some((start, end)) => {
                    edits.push(SexpEdit::replace(start, end, new_val.to_string()));
                    changed.push(format!("{} → {}", field_name, new_val));
                }
                None => match insert_property_edit(&content, &reference, field_name, new_val) {
                    Some(edit) => {
                        edits.push(edit);
                        changed.push(format!("{} + {}", field_name, new_val));
                    }
                    None => errors.push(format!(
                        "Symbol '{}' not found for new field '{}'",
                        reference, field_name
                    )),
                },
            }
        }
    }

    if !edits.is_empty() {
        let new_content = apply_edits(content, edits);
        write_atomic(&sch_path, &new_content)?;
    }

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "changes": changed,
        "errors": errors
    })))
}

/// Byte range of the "X Y R" numbers inside a property's `(at X Y R)` node,
/// within the symbol block for `reference`.
fn field_at_range(content: &str, reference: &str, field: &str) -> Option<(usize, usize)> {
    let (sym_start, sym_end) = super::sch_batch::find_symbol_block(content, reference)?;
    let block = &content[sym_start..sym_end];
    let prop_pat = format!("(property \"{}\" ", field);
    let prop_rel = block.find(&prop_pat)?;
    let after = &block[prop_rel..];
    let at_rel = after.find("(at ")? + 4;
    let close_rel = after[at_rel..].find(')')? + at_rel;
    Some((sym_start + prop_rel + at_rel, sym_start + prop_rel + close_rel))
}

async fn handle_edit_field_position(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let field = match require_str(args, "field") {
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

    let content = std::fs::read_to_string(&sch_path)?;
    let (start, end) = field_at_range(&content, &reference, &field).ok_or_else(|| {
        anyhow::anyhow!("Field '{}' not found on '{}'", field, reference)
    })?;
    let rotation = match opt_f64(args, "rotation") {
        Some(r) => r,
        // keep the field's current rotation (3rd token, absent = 0)
        None => content[start..end]
            .split_whitespace()
            .nth(2)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0),
    };
    let new_content = apply_edits(
        content,
        vec![SexpEdit::replace(start, end, format!("{} {} {}", x, y, rotation))],
    );
    write_atomic(&sch_path, &new_content)?;
    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "field": field,
        "at": { "x": x, "y": y, "rotation": rotation }
    })))
}

async fn handle_autoplace_fields(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let only: Option<Vec<String>> = args["references"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });

    let (content, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let wires = konnect_sexp::schematic::extract_wires(&tree);
    let labels = konnect_sexp::schematic::extract_labels(&tree);

    // Obstacle set: every symbol's pin bbox (incl. power symbols — their
    // wires and arrows are exactly what fields keep landing on), every wire
    // segment, every label's estimated text box (conservatively extended in
    // both reading directions since rotation isn't tracked here).
    #[derive(Clone, Copy)]
    struct Box4 {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
    }
    let mut sym_boxes: Vec<(String, Box4)> = Vec::new();
    let mut bbox_of = std::collections::HashMap::new();
    for inst in &instances {
        let t = inst.pin_transform();
        let pts: Vec<(f64, f64)> = konnect_sexp::schematic::resolve_lib_pins_for_unit(
            &lib_syms,
            &inst.lib_id,
            inst.unit,
        )
        .iter()
        .map(|p| konnect_sexp::schematic::pin_endpoint(p, t))
        .collect();
        if pts.is_empty() {
            continue;
        }
        let (mut x1, mut x2) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p.0), hi.max(p.0)));
        let (mut y1, mut y2) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p.1), hi.max(p.1)));
        if inst.reference.starts_with('#') {
            // Power symbol: one pin, but the arrow/bar graphic plus its net
            // name text hang ~5mm off the pin in an unknown direction.
            let (cx, cy) = pts[0];
            x1 = cx - 2.8;
            x2 = cx + 2.8;
            y1 = cy - 4.2;
            y2 = cy + 4.2;
        } else {
            const MIN_EXTENT: f64 = 3.81;
            if x2 - x1 < MIN_EXTENT {
                let c = (x1 + x2) / 2.0;
                x1 = c - MIN_EXTENT / 2.0;
                x2 = c + MIN_EXTENT / 2.0;
            }
            if y2 - y1 < MIN_EXTENT {
                let c = (y1 + y2) / 2.0;
                y1 = c - MIN_EXTENT / 2.0;
                y2 = c + MIN_EXTENT / 2.0;
            }
            if pts.len() >= 3 {
                // IC bodies extend past the pin-row hull (e.g. above the top
                // side-pin row); pad so fields clear the drawn rectangle.
                x1 -= 1.0;
                x2 += 1.0;
                y1 -= 2.0;
                y2 += 2.0;
            }
        }
        let b = Box4 { x1, y1, x2, y2 };
        sym_boxes.push((inst.reference.clone(), b));
        bbox_of.insert(
            (inst.reference.clone(), inst.unit),
            (b, (x1 + x2) / 2.0, (y1 + y2) / 2.0),
        );
    }
    let label_boxes: Vec<Box4> = labels
        .iter()
        .map(|l| {
            // Text extends in reading direction from the anchor (justify
            // tracks rotation), plus ~2mm for global-label flag chrome.
            let w = l.net.chars().count() as f64 * 1.33 + 2.0;
            match l.rotation as i64 {
                180 => Box4 { x1: l.x - w, y1: l.y - 1.4, x2: l.x, y2: l.y + 1.4 },
                90 => Box4 { x1: l.x - 1.4, y1: l.y - w, x2: l.x + 1.4, y2: l.y },
                270 => Box4 { x1: l.x - 1.4, y1: l.y, x2: l.x + 1.4, y2: l.y + w },
                _ => Box4 { x1: l.x, y1: l.y - 1.4, x2: l.x + w, y2: l.y + 1.4 },
            }
        })
        .collect();

    let boxes_hit = |a: Box4, b: Box4| a.x1 < b.x2 && b.x1 < a.x2 && a.y1 < b.y2 && b.y1 < a.y2;
    // Estimated text box for a centered field anchored at (x, y).
    let text_box = |txt: &str, x: f64, y: f64| -> Box4 {
        let w = (txt.chars().count() as f64 * 1.33).max(2.0);
        Box4 {
            x1: x - w / 2.0,
            y1: y - 1.3,
            x2: x + w / 2.0,
            y2: y + 1.3,
        }
    };
    let collides = |b: Box4, own_ref: &str| -> bool {
        for w in &wires {
            let wb = Box4 {
                x1: w.x1.min(w.x2) - 0.2,
                y1: w.y1.min(w.y2) - 0.2,
                x2: w.x1.max(w.x2) + 0.2,
                y2: w.y1.max(w.y2) + 0.2,
            };
            if boxes_hit(b, wb) {
                return true;
            }
        }
        for (r, sb) in &sym_boxes {
            if r != own_ref && boxes_hit(b, *sb) {
                return true;
            }
        }
        label_boxes.iter().any(|lb| boxes_hit(b, *lb))
    };

    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut placed: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    // Text boxes already claimed by fields placed earlier in this run, so two
    // neighbouring parts don't both pick the same free spot.
    let mut claimed: Vec<Box4> = Vec::new();
    // Multi-unit parts share one field set on the first unit's block; only
    // place it once per reference.
    let mut done_refs: std::collections::HashSet<String> = std::collections::HashSet::new();

    for inst in &instances {
        match &only {
            Some(refs) if !refs.contains(&inst.reference) => continue,
            None if inst.reference.starts_with('#') => continue,
            _ => {}
        }
        if !done_refs.insert(inst.reference.clone()) {
            continue;
        }
        let Some(&(b, cx, cy)) = bbox_of.get(&(inst.reference.clone(), inst.unit)) else {
            errors.push(format!("{}: no pins resolved", inst.reference));
            continue;
        };
        let ref_txt = inst.reference.clone();
        let val_txt = inst.value.clone();
        let wmax = (ref_txt.chars().count().max(val_txt.chars().count()) as f64 * 1.33) / 2.0;

        // Candidate anchor pairs (Reference, Value), nearest first: right,
        // left, above, below — each at growing clearance.
        let mut chosen: Option<((f64, f64), (f64, f64))> = None;
        'search: for gap in [1.0, 2.5, 4.0, 6.0] {
            let cands = [
                ((b.x2 + gap + wmax, cy - 1.7), (b.x2 + gap + wmax, cy + 1.7)),
                ((b.x1 - gap - wmax, cy - 1.7), (b.x1 - gap - wmax, cy + 1.7)),
                ((cx, b.y1 - gap - 4.2), (cx, b.y1 - gap - 0.9)),
                ((cx, b.y2 + gap + 0.9), (cx, b.y2 + gap + 4.2)),
            ];
            for (rp, vp) in cands {
                let rb = text_box(&ref_txt, rp.0, rp.1);
                let vb = text_box(&val_txt, vp.0, vp.1);
                let hit_claimed =
                    claimed.iter().any(|c| boxes_hit(rb, *c) || boxes_hit(vb, *c));
                if !hit_claimed && !collides(rb, &inst.reference) && !collides(vb, &inst.reference)
                {
                    chosen = Some((rp, vp));
                    claimed.push(rb);
                    claimed.push(vb);
                    break 'search;
                }
            }
        }
        // Nothing collision-free nearby: fall back to above/below like KiCad.
        let ((rx, ry), (vx, vy)) =
            chosen.unwrap_or(((cx, b.y1 - 1.8), (cx, b.y2 + 1.8)));

        // Property rotation composes with the symbol rotation at render time;
        // counter-rotate so the text always reads horizontally.
        let frot = if (inst.rotation as i64 % 180).abs() == 90 { 90 } else { 0 };
        for (field, fx, fy) in [("Reference", rx, ry), ("Value", vx, vy)] {
            match field_at_range(&content, &inst.reference, field) {
                Some((start, end)) => {
                    edits.push(SexpEdit::replace(start, end, format!("{} {} {}", fx, fy, frot)));
                }
                None => errors.push(format!("{}: field '{}' not found", inst.reference, field)),
            }
        }
        placed.push(json!({
            "reference": inst.reference,
            "ref_at": [rx, ry],
            "value_at": [vx, vy],
            "collision_free": chosen.is_some()
        }));
    }

    if !edits.is_empty() {
        let new_content = apply_edits(content, edits);
        write_atomic(&sch_path, &new_content)?;
    }

    Ok(CallToolResult::json(&json!({
        "autoplaced": placed.len(),
        "components": placed,
        "errors": errors
    })))
}

async fn handle_get_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.by_reference(&reference) {
        Some(sym) => {
            let (x, y) = sym.position();
            let rotation = sym.at.rotation.unwrap_or(0.0);
            let mirror = sym.mirror.as_deref().unwrap_or("");
            Ok(CallToolResult::json(&json!({
                "reference": sym.reference().unwrap_or("?"),
                "value": sym.value_str().unwrap_or(""),
                "footprint": sym.footprint().unwrap_or(""),
                "lib_id": sym.lib_id,
                "x": x,
                "y": y,
                "rotation": rotation,
                "mirror_x": mirror.contains('x'),
                "mirror_y": mirror.contains('y'),
                "uuid": sym.uuid
            })))
        }
        None => Ok(CallToolResult::error(format!(
            "Component '{}' not found",
            reference
        ))),
    }
}

async fn handle_list_schematic_components(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sch = cse::Schematic::load(&sch_path)?;

    let items: Vec<serde_json::Value> = sch
        .symbols
        .iter()
        .map(|sym| {
            let (x, y) = sym.position();
            let rotation = sym.at.rotation.unwrap_or(0.0);
            let mirror = sym.mirror.as_deref().unwrap_or("");
            json!({
                "reference": sym.reference().unwrap_or("?"),
                "value": sym.value_str().unwrap_or(""),
                "footprint": sym.footprint().unwrap_or(""),
                "lib_id": sym.lib_id,
                "x": x,
                "y": y,
                "rotation": rotation,
                "mirror_x": mirror.contains('x'),
                "mirror_y": mirror.contains('y')
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "count": items.len(),
        "components": items
    })))
}

async fn handle_move_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let new_x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let new_y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let (new_x, new_y) = snap_point(new_x, new_y, 1.27);

    let mut sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.by_reference_mut(&reference) {
        Some(sym) => {
            sym.move_to(new_x, new_y);
            sch.overwrite()?;
            Ok(CallToolResult::json(
                &json!({ "moved": reference, "x": new_x, "y": new_y }),
            ))
        }
        None => Err(anyhow::anyhow!("Component '{}' not found", reference)),
    }
}

async fn handle_rotate_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let rotation = match require_f64(args, "rotation") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.by_reference_mut(&reference) {
        Some(sym) => {
            sym.set_rotation(rotation);
            sch.overwrite()?;
            Ok(CallToolResult::json(
                &json!({ "rotated": reference, "rotation": rotation }),
            ))
        }
        None => Err(anyhow::anyhow!("Component '{}' not found", reference)),
    }
}

async fn handle_move_connected(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    // For now: delegate to simple move. Wire adjustment is a Phase 2 enhancement.
    handle_move_schematic_component(args, ctx).await
}

async fn handle_move_region(
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
    let dx = match require_f64(args, "dx") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let dy = match require_f64(args, "dy") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;

    // Collect references of symbols within the bounding box
    let refs_to_move: Vec<String> = sch
        .symbols
        .within_rectangle(x1, y1, x2, y2)
        .iter()
        .filter_map(|s| s.reference().map(String::from))
        .collect();

    let mut moved = Vec::new();
    for reference in &refs_to_move {
        if let Some(sym) = sch.symbols.by_reference_mut(reference) {
            let (ox, oy) = sym.position();
            let (nx, ny) = snap_point(ox + dx, oy + dy, 1.27);
            sym.move_to(nx, ny);
            moved.push(reference.clone());
        }
    }

    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "moved_count": moved.len(),
        "moved": moved
    })))
}

async fn handle_annotate_schematic(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    crate::tools::cli::annotate_schematic(&ctx.config.kicad_cli, &sch_path).await?;
    Ok(CallToolResult::text("Annotation complete."))
}

async fn handle_get_schematic_pin_locations(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let inst = match instances.iter().find(|i| i.reference == reference) {
        Some(i) => i,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    // Find the library symbol definition within the schematic's lib_symbols section
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let lib_pins = konnect_sexp::schematic::resolve_lib_pins_for_unit(&lib_syms, &inst.lib_id, inst.unit);
    let t = inst.pin_transform();
    let pins: Vec<serde_json::Value> = lib_pins
        .iter()
        .map(|p| {
            let (sx, sy) = pin_endpoint(p, t);
            json!({
                "number": p.number,
                "name": p.name,
                "x": sx,
                "y": sy
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "component_x": inst.x,
        "component_y": inst.y,
        "rotation": inst.rotation,
        "pins": pins
    })))
}

async fn handle_batch_get_pin_locations(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let (_, tree) = read_schematic(&sch_path)?; // single read
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let results: Vec<serde_json::Value> = refs
        .iter()
        .map(|reference| {
            let inst = match instances.iter().find(|i| &i.reference == reference) {
                Some(i) => i,
                None => return json!({ "reference": reference, "error": "not found" }),
            };
            let t = inst.pin_transform();
            let pins: Vec<serde_json::Value> =
                konnect_sexp::schematic::resolve_lib_pins_for_unit(&lib_syms, &inst.lib_id, inst.unit)
                    .iter()
                    .map(|p| {
                        let (sx, sy) = pin_endpoint(p, t);
                        json!({ "number": p.number, "name": p.name, "x": sx, "y": sy })
                    })
                    .collect();
            json!({ "reference": reference, "x": inst.x, "y": inst.y, "pins": pins })
        })
        .collect();

    Ok(CallToolResult::json(&json!({ "components": results })))
}

async fn handle_get_schematic_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let tmp_dir = std::env::temp_dir().join(format!("konnect_{}", new_uuid()));
    tokio::fs::create_dir_all(&tmp_dir).await?;

    // KiCAD 10 CLI only supports SVG export for schematics (no bitmap)
    let svg_path =
        crate::tools::cli::render_schematic_svg(&ctx.config.kicad_cli, &sch_path, &tmp_dir).await?;

    let svg_content = tokio::fs::read_to_string(&svg_path).await?;
    tokio::fs::remove_dir_all(&tmp_dir).await.ok();

    // Return as text content (SVG is XML text, not a raster image)
    Ok(crate::mcp::protocol::CallToolResult {
        content: vec![crate::mcp::protocol::ToolContent::Text {
            text: format!("SVG schematic rendered. {} bytes.\n\nNote: KiCAD 10 CLI exports schematics as SVG only (no bitmap). \
                          The SVG file has been generated. Use export_schematic_pdf for a PDF version.", svg_content.len()),
        }],
        is_error: false,
    })
}

async fn handle_add_component_annotation(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let key = match require_str(args, "key") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let value = match require_str(args, "value") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&sch_path)?;

    // Find the symbol block for this reference
    let ref_search = format!(r#"(property "Reference" "{reference}""#);
    let ref_pos = match content.find(&ref_search) {
        Some(o) => o,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    let before = &content[..ref_pos];
    let sym_start = match before.rfind("\n  (symbol") {
        Some(o) => o + 1,
        None => return Ok(CallToolResult::error("Could not find symbol block")),
    };
    let (_, sym_end) = match find_block_with_leading_whitespace(&content, sym_start) {
        Some(r) => r,
        None => return Ok(CallToolResult::error("Could not parse symbol block")),
    };

    // Find the position just before (instances in the symbol block, or before closing paren
    let sym_block = &content[sym_start..sym_end];
    let insert_rel = sym_block
        .find("(instances")
        .unwrap_or(sym_block.rfind(')').unwrap_or(sym_block.len() - 1));
    let insert_abs = sym_start + insert_rel;

    // Build the property S-expression
    let prop_sexp = format!(
        "    (property \"{key}\" \"{value}\"\n      (at 0 0 0)\n      (effects (font (size 1.27 1.27)) (hide yes))\n    )\n    "
    );

    let new_content = apply_edits(content, vec![SexpEdit::insert(insert_abs, prop_sexp)]);
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "added_property": key,
        "value": value
    })))
}

async fn handle_group_components(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let group_name = match require_str(args, "group_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let refs = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if refs.is_empty() {
        return Ok(CallToolResult::error("No references provided"));
    }

    let mut content = std::fs::read_to_string(&sch_path)?;
    let mut grouped = Vec::new();

    for reference in &refs {
        let ref_search = format!(r#"(property "Reference" "{reference}""#);
        let ref_pos = match content.find(&ref_search) {
            Some(o) => o,
            None => continue,
        };

        let before = &content[..ref_pos];
        let sym_start = match before.rfind("\n  (symbol") {
            Some(o) => o + 1,
            None => continue,
        };
        let (_, sym_end) = match find_block_with_leading_whitespace(&content, sym_start) {
            Some(r) => r,
            None => continue,
        };

        let sym_block = &content[sym_start..sym_end];
        let insert_rel = sym_block
            .find("(instances")
            .unwrap_or(sym_block.rfind(')').unwrap_or(sym_block.len() - 1));
        let insert_abs = sym_start + insert_rel;

        let prop_sexp = format!(
            "    (property \"Group\" \"{group_name}\"\n      (at 0 0 0)\n      (effects (font (size 1.27 1.27)) (hide yes))\n    )\n    "
        );

        content = apply_edits(content, vec![SexpEdit::insert(insert_abs, prop_sexp)]);
        grouped.push(reference.clone());
    }

    write_atomic(&sch_path, &content)?;

    Ok(CallToolResult::json(&json!({
        "group_name": group_name,
        "grouped_count": grouped.len(),
        "grouped": grouped
    })))
}

async fn handle_replace_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let new_lib_id = match require_str(args, "new_lib_id") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let mut content = std::fs::read_to_string(&sch_path)?;

    // Find the symbol block for this reference
    let ref_search = format!(r#"(property "Reference" "{reference}""#);
    let ref_pos = match content.find(&ref_search) {
        Some(o) => o,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    let before = &content[..ref_pos];
    let sym_start = match before.rfind("\n  (symbol") {
        Some(o) => o + 1,
        None => return Ok(CallToolResult::error("Could not find symbol block")),
    };

    // Find the (lib_id "OLD") and replace it
    let sym_block_start = &content[sym_start..];
    let lib_id_pat = "(lib_id \"";
    let lib_id_rel = match sym_block_start.find(lib_id_pat) {
        Some(o) => o,
        None => {
            return Ok(CallToolResult::error(
                "Could not find lib_id in symbol block",
            ))
        }
    };
    let lib_id_abs = sym_start + lib_id_rel + lib_id_pat.len();
    let lib_id_end = match content[lib_id_abs..].find('"') {
        Some(o) => lib_id_abs + o,
        None => return Ok(CallToolResult::error("Malformed lib_id")),
    };

    let old_lib_id = content[lib_id_abs..lib_id_end].to_string();

    let new_content = apply_edits(
        content,
        vec![SexpEdit::replace(
            lib_id_abs,
            lib_id_end,
            new_lib_id.clone(),
        )],
    );
    content = new_content;

    // Ensure the new library symbol definition is present
    super::ensure_lib_symbol_in_schematic(&mut content, &new_lib_id);
    write_atomic(&sch_path, &content)?;

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "old_lib_id": old_lib_id,
        "new_lib_id": new_lib_id
    })))
}

// Library symbol resolution moved to tools/mod.rs (shared with sch_wiring.rs)
