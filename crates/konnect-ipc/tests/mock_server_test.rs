//! IPC client tests against a mock KiCAD NNG server — no KiCAD required.
//!
//! A rep0 socket on tcp://127.0.0.1:<port> plays KiCAD: it decodes the
//! ApiRequest envelope and returns canned ApiResponse messages. This lets CI
//! exercise the full encode → transport → decode → error-mapping path that
//! previously only ran against a live KiCAD session.

use konnect_ipc::gen::kiapi;
use konnect_ipc::KiCadIpcClient;
use nng::options::Options;
use prost::Message;
use std::time::Duration;

/// A rep0 server answering each request via `respond`.
/// Returns the tcp:// URL to dial. The server thread exits when the socket
/// errors (i.e. when `_socket_keepalive` is dropped by the returned guard).
struct MockKicad {
    url: String,
    _thread: std::thread::JoinHandle<()>,
}

fn spawn_mock<F>(respond: F) -> MockKicad
where
    F: Fn(kiapi::common::ApiRequest) -> Option<kiapi::common::ApiResponse> + Send + 'static,
{
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
        // Signal the test thread that the listener is accepting connections;
        // without this, a client dial can race the listen and get refused.
        let _ = ready_tx.send(());
        while let Ok(msg) = socket.recv() {
            let request = match kiapi::common::ApiRequest::decode(msg.as_slice()) {
                Ok(r) => r,
                Err(_) => break,
            };
            match respond(request) {
                Some(resp) => {
                    let out = nng::Message::from(resp.encode_to_vec().as_slice());
                    if socket.send(out).is_err() {
                        break;
                    }
                }
                None => {
                    // Simulate a wedged KiCAD: never reply. The rep socket
                    // can't take another request until it replies, so just
                    // park until the test ends.
                    std::thread::sleep(Duration::from_secs(20));
                    break;
                }
            }
        }
    });

    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("mock server failed to start listening");

    MockKicad {
        url,
        _thread: thread,
    }
}

/// True when a real KiCAD session's API socket exists at the platform default
/// location. Tests that exercise the *empty socket path* code path must skip
/// in that case: the client's default-path probe would otherwise dial a live
/// KiCAD session, which mock-only CI/test runs must never do.
fn real_kicad_socket_present() -> bool {
    std::env::var("KICAD_API_SOCKET").is_ok()
        || (cfg!(unix) && std::path::Path::new("/tmp/kicad/api.sock").exists())
}

fn ok_response() -> kiapi::common::ApiResponse {
    kiapi::common::ApiResponse {
        status: Some(kiapi::common::ApiResponseStatus {
            status: kiapi::common::ApiStatusCode::AsOk as i32,
            error_message: String::new(),
        }),
        header: None,
        message: None,
    }
}

#[test]
fn ping_roundtrips_through_mock() {
    let mock = spawn_mock(|req| {
        // The envelope must carry a client name and a packed command.
        assert!(req.header.is_some());
        let header = req.header.unwrap();
        assert!(header.client_name.starts_with("konnect-"));
        let msg = req.message.expect("request must pack a command");
        assert!(
            msg.type_url.ends_with("kiapi.common.commands.Ping"),
            "unexpected type_url: {}",
            msg.type_url
        );
        Some(ok_response())
    });

    let client = KiCadIpcClient::new(&mock.url);
    assert!(client.ping().unwrap());
}

#[test]
fn kicad_error_status_maps_to_err() {
    let mock = spawn_mock(|_req| {
        Some(kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: kiapi::common::ApiStatusCode::AsBadRequest as i32,
                error_message: "no board open".to_string(),
            }),
            header: None,
            message: None,
        })
    });

    let client = KiCadIpcClient::new(&mock.url);
    // ping() swallows errors into Ok(false) by design — that's the
    // "KiCAD unreachable" UX. It must not be Ok(true) and must not hang.
    assert!(!client.ping().unwrap());

    // A typed call surfaces the error text.
    let err = client.get_open_documents().unwrap_err().to_string();
    assert!(err.contains("no board open"), "unexpected error: {err}");
}

#[test]
fn unreachable_endpoint_errors_fast() {
    // Nothing listens here; dial must fail with an error, not hang.
    let client = KiCadIpcClient::new("tcp://127.0.0.1:1");
    let start = std::time::Instant::now();
    let result = client.get_open_documents();
    assert!(result.is_err());
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "dial to dead endpoint took {:?}",
        start.elapsed()
    );
}

