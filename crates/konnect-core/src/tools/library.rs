//! `library` toolset — create and manage footprints, symbols, and KiCAD library tables.
//!
//! Operations are file-based (S-expression manipulation + directory scanning).
//! No IPC or kicad-cli is required for most tools.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_str, ToolContext, ToolDef};
use konnect_sexp::writer::write_atomic;
use serde_json::json;
use std::path::{Path, PathBuf};

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "create_footprint",
            "Create a new footprint (.kicad_mod) file from a pad layout description.",
            json!({
                "type": "object",
                "properties": {
                    "output": { "type": "string", "description": "Output .kicad_mod file path" },
                    "name": { "type": "string", "description": "Footprint name" },
                    "description": { "type": "string", "description": "Footprint description (optional)" },
                    "body_width": { "type": "number", "description": "Package body width in mm (drawn on F.Fab; courtyard wraps pads+body). Default: pad bbox" },
                    "body_height": { "type": "number", "description": "Package body height in mm" },
                    "pads": {
                        "type": "array",
                        "description": "Pad definitions",
                        "items": {
                            "type": "object",
                            "properties": {
                                "number": { "type": "string" },
                                "type": { "type": "string", "description": "'smd', 'thru_hole', 'np_thru_hole'" },
                                "shape": { "type": "string", "description": "'rect', 'oval', 'circle', 'roundrect'" },
                                "x": { "type": "number" },
                                "y": { "type": "number" },
                                "width": { "type": "number" },
                                "height": { "type": "number" },
                                "drill": { "type": "number", "description": "Drill diameter for thru-hole pads" }
                            },
                            "required": ["number", "type", "shape", "x", "y", "width", "height"]
                        }
                    }
                },
                "required": ["output", "name", "pads"]
            }),
            |args, ctx| async move { handle_create_footprint(args, ctx).await }
        ),
        tool!(
            "edit_footprint_pad",
            "Edit the size, shape, or position of a pad in an existing .kicad_mod footprint file.",
            json!({
                "type": "object",
                "properties": {
                    "footprint_path": { "type": "string", "description": "Path to .kicad_mod file" },
                    "pad_number": { "type": "string", "description": "Pad number to edit" },
                    "x": { "type": "number", "description": "New X position in mm (optional)" },
                    "y": { "type": "number", "description": "New Y position in mm (optional)" },
                    "width": { "type": "number", "description": "New pad width in mm (optional)" },
                    "height": { "type": "number", "description": "New pad height in mm (optional)" },
                    "shape": { "type": "string", "description": "New pad shape (optional)" },
                    "drill": { "type": "number", "description": "New drill diameter in mm (optional)" }
                },
                "required": ["footprint_path", "pad_number"]
            }),
            |args, ctx| async move { handle_edit_footprint_pad(args, ctx).await }
        ),
        tool!(
            "register_footprint_library",
            "Register a local footprint library directory in the KiCAD global or project library table.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .pretty directory" },
                    "nickname": { "type": "string", "description": "Library nickname" },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global' or 'project'",
                        "default": "project"
                    },
                    "project": { "type": "string", "description": "Path to .kicad_pro file (required for project scope)" }
                },
                "required": ["library_path", "nickname"]
            }),
            |args, ctx| async move { handle_register_footprint_library(args, ctx).await }
        ),
        tool!(
            "list_footprint_libraries",
            "List all registered footprint libraries (global and optionally project-level).",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Path to .kicad_pro to include project libraries (optional)" },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global', 'project', or 'all'",
                        "default": "all"
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_list_footprint_libraries(args, ctx).await }
        ),
        tool!(
            "create_symbol",
            "Create a new KiCAD schematic symbol and append it to a .kicad_sym library file.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym library file" },
                    "name": { "type": "string", "description": "Symbol name" },
                    "reference_prefix": { "type": "string", "description": "Default reference prefix (e.g. 'U')" },
                    "value": { "type": "string", "description": "Default value string" },
                    "pins": {
                        "type": "array",
                        "description": "Pin definitions",
                        "items": {
                            "type": "object",
                            "properties": {
                                "number": { "type": "string" },
                                "name": { "type": "string" },
                                "type": { "type": "string", "description": "'input', 'output', 'bidirectional', 'power_in', 'power_out', 'passive'" },
                                "x": { "type": "number" },
                                "y": { "type": "number" },
                                "angle": { "type": "number", "default": 0 },
                                "length": { "type": "number", "default": 2.54 }
                            },
                            "required": ["number", "name", "type", "x", "y"]
                        }
                    }
                },
                "required": ["library_path", "name", "reference_prefix", "pins"]
            }),
            |args, ctx| async move { handle_create_symbol(args, ctx).await }
        ),
        tool!(
            "delete_symbol",
            "Delete a symbol definition from a .kicad_sym library file.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym library file" },
                    "symbol_name": { "type": "string", "description": "Name of the symbol to delete" }
                },
                "required": ["library_path", "symbol_name"]
            }),
            |args, ctx| async move { handle_delete_symbol(args, ctx).await }
        ),
        tool!(
            "list_symbols_in_library",
            "List all symbol names defined in a .kicad_sym library file.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym library file" }
                },
                "required": ["library_path"]
            }),
            |args, ctx| async move { handle_list_symbols_in_library(args, ctx).await }
        ),
        tool!(
            "register_symbol_library",
            "Register a .kicad_sym library file in the KiCAD global or project symbol table.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym file" },
                    "nickname": { "type": "string", "description": "Library nickname" },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global' or 'project'",
                        "default": "project"
                    },
                    "project": { "type": "string", "description": "Path to .kicad_pro file (required for project scope)" }
                },
                "required": ["library_path", "nickname"]
            }),
            |args, ctx| async move { handle_register_symbol_library(args, ctx).await }
        ),
        tool!(
            "unregister_library",
            "Remove a library entry by nickname from a KiCAD symbol or footprint library table. \
             Does not delete the library files themselves.",
            json!({
                "type": "object",
                "properties": {
                    "nickname": { "type": "string", "description": "Library nickname to remove" },
                    "table": {
                        "type": "string",
                        "description": "Which table: 'symbol' or 'footprint'"
                    },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global' or 'project'",
                        "default": "project"
                    },
                    "project": { "type": "string", "description": "Path to .kicad_pro file (required for project scope)" }
                },
                "required": ["nickname", "table"]
            }),
            |args, ctx| async move { handle_unregister_library(args, ctx).await }
        ),
        tool!(
            "list_symbol_libraries",
            "List all registered symbol libraries (global and optionally project-level).",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Path to .kicad_pro to include project libraries (optional)" },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global', 'project', or 'all'",
                        "default": "all"
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_list_symbol_libraries(args, ctx).await }
        ),
        tool!(
            "search_symbols",
            "Search for symbols across all registered libraries by name or keyword.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search string (partial name or keyword match)" },
                    "limit": { "type": "integer", "description": "Maximum number of results to return", "default": 50 }
                },
                "required": ["query"]
            }),
            |args, ctx| async move { handle_search_symbols(args, ctx).await }
        ),
        tool!(
            "list_library_footprints",
            "List all footprints in a specific registered footprint library (.pretty directory).",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .pretty directory (or nickname to look up)" }
                },
                "required": ["library_path"]
            }),
            |args, ctx| async move { handle_list_library_footprints(args, ctx).await }
        ),
        tool!(
            "get_footprint_info",
            "Return detailed information about a footprint: pad layout, courtyard, description.",
            json!({
                "type": "object",
                "properties": {
                    "footprint_path": { "type": "string", "description": "Path to .kicad_mod file, OR 'Library:Footprint' identifier" }
                },
                "required": ["footprint_path"]
            }),
            |args, ctx| async move { handle_get_footprint_info(args, ctx).await }
        ),
        tool!(
            "search_footprints",
            "Search for footprints across all registered libraries by name or keyword.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search string (partial name or keyword)" },
                    "limit": { "type": "integer", "description": "Maximum number of results to return", "default": 50 }
                },
                "required": ["query"]
            }),
            |args, ctx| async move { handle_search_footprints(args, ctx).await }
        ),
        tool!(
            "get_symbol_info",
            "Return detailed information about a schematic symbol: pins, properties, description.",
            json!({
                "type": "object",
                "properties": {
                    "lib_id": { "type": "string", "description": "Library:Symbol identifier (e.g. 'Device:R')" }
                },
                "required": ["lib_id"]
            }),
            |args, ctx| async move { handle_get_symbol_info(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_create_footprint(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let output = get_path(args, "output")?;
    let name = args["name"].as_str().unwrap_or("Footprint");
    let description = args["description"].as_str().unwrap_or("");

    let pads_val = args["pads"].as_array().cloned().unwrap_or_default();
    let mut pad_sexp = String::new();
    for pad in &pads_val {
        let number = pad["number"].as_str().unwrap_or("1");
        let pad_type = pad["type"].as_str().unwrap_or("smd");
        let shape = pad["shape"].as_str().unwrap_or("rect");
        let x = pad["x"].as_f64().unwrap_or(0.0);
        let y = pad["y"].as_f64().unwrap_or(0.0);
        let w = pad["width"].as_f64().unwrap_or(1.0);
        let h = pad["height"].as_f64().unwrap_or(1.0);

        let layers = if pad_type == "smd" {
            r#"(layers "F.Cu" "F.Paste" "F.Mask")"#
        } else {
            r#"(layers "*.Cu" "*.Mask")"#
        };

        let drill_sexp = if let Some(drill) = pad["drill"].as_f64() {
            format!("(drill {})", drill)
        } else {
            String::new()
        };

        pad_sexp.push_str(&format!(
            r#"
  (pad "{}" {} {} (at {} {}) (size {} {}) {} {})"#,
            number, pad_type, shape, x, y, w, h, layers, drill_sexp
        ));
    }

    // Fabrication body, silkscreen pin-1 marker, and courtyard. A footprint
    // without these passes nothing to assembly (no polarity mark) and trips
    // DRC courtyard checks; generate them from the pad bbox / body dims.
    let graphics_sexp = {
        let (mut min_x, mut min_y, mut max_x, mut max_y) =
            (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for pad in &pads_val {
            let x = pad["x"].as_f64().unwrap_or(0.0);
            let y = pad["y"].as_f64().unwrap_or(0.0);
            let w = pad["width"].as_f64().unwrap_or(1.0);
            let h = pad["height"].as_f64().unwrap_or(1.0);
            min_x = min_x.min(x - w / 2.0);
            max_x = max_x.max(x + w / 2.0);
            min_y = min_y.min(y - h / 2.0);
            max_y = max_y.max(y + h / 2.0);
        }
        if pads_val.is_empty() {
            String::new()
        } else {
            let bw = args["body_width"].as_f64().unwrap_or(max_x - min_x);
            let bh = args["body_height"].as_f64().unwrap_or(max_y - min_y);
            let (bx1, by1, bx2, by2) = (-bw / 2.0, -bh / 2.0, bw / 2.0, bh / 2.0);
            let (cx1, cy1, cx2, cy2) = (
                min_x.min(bx1) - 0.25,
                min_y.min(by1) - 0.25,
                max_x.max(bx2) + 0.25,
                max_y.max(by2) + 0.25,
            );
            // pin-1 dot outside the courtyard corner nearest pad 1
            let (p1x, p1y) = pads_val
                .iter()
                .find(|p| p["number"].as_str() == Some("1"))
                .map(|p| (p["x"].as_f64().unwrap_or(0.0), p["y"].as_f64().unwrap_or(0.0)))
                .unwrap_or((cx1, cy1));
            let dot_x = if p1x < 0.0 { cx1 - 0.5 } else { cx2 + 0.5 };
            let dot_y = if p1y < 0.0 { cy1 } else { cy2 };
            format!(
                r#"
  (fp_rect (start {bx1:.3} {by1:.3}) (end {bx2:.3} {by2:.3})
    (stroke (width 0.1) (type solid)) (fill no) (layer "F.Fab"))
  (fp_rect (start {cx1:.3} {cy1:.3}) (end {cx2:.3} {cy2:.3})
    (stroke (width 0.05) (type solid)) (fill no) (layer "F.CrtYd"))
  (fp_circle (center {dot_x:.3} {dot_y:.3}) (end {dot_end:.3} {dot_y:.3})
    (stroke (width 0.3) (type solid)) (fill yes) (layer "F.SilkS"))
  (fp_text user "${{REFERENCE}}" (at 0 0) (layer "F.Fab")
    (effects (font (size 1 1) (thickness 0.15))))"#,
                dot_end = dot_x + 0.15,
            )
        }
    };

    let content = format!(
        r#"(footprint "{}"
  (version 20260206)
  (generator "konnect")
  (layer "F.Cu")
  (descr "{}")
  (attr {}){}{}
)"#,
        name,
        description,
        if pads_val.iter().any(|p| p["type"].as_str() == Some("smd")) {
            "smd"
        } else {
            "through_hole"
        },
        graphics_sexp,
        pad_sexp
    );

    // Ensure parent directory exists
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    write_atomic(&output, &content)?;
    normalize_with_kicad_cli(&ctx.config.kicad_cli, "fp", &output).await;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "footprint": name,
            "output": output.to_str().unwrap_or(""),
            "pad_count": pads_val.len()
        }))
        .unwrap(),
    ))
}

