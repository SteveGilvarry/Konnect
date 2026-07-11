//! `pcb_components` toolset — place, move, rotate, query, and array footprints on the PCB.
//!
//! Every tool follows the PR-#5 pattern: try the KiCAD IPC API first (live
//! board, undo-aware), fall back to editing/reading the `.kicad_pcb` file when
//! no IPC transport exists, and tag responses with `source: "ipc" | "file"`.
//! Write tools refuse to touch the file when a live session is reachable but
//! the IPC call failed (see `pcb_ipc::ipc_write_refused`).

use crate::mcp::protocol::{CallToolResult, ToolContent};
use crate::tool;
use crate::tools::pcb_file as pf;
use crate::tools::pcb_ipc::{ipc_write_refused, try_ipc, IpcAttempt};
use crate::tools::{get_path, require_f64, require_str, ToolContext, ToolDef};
use konnect_sexp::parser::SexpNode;
use konnect_sexp::writer::{apply_edits, write_atomic, SexpEdit};
use serde_json::json;

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "place_component",
            "Place a footprint on the PCB at the given position and layer. Uses KiCAD IPC when \
             available, otherwise inserts the library footprint into the .kicad_pcb file.",
            json!({
                "type": "object",
                "properties": {
                    "board":      { "type": "string" },
                    "footprint":  { "type": "string", "description": "Library:Footprint (e.g. 'Resistor_SMD:R_0402')" },
                    "reference":  { "type": "string", "description": "Reference designator" },
                    "x":          { "type": "number" },
                    "y":          { "type": "number" },
                    "rotation":   { "type": "number", "default": 0 },
                    "layer":      { "type": "string", "default": "F.Cu" }
                },
                "required": ["board", "footprint", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_place_component(args, ctx).await }
        ),
        tool!(
            "move_component",
            "Move a placed footprint to a new X/Y position (KiCAD IPC first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" },
                    "x":         { "type": "number" },
                    "y":         { "type": "number" }
                },
                "required": ["board", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_component(args, ctx).await }
        ),
        tool!(
            "rotate_component",
            "Set the rotation angle of a placed footprint (KiCAD IPC first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" },
                    "rotation":  { "type": "number", "description": "Rotation angle in degrees" }
                },
                "required": ["board", "reference", "rotation"]
            }),
            |args, ctx| async move { handle_rotate_component(args, ctx).await }
        ),
        tool!(
            "delete_component",
            "Remove a footprint from the board (KiCAD IPC first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_delete_component(args, ctx).await }
        ),
        tool!(
            "edit_component",
            "Update the value of a placed footprint, or report its current properties if no \
             value is given (KiCAD IPC first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" },
                    "value":     { "type": "string", "description": "New value string (optional)" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_edit_component(args, ctx).await }
        ),
        tool!(
            "find_component",
            "Find a footprint on the board by reference designator and return its position \
             (KiCAD IPC first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_find_component(args, ctx).await }
        ),
        tool!(
            "get_component_pads",
            "Return the pad positions and net assignments for a footprint (KiCAD IPC first, \
             file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_get_component_pads(args, ctx).await }
        ),
        tool!(
            "get_pad_position",
            "Return the board-space position of a specific pad number on a footprint (KiCAD \
             IPC first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "reference":   { "type": "string" },
                    "pad_number":  { "type": "string" }
                },
                "required": ["board", "reference", "pad_number"]
            }),
            |args, ctx| async move { handle_get_pad_position(args, ctx).await }
        ),
        tool!(
            "get_component_list",
            "List all footprints on the board with their positions, layers, and values \
             (KiCAD IPC first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_component_list(args, ctx).await }
        ),
        tool!(
            "place_component_array",
            "Place multiple copies of a footprint in a grid or line array (KiCAD IPC first, \
             file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":        { "type": "string" },
                    "footprint":    { "type": "string" },
                    "start_x":      { "type": "number" },
                    "start_y":      { "type": "number" },
                    "count_x":      { "type": "integer", "description": "Number of columns" },
                    "count_y":      { "type": "integer", "description": "Number of rows", "default": 1 },
                    "spacing_x":    { "type": "number", "description": "Column spacing in mm" },
                    "spacing_y":    { "type": "number", "description": "Row spacing in mm", "default": 0 },
                    "ref_prefix":   { "type": "string", "description": "Reference prefix (e.g. 'R')", "default": "U" },
                    "ref_start":    { "type": "integer", "description": "Starting reference number", "default": 1 }
                },
                "required": ["board", "footprint", "start_x", "start_y", "count_x", "spacing_x"]
            }),
            |args, ctx| async move { handle_place_array(args, ctx).await }
        ),
        tool!(
            "align_components",
            "Align multiple footprints along a common X or Y axis (KiCAD IPC first, file \
             fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "references":  { "type": "array", "items": { "type": "string" } },
                    "axis":        { "type": "string", "description": "'x' or 'y'", "default": "x" },
                    "value":       { "type": "number", "description": "Target coordinate to align to" }
                },
                "required": ["board", "references", "value"]
            }),
            |args, ctx| async move { handle_align_components(args, ctx).await }
        ),
        tool!(
            "duplicate_component",
            "Duplicate an existing footprint at a new position (KiCAD IPC first, file \
             fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":         { "type": "string" },
                    "reference":     { "type": "string", "description": "Reference to duplicate" },
                    "new_reference": { "type": "string", "description": "New reference designator" },
                    "x":             { "type": "number" },
                    "y":             { "type": "number" }
                },
                "required": ["board", "reference", "new_reference", "x", "y"]
            }),
            |args, ctx| async move { handle_duplicate_component(args, ctx).await }
        ),
        tool!(
            "get_board_2d_view",
            "Render the PCB as a 2-D image using kicad-cli and return it as a base64 PNG. \
             When a live KiCAD IPC session exists, renders a snapshot of the live board state \
             (source 'ipc'); otherwise renders the .kicad_pcb file (source 'file').",
            json!({
                "type": "object",
                "properties": {
                    "board":  { "type": "string" },
                    "layers": {
                        "type": "array",
                        "description": "Layers to include (empty = default copper + silkscreen)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_board_2d_view(args, ctx).await }
        ),
    ]
}

// ─── File-side helpers ────────────────────────────────────────────────────────

/// Find a footprint node in a parsed board by its Reference property.
fn file_find_footprint<'a>(tree: &'a SexpNode, reference: &str) -> Option<&'a SexpNode> {
    tree.find_all("footprint").into_iter().find(|fp| {
        fp.find_all("property").iter().any(|p| {
            p.get(1).and_then(|n| n.as_str()) == Some("Reference")
                && p.get(2).and_then(|n| n.as_str()) == Some(reference)
        })
    })
}