#[test]
fn empty_socket_path_is_configuration_error() {
    // Skip when the env var or a live default socket would make the empty
    // path resolve to a real KiCAD session.
    if real_kicad_socket_present() {
        eprintln!("SKIP: a real KiCAD API socket is present in this environment");
        return;
    }
    let client = KiCadIpcClient::new("");
    let err = client.get_open_documents().unwrap_err().to_string();
    assert!(
        err.contains("socket path not configured"),
        "unexpected error: {err}"
    );
    assert!(
        err.contains("TROUBLESHOOTING"),
        "error should link the troubleshooting guide: {err}"
    );
}

// ─── Transport-unavailable classification (file-fallback safety) ────────────
//
// Tools decide whether a file-based fallback is SAFE based on this
// classification: "no transport" may fall back, "live session answered but
// errored" must not (editing the file behind a live GUI clobbers boards).

#[test]
fn dead_endpoint_and_unconfigured_socket_classify_as_unavailable() {
    let client = KiCadIpcClient::new("tcp://127.0.0.1:1");
    let err = client.get_open_documents().unwrap_err();
    assert!(
        konnect_ipc::client::is_unavailable(&err),
        "dial failure must classify as transport-unavailable: {err}"
    );

    if !real_kicad_socket_present() {
        let client = KiCadIpcClient::new("");
        let err = client.get_open_documents().unwrap_err();
        assert!(
            konnect_ipc::client::is_unavailable(&err),
            "unconfigured socket must classify as transport-unavailable: {err}"
        );
    }
}

#[test]
fn application_error_from_live_session_is_not_unavailable() {
    let mock = spawn_mock(|_req| {
        Some(kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: kiapi::common::ApiStatusCode::AsBadRequest as i32,
                error_message: "no board open".to_string(),
            }),
            header: None,
            message: None,
        })
    });

    let client = KiCadIpcClient::new(&mock.url);
    let err = client.get_open_documents().unwrap_err();
    assert!(
        !konnect_ipc::client::is_unavailable(&err),
        "a live session's application error must NOT classify as unavailable: {err}"
    );
}

// ─── New query/save wrappers (IPC-first migration support) ──────────────────

fn packed_ok<M: Message>(msg: &M, type_name: &str) -> kiapi::common::ApiResponse {
    kiapi::common::ApiResponse {
        status: Some(kiapi::common::ApiResponseStatus {
            status: kiapi::common::ApiStatusCode::AsOk as i32,
            error_message: String::new(),
        }),
        header: None,
        message: Some(konnect_ipc::builders::pack_any(msg, type_name)),
    }
}

fn board_doc() -> kiapi::common::types::DocumentSpecifier {
    kiapi::common::types::DocumentSpecifier {
        r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
        identifier: Some(
            kiapi::common::types::document_specifier::Identifier::BoardFilename(
                "board.kicad_pcb".to_string(),
            ),
        ),
        project: None,
    }
}

fn open_docs_response() -> kiapi::common::ApiResponse {
    packed_ok(
        &kiapi::common::commands::GetOpenDocumentsResponse {
            documents: vec![board_doc()],
        },
        "kiapi.common.commands.GetOpenDocumentsResponse",
    )
}

/// Dispatch on the request's packed command type_url.
fn dispatch_mock(
    handlers: Vec<(
        &'static str,
        Box<dyn Fn(&prost_types::Any) -> kiapi::common::ApiResponse + Send + Sync>,
    )>,
) -> MockKicad {
    spawn_mock(move |req| {
        let msg = req.message.expect("request must pack a command");
        for (suffix, handler) in &handlers {
            if msg.type_url.ends_with(suffix) {
                return Some(handler(&msg));
            }
        }
        panic!("mock got unexpected command: {}", msg.type_url);
    })
}

#[test]
fn save_board_sends_save_document_for_open_board() {
    let mock = dispatch_mock(vec![
        (
            "GetOpenDocuments",
            Box::new(|_| open_docs_response()),
        ),
        (
            "SaveDocument",
            Box::new(|any| {
                let cmd = kiapi::common::commands::SaveDocument::decode(any.value.as_slice())
                    .expect("decode SaveDocument");
                assert!(cmd.document.is_some(), "SaveDocument must target a document");
                ok_response()
            }),
        ),
    ]);

    let client = KiCadIpcClient::new(&mock.url);
    client.save_board().expect("save_board should succeed");
}

#[test]
fn save_copy_of_board_targets_requested_path_with_overwrite() {
    let mock = dispatch_mock(vec![
        (
            "GetOpenDocuments",
            Box::new(|_| open_docs_response()),
        ),
        (
            "SaveCopyOfDocument",
            Box::new(|any| {
                let cmd =
                    kiapi::common::commands::SaveCopyOfDocument::decode(any.value.as_slice())
                        .expect("decode SaveCopyOfDocument");
                assert_eq!(cmd.path, "/tmp/copy.kicad_pcb");
                assert!(cmd.options.expect("options").overwrite);
                ok_response()
            }),
        ),
    ]);

    let client = KiCadIpcClient::new(&mock.url);
    client
        .save_copy_of_board("/tmp/copy.kicad_pcb")
        .expect("save_copy_of_board should succeed");
}

