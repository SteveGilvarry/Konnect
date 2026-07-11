//! `pcb_routing` toolset — traces, vias, copper pours, nets, netclasses, and diff pairs.
//!
//! Routing tools follow the PR-#5 pattern: KiCAD IPC first, file fallback
//! when no IPC transport exists, `source: "ipc" | "file"` in every response.
//! Write tools refuse the file fallback when a live session is reachable but
//! the IPC call failed (see `pcb_ipc`). `add_net`, `add_copper_pour`,
//! `create_netclass`, and `assign_net_to_class` remain file-only (the KiCAD
//! IPC API exposes no net/netclass mutation commands) and report
//! `source: "file"` explicitly.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::pcb_file as pf;
use crate::tools::pcb_ipc::{ipc_write_refused, try_ipc, IpcAttempt};
use crate::tools::{get_path, require_f64, require_str, ToolContext, ToolDef};
use konnect_sexp::writer::{apply_edits, new_uuid, write_atomic, SexpEdit};
use serde_json::json;

// ─── S-expression helpers ─────────────────────────────────────────────────────

fn format_zone(
    net_id: i32,
    net_name: &str,
    layer: &str,
    clearance: f64,
    min_w: f64,
    pts: &[(f64, f64)],
) -> String {
    let uuid = new_uuid();
    let pt_str: String = pts
        .iter()
        .map(|(x, y)| format!("\n      (xy {x} {y})"))
        .collect();
    format!(
        "\n  (zone (net {net_id}) (net_name \"{net_name}\") (layer \"{layer}\") (uuid \"{uuid}\")\n    \
         (hatch edge 0.508)\n    (connect_pads (clearance {clearance}))\n    \
         (min_thickness {min_w})\n    (fill yes)\n    \
         (polygon (pts{pt_str}\n    ))\n  )"
    )
}

