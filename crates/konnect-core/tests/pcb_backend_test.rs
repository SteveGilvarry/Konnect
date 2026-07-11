//! Backend-selection tests for the pcb_* toolsets (the PR-#5 pattern).
//!
//! Covers the three contracts of the split-brain fix:
//!   1. Without any IPC transport, every migrated tool answers from the file
//!      and says so (`source: "file"`).
//!   2. With a *reachable but failing* IPC session (mock KiCAD returning an
//!      application error), write tools REFUSE the file fallback instead of
//!      editing the board behind the live session's back.
//!   3. `save_board` explains itself when no live session exists.
//!
//! No real KiCAD is used anywhere: the "no transport" contexts dial a
//! guaranteed-dead TCP port, and the "live but failing" session is a mock NNG
//! rep server. Contexts must NEVER use an empty ipc_address here — the client
//! would probe /tmp/kicad/api.sock and could reach a real session.

use konnect_core::mcp::protocol::{CallToolResult, ToolContent};
use konnect_core::router::ToolRouter;
use konnect_core::tools::{pcb_board, pcb_components, pcb_routing, ServerConfig, ToolContext, ToolDef};
use prost::Message;
use std::sync::Arc;
use std::time::Duration;

// ─── Harness ─────────────────────────────────────────────────────────────────

fn ctx_with_addr(addr: &str) -> Arc<ToolContext> {
    Arc::new(ToolContext::new(
        ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: addr.to_string(),
            project_dir: None,
            jlcpcb_db_path: None,
        },
        Arc::new(ToolRouter::new()),
    ))
}

/// Context whose IPC dial always fails fast → IpcAttempt::Unavailable.
fn no_transport_ctx() -> Arc<ToolContext> {
    ctx_with_addr("tcp://127.0.0.1:1")
}

async fn call(
    tools: &[ToolDef],
    name: &str,
    args: serde_json::Value,
    ctx: &Arc<ToolContext>,
) -> CallToolResult {
    let tool = tools
        .iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("tool '{name}' not found in toolset"));
    (tool.handler)(&args, ctx.clone())
        .await
        .unwrap_or_else(|e| panic!("tool '{name}' returned Err: {e}"))
}

fn body_json(result: &CallToolResult) -> serde_json::Value {
    match result.content.first() {
        Some(ToolContent::Text { text }) => serde_json::from_str(text)
            .unwrap_or_else(|_| panic!("non-JSON tool response: {text}")),
        other => panic!("expected text content, got {other:?}"),
    }
}