async fn handle_edit_footprint_pad(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = get_path(args, "footprint_path")?;
    let pad_number = require_str(args, "pad_number").map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let content = tokio::fs::read_to_string(&path).await?;

    // Find the pad block:  (pad "N" ... (at X Y) (size W H) ...)
    // We search for the at/size/drill atoms and replace them individually.
    let pad_pat = format!(r#"(pad "{}""#, pad_number);
    let pad_start = content
        .find(&pad_pat)
        .ok_or_else(|| anyhow::anyhow!("Pad '{}' not found in footprint", pad_number))?;

    // Find the closing paren of this pad block (simple depth count)
    let pad_end = {
        let mut depth = 0i32;
        let mut end = pad_start;
        for (i, ch) in content[pad_start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = pad_start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        end
    };
    let pad_block = &content[pad_start..pad_end];

    // Helper: replace or add a sub-expression within the pad block
    let mut new_pad = pad_block.to_string();

    if let Some(x) = args["x"].as_f64() {
        // Replace (at OLD_X OLD_Y [ROT]) → update X
        if let Some(at_pos) = new_pad.find("(at ") {
            let at_end = new_pad[at_pos..]
                .find(')')
                .map(|i| at_pos + i + 1)
                .unwrap_or(new_pad.len());
            let at_block = &new_pad[at_pos..at_end];
            // Parse existing values
            let parts: Vec<&str> = at_block
                .trim_start_matches("(at ")
                .trim_end_matches(')')
                .split_whitespace()
                .collect();
            let old_y = parts
                .get(1)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let rot = parts.get(2).map(|s| format!(" {}", s)).unwrap_or_default();
            let new_at = format!("(at {} {}{})", x, old_y, rot);
            new_pad.replace_range(at_pos..at_end, &new_at);
        }
    }
    if let Some(y) = args["y"].as_f64() {
        if let Some(at_pos) = new_pad.find("(at ") {
            let at_end = new_pad[at_pos..]
                .find(')')
                .map(|i| at_pos + i + 1)
                .unwrap_or(new_pad.len());
            let at_block = &new_pad[at_pos..at_end];
            let parts: Vec<&str> = at_block
                .trim_start_matches("(at ")
                .trim_end_matches(')')
                .split_whitespace()
                .collect();
            let old_x = parts
                .first()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let rot = parts.get(2).map(|s| format!(" {}", s)).unwrap_or_default();
            let new_at = format!("(at {} {}{})", old_x, y, rot);
            new_pad.replace_range(at_pos..at_end, &new_at);
        }
    }
    if let (Some(w), Some(h)) = (args["width"].as_f64(), args["height"].as_f64()) {
        if let Some(sz_pos) = new_pad.find("(size ") {
            let sz_end = new_pad[sz_pos..]
                .find(')')
                .map(|i| sz_pos + i + 1)
                .unwrap_or(new_pad.len());
            let new_size = format!("(size {} {})", w, h);
            new_pad.replace_range(sz_pos..sz_end, &new_size);
        }
    }
    if let Some(drill) = args["drill"].as_f64() {
        if let Some(dr_pos) = new_pad.find("(drill ") {
            let dr_end = new_pad[dr_pos..]
                .find(')')
                .map(|i| dr_pos + i + 1)
                .unwrap_or(new_pad.len());
            let new_drill = format!("(drill {})", drill);
            new_pad.replace_range(dr_pos..dr_end, &new_drill);
        } else {
            // Insert drill before closing paren of pad
            let insert_at = new_pad.rfind(')').unwrap_or(new_pad.len());
            new_pad.insert_str(insert_at, &format!(" (drill {})", drill));
        }
    }

    // Apply the pad block replacement
    let new_content = format!(
        "{}{}{}",
        &content[..pad_start],
        new_pad,
        &content[pad_end..]
    );
    write_atomic(&path, &new_content)?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "pad": pad_number
        }))
        .unwrap(),
    ))
}