/// Summarize a footprint node in the same JSON shape the IPC path produces.
fn file_fp_summary(fp: &SexpNode) -> serde_json::Value {
    let get_prop = |name: &str| {
        fp.find_all("property")
            .iter()
            .find(|p| p.get(1).and_then(|n| n.as_str()) == Some(name))
            .and_then(|p| p.get(2))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string()
    };
    let at = fp.find("at");
    json!({
        "reference": get_prop("Reference"),
        "value": get_prop("Value"),
        "footprint": fp.get(1).and_then(|n| n.as_str()).unwrap_or(""),
        "x": at.and_then(|a| a.get_f64(1)).unwrap_or(0.0),
        "y": at.and_then(|a| a.get_f64(2)).unwrap_or(0.0),
        "rotation": at.and_then(|a| a.get_f64(3)).unwrap_or(0.0),
        "layer": fp.find("layer").and_then(|l| l.get(1)).and_then(|n| n.as_str()).unwrap_or("")
    })
}

/// Transform a footprint-local pad position into board space (rotation about
/// the footprint anchor) — shared by the IPC and file paths so both report
/// identical coordinates.
fn pad_to_board_space(fp_x: f64, fp_y: f64, fp_rot: f64, local_x: f64, local_y: f64) -> (f64, f64) {
    let rad = fp_rot.to_radians();
    (
        fp_x + local_x * rad.cos() - local_y * rad.sin(),
        fp_y + local_x * rad.sin() + local_y * rad.cos(),
    )
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_place_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let footprint = match require_str(args, "footprint") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let reference = match require_str(args, "reference") {
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
    let rotation = args["rotation"].as_f64().unwrap_or(0.0);
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();

    let fp_ipc = footprint.clone();
    let layer_ipc = layer.clone();
    match try_ipc(ctx, move |c| {
        c.place_footprint(&fp_ipc, x, y, rotation, &layer_ipc)
    })
    .await?
    {
        IpcAttempt::Ok(fp) => {
            return Ok(CallToolResult::json(&json!({
                "placed": reference,
                "footprint": fp.footprint,
                "x": fp.position.x, "y": fp.position.y,
                "rotation": fp.rotation, "layer": fp.layer,
                "source": "ipc"
            })))
        }
        IpcAttempt::Failed(msg) => return Ok(ipc_write_refused("place_component", &msg)),
        IpcAttempt::Unavailable(_) => {}
    }

    // File fallback: insert the resolved library footprint into the board.
    if layer.starts_with("B.") {
        return Ok(CallToolResult::error(
            "place_component file fallback cannot place on back-side layers (footprint \
             flipping requires KiCAD). Start KiCAD with the board open, or place on F.Cu.",
        ));
    }
    let mod_path = match pf::resolve_footprint_mod_path(&footprint) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(format!(
                "Footprint '{}' not found via fp-lib-table or standard library paths",
                footprint
            )))
        }
    };
    let mod_content = std::fs::read_to_string(&mod_path)?;
    let block = pf::footprint_sexp_for_board(&mod_content, &footprint, &reference, x, y, rotation)?;
    let content = std::fs::read_to_string(&board_path)?;
    let new_content = pf::append_to_board(content, block);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "placed": reference,
        "footprint": footprint,
        "x": x, "y": y,
        "rotation": rotation, "layer": layer,
        "source": "file"
    })))
}