fn find_net_id(content: &str, net_name: &str) -> i32 {
    // Match ` "<name>")` and read the number between the preceding `(net `
    // and the match. NOTE: `before` ends exactly at the space before the
    // quoted name, so the number runs to the END of `before` — the previous
    // implementation searched for a trailing space that never exists and
    // always parsed an empty string (net id 0 for every net).
    let search = format!(r#" "{net_name}")"#);
    if let Some(pos) = content.find(&search) {
        let before = &content[..pos];
        match before.rfind("(net ") {
            Some(net_pos) => before[net_pos + 5..].trim().parse().unwrap_or(0),
            None => 0,
        }
    } else {
        0
    }
}

/// Resolve a net name to its file net id, erroring (with a hint) when the net
/// doesn't exist — inserting copper with a wrong net id corrupts connectivity.
fn require_file_net_id(content: &str, net_name: &str) -> Result<i32, CallToolResult> {
    let id = find_net_id(content, net_name);
    if id == 0 && !net_name.is_empty() {
        return Err(CallToolResult::error(format!(
            "Net '{}' not found in the board file. Add it first with add_net.",
            net_name
        )));
    }
    Ok(id)
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "add_net",
            "Add a new net entry to the PCB file (S-expression insert, file-based only — the \
             KiCAD IPC API has no net-creation command).",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string" }
                },
                "required": ["board", "net_name"]
            }),
            |args, ctx| async move { handle_add_net(args, ctx).await }
        ),
        tool!(
            "route_trace",
            "Route a trace segment between two points on a copper layer (KiCAD IPC first, \
             file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string" },
                    "layer":    { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width": { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "layer", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_route_trace(args, ctx).await }
        ),
        tool!(
            "route_pad_to_pad",
            "Route a direct trace between two pads of named components (L-bend routing; KiCAD \
             IPC first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "net_name":    { "type": "string" },
                    "ref1":        { "type": "string", "description": "First component reference" },
                    "pad1":        { "type": "string", "description": "First pad number" },
                    "ref2":        { "type": "string", "description": "Second component reference" },
                    "pad2":        { "type": "string", "description": "Second pad number" },
                    "layer":       { "type": "string", "default": "F.Cu" },
                    "width":       { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "ref1", "pad1", "ref2", "pad2"]
            }),
            |args, ctx| async move { handle_route_pad_to_pad(args, ctx).await }
        ),
        tool!(
            "add_via",
            "Add a through-hole via at a given position and assign it to a net (KiCAD IPC \
             first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "net_name":  { "type": "string" },
                    "x":         { "type": "number" },
                    "y":         { "type": "number" },
                    "drill":     { "type": "number", "description": "Drill diameter in mm", "default": 0.4 },
                    "pad_size":  { "type": "number", "description": "Via pad diameter in mm", "default": 0.8 }
                },
                "required": ["board", "net_name", "x", "y"]
            }),
            |args, ctx| async move { handle_add_via(args, ctx).await }
        ),
        tool!(
            "add_copper_pour",
            "Add a copper fill zone polygon on a layer/net via S-expression file insert \
             (file-based only — zone protobufs were deferred upstream).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "net_name":  { "type": "string" },
                    "layer":     { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
                    "points": {
                        "type": "array",
                        "items": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } } }
                    },
                    "clearance": { "type": "number", "default": 0.2 },
                    "min_width": { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "layer", "points"]
            }),
            |args, ctx| async move { handle_add_copper_pour(args, ctx).await }
        ),
        tool!(
            "delete_trace",
            "Delete a trace segment (or via/arc) identified by its UUID (KiCAD IPC first, \
             file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "uuid":  { "type": "string", "description": "UUID of the track segment to delete" }
                },
                "required": ["board", "uuid"]
            }),
            |args, ctx| async move { handle_delete_trace(args, ctx).await }
        ),
        tool!(
            "query_traces",
            "List trace segments on the board, optionally filtered by net and/or layer \
             (KiCAD IPC first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string", "description": "Filter by net (optional)" },
                    "layer":    { "type": "string", "description": "Filter by layer (optional)" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_query_traces(args, ctx).await }
        ),
        tool!(
            "get_nets_list",
            "Return all nets defined on the PCB (KiCAD IPC first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_nets_list(args, ctx).await }
        ),
        tool!(
            "modify_trace",
            "Modify a trace segment by deleting and re-adding it with new parameters (KiCAD \
             IPC first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "uuid":      { "type": "string" },
                    "net_name":  { "type": "string" },
                    "layer":     { "type": "string" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width":     { "type": "number", "default": 0.25 }
                },
                "required": ["board", "uuid", "net_name", "layer", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_modify_trace(args, ctx).await }
        ),
        tool!(
            "create_netclass",
            "Add a netclass definition to the board's design rules (S-expression file insert, \
             file-based only — the KiCAD IPC API has no netclass mutation command).",
            json!({
                "type": "object",
                "properties": {
                    "board":        { "type": "string" },
                    "name":         { "type": "string", "description": "Netclass name (e.g. 'Power')" },
                    "clearance":    { "type": "number", "description": "Clearance in mm", "default": 0.2 },
                    "trace_width":  { "type": "number", "description": "Default trace width in mm", "default": 0.25 },
                    "via_drill":    { "type": "number", "description": "Via drill diameter in mm", "default": 0.4 },
                    "via_diameter": { "type": "number", "description": "Via pad diameter in mm", "default": 0.8 }
                },
                "required": ["board", "name"]
            }),
            |args, ctx| async move { handle_create_netclass(args, ctx).await }
        ),
        tool!(
            "assign_net_to_class",
            "Assign a net to an existing netclass in the PCB file (S-expression edit, \
             file-based only — the KiCAD IPC API has no netclass mutation command).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string", "description": "Path to .kicad_pcb file" },
                    "net_name":  { "type": "string", "description": "Net name to assign" },
                    "netclass":  { "type": "string", "description": "Netclass name to assign the net to" }
                },
                "required": ["board", "net_name", "netclass"]
            }),
            |args, ctx| async move { handle_assign_net_to_class(args, ctx).await }
        ),
        tool!(
            "route_differential_pair",
            "Route a differential pair (two parallel traces with a specified gap; KiCAD IPC \
             first, file fallback).",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_pos":  { "type": "string", "description": "Positive net name" },
                    "net_neg":  { "type": "string", "description": "Negative net name" },
                    "layer":    { "type": "string", "default": "F.Cu" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width": { "type": "number", "default": 0.1 },
                    "gap":   { "type": "number", "description": "Gap between pair traces in mm", "default": 0.1 }
                },
                "required": ["board", "net_pos", "net_neg", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_route_diff_pair(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_add_net(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    // Count existing nets to determine next net ID
    let net_id = content.matches("(net ").count() as i32;
    let net_sexp = format!("\n  (net {net_id} \"{net_name}\")");
    let new_content = pf::append_to_board(content, net_sexp);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "net_id": net_id, "net_name": net_name, "source": "file" }),
    ))
}