// ─── Library table helpers ────────────────────────────────────────────────────

/// Returns the path to the global fp-lib-table file.
fn global_fp_lib_table() -> PathBuf {
    super::kicad_config_dir().join("fp-lib-table")
}

/// Returns the path to the global sym-lib-table file.
fn global_sym_lib_table() -> PathBuf {
    super::kicad_config_dir().join("sym-lib-table")
}

/// Parse a lib-table S-expression and return list of (nickname, uri, type) tuples.
fn parse_lib_table(content: &str) -> Vec<serde_json::Value> {
    let mut libs = Vec::new();
    // Each entry: (lib (name "NICK") (type "...") (uri "...") (options "") (descr "..."))
    let mut pos = 0;
    while let Some(lib_start) = content[pos..].find("\n  (lib ").map(|i| pos + i) {
        // Find the end of this lib block
        let inner_start = lib_start + 2; // skip "\n  "
        let mut depth = 0i32;
        let mut end = inner_start;
        for (i, ch) in content[inner_start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = inner_start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        let block = &content[inner_start..end];

        let nickname = extract_sexp_string(block, "name").unwrap_or_default();
        let uri = extract_sexp_string(block, "uri").unwrap_or_default();
        let lib_type = extract_sexp_string(block, "type").unwrap_or_default();
        let descr = extract_sexp_string(block, "descr").unwrap_or_default();

        libs.push(json!({
            "nickname": nickname,
            "uri": uri,
            "type": lib_type,
            "description": descr
        }));
        pos = end;
    }
    libs
}

/// Extract a quoted string value from `(key "value")` within a block.
fn extract_sexp_string(block: &str, key: &str) -> Option<String> {
    let pat = format!("({} \"", key);
    let start = block.find(&pat)? + pat.len();
    let end = block[start..].find('"')? + start;
    Some(block[start..end].to_string())
}

async fn handle_register_footprint_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let nickname = require_str(args, "nickname").map_err(|e| anyhow::anyhow!("{:?}", e))?;
    let scope = args["scope"].as_str().unwrap_or("project");

    let table_path = if scope == "global" {
        global_fp_lib_table()
    } else if let Some(proj) = args["project"].as_str() {
        PathBuf::from(proj)
            .parent()
            .unwrap_or(Path::new("."))
            .join("fp-lib-table")
    } else {
        return Ok(CallToolResult::error(
            "For project scope, provide 'project' path to .kicad_pro file",
        ));
    };

    register_in_lib_table(
        &table_path,
        nickname,
        lib_path.to_str().unwrap_or(""),
        "KiCad",
    )
    .await?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "nickname": nickname,
            "scope": scope,
            "table": table_path.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_list_footprint_libraries(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let scope = args["scope"].as_str().unwrap_or("all");
    let mut all_libs = Vec::new();

    if scope == "global" || scope == "all" {
        let table = global_fp_lib_table();
        if table.exists() {
            let content = tokio::fs::read_to_string(&table).await?;
            let mut libs = parse_lib_table(&content);
            for lib in &mut libs {
                lib["scope"] = json!("global");
            }
            all_libs.extend(libs);
        }
    }

    if (scope == "project" || scope == "all") && args["project"].is_string() {
        let proj = PathBuf::from(args["project"].as_str().unwrap());
        let table = proj.parent().unwrap_or(Path::new(".")).join("fp-lib-table");
        if table.exists() {
            let content = tokio::fs::read_to_string(&table).await?;
            let mut libs = parse_lib_table(&content);
            for lib in &mut libs {
                lib["scope"] = json!("project");
            }
            all_libs.extend(libs);
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "count": all_libs.len(),
            "libraries": all_libs
        }))
        .unwrap(),
    ))
}