fn body_text(result: &CallToolResult) -> String {
    match result.content.first() {
        Some(ToolContent::Text { text }) => text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

const SEG_UUID: &str = "5f0d9c1e-0000-4000-8000-00000000aaaa";

fn board_fixture() -> String {
    format!(
        r#"(kicad_pcb
  (version 20250610)
  (generator "konnect")
  (general
    (thickness 1.6)
  )
  (paper "A4")
  (layers
    (0 "F.Cu" signal)
    (2 "B.Cu" signal)
    (9 "F.SilkS" user "F.Silkscreen")
    (25 "Edge.Cuts" user)
  )
  (net 0 "")
  (net 1 "GND")
  (footprint "Resistor_SMD:R_0402_1005Metric"
    (layer "F.Cu")
    (uuid "0e0e0e0e-0000-4000-8000-000000000001")
    (at 10 20)
    (property "Reference" "R1"
      (at 0 -1.17 0)
      (layer "F.SilkS")
      (uuid "0e0e0e0e-0000-4000-8000-000000000002")
    )
    (property "Value" "10k"
      (at 0 1.17 0)
      (layer "F.Fab")
      (uuid "0e0e0e0e-0000-4000-8000-000000000003")
    )
    (pad "1" smd roundrect
      (at -0.51 0)
      (size 0.54 0.64)
      (layers "F.Cu" "F.Paste" "F.Mask")
      (net 1 "GND")
      (uuid "0e0e0e0e-0000-4000-8000-000000000004")
    )
    (pad "2" smd roundrect
      (at 0.51 0)
      (size 0.54 0.64)
      (layers "F.Cu" "F.Paste" "F.Mask")
      (uuid "0e0e0e0e-0000-4000-8000-000000000005")
    )
  )
  (footprint "Resistor_SMD:R_0402_1005Metric"
    (layer "F.Cu")
    (uuid "0e0e0e0e-0000-4000-8000-000000000011")
    (at 30 20)
    (property "Reference" "R2"
      (at 0 -1.17 0)
      (layer "F.SilkS")
      (uuid "0e0e0e0e-0000-4000-8000-000000000012")
    )
    (property "Value" "10k"
      (at 0 1.17 0)
      (layer "F.Fab")
      (uuid "0e0e0e0e-0000-4000-8000-000000000013")
    )
    (pad "1" smd roundrect
      (at -0.51 0)
      (size 0.54 0.64)
      (layers "F.Cu" "F.Paste" "F.Mask")
      (net 1 "GND")
      (uuid "0e0e0e0e-0000-4000-8000-000000000014")
    )
    (pad "2" smd roundrect
      (at 0.51 0)
      (size 0.54 0.64)
      (layers "F.Cu" "F.Paste" "F.Mask")
      (uuid "0e0e0e0e-0000-4000-8000-000000000015")
    )
  )
  (segment
    (start 1 1)
    (end 5 1)
    (width 0.25)
    (layer "F.Cu")
    (net 1)
    (uuid "{SEG_UUID}")
  )
)
"#
    )
}

struct TempBoard {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl TempBoard {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, board_fixture()).expect("write fixture");
        TempBoard { _dir: dir, path }
    }
    fn arg(&self) -> String {
        self.path.to_string_lossy().to_string()
    }
    fn content(&self) -> String {
        std::fs::read_to_string(&self.path).expect("read board")
    }
}

// ─── Mock "live but failing" KiCAD session ──────────────────────────────────

/// A rep0 NNG server that answers EVERY request with an application error
/// (AS_BAD_REQUEST "no board open") — i.e. a reachable KiCAD whose calls fail
/// for a non-transport reason. Write tools must refuse against it.
struct FailingKicad {
    url: String,
    _thread: std::thread::JoinHandle<()>,
}

fn spawn_failing_kicad() -> FailingKicad {
    use konnect_ipc::gen::kiapi;
    use nng::options::Options;

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let url = format!("tcp://127.0.0.1:{port}");

    let listen_url = url.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock rep socket");
        socket
            .set_opt::<nng::options::RecvTimeout>(Some(Duration::from_secs(20)))
            .unwrap();
        socket.listen(&listen_url).expect("mock listen");
        let _ = ready_tx.send(());
        while let Ok(_msg) = socket.recv() {
            let resp = kiapi::common::ApiResponse {
                status: Some(kiapi::common::ApiResponseStatus {
                    status: kiapi::common::ApiStatusCode::AsBadRequest as i32,
                    error_message: "no board open".to_string(),
                }),
                header: None,
                message: None,
            };
            let out = nng::Message::from(resp.encode_to_vec().as_slice());
            if socket.send(out).is_err() {
                break;
            }
        }
    });
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("mock server failed to start listening");

    FailingKicad {
        url,
        _thread: thread,
    }
}

// ─── 1. File fallback without a transport: reads ────────────────────────────

#[tokio::test]
async fn component_read_tools_fall_back_to_file_without_socket() {
    let board = TempBoard::new();
    let ctx = no_transport_ctx();
    let tools = pcb_components::tools();

    let r = call(&tools, "find_component", serde_json::json!({ "board": board.arg(), "reference": "R1" }), &ctx).await;
    assert!(!r.is_error, "find_component: {}", body_text(&r));
    let v = body_json(&r);
    assert_eq!(v["source"], "file");
    assert_eq!(v["x"], 10.0);
    assert_eq!(v["y"], 20.0);

    let r = call(&tools, "get_component_list", serde_json::json!({ "board": board.arg() }), &ctx).await;
    let v = body_json(&r);
    assert_eq!(v["source"], "file");
    assert_eq!(v["count"], 2);

    let r = call(&tools, "get_component_pads", serde_json::json!({ "board": board.arg(), "reference": "R1" }), &ctx).await;
    let v = body_json(&r);
    assert_eq!(v["source"], "file");
    assert_eq!(v["pad_count"], 2);
    assert_eq!(v["pads"][0]["net"], "GND");

    let r = call(&tools, "get_pad_position", serde_json::json!({ "board": board.arg(), "reference": "R1", "pad_number": "2" }), &ctx).await;
    let v = body_json(&r);
    assert_eq!(v["source"], "file");
    assert_eq!(v["x"], 10.51);
}

