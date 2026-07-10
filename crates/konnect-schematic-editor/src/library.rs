//! Library symbol resolution — loads symbol definitions from KiCAD's installed libraries.
//!
//! KiCAD 10 stores symbols in `.kicad_symdir` directories:
//! ```text
//! C:\KiCad\10.0\share\kicad\symbols\Device.kicad_symdir\R.kicad_sym
//! C:\KiCad\10.0\share\kicad\symbols\power.kicad_symdir\VCC.kicad_sym
//! ```
//!
//! This module resolves a `lib_id` like `"Device:R"` to the full symbol S-expression
//! definition, and can inject it into a Schematic's `lib_symbols` section.

use crate::sexp::{parser, SexpNode};
use crate::Schematic;
use std::path::PathBuf;

/// Resolve a lib_id (e.g. "Device:R") to the full symbol S-expression string.
/// The returned string is the raw content of the `(symbol "R" ...)` block,
/// with the name prefixed as `"Device:R"`.
pub fn resolve_lib_symbol(lib_id: &str) -> Option<String> {
    let parts: Vec<&str> = lib_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return None;
    }
    let (library_name, symbol_name) = (parts[0], parts[1]);

    for base_dir in find_symbol_dirs() {
        // KiCAD 10: Library.kicad_symdir/SymbolName.kicad_sym
        let symdir = base_dir.join(format!("{}.kicad_symdir", library_name));
        let sym_file = symdir.join(format!("{}.kicad_sym", symbol_name));

        if sym_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&sym_file) {
                if let Some(block) = extract_symbol_block(&content, symbol_name) {
                    // Rename symbol to include library prefix
                    let mut renamed = block.replacen(
                        &format!("(symbol \"{}\"", symbol_name),
                        &format!("(symbol \"{}:{}\"", library_name, symbol_name),
                        1,
                    );
                    // Also fix (extends "ParentName") to use prefixed name
                    if let Some(ext_pos) = renamed.find("(extends \"") {
                        let after = &renamed[ext_pos + 10..];
                        if let Some(end) = after.find('"') {
                            let parent = after[..end].to_string();
                            renamed = renamed.replace(
                                &format!("(extends \"{}\")", parent),
                                &format!("(extends \"{}:{}\")", library_name, parent),
                            );
                        }
                    }
                    // Unit/variant sub-symbol names ("Name_0_1") must stay
                    // UNQUALIFIED: KiCAD prefixes only the top-level name in a
                    // schematic's lib_symbols table and rejects the whole file
                    // if nested unit names carry a "Lib:" prefix.
                    return Some(renamed);
                }
            }
        }

        // Fallback: KiCAD 8/9 format — single Library.kicad_sym file
        let legacy = base_dir.join(format!("{}.kicad_sym", library_name));
        if legacy.exists() {
            if let Ok(content) = std::fs::read_to_string(&legacy) {
                if let Some(block) = extract_symbol_block(&content, symbol_name) {
                    let mut renamed = block.replacen(
                        &format!("(symbol \"{}\"", symbol_name),
                        &format!("(symbol \"{}:{}\"", library_name, symbol_name),
                        1,
                    );
                    if let Some(ext_pos) = renamed.find("(extends \"") {
                        let after = &renamed[ext_pos + 10..];
                        if let Some(end) = after.find('"') {
                            let parent = after[..end].to_string();
                            renamed = renamed.replace(
                                &format!("(extends \"{}\")", parent),
                                &format!("(extends \"{}:{}\")", library_name, parent),
                            );
                        }
                    }
                    // Unit sub-symbol names stay unqualified — see note above.
                    return Some(renamed);
                }
            }
        }
    }
    None
}

/// Resolve a lib_id to a parsed SexpNode tree.
pub fn resolve_lib_symbol_node(lib_id: &str) -> Option<SexpNode> {
    let raw = resolve_lib_symbol(lib_id)?;
    parser::parse(&raw).ok()
}

