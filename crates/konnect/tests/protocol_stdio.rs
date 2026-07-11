//! MCP protocol tests over stdio — spawn the real binary and speak JSON-RPC.
//!
//! Codifies the smoke tests that were run by hand at release time: handshake,
//! toolset loading for the entire registry, a real file-based tool call, and
//! the structured-error taxonomy the LLM relies on for recovery.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpProcess {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_konnect"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn konnect binary");
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        let mut p = McpProcess {
            child,
            stdin,
            reader,
            next_id: 1,
        };
        // MCP handshake
        let init = p.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "protocol-test", "version": "0"}
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "konnect");
        p.notify("notifications/initialized");
        p
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", msg).unwrap();
        self.stdin.flush().unwrap();
        // Read lines until the response with our id arrives (skips any
        // notifications the server might emit).
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).unwrap();
            assert!(
                n > 0,
                "server closed stdout waiting for response to {method}"
            );
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                return v;
            }
        }
    }

    fn notify(&mut self, method: &str) {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{}", msg).unwrap();
        self.stdin.flush().unwrap();
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Value {
        let resp = self.request("tools/call", json!({"name": name, "arguments": args}));
        resp["result"].clone()
    }

    /// Parse the JSON body of a tool result's first text content.
    fn tool_body(result: &Value) -> Value {
        let text = result["content"][0]["text"].as_str().unwrap_or("{}");
        serde_json::from_str(text).unwrap_or(Value::Null)
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn handshake_baseline_and_full_registry_loads() {
    let mut p = McpProcess::spawn();

    // Baseline tools/list: starter kit + meta-tools only (small context).
    let list = p.request("tools/list", json!({}));
    let baseline = list["result"]["tools"].as_array().unwrap().len();
    assert!(
        (10..30).contains(&baseline),
        "baseline tools/list should be the small starter kit, got {baseline}"
    );

    // list_toolboxes reports the registry; every toolset must load.
    let boxes = McpProcess::tool_body(&p.call_tool("list_toolboxes", json!({})));
    let toolsets: Vec<String> = boxes["toolsets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        toolsets.len() >= 17,
        "expected 17+ toolsets, got {}",
        toolsets.len()
    );
    // No license-era fields may reappear.
    assert!(boxes.get("license_tier").is_none());
    assert!(boxes["toolsets"][0].get("tier").is_none());

    let mut total = 0u64;
    for name in &toolsets {
        let loaded = McpProcess::tool_body(&p.call_tool("load_toolset", json!({"name": name})));
        let added = loaded["tools_added"].as_u64().unwrap_or(0);
        assert!(added > 0, "toolset '{name}' loaded no tools");
        total += added;
    }
    assert_eq!(
        total,
        boxes["total_tools"].as_u64().unwrap(),
        "sum of loaded tools disagrees with list_toolboxes total"
    );
}

#[test]
fn file_based_tool_roundtrip_in_temp_project() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("proto_demo");
    let mut p = McpProcess::spawn();

    let created = p.call_tool(
        "create_project",
        json!({"name": "proto_demo", "path": proj.to_string_lossy()}),
    );
    assert_ne!(
        created["isError"],
        json!(true),
        "create_project failed: {created}"
    );
    assert!(proj.join("proto_demo.kicad_sch").exists());

    let info = p.call_tool(
        "get_project_info",
        json!({"path": proj.join("proto_demo.kicad_pro").to_string_lossy()}),
    );
    assert_ne!(
        info["isError"],
        json!(true),
        "get_project_info failed: {info}"
    );
}

#[test]
fn structured_errors_guide_recovery() {
    let mut p = McpProcess::spawn();

    // Known tool in an unloaded toolset → toolset_not_loaded naming the owner.
    let r = p.call_tool("route_trace", json!({}));
    assert_eq!(r["isError"], json!(true));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "toolset_not_loaded");
    assert_eq!(body["error"]["toolset"], "pcb_routing");

    // Unknown tool → unknown_tool.
    let r = p.call_tool("frobnicate_board", json!({}));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "unknown_tool");

    // Missing required argument → invalid_argument naming the field.
    let r = p.call_tool("create_project", json!({"path": "/tmp/x"}));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "invalid_argument");
    assert_eq!(body["error"]["field"], "name");
}

#[test]
fn unknown_method_is_json_rpc_error_not_crash() {
    let mut p = McpProcess::spawn();
    let resp = p.request("tools/definitely_not_a_method", json!({}));
    assert!(
        resp.get("error").is_some(),
        "expected JSON-RPC error: {resp}"
    );
    // Server must still be alive afterwards.
    let ping = p.request("ping", json!({}));
    assert!(ping.get("result").is_some());
}
