# Roadmap

The near-term direction for Konnect. No dates — items ship when they're solid.
Opening an issue is the best way to influence priority.

## Platform

- **Linux and macOS builds.** The code already carries `#[cfg]` branches and Unix
  paths for both platforms, and CI checks all three OSes — what remains is release
  packaging, per-platform QA against a running KiCAD, and macOS code signing /
  notarization.
- **KiCAD PCM publication** — submit the plugin to the official KiCAD addon
  repository once the first tagged release is out.

## Tools

- **Symbol & footprint creation** — author new library parts from scratch, not
  just search and place existing ones.
- **Eagle project import** — migrate legacy Eagle designs.

## Infrastructure

- **Deeper end-to-end tests** — tool-handler tests against a mocked IPC endpoint.

## Done

- ~~HTTP transport~~ — Streamable HTTP (MCP spec 2025-06-18) available via
  `transport = "http"` (or `"both"`): POST + GET (SSE) on a single `/mcp`
  endpoint, Origin validation, and a `/health` probe.
- ~~Additional export formats~~ — IPC-2581, ODB++, GenCAD, and DXF are now
  available via `export_ipc2581`, `export_odb`, `export_gencad`, and
  `export_dxf` in the `pcb_export` toolset (all backed by native `kicad-cli`
  subcommands, verified against KiCAD 10.0).
- ~~Retry/backoff for external services~~ — the JLCPCB database download and
  both LCSC datasheet lookups now retry transient failures (network errors,
  429, 5xx) with exponential backoff via `get_with_backoff` in
  `crates/konnect-core/src/tools/integration.rs`.
- ~~Component search caching~~ — `search_jlcpcb_parts`, `get_jlcpcb_part`, and
  `suggest_jlcpcb_alternatives` now cache results for 5 minutes via a shared
  `QueryCache` on `ToolContext`; responses carry a `"cached"` field.
- ~~Hierarchical sheets~~ — create and manage multi-sheet schematics via the
  new `sch_hierarchy` toolset: sheet lifecycle (add/edit/move/delete/duplicate,
  recursive hierarchy and page-numbering queries) plus sheet pin lifecycle
  (import from hierarchical labels, add/edit/delete pins, pin/label sync
  validation).
- ~~`import_svg_logo`~~ — import an SVG file as filled silkscreen/copper
  artwork via the new `import_svg_logo` tool in the `pcb_board` toolset.
  Curved paths (quadratic/cubic Bezier) are flattened into polygon outlines
  since KiCAD's board format doesn't support curves in filled shapes. Tries
  the IPC API first, falls back to a direct file edit if KiCAD isn't running.
- ~~Multi-sheet schematic viewer~~ — point the viewer at the root schematic of a
  hierarchical design and it walks every reachable sheet, renders each via
  `kicad-cli`, and offers a depth-indented sheet selector. Edits saved from KiCAD
  re-render only the changed sheets and refresh live; rendering runs against
  temp-folder snapshots so the viewer never blocks KiCAD from saving.