async fn handle_register_symbol_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let nickname = require_str(args, "nickname").map_err(|e| anyhow::anyhow!("{:?}", e))?;
    let scope = args["scope"].as_str().unwrap_or("project");

    let table_path = if scope == "global" {
        global_sym_lib_table()
    } else if let Some(proj) = args["project"].as_str() {
        PathBuf::from(proj)
            .parent()
            .unwrap_or(Path::new("."))
            .join("sym-lib-table")
    } else {
        return Ok(CallToolResult::error(
            "For project scope, provide 'project' path to .kicad_pro file",
        ));
    };

    register_in_lib_table(
        &table_path,
        nickname,
        lib_path.to_str().unwrap_or(""),
        "KiCad",
    )
    .await?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "nickname": nickname,
            "scope": scope,
            "table": table_path.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_unregister_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let nickname = match require_str(args, "nickname") {
        Ok(n) => n.to_string(),
        Err(e) => return Ok(e),
    };
    let table_kind = match require_str(args, "table") {
        Ok(t) => t.to_string(),
        Err(e) => return Ok(e),
    };
    let scope = args["scope"].as_str().unwrap_or("project");

    let table_path = match (table_kind.as_str(), scope) {
        ("symbol", "global") => global_sym_lib_table(),
        ("footprint", "global") => global_fp_lib_table(),
        (kind, "project") => {
            let Some(proj) = args["project"].as_str() else {
                return Ok(CallToolResult::error(
                    "For project scope, provide 'project' path to .kicad_pro file",
                ));
            };
            let file = if kind == "symbol" {
                "sym-lib-table"
            } else if kind == "footprint" {
                "fp-lib-table"
            } else {
                return Ok(CallToolResult::error("'table' must be 'symbol' or 'footprint'"));
            };
            PathBuf::from(proj)
                .parent()
                .unwrap_or(Path::new("."))
                .join(file)
        }
        _ => return Ok(CallToolResult::error("'table' must be 'symbol' or 'footprint'")),
    };

    if !table_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Library table not found: {}",
            table_path.display()
        )));
    }

    let content = tokio::fs::read_to_string(&table_path).await?;
    // Entries are one `(lib ...)` node per line in tables this tool manages;
    // match by the exact (name "nickname") token to avoid prefix collisions.
    let needle = format!(r#"(name "{nickname}")"#);
    let mut removed = 0usize;
    let kept: Vec<&str> = content
        .lines()
        .filter(|line| {
            let hit = line.trim_start().starts_with("(lib ") && line.contains(&needle);
            if hit {
                removed += 1;
            }
            !hit
        })
        .collect();

    if removed == 0 {
        return Ok(CallToolResult::error(format!(
            "No entry named '{}' in {}",
            nickname,
            table_path.display()
        )));
    }

    let mut new_content = kept.join("\n");
    if content.ends_with('\n') {
        new_content.push('\n');
    }
    write_atomic(&table_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "success": true,
        "nickname": nickname,
        "removed_entries": removed,
        "table": table_path.to_str().unwrap_or("")
    })))
}

