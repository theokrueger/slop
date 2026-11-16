//! End-to-end test of `mmux mcp`: a real process speaking newline-delimited
//! JSON-RPC over stdio, exactly as pi does.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn write_config(dir: &Path, agent_writes: &str) -> std::path::PathBuf {
    let path = dir.join("mmux.toml");
    std::fs::write(
        &path,
        format!(
            "[memory]\nenabled = true\npi_agent_dir = \"/nonexistent/pi\"\nagent_writes = \"{agent_writes}\"\n"
        ),
    )
    .unwrap();
    path
}

/// Runs one MCP session: writes every request, closes stdin, and returns the
/// parsed response lines.
fn session(workspace: &Path, state: &Path, requests: &[Value]) -> Vec<Value> {
    let config_path = write_config(workspace, "explicit");
    run_session(workspace, state, &config_path, requests)
}

fn run_session(
    workspace: &Path,
    state: &Path,
    config_path: &Path,
    requests: &[Value],
) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mmux"))
        .arg("--config")
        .arg(config_path)
        .arg("mcp")
        .current_dir(workspace)
        .env("XDG_STATE_HOME", state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mmux mcp");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for request in requests {
            writeln!(stdin, "{request}").expect("write request");
        }
    }
    child.stdin.take();

    let output = child.wait_with_output().expect("wait for mmux mcp");
    assert!(
        output.status.success(),
        "mmux mcp failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 protocol");
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("non-JSON on stdout: {err}: {line:?}"))
        })
        .collect()
}

fn call(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    })
}

#[test]
fn handshake_and_tool_list() {
    let dir = tempfile::TempDir::new().unwrap();
    let workspace = dir.path().join("workspace");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&workspace).unwrap();

    let responses = session(
        &workspace,
        &state,
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-06-18" } }),
            // A notification must not produce a response line.
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        ],
    );

    assert_eq!(responses.len(), 2, "{responses:?}");
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "mmux");
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["memory_search", "memory_read", "memory_write"]);
}

#[test]
fn writes_reads_and_searches_across_processes() {
    let dir = tempfile::TempDir::new().unwrap();
    let workspace = dir.path().join("workspace");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&workspace).unwrap();

    let responses = session(
        &workspace,
        &state,
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-06-18" } }),
            call(
                2,
                "memory_write",
                json!({ "title": "Restart policy", "kind": "decision",
                        "body": "panes relaunch with the same session id",
                        "tags": ["session"] }),
            ),
            call(3, "memory_read", json!({ "slug": "restart-policy" })),
            call(4, "memory_search", json!({ "query": "session" })),
        ],
    );

    let written = &responses[1]["result"];
    assert_eq!(written["isError"], false, "{written}");
    assert_eq!(written["structuredContent"]["slug"], "restart-policy");
    let hash = written["structuredContent"]["hash"].as_str().unwrap();
    assert!(!hash.is_empty());

    let read = &responses[2]["result"];
    assert_eq!(read["isError"], false);
    let text = read["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("+++"), "{text}");
    assert!(text.contains("panes relaunch"), "{text}");

    let found = &responses[3]["result"];
    assert_eq!(found["isError"], false);
    let hits = found["structuredContent"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["slug"], "restart-policy");

    // The document is plaintext on disk, under the state directory.
    let docs: Vec<_> = walk(&state);
    assert!(
        docs.iter()
            .any(|path| path.ends_with("docs/restart-policy.md")),
        "{docs:?}"
    );
}

#[test]
fn deny_blocks_writes_but_not_reads() {
    let dir = tempfile::TempDir::new().unwrap();
    let workspace = dir.path().join("workspace");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    let config_path = write_config(&workspace, "deny");

    let responses = run_session(
        &workspace,
        &state,
        &config_path,
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-06-18" } }),
            call(2, "memory_write", json!({ "title": "nope", "body": "x" })),
            call(3, "memory_search", json!({ "query": "" })),
        ],
    );

    assert_eq!(responses[1]["result"]["isError"], true);
    assert!(
        responses[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("disabled")
    );
    assert_eq!(responses[2]["result"]["isError"], false);
}

#[test]
fn unknown_methods_and_garbage_arguments_are_contained() {
    let dir = tempfile::TempDir::new().unwrap();
    let workspace = dir.path().join("workspace");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&workspace).unwrap();

    let responses = session(
        &workspace,
        &state,
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-06-18" } }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" }),
            call(3, "memory_read", json!({ "slug": "../../etc/passwd" })),
            call(4, "memory_search", json!({ "kind": "poem" })),
        ],
    );

    assert_eq!(responses[1]["error"]["code"], -32601);
    assert_eq!(responses[2]["result"]["isError"], true);
    assert_eq!(responses[3]["result"]["isError"], true);
}

#[test]
fn archived_documents_are_invisible_to_the_model() {
    let dir = tempfile::TempDir::new().unwrap();
    let workspace = dir.path().join("workspace");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&workspace).unwrap();

    // Write through MCP so the vault layout comes from the product itself.
    let responses = session(
        &workspace,
        &state,
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-06-18" } }),
            call(
                2,
                "memory_write",
                json!({ "title": "Secret note", "body": "hidden from models" }),
            ),
        ],
    );
    assert_eq!(responses[1]["result"]["isError"], false);

    // Move it into the archive, exactly as a UI delete does.
    let path = walk(&state)
        .into_iter()
        .find(|path| path.ends_with("docs/secret-note.md"))
        .expect("document on disk");
    let docs = Path::new(&path).parent().expect("docs dir").to_path_buf();
    let archive = docs.parent().expect("workspace dir").join("_archive");
    std::fs::create_dir_all(&archive).unwrap();
    std::fs::rename(&path, archive.join("1700000000000000000.md")).unwrap();

    let responses = session(
        &workspace,
        &state,
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-06-18" } }),
            call(2, "memory_read", json!({ "slug": "secret-note" })),
            call(3, "memory_search", json!({ "query": "hidden" })),
            call(
                4,
                "memory_write",
                json!({ "title": "Fresh note", "body": "live" }),
            ),
        ],
    );

    assert_eq!(
        responses[1]["result"]["isError"], true,
        "archived docs must not be readable through MCP"
    );
    assert_eq!(
        responses[2]["result"]["structuredContent"]["hits"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "archived docs must not appear in search"
    );
    // The archived slug is not reserved: a fresh document can use the name.
    assert_eq!(
        responses[3]["result"]["structuredContent"]["slug"],
        "fresh-note"
    );
}

fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path.to_string_lossy().into_owned());
            }
        }
    }
    out
}
