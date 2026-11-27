//! Subprocess smoke test verifying that malformed JSON-RPC lines, unknown
//! methods, and invalid `tools/call` arguments never abort or kill the
//! `flashbrowse mcp` process (including under `--release`).

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn malformed_input_and_invalid_params_do_not_kill_mcp_binary() {
    let bin = env!("CARGO_BIN_EXE_flashbrowse");
    let mut child = Command::new(bin)
        .args(["mcp", "--log-format", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn flashbrowse mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    let mut send_and_read = |line: &str| -> serde_json::Value {
        writeln!(stdin, "{line}").expect("write line");
        stdin.flush().expect("flush stdin");
        let mut out = String::new();
        let n = stdout.read_line(&mut out).expect("read response line");
        assert!(n > 0, "MCP process exited unexpectedly on input: {line}");

        serde_json::from_str(&out).expect("valid JSON-RPC response")
    };

    let init = send_and_read(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
    );
    assert!(init.get("result").is_some(), "{init}");

    // 1. Broken JSON -> -32700 parse error, server stays alive.
    let parse_err = send_and_read("{not valid json");
    assert_eq!(parse_err["error"]["code"], -32700);

    // 2. An oversized frame is rejected without losing the next frame.
    let oversized = "x".repeat(1024 * 1024 + 1);
    let too_large = send_and_read(&oversized);
    assert_eq!(too_large["error"]["code"], -32700);
    assert_eq!(too_large["error"]["message"], "Parse error: line too large");

    // 3. Non-object JSON -> -32700 parse error, server stays alive.
    let bad_req = send_and_read("[1, 2, 3]");
    assert_eq!(bad_req["error"]["code"], -32700);

    let deep = format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"ping","params":{}0{}}}"#,
        "[".repeat(256),
        "]".repeat(256)
    );
    assert_eq!(send_and_read(&deep)["error"]["code"], -32700);

    // 4. Unknown method -> -32601 method not found.
    let unknown_method = send_and_read(r#"{"jsonrpc":"2.0","id":2,"method":"nope/missing"}"#);
    assert_eq!(unknown_method["error"]["code"], -32601);

    // 5. Malformed tools/call payloads -> -32602 invalid params.
    for (id, payload) in [
        (
            3,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":"wrong-type"}"#,
        ),
        (
            4,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":123}}"#,
        ),
        (
            5,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":"file:///etc/passwd"}}}"#,
        ),
        (
            6,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_pdf","arguments":{"name":"CON"}}}"#,
        ),
        (
            7,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"browser_storage_state","arguments":{"action":"export","name":"CON"}}}"#,
        ),
        (
            10,
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"browser_extract_markdown","arguments":[]}}"#,
        ),
    ] {
        let resp = send_and_read(payload);
        assert_eq!(resp["id"], id);
        assert_eq!(resp["error"]["code"], -32602, "{resp}");
    }

    // 6. Server still serves tools/list with all 18 tools afterwards.
    let list = send_and_read(r#"{"jsonrpc":"2.0","id":8,"method":"tools/list"}"#);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 18);

    drop(stdin);
    let status = child.wait().expect("wait for mcp exit");
    assert!(status.success(), "mcp exited non-zero: {status}");
}

#[test]
fn closed_stdout_is_not_a_panic() {
    let (reader, writer) = std::io::pipe().expect("create pipe");
    drop(reader);
    let output = Command::new(env!("CARGO_BIN_EXE_flashbrowse"))
        .args(["stats", "--limit", "0", "--json"])
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .output()
        .expect("run stats");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[cfg(target_os = "linux")]
#[test]
fn failing_stderr_does_not_stop_mcp() {
    let stderr = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_flashbrowse"))
        .args(["--verbose", "--chrome-path", "/no/such/chromium", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    write!(stdin, "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}}\n{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{{\"name\":\"browser_open\",\"arguments\":{{\"url\":\"https://example.com\"}}}}}}\n{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}}\n").unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[2]["id"], 3);
}