async fn handle_list_symbol_libraries(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let scope = args["scope"].as_str().unwrap_or("all");
    let mut all_libs = Vec::new();

    if scope == "global" || scope == "all" {
        let table = global_sym_lib_table();
        if table.exists() {
            let content = tokio::fs::read_to_string(&table).await?;
            let mut libs = parse_lib_table(&content);
            for lib in &mut libs {
                lib["scope"] = json!("global");
            }
            all_libs.extend(libs);
        }
    }

    if (scope == "project" || scope == "all") && args["project"].is_string() {
        let proj = PathBuf::from(args["project"].as_str().unwrap());
        let table = proj
            .parent()
            .unwrap_or(Path::new("."))
            .join("sym-lib-table");
        if table.exists() {
            let content = tokio::fs::read_to_string(&table).await?;
            let mut libs = parse_lib_table(&content);
            for lib in &mut libs {
                lib["scope"] = json!("project");
            }
            all_libs.extend(libs);
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "count": all_libs.len(),
            "libraries": all_libs
        }))
        .unwrap(),
    ))
}

/// Insert a new `(lib ...)` entry into a lib-table file (fp-lib-table or sym-lib-table).
/// Creates the file with minimal scaffolding if it doesn't exist.
async fn register_in_lib_table(
    table_path: &Path,
    nickname: &str,
    uri: &str,
    lib_type: &str,
) -> anyhow::Result<()> {
    let content = if table_path.exists() {
        tokio::fs::read_to_string(table_path).await?
    } else {
        // Header token must match the table kind — KiCAD refuses to load a
        // sym-lib-table whose root token says fp_lib_table.
        let header = if table_path
            .file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| f.starts_with("sym-lib-table"))
        {
            "sym_lib_table"
        } else {
            "fp_lib_table"
        };
        format!("({}\n  (version 7)\n)\n", header)
    };

    // Check if nickname already registered
    if content.contains(&format!("(name \"{}\")", nickname)) {
        return Ok(()); // already registered, idempotent
    }

    // Find closing paren of the root expression
    let insert_pos = content.rfind(')').unwrap_or(content.len());
    let entry = format!(
        "\n  (lib (name \"{}\") (type \"{}\") (uri \"{}\") (options \"\") (descr \"\"))",
        nickname, lib_type, uri
    );

    let new_content = format!("{}{}\n)", &content[..insert_pos], entry);

    if let Some(parent) = table_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    write_atomic(table_path, &new_content)?;
    Ok(())
}

// ─── Symbol library tools ─────────────────────────────────────────────────────

/// Best-effort normalization of a freshly written library file to the current
/// KiCad format via `kicad-cli sym|fp upgrade --force`. Konnect's templates
/// carry the right format version, but KiCad normalizes more than the version
/// header on load (default fields, node order); files that skip this step can
/// trigger lib_symbol_mismatch ERC warnings against embedded copies. Silently
/// skipped when kicad-cli is not configured or the invocation fails.
async fn normalize_with_kicad_cli(cli: &str, kind: &str, path: &Path) {
    if cli.is_empty() {
        return;
    }
    let _ = tokio::process::Command::new(cli)
        .args([kind, "upgrade", "--force"])
        .arg(path)
        .output()
        .await;
}