async fn handle_route_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
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
    let width = args["width"].as_f64().unwrap_or(0.25);

    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();
    match try_ipc(ctx, move |c| {
        c.add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2)
    })
    .await?
    {
        IpcAttempt::Ok(()) => {
            return Ok(CallToolResult::json(&json!({
                "net": net_name, "layer": layer, "width": width,
                "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 },
                "source": "ipc"
            })))
        }
        IpcAttempt::Failed(msg) => return Ok(ipc_write_refused("route_trace", &msg)),
        IpcAttempt::Unavailable(_) => {}
    }

    let content = std::fs::read_to_string(&board_path)?;
    let net_id = match require_file_net_id(&content, &net_name) {
        Ok(id) => id,
        Err(e) => return Ok(e),
    };
    let segment = pf::format_segment(x1, y1, x2, y2, width, &layer, net_id);
    let new_content = pf::append_to_board(content, segment);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "net": net_name, "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 },
        "source": "file"
    })))
}

async fn handle_route_pad_to_pad(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref1 = match require_str(args, "ref1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pad1 = match require_str(args, "pad1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref2 = match require_str(args, "ref2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pad2 = match require_str(args, "pad2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();
    let width = args["width"].as_f64().unwrap_or(0.25);

    // IPC path: pad positions AND track creation both come from the live
    // board, so a footprint moved over IPC routes correctly. (The old code
    // read pad positions from the file while writing tracks over IPC — a
    // split-brain within a single tool.)
    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();
    let (r1, p1, r2, p2) = (ref1.clone(), pad1.clone(), ref2.clone(), pad2.clone());
    match try_ipc(ctx, move |c| {
        let pad_pos = |reference: &str, pad_number: &str| -> anyhow::Result<(f64, f64)> {
            let (fp, pads) = c
                .get_footprint_pads(reference)?
                .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", reference))?;
            let pad = pads
                .iter()
                .find(|p| p.number == pad_number)
                .ok_or_else(|| anyhow::anyhow!("Pad '{}' not found on '{}'", pad_number, reference))?;
            let rad = fp.rotation.to_radians();
            Ok((
                fp.position.x + pad.position.x * rad.cos() - pad.position.y * rad.sin(),
                fp.position.y + pad.position.x * rad.sin() + pad.position.y * rad.cos(),
            ))
        };
        let (x1, y1) = pad_pos(&r1, &p1)?;
        let (x2, y2) = pad_pos(&r2, &p2)?;
        if (x1 - x2).abs() < 0.01 || (y1 - y2).abs() < 0.01 {
            c.add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2)?;
        } else {
            c.add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y1)?;
            c.add_track(&net_ipc, &layer_ipc, width, x2, y1, x2, y2)?;
        }
        Ok(((x1, y1), (x2, y2)))
    })
    .await?
    {
        IpcAttempt::Ok(((x1, y1), (x2, y2))) => {
            return Ok(CallToolResult::json(&json!({
                "routed": true,
                "net": net_name, "layer": layer, "width": width,
                "from": { "ref": ref1, "pad": pad1, "x": x1, "y": y1 },
                "to":   { "ref": ref2, "pad": pad2, "x": x2, "y": y2 },
                "source": "ipc"
            })))
        }
        IpcAttempt::Failed(msg) => return Ok(ipc_write_refused("route_pad_to_pad", &msg)),
        IpcAttempt::Unavailable(_) => {}
    }

    // File fallback: pad positions and segments both from/into the file.
    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;
    let (x1, y1) = find_pad_board_position(&tree, &ref1, &pad1)?;
    let (x2, y2) = find_pad_board_position(&tree, &ref2, &pad2)?;
    let net_id = match require_file_net_id(&content, &net_name) {
        Ok(id) => id,
        Err(e) => return Ok(e),
    };

    let mut segments = String::new();
    if (x1 - x2).abs() < 0.01 || (y1 - y2).abs() < 0.01 {
        segments.push_str(&pf::format_segment(x1, y1, x2, y2, width, &layer, net_id));
    } else {
        segments.push_str(&pf::format_segment(x1, y1, x2, y1, width, &layer, net_id));
        segments.push_str(&pf::format_segment(x2, y1, x2, y2, width, &layer, net_id));
    }
    let new_content = pf::append_to_board(content, segments);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "routed": true,
        "net": net_name, "layer": layer, "width": width,
        "from": { "ref": ref1, "pad": pad1, "x": x1, "y": y1 },
        "to":   { "ref": ref2, "pad": pad2, "x": x2, "y": y2 },
        "source": "file"
    })))
}