#[tokio::test]
async fn routing_read_tools_fall_back_to_file_without_socket() {
    let board = TempBoard::new();
    let ctx = no_transport_ctx();
    let tools = pcb_routing::tools();

    let r = call(&tools, "get_nets_list", serde_json::json!({ "board": board.arg() }), &ctx).await;
    let v = body_json(&r);
    assert_eq!(v["source"], "file");
    assert_eq!(v["count"], 2);
    assert!(v["nets"].as_array().unwrap().iter().any(|n| n["name"] == "GND"));

    let r = call(&tools, "query_traces", serde_json::json!({ "board": board.arg(), "net_name": "GND" }), &ctx).await;
    let v = body_json(&r);
    assert_eq!(v["source"], "file");
    assert_eq!(v["count"], 1);
    assert_eq!(v["traces"][0]["layer"], "F.Cu");
}

#[tokio::test]
async fn board_read_tools_fall_back_to_file_without_socket() {
    let board = TempBoard::new();
    let ctx = no_transport_ctx();
    let tools = pcb_board::tools();

    let r = call(&tools, "get_board_info", serde_json::json!({ "board": board.arg() }), &ctx).await;
    let v = body_json(&r);
    assert_eq!(v["source"], "file");
    assert_eq!(v["layer_count"], 4);
    assert_eq!(v["net_count"], 1);

    let r = call(&tools, "get_layer_list", serde_json::json!({ "board": board.arg() }), &ctx).await;
    let v = body_json(&r);
    assert_eq!(v["source"], "file");
    assert_eq!(v["count"], 4);
    assert_eq!(v["layers"][0]["name"], "F.Cu");

    let r = call(&tools, "get_board_extents", serde_json::json!({ "board": board.arg() }), &ctx).await;
    let v = body_json(&r);
    assert_eq!(v["source"], "file");
}

// ─── 1b. File fallback without a transport: writes ──────────────────────────