async fn handle_create_symbol(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let name = args["name"].as_str().unwrap_or("Symbol");
    let ref_prefix = args["reference_prefix"].as_str().unwrap_or("U");
    let value_str = args["value"].as_str().unwrap_or(name);
    let pins_val = args["pins"].as_array().cloned().unwrap_or_default();

    // Build pin S-expressions
    let mut pins_sexp = String::new();
    for pin in &pins_val {
        let number = pin["number"].as_str().unwrap_or("1");
        let pin_name = pin["name"].as_str().unwrap_or("~");
        let pin_type = pin["type"].as_str().unwrap_or("passive");
        let x = pin["x"].as_f64().unwrap_or(0.0);
        let y = pin["y"].as_f64().unwrap_or(0.0);
        let angle = pin["angle"].as_f64().unwrap_or(0.0);
        let length = pin["length"].as_f64().unwrap_or(2.54);

        pins_sexp.push_str(&format!(
            r#"
    (pin {} line (at {} {} {})
      (length {})
      (name "{}" (effects (font (size 1.27 1.27))))
      (number "{}" (effects (font (size 1.27 1.27))))
    )"#,
            pin_type, x, y, angle, length, pin_name, number
        ));
    }

    // Auto body rectangle: span the pin bounding box inset by each side's pin
    // length so the box meets the pin roots. A symbol without a body renders
    // as floating pin stubs.
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    let mut max_len = 2.54f64;
    for pin in &pins_val {
        let x = pin["x"].as_f64().unwrap_or(0.0);
        let y = pin["y"].as_f64().unwrap_or(0.0);
        max_len = max_len.max(pin["length"].as_f64().unwrap_or(2.54));
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    let body = if pins_val.is_empty() {
        String::new()
    } else {
        let (bx1, bx2) = (min_x + max_len, max_x - max_len);
        let (by1, by2) = (min_y - 2.54, max_y + 2.54);
        format!(
            r#"
      (rectangle (start {} {}) (end {} {})
        (stroke (width 0.254) (type default))
        (fill (type background))
      )"#,
            bx1, by2, bx2, by1
        )
    };
    let ref_y = if pins_val.is_empty() {
        0.0
    } else {
        max_y + 4.5
    };
    let val_y = if pins_val.is_empty() {
        -2.54
    } else {
        min_y - 4.5
    };

    let symbol_sexp = format!(
        r#"
  (symbol "{}"
    (pin_names (offset 1.016))
    (in_bom yes)
    (on_board yes)
    (property "Reference" "{}" (at 0 {} 0) (effects (font (size 1.27 1.27))))
    (property "Value" "{}" (at 0 {} 0) (effects (font (size 1.27 1.27))))
    (property "Footprint" "" (at 0 0 0) (effects (font (size 1.27 1.27)) hide))
    (property "Datasheet" "~" (at 0 0 0) (effects (font (size 1.27 1.27)) hide))
    (symbol "{}_0_1"{}{}
    )
  )"#,
        name, ref_prefix, ref_y, value_str, val_y, name, body, pins_sexp
    );

    // If file doesn't exist, create scaffold
    let content = if lib_path.exists() {
        tokio::fs::read_to_string(&lib_path).await?
    } else {
        "(kicad_symbol_lib\n  (version 20251024)\n  (generator \"konnect\")\n)\n".to_string()
    };

    // Insert before closing paren of root expression
    let insert_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = format!("{}{}\n)", &content[..insert_pos], symbol_sexp);

    if let Some(parent) = lib_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    write_atomic(&lib_path, &new_content)?;
    normalize_with_kicad_cli(&ctx.config.kicad_cli, "sym", &lib_path).await;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "symbol": name,
            "library": lib_path.to_str().unwrap_or(""),
            "pin_count": pins_val.len()
        }))
        .unwrap(),
    ))
}

async fn handle_delete_symbol(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let symbol_name = require_str(args, "symbol_name").map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let content = tokio::fs::read_to_string(&lib_path).await?;

    // Find `  (symbol "NAME"` block
    let pat = format!(r#"  (symbol "{}""#, symbol_name);
    let start = content
        .find(&pat)
        .ok_or_else(|| anyhow::anyhow!("Symbol '{}' not found in library", symbol_name))?;

    // Walk back to find preceding newline
    let block_start = content[..start].rfind('\n').map(|i| i + 1).unwrap_or(start);

    // Walk forward to find end of block (depth count)
    let mut depth = 0i32;
    let mut end = start;
    for (i, ch) in content[start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    // Skip trailing newline
    let end = if content[end..].starts_with('\n') {
        end + 1
    } else {
        end
    };

    let new_content = format!("{}{}", &content[..block_start], &content[end..]);
    write_atomic(&lib_path, &new_content)?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "deleted": symbol_name
        }))
        .unwrap(),
    ))
}

async fn handle_list_symbols_in_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let content = tokio::fs::read_to_string(&lib_path).await?;

    // Match all top-level symbol names: `  (symbol "NAME"` at depth 1
    let mut symbols = Vec::new();
    let mut search = content.as_str();
    while let Some(pos) = search.find("\n  (symbol \"") {
        let after = &search[pos + 13..]; // skip `\n  (symbol "`
        if let Some(end) = after.find('"') {
            let sym_name = &after[..end];
            // Exclude sub-units like "NAME_0_1"
            if !sym_name.contains('_') || {
                // Allow symbols whose name contains underscores but are NOT sub-unit patterns
                let parts: Vec<&str> = sym_name.rsplitn(3, '_').collect();
                parts.len() < 3 || parts[0].parse::<u32>().is_err()
            } {
                symbols.push(sym_name.to_string());
            }
            search = &search[pos + 1..];
        } else {
            break;
        }
    }
    symbols.sort();
    symbols.dedup();

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "library": lib_path.to_str().unwrap_or(""),
            "count": symbols.len(),
            "symbols": symbols
        }))
        .unwrap(),
    ))
}