/// Look up a pad's board-space (x, y) position from the parsed PCB S-expression tree.
fn find_pad_board_position(
    tree: &konnect_sexp::parser::SexpNode,
    reference: &str,
    pad_number: &str,
) -> anyhow::Result<(f64, f64)> {
    let fp_node = tree
        .find_all("footprint")
        .into_iter()
        .find(|fp| {
            fp.find_all("property").iter().any(|p| {
                p.get(1).and_then(|n| n.as_str()) == Some("Reference")
                    && p.get(2).and_then(|n| n.as_str()) == Some(reference)
            })
        })
        .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found on board", reference))?;

    let fp_at = fp_node.find("at");
    let fp_x = fp_at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
    let fp_y = fp_at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);
    let fp_rot = fp_at.and_then(|a| a.get_f64(3)).unwrap_or(0.0);

    let pad = fp_node
        .find_all("pad")
        .into_iter()
        .find(|p| p.get(1).and_then(|n| n.as_str()) == Some(pad_number))
        .ok_or_else(|| anyhow::anyhow!("Pad '{}' not found on '{}'", pad_number, reference))?;

    let pad_at = pad
        .find("at")
        .ok_or_else(|| anyhow::anyhow!("Pad has no (at) node"))?;
    let local_x = pad_at.get_f64(1).unwrap_or(0.0);
    let local_y = pad_at.get_f64(2).unwrap_or(0.0);

    // Transform local pad coords to board space (rotation)
    let rad = fp_rot.to_radians();
    let board_x = fp_x + local_x * rad.cos() - local_y * rad.sin();
    let board_y = fp_y + local_x * rad.sin() + local_y * rad.cos();

    Ok((board_x, board_y))
}

async fn handle_add_via(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
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
    let drill = args["drill"].as_f64().unwrap_or(0.4);
    let pad_size = args["pad_size"].as_f64().unwrap_or(0.8);

    let net_ipc = net_name.clone();
    match try_ipc(ctx, move |c| c.add_via(&net_ipc, x, y, drill, pad_size)).await? {
        IpcAttempt::Ok(()) => {
            return Ok(CallToolResult::json(&json!({
                "net": net_name, "x": x, "y": y, "drill": drill, "pad_size": pad_size,
                "source": "ipc"
            })))
        }
        IpcAttempt::Failed(msg) => return Ok(ipc_write_refused("add_via", &msg)),
        IpcAttempt::Unavailable(_) => {}
    }

    let content = std::fs::read_to_string(&board_path)?;
    let net_id = match require_file_net_id(&content, &net_name) {
        Ok(id) => id,
        Err(e) => return Ok(e),
    };
    let via = pf::format_via(x, y, pad_size, drill, net_id);
    let new_content = pf::append_to_board(content, via);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "net": net_name, "x": x, "y": y, "drill": drill, "pad_size": pad_size,
        "source": "file"
    })))
}

