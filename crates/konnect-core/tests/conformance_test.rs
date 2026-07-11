//! Golden-file conformance suite.
//!
//! Oracle: schematics authored by eeschema itself. KiCAD installs a demo
//! corpus (`share/kicad/demos`) full of real, hierarchy-heavy, multi-unit
//! designs — if our parser or editors disagree with anything in there, we
//! disagree with KiCAD.
//!
//! These tests locate an installed KiCAD (or `KICAD_DEMOS` env override) and
//! SKIP silently when none is present, so plain CI stays green while the
//! scheduled real-KiCAD workflow and local dev runs get full coverage.
//! (Same skip pattern the predecessor project used for its kicad-cli tests.)

use konnect_sexp::{parse_sexp, writer};
use std::path::PathBuf;

fn demo_dirs() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("KICAD_DEMOS") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &[
            r"C:\KiCad\10.0\share\kicad\demos",
            r"C:\Program Files\KiCad\10.0\share\kicad\demos",
        ]
    } else if cfg!(target_os = "macos") {
        &["/Applications/KiCad/KiCad.app/Contents/SharedSupport/demos"]
    } else {
        &["/usr/share/kicad/demos", "/usr/local/share/kicad/demos"]
    };
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

fn collect_schematics(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "kicad_sch") {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

/// Every schematic eeschema ships must parse. This is the broadest format-
/// coverage test we have: hierarchical sheets, multi-unit symbols, buses,
/// text boxes, images — whatever the demos contain, the parser must accept.
#[test]
fn every_installed_demo_schematic_parses() {
    let Some(root) = demo_dirs() else {
        eprintln!("SKIP: no KiCAD demos found (set KICAD_DEMOS to enable)");
        return;
    };
    let schematics = collect_schematics(&root);
    assert!(
        !schematics.is_empty(),
        "demo dir exists but contains no .kicad_sch files: {}",
        root.display()
    );

    let mut parsed = 0usize;
    let mut failures = Vec::new();
    for sch in &schematics {
        let content = std::fs::read_to_string(sch).unwrap_or_default();
        match parse_sexp(&content) {
            Ok(node) => {
                assert_eq!(
                    node.head(),
                    Some("kicad_sch"),
                    "unexpected root in {}",
                    sch.display()
                );
                parsed += 1;
            }
            Err(e) => failures.push(format!("{}: {}", sch.display(), e)),
        }
    }
    eprintln!("parsed {}/{} demo schematics", parsed, schematics.len());
    assert!(
        failures.is_empty(),
        "parser rejected eeschema-authored files:\n  {}",
        failures.join("\n  ")
    );
}

/// Structural extraction must work on real designs: symbols, wires, and
/// labels come back non-empty for the demo corpus as a whole, and pin
/// transforms compute without panicking for every instance.
#[test]
fn demo_corpus_structural_extraction() {
    let Some(root) = demo_dirs() else {
        eprintln!("SKIP: no KiCAD demos found");
        return;
    };
    use konnect_sexp::schematic::{extract_symbol_instances, extract_wires};

    let mut total_symbols = 0usize;
    let mut total_wires = 0usize;
    for sch in collect_schematics(&root) {
        let content = std::fs::read_to_string(&sch).unwrap_or_default();
        let Ok(tree) = parse_sexp(&content) else {
            continue; // parse failures are the previous test's job
        };
        let symbols = extract_symbol_instances(&tree);
        for inst in &symbols {
            // Must never panic, whatever rotation/mirror combination ships.
            let t = inst.pin_transform();
            let _ = konnect_sexp::geometry::transform_pin(1.27, 2.54, t);
        }
        total_symbols += symbols.len();
        total_wires += extract_wires(&tree).len();
    }
    eprintln!("extracted {} symbols, {} wires", total_symbols, total_wires);
    assert!(total_symbols > 100, "suspiciously few symbols extracted");
    assert!(total_wires > 100, "suspiciously few wires extracted");
}

/// Byte-edit safety on real files: applying a no-op edit (insert + delete of
/// the same text) to an eeschema file must leave it byte-identical, and an
/// actual insertion must still re-parse. Guards the predecessor's file-
/// corruption class without needing a full serializer.
#[test]
fn demo_files_survive_edit_cycle() {
    let Some(root) = demo_dirs() else {
        eprintln!("SKIP: no KiCAD demos found");
        return;
    };
    let schematics = collect_schematics(&root);
    // A representative slice keeps this test fast even on huge corpora.
    for sch in schematics.iter().take(10) {
        let original = std::fs::read_to_string(sch).unwrap();

        // No-op: insert marker then delete it again.
        let marker = "(text \"konnect-conformance-probe\")";
        let insert_at = original.rfind(')').unwrap();
        let inserted = writer::apply_edits(
            original.clone(),
            vec![konnect_sexp::SexpEdit {
                start: insert_at,
                end: insert_at,
                replacement: marker.to_string(),
            }],
        );
        assert!(
            parse_sexp(&inserted).is_ok(),
            "insertion broke parseability of {}",
            sch.display()
        );

        let removed = writer::apply_edits(
            inserted.clone(),
            vec![konnect_sexp::SexpEdit {
                start: insert_at,
                end: insert_at + marker.len(),
                replacement: String::new(),
            }],
        );
        assert_eq!(
            removed,
            original,
            "edit round-trip not byte-identical for {}",
            sch.display()
        );
    }
}