async fn handle_move_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
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

    let ref_ipc = reference.clone();
    match try_ipc(ctx, move |c| c.move_footprint(&ref_ipc, x, y)).await? {
        IpcAttempt::Ok(()) => {
            return Ok(CallToolResult::json(
                &json!({ "moved": reference, "x": x, "y": y, "source": "ipc" }),
            ))
        }
        IpcAttempt::Failed(msg) => return Ok(ipc_write_refused("move_component", &msg)),
        IpcAttempt::Unavailable(_) => {}
    }

    let content = std::fs::read_to_string(&board_path)?;
    let span = match pf::find_footprint_block(&content, &reference) {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(format!(
                "Footprint '{}' not found",
                reference
            )))
        }
    };
    let edit = pf::footprint_move_edit(&content, span, x, y)?;
    let new_content = apply_edits(content, vec![edit]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "moved": reference, "x": x, "y": y, "source": "file" }),
    ))
}

async fn handle_rotate_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let rotation = match require_f64(args, "rotation") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let ref_ipc = reference.clone();
    match try_ipc(ctx, move |c| c.rotate_footprint(&ref_ipc, rotation)).await? {
        IpcAttempt::Ok(()) => {
            return Ok(CallToolResult::json(
                &json!({ "rotated": reference, "rotation": rotation, "source": "ipc" }),
            ))
        }
        IpcAttempt::Failed(msg) => return Ok(ipc_write_refused("rotate_component", &msg)),
        IpcAttempt::Unavailable(_) => {}
    }

    let content = std::fs::read_to_string(&board_path)?;
    let span = match pf::find_footprint_block(&content, &reference) {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(format!(
                "Footprint '{}' not found",
                reference
            )))
        }
    };
    let edits = pf::footprint_rotation_edits(&content, span, rotation)?;
    let new_content = apply_edits(content, edits);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "rotated": reference, "rotation": rotation, "source": "file" }),
    ))
}