async fn handle_add_copper_pour(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let clearance = args["clearance"].as_f64().unwrap_or(0.2);
    let min_w = args["min_width"].as_f64().unwrap_or(0.25);
    let pts_arr = match args["points"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'points' array")),
    };

    let pts: Vec<(f64, f64)> = pts_arr
        .iter()
        .filter_map(|p| Some((p["x"].as_f64()?, p["y"].as_f64()?)))
        .collect();
    if pts.len() < 3 {
        return Ok(CallToolResult::error("Zone requires at least 3 points"));
    }

    let content = std::fs::read_to_string(&board_path)?;
    let net_id = find_net_id(&content, &net_name);
    let zone_s = format_zone(net_id, &net_name, &layer, clearance, min_w, &pts);
    let new_content = pf::append_to_board(content, zone_s);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "net": net_name, "layer": layer, "points": pts.len(), "source": "file" }),
    ))
}

async fn handle_delete_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let uuid_ipc = uuid.clone();
    match try_ipc(ctx, move |c| c.delete_track(&uuid_ipc)).await? {
        IpcAttempt::Ok(()) => {
            return Ok(CallToolResult::json(
                &json!({ "deleted_uuid": uuid, "source": "ipc" }),
            ))
        }
        IpcAttempt::Failed(msg) => return Ok(ipc_write_refused("delete_trace", &msg)),
        IpcAttempt::Unavailable(_) => {}
    }

    let content = std::fs::read_to_string(&board_path)?;
    let (s, _) = match pf::find_block_by_uuid(&content, &["segment", "via", "arc"], &uuid) {
        Some(span) => span,
        None => {
            return Ok(CallToolResult::error(format!(
                "No track segment, via, or arc with uuid '{}' found",
                uuid
            )))
        }
    };
    let (ws, we) = konnect_sexp::writer::find_block_with_leading_whitespace(&content, s)
        .ok_or_else(|| anyhow::anyhow!("Unbalanced track block"))?;
    let new_content = apply_edits(content, vec![SexpEdit::delete(ws, we)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "deleted_uuid": uuid, "source": "file" }),
    ))
}

async fn handle_query_traces(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net = args["net_name"].as_str().map(String::from);
    let layer = args["layer"].as_str().map(String::from);

    let net_ipc = net.clone();
    let layer_ipc = layer.clone();
    if let IpcAttempt::Ok(tracks) = try_ipc(ctx, move |c| {
        c.get_tracks(net_ipc.as_deref(), layer_ipc.as_deref())
    })
    .await?
    {
        let items: Vec<serde_json::Value> = tracks
            .iter()
            .map(|t| {
                json!({
                    "net": t.net_name, "layer": t.layer, "width": t.width,
                    "x1": t.start.x, "y1": t.start.y,
                    "x2": t.end.x,   "y2": t.end.y
                })
            })
            .collect();
        return Ok(CallToolResult::json(
            &json!({ "count": items.len(), "traces": items, "source": "ipc" }),
        ));
    }

    // File fallback: parse (segment ...) blocks; net ids map to names via the
    // top-level (net <id> "<name>") declarations.
    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;
    let net_names: std::collections::HashMap<i32, String> = tree
        .find_all("net")
        .iter()
        .filter_map(|n| {
            Some((
                n.get_f64(1)? as i32,
                n.get(2)?.as_str().unwrap_or("").to_string(),
            ))
        })
        .collect();

    let items: Vec<serde_json::Value> = tree
        .find_all("segment")
        .iter()
        .filter_map(|seg| {
            let start = seg.find("start")?;
            let end = seg.find("end")?;
            let seg_layer = seg
                .find("layer")
                .and_then(|l| l.get(1))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let net_id = seg.find("net").and_then(|n| n.get_f64(1)).unwrap_or(0.0) as i32;
            let net_name = net_names.get(&net_id).cloned().unwrap_or_default();
            if let Some(nf) = &net {
                if &net_name != nf {
                    return None;
                }
            }
            if let Some(lf) = &layer {
                if &seg_layer != lf {
                    return None;
                }
            }
            Some(json!({
                "net": net_name, "layer": seg_layer,
                "width": seg.find("width").and_then(|w| w.get_f64(1)).unwrap_or(0.0),
                "x1": start.get_f64(1)?, "y1": start.get_f64(2)?,
                "x2": end.get_f64(1)?,   "y2": end.get_f64(2)?,
                "uuid": seg.find("uuid").and_then(|u| u.get(1)).and_then(|n| n.as_str()).unwrap_or("")
            }))
        })
        .collect();

    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "traces": items, "source": "file" }),
    ))
}