async fn handle_search_symbols(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let query = args["query"].as_str().unwrap_or("").to_lowercase();
    let limit = args["limit"].as_u64().unwrap_or(50) as usize;

    // Walk all global symbol libraries
    let table = global_sym_lib_table();
    let mut results = Vec::new();

    if table.exists() {
        let table_content = tokio::fs::read_to_string(&table).await?;
        let libs = parse_lib_table(&table_content);

        'outer: for lib in &libs {
            let uri = lib["uri"].as_str().unwrap_or("");
            // Expand KiCAD env vars ${KICAD8_SYMBOL_DIR} etc. — skip if unresolvable
            if uri.starts_with("${") {
                continue;
            }
            let lib_path = PathBuf::from(uri);
            if !lib_path.exists() {
                continue;
            }
            let lib_content = match tokio::fs::read_to_string(&lib_path).await {
                Ok(c) => c,
                Err(_) => continue,
            };

            let nickname = lib["nickname"].as_str().unwrap_or("");
            let mut search = lib_content.as_str();
            while let Some(pos) = search.find("\n  (symbol \"") {
                let after = &search[pos + 13..];
                if let Some(end) = after.find('"') {
                    let sym_name = &after[..end];
                    if sym_name.to_lowercase().contains(&query) && !sym_name.contains('_') {
                        results.push(json!({
                            "library": nickname,
                            "name": sym_name,
                            "id": format!("{}:{}", nickname, sym_name)
                        }));
                        if results.len() >= limit {
                            break 'outer;
                        }
                    }
                    search = &search[pos + 1..];
                } else {
                    break;
                }
            }
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "query": query,
            "count": results.len(),
            "results": results
        }))
        .unwrap(),
    ))
}

async fn handle_list_library_footprints(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let library_path_str =
        require_str(args, "library_path").map_err(|e| anyhow::anyhow!("{:?}", e))?;
    let lib_dir = PathBuf::from(library_path_str);

    if !lib_dir.is_dir() {
        return Ok(CallToolResult::error(format!(
            "Not a directory: {}",
            library_path_str
        )));
    }

    let mut footprints = Vec::new();
    let mut rd = tokio::fs::read_dir(&lib_dir).await?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".kicad_mod") {
            footprints.push(name_str.trim_end_matches(".kicad_mod").to_string());
        }
    }
    footprints.sort();

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "library": library_path_str,
            "count": footprints.len(),
            "footprints": footprints
        }))
        .unwrap(),
    ))
}

