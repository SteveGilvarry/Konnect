//! `sch_batch` toolset — bulk/batch operations on schematic elements.
//!
//! **Critical invariant**: every write handler performs a single file read,
//! collects ALL mutations as `SexpEdit` values against the original content,
//! then calls `write_atomic` exactly once. This fixes the Python bug where
//! `batch_connect_to_net` did N separate read/write cycles.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, opt_str, require_f64, require_str, ToolDef};
use konnect_sexp::{
    geometry::{point_on_segment, points_coincident, snap_point},
    schematic::{
        extract_labels, extract_lib_pins, extract_symbol_instances, extract_wires,
        format_net_label, format_wire, pin_endpoint, read_schematic,
    },
    writer::{apply_edits, find_block_with_leading_whitespace, new_uuid, write_atomic, SexpEdit},
};
use serde_json::json;

// Re-use the crate-internal net-graph primitives from sch_analysis.
use super::sch_analysis::build_net_graph;

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
            "batch_delete",
            "Delete multiple schematic items (wires, labels, junctions, components) by UUID \
             or component reference designator — single file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "uuids": {
                        "type": "array",
                        "description": "UUIDs of items to delete",
                        "items": { "type": "string" }
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
            "Move multiple components by a uniform dx/dy offset in a single atomic file write.",
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
            "Apply field updates (Value, Footprint, custom properties) to multiple components \
             in a single atomic file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "edits": {
                        "type": "array",
                        "description": "List of {reference, value?, footprint?, fields?} edit objects",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "value": { "type": "string" },
                                "footprint": { "type": "string" },
                                "fields": {
                                    "type": "object",
                                    "description": "Additional property fields as key:value pairs"
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
                        "description": "Stub direction: 'left', 'right', 'up', 'down'",
                        "default": "right"
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
                    "rotation": { "type": "number", "description": "Rotation in degrees", "default": 0 }
                },
                "required": ["schematic", "text", "x", "y"]
            }),
            |args, ctx| async move { handle_add_schematic_text(args, ctx).await }
        ),
        tool!(
            "get_schematic_layout",
            "Return a compact spatial summary of the schematic: component positions, \
             bounding box, and optionally wire segments and label locations.",
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
            "Check that every non-passive pin on every component has at least one wire \
             or label connected. Reports unconnected pins with reference, pin number, \
             and schematic position.",
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

/// Find the `(symbol ...)` block for a reference designator.
/// Returns `(block_start, block_end)` byte offsets in `content`.
pub(crate) fn find_symbol_block(content: &str, reference: &str) -> Option<(usize, usize)> {
    let search_pat = format!(r#"(property "Reference" "{reference}""#);
    let ref_offset = content.find(&search_pat)?;
    let before = &content[..ref_offset];
    let sym_start = before.rfind("\n  (symbol").map(|p| p + 1)?;
    find_block_with_leading_whitespace(content, sym_start)
}

/// Return `(val_start, val_end)` byte offsets in `content` for the *value* portion
/// of a `(property "FieldName" "VALUE" ...)` node within the symbol identified by
/// `reference`. Only the bytes inside the opening quote are included (i.e. the
/// replacement does NOT need to include surrounding quotes).
pub(crate) fn field_value_range(content: &str, reference: &str, field: &str) -> Option<(usize, usize)> {
    let ref_search = format!(r#"(property "Reference" "{reference}""#);
    let ref_pos = content.find(&ref_search)?;
    let before = &content[..ref_pos];
    let sym_start = before.rfind("\n  (symbol").map(|p| p + 1)?;
    let (_, sym_end) = find_block_with_leading_whitespace(content, sym_start)?;
    let sym_block = &content[sym_start..sym_end];

    let field_search = format!(r#"(property "{field}" ""#);
    let field_rel = sym_block.find(&field_search)?;
    let val_start = sym_start + field_rel + field_search.len();
    // find the closing quote of the current value
    let val_end = val_start + content[val_start..].find('"')?;
    Some((val_start, val_end))
}

/// Build an insertion edit that adds a new hidden `(property ...)` node to the
/// symbol identified by `reference`. Used when a field named in an edit spec does
/// not exist yet (e.g. stamping MPN/LCSC fields). The property is placed at the
/// symbol origin, hidden, and inserted just before the `(instances` node so the
/// file stays in canonical property order.
pub(crate) fn insert_property_edit(
    content: &str,
    reference: &str,
    field: &str,
    value: &str,
) -> Option<SexpEdit> {
    let (sym_start, sym_end) = find_symbol_block(content, reference)?;
    let sym_block = &content[sym_start..sym_end];
    // Symbol origin from the block's own "(at X Y ...)" node.
    let at_rest = &sym_block[sym_block.find("(at ")? + 4..];
    let mut it = at_rest.split_whitespace();
    let x = it.next()?.trim_end_matches(')');
    let y = it.next()?.trim_end_matches(')');
    let offset = match sym_block.find("\n    (instances") {
        Some(rel) => sym_start + rel,
        // No instances node: insert before the block's closing paren.
        None => sym_end - 1,
    };
    let esc = value.replace('\\', "\\\\").replace('"', "\\\"");
    let prop = format!(
        "\n    (property \"{field}\" \"{esc}\"\n      (at {x} {y} 0)\n      (effects\n        (font\n          (size 1.27 1.27))\n        (hide yes)))"
    );
    Some(SexpEdit::insert(offset, prop))
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

        let inst = match instances.iter().find(|i| i.reference == reference) {
            Some(i) => i,
            None => {
                errors.push(format!("Component '{}' not found", reference));
                continue;
            }
        };

        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));

        let pin_ep = lib_sym.and_then(|sym| {
            extract_lib_pins(sym)
                .into_iter()
                .find(|p| p.number == pin_number)
                .map(|p| pin_endpoint(&p, inst.pin_transform()))
        });

        match pin_ep {
            Some((px, py)) => {
                inserts.push_str(&format_net_label(&net_name, px, py, 0.0));
                added.push(json!({
                    "reference": reference,
                    "pin": pin_number,
                    "x": px,
                    "y": py
                }));
            }
            None => errors.push(format!("Pin '{}' not found on '{}'", pin_number, reference)),
        }
    }

    if !inserts.is_empty() {
        let close_pos = content.rfind(')').unwrap_or(content.len());
        let edits = vec![SexpEdit::insert(close_pos, inserts)];
        let new_content = apply_edits(content, edits);
        write_atomic(&sch_path, &new_content)?;
    }

    Ok(CallToolResult::json(&json!({
        "net": net_name,
        "added": added,
        "added_count": added.len(),
        "errors": errors
    })))
}

async fn handle_batch_delete(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let content = std::fs::read_to_string(&sch_path)?;

    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // Delete by UUID — walk back from uuid node to enclosing top-level block
    if let Some(uuids) = args["uuids"].as_array() {
        for uuid_val in uuids {
            let uuid = match uuid_val.as_str() {
                Some(u) => u,
                None => continue,
            };
            let pattern = format!(r#"(uuid "{}")"#, uuid);
            match content.find(&pattern) {
                Some(uuid_pos) => {
                    let before = &content[..uuid_pos];
                    // Top-level schematic items are at 2-space indent: "\n  ("
                    match before.rfind("\n  (").map(|p| p + 1) {
                        Some(block_start) => {
                            match find_block_with_leading_whitespace(&content, block_start) {
                                Some((del_start, del_end)) => {
                                    edits.push(SexpEdit::delete(del_start, del_end));
                                    deleted.push(uuid.to_string());
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
            match find_symbol_block(&content, reference) {
                Some((del_start, del_end)) => {
                    edits.push(SexpEdit::delete(del_start, del_end));
                    deleted.push(reference.to_string());
                }
                None => errors.push(format!("Component '{}' not found", reference)),
            }
        }
    }

    let new_content = apply_edits(content, edits);
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "deleted_count": deleted.len(),
        "deleted": deleted,
        "errors": errors
    })))
}

async fn handle_bulk_move(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = args["references"].as_array().cloned().unwrap_or_default();
    let dx = match require_f64(args, "dx") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let dy = match require_f64(args, "dy") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&sch_path)?;
    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut moved: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r,
            None => continue,
        };

        // Locate symbol block for this reference
        let ref_search = format!(r#"(property "Reference" "{reference}""#);
        let ref_pos = match content.find(&ref_search) {
            Some(p) => p,
            None => {
                errors.push(format!("'{}' not found", reference));
                continue;
            }
        };
        let before = &content[..ref_pos];
        let sym_start = match before.rfind("\n  (symbol").map(|p| p + 1) {
            Some(p) => p,
            None => {
                errors.push(format!("Symbol block for '{}' not found", reference));
                continue;
            }
        };
        let (_, sym_end) = match find_block_with_leading_whitespace(&content, sym_start) {
            Some(r) => r,
            None => {
                errors.push(format!("Cannot parse symbol block for '{}'", reference));
                continue;
            }
        };

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
        moved.push(json!({
            "reference": reference,
            "old_x": x, "old_y": y,
            "new_x": new_x, "new_y": new_y
        }));
    }

    let new_content = apply_edits(content, edits);
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "moved_count": moved.len(),
        "moved": moved,
        "dx": dx, "dy": dy,
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

    let content = std::fs::read_to_string(&sch_path)?;
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

        // Standard fields
        for (field, key) in &[("Value", "value"), ("Footprint", "footprint")] {
            if let Some(new_val) = edit_spec[key].as_str() {
                match field_value_range(&content, reference, field) {
                    Some((start, end)) => {
                        file_edits.push(SexpEdit::replace(start, end, new_val.to_string()));
                        component_changes.push(format!("{} → {}", field, new_val));
                    }
                    None => errors.push(format!("Field '{}' not found on '{}'", field, reference)),
                }
            }
        }

        // Arbitrary extra fields from "fields" object
        if let Some(fields_obj) = edit_spec["fields"].as_object() {
            for (field_name, field_val) in fields_obj {
                if let Some(new_val) = field_val.as_str() {
                    match field_value_range(&content, reference, field_name) {
                        Some((start, end)) => {
                            file_edits.push(SexpEdit::replace(start, end, new_val.to_string()));
                            component_changes.push(format!("{} → {}", field_name, new_val));
                        }
                        None => match insert_property_edit(&content, reference, field_name, new_val)
                        {
                            Some(edit) => {
                                file_edits.push(edit);
                                component_changes.push(format!("{} + {}", field_name, new_val));
                            }
                            None => errors.push(format!(
                                "Symbol '{}' not found for new field '{}'",
                                reference, field_name
                            )),
                        },
                    }
                }
            }
        }

        if !component_changes.is_empty() {
            changed.push(json!({
                "reference": reference,
                "changes": component_changes
            }));
        }
    }

    let new_content = apply_edits(content, file_edits);
    write_atomic(&sch_path, &new_content)?;

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

    let content = std::fs::read_to_string(&sch_path)?;
    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r,
            None => continue,
        };
        match find_symbol_block(&content, reference) {
            Some((del_start, del_end)) => {
                edits.push(SexpEdit::delete(del_start, del_end));
                deleted.push(reference.to_string());
            }
            None => errors.push(format!("Component '{}' not found", reference)),
        }
    }

    let new_content = apply_edits(content, edits);
    write_atomic(&sch_path, &new_content)?;

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
    let direction = opt_str(args, "direction").unwrap_or("right");

    // Stub is 2.54mm (2×1.27 grid units)
    let stub = 2.54_f64;
    let (wire_end_x, wire_end_y, label_rot) = match direction {
        "left" => (x - stub, y, 180.0),
        "up" => (x, y - stub, 90.0),
        "down" => (x, y + stub, 270.0),
        _ => (x + stub, y, 0.0), // "right" default
    };

    let wire_sexp = format_wire(x, y, wire_end_x, wire_end_y);
    let label_sexp = format_net_label(&net_name, wire_end_x, wire_end_y, label_rot);

    let content = std::fs::read_to_string(&sch_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let edits = vec![SexpEdit::insert(
        close_pos,
        format!("{wire_sexp}{label_sexp}"),
    )];
    let new_content = apply_edits(content, edits);
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "net": net_name,
        "stub_root": { "x": x, "y": y },
        "label_position": { "x": wire_end_x, "y": wire_end_y },
        "direction": direction,
        "label_rotation": label_rot
    })))
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
    let uuid = new_uuid();

    // Escape backslashes, quotes, and newlines - KiCAD's s-expression parser
    // rejects raw newlines inside quoted strings ("unterminated delimited
    // string"); multi-line text must use literal \n escapes.
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");

    let text_sexp = format!(
        "\n  (text \"{escaped}\"\n    (at {x} {y} {rotation})\n    \
         (effects (font (size {size} {size})))\n    (uuid \"{uuid}\")\n  )"
    );

    let content = std::fs::read_to_string(&sch_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let edits = vec![SexpEdit::insert(close_pos, text_sexp)];
    let new_content = apply_edits(content, edits);
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "added": text,
        "x": x, "y": y,
        "size": size,
        "rotation": rotation,
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

    let components: Vec<serde_json::Value> = instances
        .iter()
        .map(|i| {
            json!({
                "reference": i.reference,
                "value": i.value,
                "lib_id": i.lib_id,
                "x": i.x, "y": i.y,
                "rotation": i.rotation,
                "mirror_x": i.mirror_x,
                "mirror_y": i.mirror_y
            })
        })
        .collect();

    // Bounding box over component origins
    let (mut min_x, mut min_y) = (f64::MAX, f64::MAX);
    let (mut max_x, mut max_y) = (f64::MIN, f64::MIN);
    for i in &instances {
        min_x = min_x.min(i.x);
        min_y = min_y.min(i.y);
        max_x = max_x.max(i.x);
        max_y = max_y.max(i.y);
    }
    let bbox = if instances.is_empty() {
        json!({ "x_min": 0, "y_min": 0, "x_max": 0, "y_max": 0 })
    } else {
        json!({ "x_min": min_x, "y_min": min_y, "x_max": max_x, "y_max": max_y })
    };

    let mut result = json!({
        "component_count": components.len(),
        "components": components,
        "bounding_box": bbox
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
    let labels = extract_labels(&tree);
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // Collect all valid pin endpoints
    let mut pin_points: Vec<(f64, f64)> = Vec::new();
    for inst in &instances {
        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
        if let Some(sym) = lib_sym {
            let t = inst.pin_transform();
            for pin in extract_lib_pins(sym) {
                pin_points.push(pin_endpoint(&pin, t));
            }
        }
    }

    let label_points: Vec<(f64, f64)> = labels.iter().map(|l| (l.x, l.y)).collect();
    // All wire endpoints as a flat list (for quick counting)
    let all_wire_eps: Vec<(f64, f64)> = wires
        .iter()
        .flat_map(|w| [(w.x1, w.y1), (w.x2, w.y2)])
        .collect();

    let is_connected = |px: f64, py: f64| -> bool {
        // Another wire endpoint at the same position (count >= 2 because px/py itself is in the list)
        let same_ep_count = all_wire_eps
            .iter()
            .filter(|(wx, wy)| points_coincident(px, py, *wx, *wy, tol))
            .count();
        if same_ep_count >= 2 {
            return true;
        }

        // T-junction: lies on the INTERIOR of another wire
        if wires.iter().any(|w| {
            point_on_segment(px, py, w.x1, w.y1, w.x2, w.y2, tol)
                && !points_coincident(px, py, w.x1, w.y1, tol)
                && !points_coincident(px, py, w.x2, w.y2, tol)
        }) {
            return true;
        }

        // Label at this point
        if label_points
            .iter()
            .any(|(lx, ly)| points_coincident(px, py, *lx, *ly, tol))
        {
            return true;
        }

        // Pin endpoint at this point
        if pin_points
            .iter()
            .any(|(ppx, ppy)| points_coincident(px, py, *ppx, *ppy, tol))
        {
            return true;
        }

        false
    };

    let mut floating: Vec<serde_json::Value> = Vec::new();
    for w in &wires {
        for (px, py) in [(w.x1, w.y1), (w.x2, w.y2)] {
            if !is_connected(px, py) {
                floating.push(json!({ "x": px, "y": py, "wire_uuid": w.uuid }));
            }
        }
    }

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
    let filter_refs: Vec<String> = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let tol = 0.01_f64;

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // No-connect positions (pins with intentional no-connect markers are exempt)
    let no_connect_pts: Vec<(f64, f64)> = tree
        .find_all("no_connect")
        .iter()
        .filter_map(|n| {
            let at = n.find("at")?;
            Some((at.get_f64(1)?, at.get_f64(2)?))
        })
        .collect();

    // Build net graph so we can check connectivity
    let mut g = build_net_graph(&wires, &labels);
    // Also build flat wire-endpoint list for direct presence checks
    let all_wire_eps: Vec<(f64, f64)> = wires
        .iter()
        .flat_map(|w| [(w.x1, w.y1), (w.x2, w.y2)])
        .collect();

    // `g.net_at` requires &mut self, so we need a `mut` closure.
    let mut has_connection = |px: f64, py: f64| -> bool {
        // Connected to a wire endpoint
        if all_wire_eps
            .iter()
            .any(|(wx, wy)| points_coincident(px, py, *wx, *wy, tol))
        {
            return true;
        }
        // Or has a named net (label at or reachable from pin via wires)
        g.net_at(px, py).is_some()
    };

    let mut unconnected: Vec<serde_json::Value> = Vec::new();

    for inst in &instances {
        if !filter_refs.is_empty() && !filter_refs.contains(&inst.reference) {
            continue;
        }
        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
        if let Some(sym) = lib_sym {
            let t = inst.pin_transform();
            for pin in extract_lib_pins(sym) {
                let (px, py) = pin_endpoint(&pin, t);

                // Skip intentional no-connects
                if no_connect_pts
                    .iter()
                    .any(|(nx, ny)| points_coincident(px, py, *nx, *ny, tol))
                {
                    continue;
                }

                if !has_connection(px, py) {
                    unconnected.push(json!({
                        "reference": inst.reference,
                        "value": inst.value,
                        "pin": pin.number,
                        "pin_name": pin.name,
                        "x": px,
                        "y": py
                    }));
                }
            }
        }
    }

    Ok(CallToolResult::json(&json!({
        "valid": unconnected.is_empty(),
        "unconnected_count": unconnected.len(),
        "unconnected_pins": unconnected
    })))
}