async fn handle_get_nets_list(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;

    if let IpcAttempt::Ok(nets) = try_ipc(ctx, |c| c.get_nets()).await? {
        let items: Vec<serde_json::Value> = nets
            .iter()
            .map(|n| json!({ "name": n.name, "netcode": n.netcode }))
            .collect();
        return Ok(CallToolResult::json(
            &json!({ "count": items.len(), "nets": items, "source": "ipc" }),
        ));
    }

    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;
    let items: Vec<serde_json::Value> = tree
        .find_all("net")
        .iter()
        .filter_map(|n| {
            Some(json!({
                "name": n.get(2)?.as_str().unwrap_or(""),
                "netcode": n.get_f64(1)? as i32
            }))
        })
        .collect();
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "nets": items, "source": "file" }),
    ))
}

async fn handle_modify_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
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
    let width = args["width"].as_f64().unwrap_or(0.25);

    let uuid_ipc = uuid.clone();
    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();
    match try_ipc(ctx, move |c| {
        c.delete_track(&uuid_ipc)?;
        c.add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2)
    })
    .await?
    {
        IpcAttempt::Ok(()) => {
            return Ok(CallToolResult::json(&json!({
                "modified_uuid": uuid,
                "net": net_name, "layer": layer, "width": width,
                "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 },
                "source": "ipc"
            })))
        }
        IpcAttempt::Failed(msg) => return Ok(ipc_write_refused("modify_trace", &msg)),
        IpcAttempt::Unavailable(_) => {}
    }

    let content = std::fs::read_to_string(&board_path)?;
    let net_id = match require_file_net_id(&content, &net_name) {
        Ok(id) => id,
        Err(e) => return Ok(e),
    };
    let (s, _) = match pf::find_block_by_uuid(&content, &["segment", "via", "arc"], &uuid) {
        Some(span) => span,
        None => {
            return Ok(CallToolResult::error(format!(
                "No track segment, via, or arc with uuid '{}' found",
                uuid
            )))
        }
    };
    let (ws, we) = konnect_sexp::writer::find_block_with_leading_whitespace(&content, s)
        .ok_or_else(|| anyhow::anyhow!("Unbalanced track block"))?;
    let segment = pf::format_segment(x1, y1, x2, y2, width, &layer, net_id);
    let close = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(
        content,
        vec![SexpEdit::delete(ws, we), SexpEdit::insert(close, segment)],
    );
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "modified_uuid": uuid,
        "net": net_name, "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 },
        "source": "file"
    })))
}

async fn handle_create_netclass(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let name = match require_str(args, "name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let clearance = args["clearance"].as_f64().unwrap_or(0.2);
    let trace_width = args["trace_width"].as_f64().unwrap_or(0.25);
    let via_drill = args["via_drill"].as_f64().unwrap_or(0.4);
    let via_dia = args["via_diameter"].as_f64().unwrap_or(0.8);

    let netclass_sexp = format!(
        "\n      (netclass \"{name}\"\n        (clearance {clearance})\n        \
         (trace_width {trace_width})\n        (via_drill {via_drill})\n        \
         (via_diameter {via_dia})\n      )"
    );

    let content = std::fs::read_to_string(&board_path)?;
    // Find (net_classes block or (net_settings block to insert into
    let insert_pos = if let Some(nc_pos) = content.find("(net_classes") {
        // Find closing paren of (net_classes ...)
        let block = &content[nc_pos..];
        nc_pos
            + block
                .find("\n    )")
                .unwrap_or(block.find(')').unwrap_or(block.len() - 1))
    } else {
        // No net_classes block; insert before last )
        content.rfind(')').unwrap_or(content.len())
    };

    let new_content = apply_edits(content, vec![SexpEdit::insert(insert_pos, netclass_sexp)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "created_netclass": name,
        "clearance": clearance, "trace_width": trace_width,
        "via_drill": via_drill, "via_diameter": via_dia,
        "source": "file"
    })))
}

