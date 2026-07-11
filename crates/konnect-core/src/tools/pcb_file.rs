//! Shared `.kicad_pcb` file-edit helpers backing the file-fallback paths of
//! the `pcb_*` toolsets (see `pcb_ipc` for the IPC-first pattern).
//!
//! All edits follow the konnect-sexp rule: targeted string edits on the raw
//! content, never parse → re-serialize round-trips.

use konnect_sexp::writer::{find_balanced_block, new_uuid, SexpEdit};
use std::path::PathBuf;

// ─── Generic s-expression block scanning ─────────────────────────────────────

/// Byte spans of all depth-1 sublists inside a balanced `(head ...)` block,
/// quote-aware so parens inside strings don't confuse the depth count.
pub fn direct_child_spans(block: &str) -> Vec<(usize, usize)> {
    let bytes = block.as_bytes();
    let mut spans = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    let mut child_start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match b {
                b'\\' => escape = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'(' => {
                depth += 1;
                if depth == 2 {
                    child_start = i;
                }
            }
            b')' => {
                if depth == 2 {
                    spans.push((child_start, i + 1));
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    spans
}

/// The head token of a `(head ...)` s-expression string.
pub fn node_head(node: &str) -> &str {
    node[1..]
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '"')
        .next()
        .unwrap_or("")
}

/// Parse `(at x y [rot])` text into `(x, y, rot)`.
pub fn parse_at(at_node: &str) -> (f64, f64, f64) {
    let inner = at_node
        .trim_start_matches('(')
        .trim_start_matches("at")
        .trim_end_matches(')');
    let mut it = inner.split_whitespace();
    let x = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let y = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let r = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    (x, y, r)
}

/// Normalize an angle into `[0, 360)`.
pub fn normalize_angle(a: f64) -> f64 {
    let r = a % 360.0;
    if r < 0.0 {
        r + 360.0
    } else {
        r
    }
}

fn format_at(x: f64, y: f64, rot: f64) -> String {
    let rot = normalize_angle(rot);
    if rot == 0.0 {
        format!("(at {x} {y})")
    } else {
        format!("(at {x} {y} {rot})")
    }
}

/// Span of the block-level `(at ...)` anchor — the first depth-1 `at` child.
/// Children like pads/properties keep their own `(at ...)` one level deeper,
/// so they are not confused with the anchor.
pub fn anchor_at_span(block: &str) -> Option<(usize, usize)> {
    direct_child_spans(block)
        .into_iter()
        .find(|&(s, e)| node_head(&block[s..e]) == "at")
}

/// Insert `sexp` before the board file's final closing paren.
pub fn append_to_board(content: String, sexp: String) -> String {
    let close = content.rfind(')').unwrap_or(content.len());
    konnect_sexp::writer::apply_edits(content, vec![SexpEdit::insert(close, sexp)])
}

// ─── Footprint blocks ────────────────────────────────────────────────────────

/// Byte span of the `(footprint ...)` block whose Reference property equals
/// `reference`.
pub fn find_footprint_block(content: &str, reference: &str) -> Option<(usize, usize)> {
    let needle = format!("(property \"Reference\" \"{}\"", reference);
    let mut pos = 0;
    while let Some(rel) = content[pos..].find("(footprint") {
        let start = pos + rel;
        let (s, e) = find_balanced_block(content, start)?;
        if content[s..e].contains(&needle) {
            return Some((s, e));
        }
        pos = e;
    }
    None
}

/// Edit that moves the footprint at `fp_span` to a new position, preserving
/// its rotation. Pad/text positions are footprint-relative in the file
/// format, so only the anchor changes.
pub fn footprint_move_edit(
    content: &str,
    fp_span: (usize, usize),
    x: f64,
    y: f64,
) -> anyhow::Result<SexpEdit> {
    let (fs, fe) = fp_span;
    let block = &content[fs..fe];
    let (a_s, a_e) =
        anchor_at_span(block).ok_or_else(|| anyhow::anyhow!("Footprint has no (at) anchor"))?;
    let (_, _, rot) = parse_at(&block[a_s..a_e]);
    Ok(SexpEdit::replace(fs + a_s, fs + a_e, format_at(x, y, rot)))
}

/// Edits that set the footprint at `fp_span` to `new_rot` degrees.
///
/// KiCad board-file semantics: pad / property / fp_text `(at ...)` angles
/// inside a footprint are stored as child-relative angle PLUS the footprint
/// angle, while their x/y stay in unrotated footprint-local coordinates. A
/// correct rotation therefore rewrites the anchor angle AND adds the delta to
/// every child angle — changing only the anchor leaves pad orientations stale
/// (the classic courtyard/pad-shape corruption from naive scripts).
pub fn footprint_rotation_edits(
    content: &str,
    fp_span: (usize, usize),
    new_rot: f64,
) -> anyhow::Result<Vec<SexpEdit>> {
    let (fs, fe) = fp_span;
    let block = &content[fs..fe];
    let (a_s, a_e) =
        anchor_at_span(block).ok_or_else(|| anyhow::anyhow!("Footprint has no (at) anchor"))?;
    let (x, y, old_rot) = parse_at(&block[a_s..a_e]);
    let delta = new_rot - old_rot;

    let mut edits = vec![SexpEdit::replace(
        fs + a_s,
        fs + a_e,
        format_at(x, y, new_rot),
    )];
    if delta != 0.0 {
        for (c_s, c_e) in direct_child_spans(block) {
            let child = &block[c_s..c_e];
            if !matches!(node_head(child), "pad" | "property" | "fp_text") {
                continue;
            }
            if let Some((p_s, p_e)) = anchor_at_span(child) {
                let (px, py, pr) = parse_at(&child[p_s..p_e]);
                edits.push(SexpEdit::replace(
                    fs + c_s + p_s,
                    fs + c_s + p_e,
                    format_at(px, py, pr + delta),
                ));
            }
        }
    }
    Ok(edits)
}

fn escape_sexp_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Locate the value span of `(property "<name>" "<value>"` inside `block`
/// (offsets relative to `block`, excluding the quotes).
fn property_value_span(block: &str, name: &str) -> Option<(usize, usize)> {
    let pat = format!("(property \"{}\" \"", name);
    let start = block.find(&pat)? + pat.len();
    let mut end = start;
    let bytes = block.as_bytes();
    while end < bytes.len() {
        match bytes[end] {
            b'\\' => end += 2,
            b'"' => return Some((start, end)),
            _ => end += 1,
        }
    }
    None
}

/// Edit that replaces the footprint's Value property text.
pub fn footprint_set_value_edit(
    content: &str,
    fp_span: (usize, usize),
    new_value: &str,
) -> anyhow::Result<SexpEdit> {
    let (fs, fe) = fp_span;
    let block = &content[fs..fe];
    let (v_s, v_e) = property_value_span(block, "Value")
        .ok_or_else(|| anyhow::anyhow!("Footprint has no Value property"))?;
    Ok(SexpEdit::replace(
        fs + v_s,
        fs + v_e,
        escape_sexp_string(new_value),
    ))
}

/// Replace every `(uuid "...")` value in `block` with a freshly generated
/// UUID. Used when duplicating a footprint block — KiCad requires unique
/// KIIDs across the board.
pub fn regenerate_uuids(block: &str) -> String {
    let mut out = String::with_capacity(block.len());
    let mut rest = block;
    while let Some(p) = rest.find("(uuid \"") {
        let v_start = p + "(uuid \"".len();
        let Some(v_len) = rest[v_start..].find('"') else {
            break;
        };
        out.push_str(&rest[..v_start]);
        out.push_str(&new_uuid());
        rest = &rest[v_start + v_len..];
    }
    out.push_str(rest);
    out
}

/// Replace the Reference property value inside a detached footprint block
/// string (used for duplication / library placement, not for in-file edits).
pub fn set_reference_in_block(block: &str, new_reference: &str) -> anyhow::Result<String> {
    let (v_s, v_e) = property_value_span(block, "Reference")
        .ok_or_else(|| anyhow::anyhow!("Footprint block has no Reference property"))?;
    let mut out = String::with_capacity(block.len());
    out.push_str(&block[..v_s]);
    out.push_str(&escape_sexp_string(new_reference));
    out.push_str(&block[v_e..]);
    Ok(out)
}

// ─── Library footprint resolution (file-based place_component) ──────────────

/// Expand `${VAR}` references in an fp-lib-table URI from the environment,
/// with built-in defaults for the standard KiCAD footprint-dir variables.
fn expand_kicad_uri(uri: &str) -> String {
    let mut out = String::new();
    let mut rest = uri;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let var = &after[..end];
                if let Ok(v) = std::env::var(var) {
                    out.push_str(&v);
                } else if var.ends_with("_FOOTPRINT_DIR") {
                    if let Some(dir) = standard_footprint_dirs().into_iter().find(|d| d.is_dir()) {
                        out.push_str(&dir.to_string_lossy());
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(rest);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Platform-standard directories holding the bundled `*.pretty` libraries.
fn standard_footprint_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for var in ["KICAD10_FOOTPRINT_DIR", "KICAD9_FOOTPRINT_DIR"] {
        if let Ok(v) = std::env::var(var) {
            dirs.push(PathBuf::from(v));
        }
    }
    #[cfg(target_os = "macos")]
    dirs.push(PathBuf::from(
        "/Applications/KiCad/KiCad.app/Contents/SharedSupport/footprints",
    ));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        dirs.push(PathBuf::from("/usr/share/kicad/footprints"));
        dirs.push(PathBuf::from("/usr/local/share/kicad/footprints"));
    }
    #[cfg(target_os = "windows")]
    {
        dirs.push(PathBuf::from(r"C:\Program Files\KiCad\10.0\share\kicad\footprints"));
        dirs.push(PathBuf::from(r"C:\Program Files\KiCad\9.0\share\kicad\footprints"));
    }
    dirs
}

/// Resolve a `Library:Footprint` id to a `.kicad_mod` file: first via the
/// global fp-lib-table (with `${VAR}` expansion), then the platform-standard
/// footprint directories.
pub fn resolve_footprint_mod_path(lib_id: &str) -> Option<PathBuf> {
    let (nick, name) = lib_id.split_once(':')?;

    let table = super::kicad_config_dir().join("fp-lib-table");
    if let Ok(tc) = std::fs::read_to_string(&table) {
        // Entries look like: (lib (name "NICK") ... (uri "..."))
        let pat = format!("(name \"{}\")", nick);
        if let Some(p) = tc.find(&pat) {
            if let Some(u_start) = tc[p..].find("(uri \"").map(|i| p + i + "(uri \"".len()) {
                if let Some(u_len) = tc[u_start..].find('"') {
                    let uri = expand_kicad_uri(&tc[u_start..u_start + u_len]);
                    let candidate = PathBuf::from(uri).join(format!("{name}.kicad_mod"));
                    if candidate.exists() {
                        return Some(candidate);
                    }
                }
            }
        }
    }

    for base in standard_footprint_dirs() {
        let candidate = base
            .join(format!("{nick}.pretty"))
            .join(format!("{name}.kicad_mod"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Turn a library `.kicad_mod` file's content into a board-ready
/// `(footprint ...)` block: renamed to the full `Library:Footprint` id,
/// positioned at `(x, y, rot)`, reference set, library-file-only header
/// tokens stripped, and a footprint-level uuid added. `rot` is applied with
/// correct board-file semantics (child angles offset by the rotation).
pub fn footprint_sexp_for_board(
    mod_content: &str,
    lib_id: &str,
    reference: &str,
    x: f64,
    y: f64,
    rot: f64,
) -> anyhow::Result<String> {
    let start = mod_content
        .find("(footprint")
        .ok_or_else(|| anyhow::anyhow!("No (footprint ...) block in .kicad_mod file"))?;
    let (s, e) = find_balanced_block(mod_content, start)
        .ok_or_else(|| anyhow::anyhow!("Unbalanced .kicad_mod content"))?;
    let block = &mod_content[s..e];

    // Rename the footprint to its full Library:Footprint id.
    let name_start = block
        .find('"')
        .ok_or_else(|| anyhow::anyhow!("Footprint block has no name string"))?
        + 1;
    let name_end = block[name_start..]
        .find('"')
        .map(|i| name_start + i)
        .ok_or_else(|| anyhow::anyhow!("Unterminated footprint name"))?;
    let escaped_id = escape_sexp_string(lib_id);
    // Name-end offset in the renamed block (the name just changed length).
    let out_name_end = name_start + escaped_id.len();
    let mut out = format!(
        "{}{}{}",
        &block[..name_start],
        escaped_id,
        &block[name_end..]
    );

    // Strip library-file-only header tokens (version / generator) and give
    // the instance a uuid + position. Insert `(at ...)` right after the name.
    let mut edits: Vec<SexpEdit> = Vec::new();
    for (c_s, c_e) in direct_child_spans(&out) {
        if matches!(
            node_head(&out[c_s..c_e]),
            "version" | "generator" | "generator_version"
        ) {
            // Also strip leading whitespace back to the previous newline.
            let mut ws = c_s;
            let bytes = out.as_bytes();
            while ws > 0 && (bytes[ws - 1] == b' ' || bytes[ws - 1] == b'\t') {
                ws -= 1;
            }
            if ws > 0 && bytes[ws - 1] == b'\n' {
                ws -= 1;
            }
            edits.push(SexpEdit::delete(ws, c_e));
        }
    }
    // Insert the anchor unrotated; footprint_rotation_edits below applies the
    // requested angle so pad/text child angles get the same delta.
    let at_insert = format!("\n    (at {x} {y})");
    edits.push(SexpEdit::insert(out_name_end + 1, at_insert));
    let uuid_insert = format!("\n    (uuid \"{}\")", new_uuid());
    edits.push(SexpEdit::insert(out_name_end + 1, uuid_insert));
    out = konnect_sexp::writer::apply_edits(out, edits);

    // Reference: library footprints ship with REF**.
    out = set_reference_in_block(&out, reference)?;

    // Board-file rotation semantics for the children.
    if normalize_angle(rot) != 0.0 {
        let span = (0, out.len());
        let edits = footprint_rotation_edits(&out, span, rot)?;
        out = konnect_sexp::writer::apply_edits(out, edits);
    }

    // Fresh uuids in case the library block carried any.
    Ok(format!("\n  {}", regenerate_uuids(&out)))
}

// ─── Tracks and vias ─────────────────────────────────────────────────────────

/// A `(segment ...)` track for direct file insertion.
pub fn format_segment(
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    width: f64,
    layer: &str,
    net_id: i32,
) -> String {
    let uuid = new_uuid();
    format!(
        "\n  (segment\n    (start {x1} {y1})\n    (end {x2} {y2})\n    (width {width})\n    \
         (layer \"{layer}\")\n    (net {net_id})\n    (uuid \"{uuid}\")\n  )"
    )
}

/// A through-hole `(via ...)` for direct file insertion.
pub fn format_via(x: f64, y: f64, size: f64, drill: f64, net_id: i32) -> String {
    let uuid = new_uuid();
    format!(
        "\n  (via\n    (at {x} {y})\n    (size {size})\n    (drill {drill})\n    \
         (layers \"F.Cu\" \"B.Cu\")\n    (net {net_id})\n    (uuid \"{uuid}\")\n  )"
    )
}

/// Find the span of the top-level block (with one of the given heads) that
/// contains `(uuid "<uuid>")`. Used to delete/modify tracks by UUID.
pub fn find_block_by_uuid(
    content: &str,
    heads: &[&str],
    uuid: &str,
) -> Option<(usize, usize)> {
    let needle = format!("(uuid \"{}\")", uuid);
    for head in heads {
        let pat = format!("({head}");
        let mut pos = 0;
        while let Some(rel) = content[pos..].find(&pat) {
            let start = pos + rel;
            // Guard against prefix matches like "(via" inside "(vias".
            let after = content.as_bytes().get(start + pat.len());
            if !matches!(after, Some(b' ') | Some(b'\n') | Some(b'\t') | Some(b'(')) {
                pos = start + pat.len();
                continue;
            }
            let Some((s, e)) = find_balanced_block(content, start) else {
                break;
            };
            if content[s..e].contains(&needle) {
                return Some((s, e));
            }
            pos = e;
        }
    }
    None
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_sexp::writer::apply_edits;

    const BOARD: &str = r#"(kicad_pcb
  (version 20250610)
  (generator "konnect")
  (net 0 "")
  (net 1 "GND")
  (footprint "Resistor_SMD:R_0402_1005Metric"
    (layer "F.Cu")
    (uuid "aaaaaaaa-1111-2222-3333-444444444444")
    (at 10 20)
    (property "Reference" "R1"
      (at 0 -1.17 0)
      (layer "F.SilkS")
      (uuid "bbbbbbbb-1111-2222-3333-444444444444")
    )
    (property "Value" "10k"
      (at 0 1.17 0)
      (layer "F.Fab")
      (uuid "cccccccc-1111-2222-3333-444444444444")
    )
    (pad "1" smd roundrect
      (at -0.51 0)
      (size 0.54 0.64)
      (layers "F.Cu" "F.Paste" "F.Mask")
      (net 1 "GND")
      (uuid "dddddddd-1111-2222-3333-444444444444")
    )
    (pad "2" smd roundrect
      (at 0.51 0 90)
      (size 0.54 0.64)
      (layers "F.Cu" "F.Paste" "F.Mask")
      (uuid "eeeeeeee-1111-2222-3333-444444444444")
    )
  )
  (segment
    (start 1 1)
    (end 2 1)
    (width 0.25)
    (layer "F.Cu")
    (net 1)
    (uuid "ffffffff-1111-2222-3333-444444444444")
  )
)
"#;

    #[test]
    fn find_footprint_block_matches_reference() {
        let (s, e) = find_footprint_block(BOARD, "R1").expect("R1 exists");
        assert!(BOARD[s..e].starts_with("(footprint"));
        assert!(BOARD[s..e].contains("R_0402"));
        assert!(find_footprint_block(BOARD, "R2").is_none());
    }

    #[test]
    fn move_edit_replaces_only_the_anchor() {
        let span = find_footprint_block(BOARD, "R1").unwrap();
        let edit = footprint_move_edit(BOARD, span, 55.0, 66.5).unwrap();
        let moved = apply_edits(BOARD.to_string(), vec![edit]);
        assert!(moved.contains("(at 55 66.5)"));
        // Pad-relative positions untouched.
        assert!(moved.contains("(at -0.51 0)"));
        assert!(moved.contains("(at 0 -1.17 0)"));
    }

    #[test]
    fn rotation_edits_offset_child_angles() {
        let span = find_footprint_block(BOARD, "R1").unwrap();
        let edits = footprint_rotation_edits(BOARD, span, 90.0).unwrap();
        let rotated = apply_edits(BOARD.to_string(), edits);
        // Anchor gains the angle; children gain the delta.
        assert!(rotated.contains("(at 10 20 90)"));
        assert!(rotated.contains("(at -0.51 0 90)"), "pad 1 angle offset");
        assert!(rotated.contains("(at 0.51 0 180)"), "pad 2 angle 90+90");
        assert!(rotated.contains("(at 0 -1.17 90)"), "reference text angle");
    }

    #[test]
    fn set_value_edit_rewrites_value_property() {
        let span = find_footprint_block(BOARD, "R1").unwrap();
        let edit = footprint_set_value_edit(BOARD, span, "4.7k").unwrap();
        let edited = apply_edits(BOARD.to_string(), vec![edit]);
        assert!(edited.contains("(property \"Value\" \"4.7k\""));
        assert!(!edited.contains("\"10k\""));
    }

    #[test]
    fn regenerate_uuids_replaces_every_uuid() {
        let span = find_footprint_block(BOARD, "R1").unwrap();
        let fresh = regenerate_uuids(&BOARD[span.0..span.1]);
        assert!(!fresh.contains("aaaaaaaa"));
        assert!(!fresh.contains("dddddddd"));
        assert_eq!(fresh.matches("(uuid \"").count(), 5);
    }

    #[test]
    fn find_block_by_uuid_finds_segment() {
        let (s, e) = find_block_by_uuid(
            BOARD,
            &["segment", "via", "arc"],
            "ffffffff-1111-2222-3333-444444444444",
        )
        .expect("segment found");
        assert!(BOARD[s..e].starts_with("(segment"));
        assert!(find_block_by_uuid(BOARD, &["segment"], "no-such-uuid").is_none());
    }

    #[test]
    fn footprint_sexp_for_board_prepares_library_block() {
        let mod_content = r#"(footprint "R_0402_1005Metric"
  (version 20240108)
  (generator "pcbnew")
  (layer "F.Cu")
  (descr "Resistor SMD 0402 (chip)")
  (property "Reference" "REF**"
    (at 0 -1.17 0)
    (layer "F.SilkS")
  )
  (property "Value" "R_0402_1005Metric"
    (at 0 1.17 0)
    (layer "F.Fab")
  )
  (pad "1" smd roundrect
    (at -0.51 0)
    (size 0.54 0.64)
    (layers "F.Cu" "F.Paste" "F.Mask")
  )
)
"#;
        let block = footprint_sexp_for_board(
            mod_content,
            "Resistor_SMD:R_0402_1005Metric",
            "R7",
            12.0,
            34.0,
            90.0,
        )
        .unwrap();
        assert!(block.contains("(footprint \"Resistor_SMD:R_0402_1005Metric\""));
        assert!(block.contains("(at 12 34 90)"));
        assert!(block.contains("(property \"Reference\" \"R7\""));
        assert!(block.contains("(at -0.51 0 90)"), "pad angle offset by rot");
        assert!(block.contains("(uuid \""));
        assert!(!block.contains("(version"));
        assert!(!block.contains("(generator"));
        assert!(!block.contains("REF**"));
    }

    #[test]
    fn expand_kicad_uri_expands_env_vars() {
        std::env::set_var("KONNECT_TEST_FP_DIR", "/tmp/fps");
        assert_eq!(
            expand_kicad_uri("${KONNECT_TEST_FP_DIR}/Resistor_SMD.pretty"),
            "/tmp/fps/Resistor_SMD.pretty"
        );
    }
}