async fn handle_get_footprint_info(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let fp_path_str =
        require_str(args, "footprint_path").map_err(|e| anyhow::anyhow!("{:?}", e))?;

    // Resolve "Library:Footprint" form via global fp-lib-table
    let path =
        if fp_path_str.contains(':') && !fp_path_str.contains('/') && !fp_path_str.contains('\\') {
            let parts: Vec<&str> = fp_path_str.splitn(2, ':').collect();
            let (nick, fp_name) = (parts[0], parts[1]);
            let table = global_fp_lib_table();
            if table.exists() {
                let tc = tokio::fs::read_to_string(&table).await?;
                let libs = parse_lib_table(&tc);
                if let Some(lib) = libs.iter().find(|l| l["nickname"].as_str() == Some(nick)) {
                    let uri = lib["uri"].as_str().unwrap_or("");
                    PathBuf::from(uri).join(format!("{}.kicad_mod", fp_name))
                } else {
                    return Ok(CallToolResult::error(format!(
                        "Library '{}' not found in fp-lib-table",
                        nick
                    )));
                }
            } else {
                return Ok(CallToolResult::error("Global fp-lib-table not found"));
            }
        } else {
            PathBuf::from(fp_path_str)
        };

    let content = tokio::fs::read_to_string(&path).await?;

    // Parse basic info: description, pads
    let description = extract_sexp_string(&content, "descr").unwrap_or_default();
    let fp_name = extract_sexp_string(&content, "footprint").unwrap_or_else(|| {
        path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });

    // Count pads
    let pad_count = content.matches("\n  (pad ").count();

    // Extract courtyard bbox (gr_poly on B.CrtYd or F.CrtYd) — simplified
    let has_courtyard = content.contains("B.CrtYd") || content.contains("F.CrtYd");
    let has_3d = content.contains("(model ");

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "name": fp_name,
            "description": description,
            "pad_count": pad_count,
            "has_courtyard": has_courtyard,
            "has_3d_model": has_3d,
            "path": path.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

// ─── search_footprints (moved from verification toolset) ─────────────────────

async fn handle_search_footprints(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let query = args["query"].as_str().unwrap_or("").to_lowercase();
    let limit = args["limit"].as_u64().unwrap_or(50) as usize;

    // Walk global fp-lib-table
    let fp_lib_table_path = super::kicad_config_dir().join("fp-lib-table");

    let mut results = Vec::new();

    if fp_lib_table_path.exists() {
        let tc = tokio::fs::read_to_string(&fp_lib_table_path).await?;

        // Parse lib entries
        let mut search = tc.as_str();
        'outer: while let Some(lib_pos) = search.find("\n  (lib ") {
            let block_start = lib_pos + 3;
            let mut depth = 0i32;
            let mut block_end = block_start;
            for (i, ch) in search[block_start..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            block_end = block_start + i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let block = &search[block_start..block_end];
            let nickname = extract_sexp_string(block, "name").unwrap_or_default();
            let uri = extract_sexp_string(block, "uri").unwrap_or_default();

            if !uri.starts_with("${") {
                let dir = PathBuf::from(uri);
                if dir.is_dir() {
                    if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
                        while let Ok(Some(entry)) = rd.next_entry().await {
                            let fname = entry.file_name();
                            let fname_str = fname.to_string_lossy();
                            if fname_str.ends_with(".kicad_mod") {
                                let fp_name = fname_str.trim_end_matches(".kicad_mod");
                                if fp_name.to_lowercase().contains(&query) {
                                    results.push(json!({
                                        "library": nickname,
                                        "name": fp_name,
                                        "id": format!("{}:{}", nickname, fp_name)
                                    }));
                                    if results.len() >= limit {
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            search = &search[lib_pos + 1..];
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "query": args["query"].as_str().unwrap_or(""),
            "count": results.len(),
            "results": results
        }))
        .unwrap(),
    ))
}

// ─── get_symbol_info (moved from verification toolset) ───────────────────────

async fn handle_get_symbol_info(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_id = require_str(args, "lib_id").map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let parts: Vec<&str> = lib_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Ok(CallToolResult::error(
            "lib_id must be in 'Library:Symbol' format (e.g. 'Device:R')",
        ));
    }
    let (lib_nick, sym_name) = (parts[0], parts[1]);

    // Look up library path from global sym-lib-table
    let sym_lib_table_path = super::kicad_config_dir().join("sym-lib-table");

    let lib_path = if sym_lib_table_path.exists() {
        let tc = tokio::fs::read_to_string(&sym_lib_table_path).await?;
        // Parse lib entries: (lib (name "NICK") ... (uri "PATH") ...)
        let pat = format!(r#"(name "{}")"#, lib_nick);
        if let Some(block_start) = tc.find(&pat) {
            let block_end = tc[block_start..]
                .find(")\n")
                .map(|i| block_start + i + 2)
                .unwrap_or(tc.len());
            let block = &tc[block_start..block_end];
            if let Some(uri_pos) = block.find("(uri \"") {
                let after = &block[uri_pos + 6..];
                after.find('"').map(|end| PathBuf::from(&after[..end]))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let lib_path = match lib_path {
        Some(p) if !p.to_str().unwrap_or("").starts_with("${") => p,
        _ => {
            return Ok(CallToolResult::error(format!(
                "Library '{}' not found or path uses unresolved env var",
                lib_nick
            )));
        }
    };

    let content = tokio::fs::read_to_string(&lib_path).await?;

    // Find symbol block
    let sym_pat = format!(r#"  (symbol "{}""#, sym_name);
    let sym_start = content.find(&sym_pat).ok_or_else(|| {
        anyhow::anyhow!("Symbol '{}' not found in library '{}'", sym_name, lib_nick)
    })?;

    let sym_end = {
        let mut depth = 0i32;
        let mut end = sym_start;
        for (i, ch) in content[sym_start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = sym_start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        end
    };
    let sym_block = &content[sym_start..sym_end];

    // Extract pins
    let mut pins = Vec::new();
    let mut search = sym_block;
    while let Some(pos) = search.find("\n    (pin ") {
        let inner = &search[pos + 10..];
        let pin_type = inner.split_whitespace().next().unwrap_or("").to_string();
        let pin_name = extract_sexp_string(inner, "name").unwrap_or_default();
        let pin_num = extract_sexp_string(inner, "number").unwrap_or_default();
        let (px, py) = extract_at_xy(inner).unwrap_or((0.0, 0.0));
        pins.push(json!({
            "number": pin_num,
            "name": pin_name,
            "type": pin_type,
            "x": px,
            "y": py
        }));
        search = &search[pos + 1..];
    }

    // Extract properties
    let mut properties = serde_json::Map::new();
    let mut search = sym_block;
    while let Some(pos) = search.find("\n    (property \"") {
        let inner = &search[pos + 16..];
        if let Some(key_end) = inner.find('"') {
            let key = &inner[..key_end];
            let val_start = inner[key_end + 1..]
                .find('"')
                .map(|i| key_end + 1 + i + 1)
                .unwrap_or(0);
            let val_end = inner[val_start..]
                .find('"')
                .map(|i| val_start + i)
                .unwrap_or(0);
            if val_end > val_start {
                properties.insert(
                    key.to_string(),
                    json!(inner[val_start..val_end].to_string()),
                );
            }
        }
        search = &search[pos + 1..];
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "lib_id": lib_id,
            "name": sym_name,
            "library": lib_nick,
            "pin_count": pins.len(),
            "pins": pins,
            "properties": properties
        }))
        .unwrap(),
    ))
}

fn extract_at_xy(block: &str) -> Option<(f64, f64)> {
    let pos = block.find("(at ")?;
    let after = &block[pos + 4..];
    let end = after.find(')')?;
    let parts: Vec<&str> = after[..end].split_whitespace().collect();
    let x = parts.first()?.parse::<f64>().ok()?;
    let y = parts.get(1)?.parse::<f64>().ok()?;
    Some((x, y))
}