async fn handle_assign_net_to_class(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let netclass = match require_str(args, "netclass") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;

    // Find the netclass block: (netclass "NAME" ...)
    let nc_pat = format!("(netclass \"{}\"", netclass);
    let nc_pos = match content.find(&nc_pat) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(format!(
                "Netclass '{}' not found in board file",
                netclass
            )))
        }
    };

    // Find the closing paren of the netclass block
    let mut depth = 0i32;
    let mut nc_end = nc_pos;
    for (i, ch) in content[nc_pos..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    nc_end = nc_pos + i;
                    break;
                }
            }
            _ => {}
        }
    }

    // Check if net is already assigned
    let nc_block = &content[nc_pos..nc_end];
    let net_check = format!("(net \"{}\")", net_name);
    if nc_block.contains(&net_check) {
        return Ok(CallToolResult::json(&json!({
            "already_assigned": true,
            "net_name": net_name,
            "netclass": netclass,
            "source": "file"
        })));
    }

    // Insert the net assignment before the closing paren of the netclass block
    let net_entry = format!("\n        (net \"{}\")", net_name);
    let new_content = apply_edits(content, vec![SexpEdit::insert(nc_end, net_entry)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "assigned": true,
        "net_name": net_name,
        "netclass": netclass,
        "source": "file"
    })))
}

async fn handle_route_diff_pair(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_pos = match require_str(args, "net_pos") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let net_neg = match require_str(args, "net_neg") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();
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
    let width = args["width"].as_f64().unwrap_or(0.1);
    let gap = args["gap"].as_f64().unwrap_or(0.1);
    let offset = (gap + width) / 2.0;

    // Route two parallel traces offset perpendicular to the direction
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt().max(1e-9);
    let perp_x = -dy / len * offset;
    let perp_y = dx / len * offset;

    let np_ipc = net_pos.clone();
    let nn_ipc = net_neg.clone();
    let layer_ipc = layer.clone();
    match try_ipc(ctx, move |c| {
        c.add_track(
            &np_ipc,
            &layer_ipc,
            width,
            x1 + perp_x,
            y1 + perp_y,
            x2 + perp_x,
            y2 + perp_y,
        )?;
        c.add_track(
            &nn_ipc,
            &layer_ipc,
            width,
            x1 - perp_x,
            y1 - perp_y,
            x2 - perp_x,
            y2 - perp_y,
        )
    })
    .await?
    {
        IpcAttempt::Ok(()) => {
            return Ok(CallToolResult::json(&json!({
                "net_pos": net_pos, "net_neg": net_neg,
                "layer": layer, "width": width, "gap": gap,
                "source": "ipc"
            })))
        }
        IpcAttempt::Failed(msg) => return Ok(ipc_write_refused("route_differential_pair", &msg)),
        IpcAttempt::Unavailable(_) => {}
    }

    let content = std::fs::read_to_string(&board_path)?;
    let pos_id = match require_file_net_id(&content, &net_pos) {
        Ok(id) => id,
        Err(e) => return Ok(e),
    };
    let neg_id = match require_file_net_id(&content, &net_neg) {
        Ok(id) => id,
        Err(e) => return Ok(e),
    };
    let mut segments = String::new();
    segments.push_str(&pf::format_segment(
        x1 + perp_x,
        y1 + perp_y,
        x2 + perp_x,
        y2 + perp_y,
        width,
        &layer,
        pos_id,
    ));
    segments.push_str(&pf::format_segment(
        x1 - perp_x,
        y1 - perp_y,
        x2 - perp_x,
        y2 - perp_y,
        width,
        &layer,
        neg_id,
    ));
    let new_content = pf::append_to_board(content, segments);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "net_pos": net_pos, "net_neg": net_neg,
        "layer": layer, "width": width, "gap": gap,
        "source": "file"
    })))
}