async fn handle_delete_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let ref_ipc = reference.clone();
    match try_ipc(ctx, move |c| c.delete_footprint(&ref_ipc)).await? {
        IpcAttempt::Ok(()) => {
            return Ok(CallToolResult::json(
                &json!({ "deleted": reference, "source": "ipc" }),
            ))
        }
        IpcAttempt::Failed(msg) => return Ok(ipc_write_refused("delete_component", &msg)),
        IpcAttempt::Unavailable(_) => {}
    }

    let content = std::fs::read_to_string(&board_path)?;
    let (s, _) = match pf::find_footprint_block(&content, &reference) {
        Some(span) => span,
        None => {
            return Ok(CallToolResult::error(format!(
                "Footprint '{}' not found",
                reference
            )))
        }
    };
    let (ws, we) = konnect_sexp::writer::find_block_with_leading_whitespace(&content, s)
        .ok_or_else(|| anyhow::anyhow!("Unbalanced footprint block"))?;
    let new_content = apply_edits(content, vec![SexpEdit::delete(ws, we)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "deleted": reference, "source": "file" }),
    ))
}

async fn handle_edit_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let new_value = args["value"].as_str().map(String::from);

    let ref_ipc = reference.clone();
    let value_ipc = new_value.clone();
    let attempt = try_ipc(ctx, move |c| match &value_ipc {
        Some(v) => c.set_footprint_value(&ref_ipc, v),
        None => c
            .get_footprint(&ref_ipc)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", ref_ipc)),
    })
    .await?;
    match attempt {
        IpcAttempt::Ok(fp) => {
            return Ok(CallToolResult::json(&json!({
                "reference": fp.reference,
                "value": fp.value,
                "footprint": fp.footprint,
                "source": "ipc"
            })))
        }
        // Only refuse when the tool would WRITE; a pure query may fall back.
        IpcAttempt::Failed(msg) if new_value.is_some() => {
            return Ok(ipc_write_refused("edit_component", &msg))
        }
        IpcAttempt::Failed(_) | IpcAttempt::Unavailable(_) => {}
    }

    let content = std::fs::read_to_string(&board_path)?;
    match &new_value {
        Some(v) => {
            let span = match pf::find_footprint_block(&content, &reference) {
                Some(s) => s,
                None => {
                    return Ok(CallToolResult::error(format!(
                        "Footprint '{}' not found",
                        reference
                    )))
                }
            };
            let edit = pf::footprint_set_value_edit(&content, span, v)?;
            let new_content = apply_edits(content, vec![edit]);
            write_atomic(&board_path, &new_content)?;
            Ok(CallToolResult::json(&json!({
                "reference": reference,
                "value": v,
                "source": "file"
            })))
        }
        None => {
            let tree = konnect_sexp::parser::parse_sexp(&content)?;
            match file_find_footprint(&tree, &reference) {
                Some(fp) => {
                    let mut summary = file_fp_summary(fp);
                    summary["source"] = json!("file");
                    Ok(CallToolResult::json(&summary))
                }
                None => Ok(CallToolResult::error(format!(
                    "Footprint '{}' not found",
                    reference
                ))),
            }
        }
    }
}

async fn handle_find_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let ref_ipc = reference.clone();
    if let IpcAttempt::Ok(fp) = try_ipc(ctx, move |c| {
        c.get_footprint(&ref_ipc)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", ref_ipc))
    })
    .await?
    {
        return Ok(CallToolResult::json(&json!({
            "reference": fp.reference,
            "value": fp.value,
            "footprint": fp.footprint,
            "x": fp.position.x, "y": fp.position.y,
            "rotation": fp.rotation, "layer": fp.layer,
            "source": "ipc"
        })));
    }

    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;
    match file_find_footprint(&tree, &reference) {
        Some(fp) => {
            let mut summary = file_fp_summary(fp);
            summary["source"] = json!("file");
            Ok(CallToolResult::json(&summary))
        }
        None => Ok(CallToolResult::error(format!(
            "Footprint '{}' not found",
            reference
        ))),
    }
}