#[tokio::test]
async fn component_write_tools_fall_back_to_file_without_socket() {
    let board = TempBoard::new();
    let ctx = no_transport_ctx();
    let tools = pcb_components::tools();

    let r = call(&tools, "move_component", serde_json::json!({ "board": board.arg(), "reference": "R1", "x": 42.0, "y": 43.0 }), &ctx).await;
    assert!(!r.is_error, "move_component: {}", body_text(&r));
    assert_eq!(body_json(&r)["source"], "file");
    assert!(board.content().contains("(at 42 43)"));

    let r = call(&tools, "rotate_component", serde_json::json!({ "board": board.arg(), "reference": "R1", "rotation": 90.0 }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");
    let c = board.content();
    assert!(c.contains("(at 42 43 90)"), "anchor angle set");
    assert!(c.contains("(at -0.51 0 90)"), "pad angle offset with footprint");

    let r = call(&tools, "edit_component", serde_json::json!({ "board": board.arg(), "reference": "R1", "value": "4.7k" }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");
    assert!(board.content().contains("(property \"Value\" \"4.7k\""));

    let r = call(&tools, "duplicate_component", serde_json::json!({ "board": board.arg(), "reference": "R1", "new_reference": "R9", "x": 70.0, "y": 80.0 }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");
    let c = board.content();
    assert!(c.contains("(property \"Reference\" \"R9\""));

    let r = call(&tools, "align_components", serde_json::json!({ "board": board.arg(), "references": ["R1", "R2"], "axis": "y", "value": 25.0 }), &ctx).await;
    let v = body_json(&r);
    assert_eq!(v["source"], "file");
    assert_eq!(v["aligned_count"], 2);
    assert!(board.content().contains("(at 30 25)"), "R2 aligned to y=25");

    let r = call(&tools, "delete_component", serde_json::json!({ "board": board.arg(), "reference": "R2" }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");
    assert!(!board.content().contains("\"R2\""));

    // The mutated board must still parse.
    konnect_sexp::parser::parse_sexp(&board.content()).expect("board still parses");
}

#[tokio::test]
async fn place_component_file_fallback_resolves_library_footprint() {
    // A private footprint library, injected via the standard env var.
    let libdir = tempfile::tempdir().expect("tempdir");
    let pretty = libdir.path().join("TestLib.pretty");
    std::fs::create_dir_all(&pretty).unwrap();
    std::fs::write(
        pretty.join("TP_1mm.kicad_mod"),
        r#"(footprint "TP_1mm"
  (version 20240108)
  (generator "pcbnew")
  (layer "F.Cu")
  (property "Reference" "REF**"
    (at 0 -1.2 0)
    (layer "F.SilkS")
  )
  (property "Value" "TP_1mm"
    (at 0 1.2 0)
    (layer "F.Fab")
  )
  (pad "1" smd circle
    (at 0 0)
    (size 1 1)
    (layers "F.Cu" "F.Mask")
  )
)
"#,
    )
    .unwrap();
    std::env::set_var("KICAD10_FOOTPRINT_DIR", libdir.path());

    let board = TempBoard::new();
    let ctx = no_transport_ctx();
    let tools = pcb_components::tools();

    let r = call(&tools, "place_component", serde_json::json!({
        "board": board.arg(), "footprint": "TestLib:TP_1mm", "reference": "TP1",
        "x": 5.0, "y": 6.0, "rotation": 0.0
    }), &ctx).await;
    assert!(!r.is_error, "place_component: {}", body_text(&r));
    assert_eq!(body_json(&r)["source"], "file");
    let c = board.content();
    assert!(c.contains("(footprint \"TestLib:TP_1mm\""));
    assert!(c.contains("(property \"Reference\" \"TP1\""));
    konnect_sexp::parser::parse_sexp(&c).expect("board still parses");

    // Array placement through the same resolution path.
    let r = call(&tools, "place_component_array", serde_json::json!({
        "board": board.arg(), "footprint": "TestLib:TP_1mm",
        "start_x": 50.0, "start_y": 50.0, "count_x": 3, "spacing_x": 2.0,
        "ref_prefix": "TP", "ref_start": 2
    }), &ctx).await;
    let v = body_json(&r);
    assert_eq!(v["source"], "file");
    assert_eq!(v["placed_count"], 3);
    let c = board.content();
    for r in ["TP2", "TP3", "TP4"] {
        assert!(c.contains(&format!("(property \"Reference\" \"{r}\"")));
    }
}

#[tokio::test]
async fn routing_write_tools_fall_back_to_file_without_socket() {
    let board = TempBoard::new();
    let ctx = no_transport_ctx();
    let tools = pcb_routing::tools();

    let r = call(&tools, "route_trace", serde_json::json!({
        "board": board.arg(), "net_name": "GND", "layer": "F.Cu",
        "x1": 1.0, "y1": 2.0, "x2": 3.0, "y2": 2.0, "width": 0.3
    }), &ctx).await;
    assert!(!r.is_error, "route_trace: {}", body_text(&r));
    assert_eq!(body_json(&r)["source"], "file");
    assert!(board.content().contains("(start 1 2)"));

    // Unknown net → actionable error, no silent net-0 copper.
    let r = call(&tools, "route_trace", serde_json::json!({
        "board": board.arg(), "net_name": "NOPE", "layer": "F.Cu",
        "x1": 0.0, "y1": 0.0, "x2": 1.0, "y2": 0.0
    }), &ctx).await;
    assert!(r.is_error);
    assert!(body_text(&r).contains("add_net"));

    let r = call(&tools, "add_via", serde_json::json!({
        "board": board.arg(), "net_name": "GND", "x": 7.0, "y": 8.0
    }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");
    assert!(board.content().contains("(at 7 8)"));

    let r = call(&tools, "route_pad_to_pad", serde_json::json!({
        "board": board.arg(), "net_name": "GND",
        "ref1": "R1", "pad1": "2", "ref2": "R2", "pad2": "1"
    }), &ctx).await;
    let v = body_json(&r);
    assert_eq!(v["source"], "file");
    assert_eq!(v["from"]["x"], 10.51);
    assert_eq!(v["to"]["x"], 29.49);

    let r = call(&tools, "route_differential_pair", serde_json::json!({
        "board": board.arg(), "net_pos": "GND", "net_neg": "GND",
        "x1": 0.0, "y1": 10.0, "x2": 5.0, "y2": 10.0
    }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");

    let r = call(&tools, "modify_trace", serde_json::json!({
        "board": board.arg(), "uuid": SEG_UUID, "net_name": "GND", "layer": "B.Cu",
        "x1": 1.0, "y1": 1.0, "x2": 9.0, "y2": 1.0, "width": 0.5
    }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");
    let c = board.content();
    assert!(!c.contains(SEG_UUID), "old segment replaced");
    assert!(c.contains("(layer \"B.Cu\")"));

    let r = call(&tools, "add_net", serde_json::json!({ "board": board.arg(), "net_name": "VCC" }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");

    konnect_sexp::parser::parse_sexp(&board.content()).expect("board still parses");
}

#[tokio::test]
async fn delete_trace_file_fallback_removes_segment_by_uuid() {
    let board = TempBoard::new();
    let ctx = no_transport_ctx();
    let tools = pcb_routing::tools();

    let r = call(&tools, "delete_trace", serde_json::json!({ "board": board.arg(), "uuid": SEG_UUID }), &ctx).await;
    assert!(!r.is_error, "delete_trace: {}", body_text(&r));
    assert_eq!(body_json(&r)["source"], "file");
    assert!(!board.content().contains("(segment"));

    let r = call(&tools, "delete_trace", serde_json::json!({ "board": board.arg(), "uuid": "not-there" }), &ctx).await;
    assert!(r.is_error);
}

#[tokio::test]
async fn board_write_tools_report_source_without_socket() {
    let board = TempBoard::new();
    let ctx = no_transport_ctx();
    let tools = pcb_board::tools();

    let r = call(&tools, "set_board_size", serde_json::json!({ "board": board.arg(), "width": 50.0, "height": 40.0 }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");

    let r = call(&tools, "add_board_text", serde_json::json!({ "board": board.arg(), "text": "REV A", "x": 1.0, "y": 1.0 }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");

    let r = call(&tools, "set_active_layer", serde_json::json!({ "board": board.arg(), "layer": "B.Cu" }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");

    // Deliberately file-only tools must still declare their source.
    let r = call(&tools, "add_mounting_hole", serde_json::json!({ "board": board.arg(), "x": 3.0, "y": 3.0 }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");

    let r = call(&tools, "add_zone", serde_json::json!({
        "board": board.arg(), "net_name": "GND", "layer": "F.Cu",
        "points": [ {"x": 0.0, "y": 0.0}, {"x": 10.0, "y": 0.0}, {"x": 10.0, "y": 10.0} ]
    }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");

    let r = call(&tools, "add_layer", serde_json::json!({ "board": board.arg(), "layer_name": "In1.Cu" }), &ctx).await;
    assert_eq!(body_json(&r)["source"], "file");

    konnect_sexp::parser::parse_sexp(&board.content()).expect("board still parses");
}

// ─── 2. Write refusal against a live-but-failing session ────────────────────

#[tokio::test]
async fn write_tools_refuse_file_fallback_when_live_session_errors() {
    let mock = spawn_failing_kicad();
    let board = TempBoard::new();
    let before = board.content();
    let ctx = ctx_with_addr(&mock.url);

    let comp_tools = pcb_components::tools();
    let routing_tools = pcb_routing::tools();
    let board_tools = pcb_board::tools();

    let cases: Vec<(&[ToolDef], &str, serde_json::Value)> = vec![
        (&comp_tools, "move_component", serde_json::json!({ "board": board.arg(), "reference": "R1", "x": 1.0, "y": 1.0 })),
        (&comp_tools, "rotate_component", serde_json::json!({ "board": board.arg(), "reference": "R1", "rotation": 90.0 })),
        (&comp_tools, "delete_component", serde_json::json!({ "board": board.arg(), "reference": "R1" })),
        (&comp_tools, "edit_component", serde_json::json!({ "board": board.arg(), "reference": "R1", "value": "1k" })),
        (&comp_tools, "place_component", serde_json::json!({ "board": board.arg(), "footprint": "X:Y", "reference": "U9", "x": 1.0, "y": 1.0 })),
        (&comp_tools, "duplicate_component", serde_json::json!({ "board": board.arg(), "reference": "R1", "new_reference": "R8", "x": 1.0, "y": 1.0 })),
        (&comp_tools, "align_components", serde_json::json!({ "board": board.arg(), "references": ["R1"], "value": 5.0 })),
        (&comp_tools, "place_component_array", serde_json::json!({ "board": board.arg(), "footprint": "X:Y", "start_x": 0.0, "start_y": 0.0, "count_x": 2, "spacing_x": 1.0 })),
        (&routing_tools, "route_trace", serde_json::json!({ "board": board.arg(), "net_name": "GND", "layer": "F.Cu", "x1": 0.0, "y1": 0.0, "x2": 1.0, "y2": 0.0 })),
        (&routing_tools, "add_via", serde_json::json!({ "board": board.arg(), "net_name": "GND", "x": 1.0, "y": 1.0 })),
        (&routing_tools, "delete_trace", serde_json::json!({ "board": board.arg(), "uuid": SEG_UUID })),
        (&routing_tools, "modify_trace", serde_json::json!({ "board": board.arg(), "uuid": SEG_UUID, "net_name": "GND", "layer": "F.Cu", "x1": 0.0, "y1": 0.0, "x2": 1.0, "y2": 0.0 })),
        (&routing_tools, "route_pad_to_pad", serde_json::json!({ "board": board.arg(), "net_name": "GND", "ref1": "R1", "pad1": "1", "ref2": "R2", "pad2": "1" })),
        (&routing_tools, "route_differential_pair", serde_json::json!({ "board": board.arg(), "net_pos": "GND", "net_neg": "GND", "x1": 0.0, "y1": 0.0, "x2": 1.0, "y2": 0.0 })),
        (&board_tools, "set_board_size", serde_json::json!({ "board": board.arg(), "width": 10.0, "height": 10.0 })),
        (&board_tools, "add_board_outline", serde_json::json!({ "board": board.arg(), "x1": 0.0, "y1": 0.0, "x2": 5.0, "y2": 5.0 })),
        (&board_tools, "add_board_text", serde_json::json!({ "board": board.arg(), "text": "X", "x": 0.0, "y": 0.0 })),
        (&board_tools, "set_active_layer", serde_json::json!({ "board": board.arg(), "layer": "F.Cu" })),
    ];

    for (tools, name, args) in cases {
        let r = call(tools, name, args, &ctx).await;
        assert!(r.is_error, "{name} must refuse against a failing live session");
        let text = body_text(&r);
        assert!(
            text.contains("Refusing"),
            "{name} refusal must explain itself, got: {text}"
        );
    }

    assert_eq!(
        board.content(),
        before,
        "no write tool may touch the file while a live session is reachable"
    );
}

// ─── 3. save_board semantics ─────────────────────────────────────────────────

#[tokio::test]
async fn save_board_errors_without_live_session() {
    let board = TempBoard::new();
    let ctx = no_transport_ctx();
    let tools = pcb_board::tools();

    let r = call(&tools, "save_board", serde_json::json!({ "board": board.arg() }), &ctx).await;
    assert!(r.is_error);
    let text = body_text(&r);
    assert!(
        text.contains("live KiCAD IPC session"),
        "must explain that a live session is required: {text}"
    );
    assert!(
        text.contains("authoritative"),
        "must explain the file is already current: {text}"
    );
}

#[tokio::test]
async fn save_board_reports_ipc_failure_from_live_session() {
    let mock = spawn_failing_kicad();
    let ctx = ctx_with_addr(&mock.url);
    let tools = pcb_board::tools();

    let r = call(&tools, "save_board", serde_json::json!({}), &ctx).await;
    assert!(r.is_error);
    let text = body_text(&r);
    assert!(text.contains("reachable"), "unexpected error text: {text}");
}