#[test]
fn get_title_block_info_roundtrips() {
    let mock = dispatch_mock(vec![
        (
            "GetOpenDocuments",
            Box::new(|_| open_docs_response()),
        ),
        (
            "GetTitleBlockInfo",
            Box::new(|_| {
                packed_ok(
                    &kiapi::common::types::TitleBlockInfo {
                        title: "Test Board".into(),
                        date: "2026-07-12".into(),
                        revision: "A".into(),
                        company: "ACME".into(),
                        ..Default::default()
                    },
                    "kiapi.common.types.TitleBlockInfo",
                )
            }),
        ),
    ]);

    let client = KiCadIpcClient::new(&mock.url);
    let tb = client.get_title_block_info().expect("title block");
    assert_eq!(tb.title, "Test Board");
    assert_eq!(tb.revision, "A");
    assert_eq!(tb.company, "ACME");
}

#[test]
fn get_footprint_pads_decodes_definition_pads() {
    use kiapi::board::types as bt;
    use kiapi::common::types as ct;

    let text_field = |name: &str, value: &str| bt::Field {
        id: None,
        name: name.to_string(),
        text: Some(bt::BoardText {
            id: None,
            text: Some(ct::Text {
                position: None,
                attributes: None,
                text: value.to_string(),
                hyperlink: String::new(),
            }),
            layer: bt::BoardLayer::BlFSilkS as i32,
            knockout: false,
            locked: ct::LockedState::LsUnlocked as i32,
        }),
        visible: true,
    };

    let pad = bt::Pad {
        number: "1".to_string(),
        net: Some(bt::Net {
            code: None,
            name: "GND".to_string(),
        }),
        position: Some(ct::Vector2 {
            x_nm: -500_000,
            y_nm: 0,
        }),
        ..Default::default()
    };

    let fp = bt::FootprintInstance {
        position: Some(ct::Vector2 {
            x_nm: 10_000_000,
            y_nm: 20_000_000,
        }),
        orientation: Some(ct::Angle { value_degrees: 90.0 }),
        layer: bt::BoardLayer::BlFCu as i32,
        reference_field: Some(text_field("Reference", "R1")),
        value_field: Some(text_field("Value", "10k")),
        definition: Some(bt::Footprint {
            items: vec![konnect_ipc::builders::pack_any(
                &pad,
                "kiapi.board.types.Pad",
            )],
            ..Default::default()
        }),
        ..Default::default()
    };

    let items_response = packed_ok(
        &kiapi::common::commands::GetItemsResponse {
            items: vec![konnect_ipc::builders::pack_any(
                &fp,
                "kiapi.board.types.FootprintInstance",
            )],
            ..Default::default()
        },
        "kiapi.common.commands.GetItemsResponse",
    );

    let mock = dispatch_mock(vec![
        (
            "GetOpenDocuments",
            Box::new(|_| open_docs_response()),
        ),
        (
            "GetItems",
            Box::new(move |_| items_response.clone()),
        ),
    ]);

    let client = KiCadIpcClient::new(&mock.url);
    let (summary, pads) = client
        .get_footprint_pads("R1")
        .expect("query should succeed")
        .expect("R1 should be found");
    assert_eq!(summary.reference, "R1");
    assert_eq!(summary.position.x, 10.0);
    assert_eq!(summary.rotation, 90.0);
    assert_eq!(pads.len(), 1);
    assert_eq!(pads[0].number, "1");
    assert_eq!(pads[0].net, "GND");
    assert_eq!(pads[0].position.x, -0.5);

    // Unknown reference: Ok(None), not an error.
    assert!(client.get_footprint_pads("R99").expect("ok").is_none());
}

/// The regression the recv timeout exists for: a server that accepts the
/// request and never replies. The predecessor project hung >600 s here; the
/// client must give up at its recv timeout instead.
///
/// Ignored by default: it necessarily takes the full 30 s recv timeout.
/// Run explicitly with: cargo test -p konnect-ipc -- --ignored
#[test]
#[ignore = "takes ~30s (full recv timeout) by design"]
fn wedged_server_times_out_instead_of_hanging() {
    let mock = spawn_mock(|_req| None); // accept, never respond

    let client = KiCadIpcClient::new(&mock.url);
    let start = std::time::Instant::now();
    let result = client.get_open_documents();
    assert!(result.is_err(), "expected timeout error");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_secs(25) && elapsed < Duration::from_secs(60),
        "expected ~30s recv timeout, got {elapsed:?}"
    );
}