async fn handle_get_component_pads(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    // IPC first: pads from the live board (positions footprint-relative).
    let ref_ipc = reference.clone();
    if let IpcAttempt::Ok(Some((fp, pads))) =
        try_ipc(ctx, move |c| c.get_footprint_pads(&ref_ipc)).await?
    {
        let items: Vec<serde_json::Value> = pads
            .iter()
            .map(|p| {
                let (bx, by) = pad_to_board_space(
                    fp.position.x,
                    fp.position.y,
                    fp.rotation,
                    p.position.x,
                    p.position.y,
                );
                json!({ "number": p.number, "x": bx, "y": by, "net": p.net })
            })
            .collect();
        return Ok(CallToolResult::json(&json!({
            "reference": reference,
            "pad_count": items.len(),
            "pads": items,
            "source": "ipc"
        })));
    }

    // File fallback.
    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;
    let fp_node = match file_find_footprint(&tree, &reference) {
        Some(n) => n,
        None => {
            return Ok(CallToolResult::error(format!(
                "Footprint '{}' not found",
                reference
            )))
        }
    };

    let fp_at = fp_node.find("at");
    let fp_x = fp_at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
    let fp_y = fp_at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);
    let fp_rot = fp_at.and_then(|a| a.get_f64(3)).unwrap_or(0.0);

    let pads: Vec<serde_json::Value> = fp_node
        .find_all("pad")
        .iter()
        .filter_map(|pad| {
            let number = pad.get(1)?.as_str()?.to_string();
            let pad_at = pad.find("at")?;
            let local_x = pad_at.get_f64(1)?;
            let local_y = pad_at.get_f64(2)?;
            let (board_x, board_y) = pad_to_board_space(fp_x, fp_y, fp_rot, local_x, local_y);
            let net = pad
                .find("net")
                .and_then(|n| n.get(2))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            Some(json!({ "number": number, "x": board_x, "y": board_y, "net": net }))
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "pad_count": pads.len(),
        "pads": pads,
        "source": "file"
    })))
}

async fn handle_get_pad_position(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let pad_number = match require_str(args, "pad_number") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pads_result = handle_get_component_pads(args, ctx).await?;
    // Parse the result and filter for the specific pad number
    if let Some(ToolContent::Text { text }) = pads_result.content.first() {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(pads) = parsed["pads"].as_array() {
                if let Some(pad) = pads
                    .iter()
                    .find(|p| p["number"].as_str() == Some(&pad_number))
                {
                    let mut pad = pad.clone();
                    pad["source"] = parsed["source"].clone();
                    return Ok(CallToolResult::json(&pad));
                }
            }
        }
    }
    Ok(CallToolResult::error(format!(
        "Pad '{}' not found",
        pad_number
    )))
}

async fn handle_get_component_list(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;

    if let IpcAttempt::Ok(fps) = try_ipc(ctx, |c| c.list_footprints()).await? {
        let items: Vec<serde_json::Value> = fps
            .iter()
            .map(|fp| {
                json!({
                    "reference": fp.reference,
                    "value": fp.value,
                    "footprint": fp.footprint,
                    "x": fp.position.x, "y": fp.position.y,
                    "rotation": fp.rotation, "layer": fp.layer
                })
            })
            .collect();
        return Ok(CallToolResult::json(
            &json!({ "count": items.len(), "components": items, "source": "ipc" }),
        ));
    }

    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;
    let items: Vec<serde_json::Value> = tree
        .find_all("footprint")
        .iter()
        .map(|fp| file_fp_summary(fp))
        .collect();
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "components": items, "source": "file" }),
    ))
}