/// Resolve a lib_id to a parsed, self-contained SexpNode tree with any
/// `extends` chain flattened, the way KiCAD's GUI embeds derived symbols:
/// the child keeps its own name and `(property ...)` fields, and inherits
/// everything else (graphics, pins, unit sub-symbols, pin_names/pin_numbers,
/// body attributes) from its resolved parent. Unit sub-symbols are renamed
/// from `Parent_U_S` to `Child_U_S` to match the top-level name.
pub fn resolve_lib_symbol_node_flattened(lib_id: &str) -> Option<SexpNode> {
    let child = resolve_lib_symbol_node(lib_id)?;
    let Some(extends) = child
        .find("extends")
        .and_then(|e| e.value())
        .map(str::to_owned)
    else {
        return Some(child); // not derived
    };

    // resolve_lib_symbol qualifies extends with the child's library prefix.
    let parent_lib_id = if extends.contains(':') {
        extends
    } else {
        let lib = lib_id.split(':').next().unwrap_or("");
        format!("{}:{}", lib, extends)
    };
    // Recurse: parents can themselves be derived.
    let parent = resolve_lib_symbol_node_flattened(&parent_lib_id)?;

    let child_base = lib_id.split(':').nth(1).unwrap_or(lib_id);
    let parent_base = parent_lib_id.split(':').nth(1).unwrap_or(&parent_lib_id);

    // Start from the parent's full definition.
    let SexpNode::List(parent_children) = parent else {
        return Some(child);
    };
    let mut merged: Vec<SexpNode> = Vec::with_capacity(parent_children.len());
    for node in parent_children {
        match node {
            // Drop the parent's name and properties; the child supplies both.
            SexpNode::Str(_) => merged.push(SexpNode::Str(lib_id.to_owned())),
            ref n if n.tag() == Some("property") => {}
            // Rename unit sub-symbols Parent_U_S -> Child_U_S.
            SexpNode::List(mut sub) if sub.first().and_then(|t| t.text()) == Some("symbol") => {
                for c in sub.iter_mut().skip(1) {
                    if let SexpNode::Str(name) = c {
                        if let Some(rest) = name.strip_prefix(&format!("{}_", parent_base)) {
                            *name = format!("{}_{}", child_base, rest);
                        }
                        break;
                    }
                }
                merged.push(SexpNode::List(sub));
            }
            other => merged.push(other),
        }
    }

    // Insert the child's properties after the header attributes (i.e. before
    // the first unit sub-symbol), preserving KiCAD's usual ordering.
    let child_props: Vec<SexpNode> = child
        .args()
        .iter()
        .filter(|n| n.tag() == Some("property"))
        .cloned()
        .collect();
    let insert_at = merged
        .iter()
        .position(|n| n.tag() == Some("symbol"))
        .unwrap_or(merged.len());
    for (i, prop) in child_props.into_iter().enumerate() {
        merged.insert(insert_at + i, prop);
    }

    Some(SexpNode::List(merged))
}

/// Ensure a library symbol definition is present in the schematic's lib_symbols section.
/// If the symbol is already present (by name), does nothing.
/// If the lib_symbols node doesn't exist in raw_other, creates one.
/// Handles `(extends "ParentName")` — automatically embeds the parent symbol too.
pub fn ensure_lib_symbol(schematic: &mut Schematic, lib_id: &str) {
    // Check if already present
    let check_name = format!("\"{}\"", lib_id);
    let already_present = schematic.raw_other.iter().any(|node| {
        if node.tag() == Some("lib_symbols") {
            let content = format!("{:?}", node);
            content.contains(&check_name)
        } else {
            false
        }
    });
    if already_present {
        return;
    }

    // Resolve and embed the symbol. Derived symbols (`extends`) are flattened
    // into self-contained definitions first: KiCAD's schematic loader does not
    // resolve `extends` inside a lib_symbols table (the GUI flattens derived
    // symbols when it embeds them), so an extends-only embed renders with no
    // body and fails lib_symbol_mismatch checks.
    let sym_node = match resolve_lib_symbol_node_flattened(lib_id) {
        Some(n) => n,
        None => return,
    };

    // Find or create the lib_symbols node
    let lib_syms_idx = schematic
        .raw_other
        .iter()
        .position(|n| n.tag() == Some("lib_symbols"));

    match lib_syms_idx {
        Some(idx) => {
            // Append the symbol to the existing lib_symbols list
            if let SexpNode::List(ref mut children) = schematic.raw_other[idx] {
                children.push(sym_node);
            }
        }
        None => {
            // Create a new lib_symbols node with this symbol
            let lib_syms =
                SexpNode::List(vec![SexpNode::Atom("lib_symbols".to_string()), sym_node]);
            // Insert at the beginning of raw_other (lib_symbols should come early)
            schematic.raw_other.insert(0, lib_syms);
        }
    }
}

/// Extract a `(symbol "NAME" ...)` block from file content by balanced-paren matching.
fn extract_symbol_block(content: &str, symbol_name: &str) -> Option<String> {
    let pattern = format!("(symbol \"{}\"", symbol_name);
    let start = content.find(&pattern)?;
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
    if end > start {
        Some(content[start..end].to_string())
    } else {
        None
    }
}

/// Find directories where KiCAD symbol libraries are stored.
pub fn find_symbol_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(dir) = std::env::var("KICAD10_SYMBOL_DIR") {
        let p = PathBuf::from(&dir);
        if p.is_dir() {
            dirs.push(p);
        }
    }

    #[cfg(target_os = "windows")]
    {
        let candidates = [
            r"C:\KiCad\10.0\share\kicad\symbols",
            r"C:\Program Files\KiCad\10.0\share\kicad\symbols",
            r"C:\KiCad\9.0\share\kicad\symbols",
            r"C:\Program Files\KiCad\9.0\share\kicad\symbols",
        ];
        for c in &candidates {
            let p = PathBuf::from(c);
            if p.is_dir() && !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let candidates = ["/usr/share/kicad/symbols", "/usr/local/share/kicad/symbols"];
        for c in &candidates {
            let p = PathBuf::from(c);
            if p.is_dir() && !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }

    dirs
}