async fn handle_place_array(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let footprint = match require_str(args, "footprint") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let start_x = match require_f64(args, "start_x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let start_y = match require_f64(args, "start_y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let count_x = args["count_x"].as_u64().unwrap_or(1) as usize;
    let count_y = args["count_y"].as_u64().unwrap_or(1) as usize;
    let spacing_x = match require_f64(args, "spacing_x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let spacing_y = args["spacing_y"].as_f64().unwrap_or(spacing_x);
    let prefix = args["ref_prefix"].as_str().unwrap_or("U").to_string();
    let ref_start = args["ref_start"].as_u64().unwrap_or(1) as usize;

    let positions: Vec<(String, f64, f64)> = (0..count_y)
        .flat_map(|row| (0..count_x).map(move |col| (row, col)))
        .enumerate()
        .map(|(i, (row, col))| {
            (
                format!("{prefix}{}", ref_start + i),
                start_x + col as f64 * spacing_x,
                start_y + row as f64 * spacing_y,
            )
        })
        .collect();

    // Choose the backend once for the whole array.
    match try_ipc(ctx, |c| c.get_open_documents()).await? {
        IpcAttempt::Ok(_) => {
            let mut placed = Vec::new();
            for (reference, x, y) in &positions {
                let fp_id = footprint.clone();
                let (x, y) = (*x, *y);
                match try_ipc(ctx, move |c| c.place_footprint(&fp_id, x, y, 0.0, "F.Cu")).await? {
                    IpcAttempt::Ok(fp) => placed.push(
                        json!({ "reference": reference, "x": fp.position.x, "y": fp.position.y }),
                    ),
                    IpcAttempt::Failed(e) | IpcAttempt::Unavailable(e) => {
                        return Ok(CallToolResult::error(format!(
                            "IPC error placing {}: {}",
                            reference, e
                        )))
                    }
                }
            }
            Ok(CallToolResult::json(
                &json!({ "placed_count": placed.len(), "components": placed, "source": "ipc" }),
            ))
        }
        IpcAttempt::Failed(msg) => Ok(ipc_write_refused("place_component_array", &msg)),
        IpcAttempt::Unavailable(_) => {
            let mod_path = match pf::resolve_footprint_mod_path(&footprint) {
                Some(p) => p,
                None => {
                    return Ok(CallToolResult::error(format!(
                        "Footprint '{}' not found via fp-lib-table or standard library paths",
                        footprint
                    )))
                }
            };
            let mod_content = std::fs::read_to_string(&mod_path)?;
            let mut blocks = String::new();
            let mut placed = Vec::new();
            for (reference, x, y) in &positions {
                blocks.push_str(&pf::footprint_sexp_for_board(
                    &mod_content,
                    &footprint,
                    reference,
                    *x,
                    *y,
                    0.0,
                )?);
                placed.push(json!({ "reference": reference, "x": x, "y": y }));
            }
            let content = std::fs::read_to_string(&board_path)?;
            let new_content = pf::append_to_board(content, blocks);
            write_atomic(&board_path, &new_content)?;
            Ok(CallToolResult::json(
                &json!({ "placed_count": placed.len(), "components": placed, "source": "file" }),
            ))
        }
    }
}

async fn handle_align_components(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let refs: Vec<String> = args["references"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.as_str().map(String::from))
        .collect();
    let axis = args["axis"].as_str().unwrap_or("x").to_string();
    let value = match require_f64(args, "value") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    match try_ipc(ctx, |c| c.get_open_documents()).await? {
        IpcAttempt::Ok(_) => {
            let mut aligned = Vec::new();
            for reference in &refs {
                let ref_ipc = reference.clone();
                let axis_ipc = axis.clone();
                let res = try_ipc(ctx, move |c| {
                    let fp = c
                        .get_footprint(&ref_ipc)?
                        .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", ref_ipc))?;
                    let (nx, ny) = if axis_ipc == "y" {
                        (fp.position.x, value)
                    } else {
                        (value, fp.position.y)
                    };
                    c.move_footprint(&ref_ipc, nx, ny)?;
                    Ok((nx, ny))
                })
                .await?;
                match res {
                    IpcAttempt::Ok((nx, ny)) => {
                        aligned.push(json!({ "reference": reference, "x": nx, "y": ny }))
                    }
                    IpcAttempt::Failed(e) | IpcAttempt::Unavailable(e) => {
                        return Ok(CallToolResult::error(format!("IPC error: {}", e)))
                    }
                }
            }
            Ok(CallToolResult::json(
                &json!({ "aligned_count": aligned.len(), "components": aligned, "source": "ipc" }),
            ))
        }
        IpcAttempt::Failed(msg) => Ok(ipc_write_refused("align_components", &msg)),
        IpcAttempt::Unavailable(_) => {
            let content = std::fs::read_to_string(&board_path)?;
            let mut edits = Vec::new();
            let mut aligned = Vec::new();
            for reference in &refs {
                let span = match pf::find_footprint_block(&content, reference) {
                    Some(s) => s,
                    None => {
                        return Ok(CallToolResult::error(format!(
                            "Footprint '{}' not found",
                            reference
                        )))
                    }
                };
                let block = &content[span.0..span.1];
                let (a_s, a_e) = match pf::anchor_at_span(block) {
                    Some(s) => s,
                    None => {
                        return Ok(CallToolResult::error(format!(
                            "Footprint '{}' has no (at) anchor",
                            reference
                        )))
                    }
                };
                let (cx, cy, _) = pf::parse_at(&block[a_s..a_e]);
                let (nx, ny) = if axis == "y" { (cx, value) } else { (value, cy) };
                edits.push(pf::footprint_move_edit(&content, span, nx, ny)?);
                aligned.push(json!({ "reference": reference, "x": nx, "y": ny }));
            }
            let new_content = apply_edits(content, edits);
            write_atomic(&board_path, &new_content)?;
            Ok(CallToolResult::json(
                &json!({ "aligned_count": aligned.len(), "components": aligned, "source": "file" }),
            ))
        }
    }
}

async fn handle_duplicate_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let new_reference = match require_str(args, "new_reference") {
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

    // IPC path: place a fresh copy of the source footprint's library part.
    let ref_ipc = reference.clone();
    match try_ipc(ctx, move |c| {
        let src = c
            .get_footprint(&ref_ipc)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", ref_ipc))?;
        c.place_footprint(&src.footprint, x, y, src.rotation, &src.layer)
    })
    .await?
    {
        IpcAttempt::Ok(fp) => {
            return Ok(CallToolResult::json(&json!({
                "duplicated_from": reference,
                "new_reference": new_reference,
                "x": fp.position.x, "y": fp.position.y,
                "source": "ipc"
            })))
        }
        IpcAttempt::Failed(msg) => return Ok(ipc_write_refused("duplicate_component", &msg)),
        IpcAttempt::Unavailable(_) => {}
    }

    // File fallback: copy the block, refresh identity, move the anchor.
    let content = std::fs::read_to_string(&board_path)?;
    let span = match pf::find_footprint_block(&content, &reference) {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(format!(
                "Footprint '{}' not found",
                reference
            )))
        }
    };
    let block = content[span.0..span.1].to_string();
    let block = pf::set_reference_in_block(&block, &new_reference)?;
    let block = pf::regenerate_uuids(&block);
    let edit = pf::footprint_move_edit(&block, (0, block.len()), x, y)?;
    let block = apply_edits(block, vec![edit]);
    let new_content = pf::append_to_board(content, format!("\n  {}", block));
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "duplicated_from": reference,
        "new_reference": new_reference,
        "x": x, "y": y,
        "source": "file"
    })))
}

async fn handle_get_board_2d_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    use base64::Engine;
    let board_path = get_path(args, "board")?;
    let layers: Vec<String> = args["layers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                "F.Cu".into(),
                "B.Cu".into(),
                "F.SilkS".into(),
                "B.SilkS".into(),
                "Edge.Cuts".into(),
            ]
        });

    // IPC first: snapshot the LIVE board to a temp copy so the render shows
    // IPC-edited state instead of the possibly-stale file on disk.
    let live_copy = std::env::temp_dir().join(format!(
        "konnect-live-render-{}.kicad_pcb",
        std::process::id()
    ));
    let live_copy_str = live_copy.to_string_lossy().to_string();
    let (render_src, source) =
        match try_ipc(ctx, move |c| c.save_copy_of_board(&live_copy_str)).await? {
            IpcAttempt::Ok(()) => (live_copy.clone(), "ipc"),
            // Rendering is a read: fall back to the file on any IPC failure.
            IpcAttempt::Failed(_) | IpcAttempt::Unavailable(_) => (board_path.clone(), "file"),
        };

    let tmp = render_src.with_extension("render.png");
    let layer_refs: Vec<&str> = layers.iter().map(String::as_str).collect();
    let render_result =
        super::cli::render_pcb_png(&ctx.config.kicad_cli, &render_src, &tmp, &layer_refs).await;
    if source == "ipc" {
        let _ = tokio::fs::remove_file(&live_copy).await;
    }
    render_result?;
    let bytes = tokio::fs::read(&tmp).await?;
    let _ = tokio::fs::remove_file(&tmp).await;

    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let mut result = CallToolResult::image(b64, "image/png");
    result.content.push(ToolContent::Text {
        text: json!({ "source": source }).to_string(),
    });
    Ok(result)
}
