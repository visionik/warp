//! Tests for the ACP harness.
//!
//! Sync tests cover pure functions (path validation, harness setup).
//! Async tests use `tokio::io::duplex` to create an in-memory bidirectional
//! channel so [`AcpConnection`] methods can be exercised without spawning a
//! real subprocess.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{split, AsyncBufReadExt, AsyncWriteExt, BufReader};
use warp_cli::agent::Harness;

use super::super::super::AgentDriverError;
use super::super::{harness_kind, HarnessKind};
use super::{resolve_safe_path, AcpConnection, AcpHarness, ThirdPartyHarness};

// ── helpers ───────────────────────────────────────────────────────────────────

fn assert_harness_setup_failed(err: &AgentDriverError) -> (&str, &str) {
    match err {
        AgentDriverError::HarnessSetupFailed { harness, reason } => (harness, reason),
        other => panic!("expected HarnessSetupFailed, got: {other}"),
    }
}

fn make_test_conn(
    working_dir: PathBuf,
) -> (
    AcpConnection<
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    >,
    tokio::io::DuplexStream,
) {
    let (conn_side, agent_side) = tokio::io::duplex(65_536);
    let (conn_read, conn_write) = split(conn_side);
    let conn = AcpConnection::new(conn_write, BufReader::new(conn_read), working_dir);
    (conn, agent_side)
}

async fn read_response(agent: &mut tokio::io::DuplexStream) -> Value {
    let mut reader = BufReader::new(agent);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    serde_json::from_str(line.trim()).expect("valid JSON")
}

fn make_temp_dir() -> TempDir {
    tempfile::tempdir().unwrap()
}

// ── AcpHarness setup / validation ─────────────────────────────────────────────

#[test]
fn new_returns_error_when_command_is_none() {
    let err = AcpHarness::new(None).unwrap_err();
    let (harness, reason) = assert_harness_setup_failed(&err);
    assert_eq!(harness, "acp");
    assert!(reason.contains("requires a `command`"), "{reason}");
}

#[test]
fn new_returns_error_when_command_is_blank() {
    let err = AcpHarness::new(Some("   ".into())).unwrap_err();
    let (_, reason) = assert_harness_setup_failed(&err);
    assert!(reason.contains("requires a `command`"));
}

#[test]
fn new_accepts_a_non_empty_command() {
    let harness = AcpHarness::new(Some("acp-agent --foo".into())).unwrap();
    assert_eq!(harness.harness(), Harness::Acp);
}

#[test]
fn validate_fails_when_binary_is_not_on_path() {
    let harness = AcpHarness::new(Some("__nonexistent_acp_agent_xyz__".into())).unwrap();
    let err = harness.validate().unwrap_err();
    let (label, reason) = assert_harness_setup_failed(&err);
    assert_eq!(label, "acp");
    assert!(reason.contains("__nonexistent_acp_agent_xyz__"), "{reason}");
    assert!(reason.contains("not found"));
}

#[cfg(not(windows))]
#[test]
fn validate_succeeds_for_known_binary_with_args() {
    let harness = AcpHarness::new(Some("ls --color=never".into())).unwrap();
    assert!(harness.validate().is_ok());
}

#[test]
fn validate_rejects_unparseable_command() {
    let harness = AcpHarness::new(Some("acp-agent 'unterminated".into())).unwrap();
    let err = harness.validate().unwrap_err();
    let (_, reason) = assert_harness_setup_failed(&err);
    assert!(reason.contains("not a valid shell expression"), "{reason}");
}

#[test]
fn harness_kind_acp_requires_command() {
    let err = harness_kind(Harness::Acp, None).unwrap_err();
    let (harness, _) = assert_harness_setup_failed(&err);
    assert_eq!(harness, "acp");
}

#[test]
fn harness_kind_acp_with_command_returns_third_party() {
    let kind = harness_kind(Harness::Acp, Some("my-acp-agent".into())).unwrap();
    match kind {
        HarnessKind::ThirdParty(harness) => assert_eq!(harness.harness(), Harness::Acp),
        HarnessKind::Oz | HarnessKind::Unsupported(_) => panic!("expected ThirdParty"),
    }
}

#[test]
fn harness_kind_oz_ignores_acp_command() {
    let kind = harness_kind(Harness::Oz, Some("ignored".into())).unwrap();
    assert_eq!(kind.harness(), Harness::Oz);
}

// ── resolve_safe_path ──────────────────────────────────────────────────────────

fn wd() -> PathBuf {
    PathBuf::from("/home/user/project")
}

#[test]
fn safe_path_relative_inside_wd() {
    let p = resolve_safe_path("src/main.rs", &wd()).unwrap();
    assert_eq!(p, Path::new("/home/user/project/src/main.rs"));
}

#[test]
fn safe_path_dotslash_inside_wd() {
    let p = resolve_safe_path("./README.md", &wd()).unwrap();
    assert_eq!(p, Path::new("/home/user/project/README.md"));
}

#[test]
fn safe_path_absolute_inside_wd() {
    let p = resolve_safe_path("/home/user/project/file.txt", &wd()).unwrap();
    assert_eq!(p, Path::new("/home/user/project/file.txt"));
}

#[test]
fn safe_path_dotdot_escape_rejected() {
    let err = resolve_safe_path("../../etc/passwd", &wd()).unwrap_err();
    assert!(
        err.to_string().contains("escapes the working directory"),
        "{err}"
    );
}

#[test]
fn safe_path_absolute_escape_rejected() {
    let err = resolve_safe_path("/etc/passwd", &wd()).unwrap_err();
    assert!(
        err.to_string().contains("escapes the working directory"),
        "{err}"
    );
}

#[test]
fn safe_path_dotdot_then_back_inside_wd() {
    let p = resolve_safe_path("subdir/../file.txt", &wd()).unwrap();
    assert_eq!(p, Path::new("/home/user/project/file.txt"));
}

#[test]
fn safe_path_deeply_nested_is_ok() {
    let p = resolve_safe_path("a/b/c/d/e.txt", &wd()).unwrap();
    assert_eq!(p, Path::new("/home/user/project/a/b/c/d/e.txt"));
}

// ── AcpConnection: fs/readTextFile ────────────────────────────────────────────

#[tokio::test]
async fn fs_read_existing_file_returns_content() {
    let dir = make_temp_dir();
    tokio::fs::write(dir.path().join("hello.txt"), "hello world")
        .await
        .unwrap();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "fs/readTextFile",
        "params": { "path": "hello.txt" }
    }))
    .await
    .unwrap();

    let resp = read_response(&mut agent).await;
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["content"], "hello world");
}

#[tokio::test]
async fn fs_read_nonexistent_file_returns_error_response() {
    let dir = make_temp_dir();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "fs/readTextFile",
        "params": { "path": "missing.txt" }
    }))
    .await
    .unwrap();

    let resp = read_response(&mut agent).await;
    assert_eq!(resp["id"], 2);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Cannot read"),
        "{resp}"
    );
}

#[tokio::test]
async fn fs_read_path_escape_propagates_error() {
    let dir = make_temp_dir();
    let (mut conn, _agent) = make_test_conn(dir.path().to_path_buf());
    // resolve_safe_path fails → handle_client_request returns Err
    assert!(conn
        .handle_client_request(&serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "fs/readTextFile",
            "params": { "path": "../../etc/passwd" }
        }))
        .await
        .is_err());
}

// ── AcpConnection: fs/writeTextFile ───────────────────────────────────────────

#[tokio::test]
async fn fs_write_creates_new_file() {
    let dir = make_temp_dir();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 4, "method": "fs/writeTextFile",
        "params": { "path": "out.txt", "content": "written by test" }
    }))
    .await
    .unwrap();

    let resp = read_response(&mut agent).await;
    assert_eq!(resp["id"], 4);
    assert!(resp.get("error").is_none(), "{resp}");
    let content = tokio::fs::read_to_string(dir.path().join("out.txt"))
        .await
        .unwrap();
    assert_eq!(content, "written by test");
}

#[tokio::test]
async fn fs_write_auto_creates_parent_dirs() {
    let dir = make_temp_dir();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 5, "method": "fs/writeTextFile",
        "params": { "path": "a/b/c/deep.txt", "content": "deep" }
    }))
    .await
    .unwrap();

    let resp = read_response(&mut agent).await;
    assert_eq!(resp["id"], 5);
    assert!(resp.get("error").is_none(), "{resp}");
    assert!(dir.path().join("a/b/c/deep.txt").exists());
}

#[tokio::test]
async fn fs_write_path_escape_propagates_error() {
    let dir = make_temp_dir();
    let (mut conn, _agent) = make_test_conn(dir.path().to_path_buf());
    assert!(conn
        .handle_client_request(&serde_json::json!({
            "jsonrpc": "2.0", "id": 6, "method": "fs/writeTextFile",
            "params": { "path": "../../evil.txt", "content": "evil" }
        }))
        .await
        .is_err());
}

// ── AcpConnection: terminal/create ────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn terminal_create_spawns_process_and_returns_id() {
    let dir = make_temp_dir();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 7, "method": "terminal/create",
        "params": { "command": "true", "args": [] }
    }))
    .await
    .unwrap();

    let resp = read_response(&mut agent).await;
    assert_eq!(resp["id"], 7);
    let tid = resp["result"]["terminalId"].as_str().unwrap();
    assert!(!tid.is_empty());
    assert!(conn.terminals.contains_key(tid));
}

#[tokio::test]
async fn terminal_create_empty_command_returns_error() {
    let dir = make_temp_dir();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 8, "method": "terminal/create",
        "params": { "command": "" }
    }))
    .await
    .unwrap();

    let resp = read_response(&mut agent).await;
    assert_eq!(resp["id"], 8);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("'command' is required"));
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_create_nonexistent_binary_returns_error() {
    let dir = make_temp_dir();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 9, "method": "terminal/create",
        "params": { "command": "__nonexistent_xyz__" }
    }))
    .await
    .unwrap();

    let resp = read_response(&mut agent).await;
    assert_eq!(resp["id"], 9);
    assert!(resp["error"].is_object(), "{resp}");
}

// ── AcpConnection: terminal/getOutput, kill, release ─────────────────────────

#[tokio::test]
async fn terminal_get_output_unknown_id_returns_error() {
    let dir = make_temp_dir();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 10, "method": "terminal/getOutput",
        "params": { "terminalId": "ghost" }
    }))
    .await
    .unwrap();

    let resp = read_response(&mut agent).await;
    assert_eq!(resp["id"], 10);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown terminal"));
}

#[tokio::test]
async fn terminal_kill_unknown_id_is_noop_success() {
    let dir = make_temp_dir();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 11, "method": "terminal/kill",
        "params": { "terminalId": "ghost" }
    }))
    .await
    .unwrap();

    let resp = read_response(&mut agent).await;
    assert_eq!(resp["id"], 11);
    assert!(resp.get("error").is_none(), "{resp}");
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_release_removes_entry_from_map() {
    let dir = make_temp_dir();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    // create
    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 12, "method": "terminal/create",
        "params": { "command": "true" }
    }))
    .await
    .unwrap();
    let create_resp = read_response(&mut agent).await;
    let tid = create_resp["result"]["terminalId"]
        .as_str()
        .unwrap()
        .to_string();

    // release
    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 13, "method": "terminal/release",
        "params": { "terminalId": tid }
    }))
    .await
    .unwrap();
    let release_resp = read_response(&mut agent).await;
    assert_eq!(release_resp["id"], 13);
    assert!(release_resp.get("error").is_none(), "{release_resp}");
    assert!(!conn.terminals.contains_key(&tid));
}

// ── AcpConnection: request/permission ─────────────────────────────────────────

#[tokio::test]
async fn permission_auto_approves_first_option() {
    let dir = make_temp_dir();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 14, "method": "request/permission",
        "params": {
            "toolCall": { "name": "run_shell_command", "input": {} },
            "options": [
                { "id": "allow", "label": "Allow" },
                { "id": "deny",  "label": "Deny"  }
            ]
        }
    }))
    .await
    .unwrap();

    let resp = read_response(&mut agent).await;
    assert_eq!(resp["id"], 14);
    assert_eq!(resp["result"]["outcome"]["optionId"], "allow");
}

#[tokio::test]
async fn permission_fallback_when_no_options() {
    let dir = make_temp_dir();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 15, "method": "request/permission",
        "params": { "options": [] }
    }))
    .await
    .unwrap();

    let resp = read_response(&mut agent).await;
    assert_eq!(resp["id"], 15);
    assert_eq!(resp["result"]["outcome"]["optionId"], "allow");
}

// ── AcpConnection: unknown method / notification ──────────────────────────────

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let dir = make_temp_dir();
    let (mut conn, mut agent) = make_test_conn(dir.path().to_path_buf());

    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "id": 16, "method": "some/unknownMethod", "params": {}
    }))
    .await
    .unwrap();

    let resp = read_response(&mut agent).await;
    assert_eq!(resp["id"], 16);
    assert_eq!(resp["error"]["code"], -32601);
}

#[tokio::test]
async fn notification_produces_no_response() {
    let dir = make_temp_dir();
    let (mut conn, _agent) = make_test_conn(dir.path().to_path_buf());
    // No id → notification; must complete without writing anything.
    conn.handle_client_request(&serde_json::json!({
        "jsonrpc": "2.0", "method": "session/update", "params": {}
    }))
    .await
    .unwrap();
}

// ── AcpConnection::call() with interleaved agent requests ─────────────────────

#[tokio::test]
async fn call_dispatches_interleaved_client_request_before_response() {
    let dir = make_temp_dir();
    tokio::fs::write(dir.path().join("data.txt"), "interleaved")
        .await
        .unwrap();
    let (mut conn, agent) = make_test_conn(dir.path().to_path_buf());

    let agent_task = tokio::spawn(async move {
        let (mut ar, mut aw) = tokio::io::split(agent);
        let mut reader = BufReader::new(&mut ar);

        // Read the outgoing request.
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let req: Value = serde_json::from_str(line.trim()).unwrap();
        let orig_id = req["id"].as_u64().unwrap();

        // Send an interleaved fs/readTextFile request.
        let fs_req = serde_json::json!({
            "jsonrpc": "2.0", "id": 99,
            "method": "fs/readTextFile", "params": { "path": "data.txt" }
        });
        let mut l = serde_json::to_string(&fs_req).unwrap();
        l.push('\n');
        aw.write_all(l.as_bytes()).await.unwrap();
        aw.flush().await.unwrap();

        // Read and verify the fs response.
        let mut fs_line = String::new();
        reader.read_line(&mut fs_line).await.unwrap();
        let fs_resp: Value = serde_json::from_str(fs_line.trim()).unwrap();
        assert_eq!(
            fs_resp["result"]["content"], "interleaved",
            "unexpected: {fs_resp}"
        );

        // Send the final response.
        let final_resp = serde_json::json!({
            "jsonrpc": "2.0", "id": orig_id,
            "result": { "sessionId": "test-session-ok" }
        });
        let mut fl = serde_json::to_string(&final_resp).unwrap();
        fl.push('\n');
        aw.write_all(fl.as_bytes()).await.unwrap();
        aw.flush().await.unwrap();
    });

    #[derive(serde::Deserialize)]
    struct SessionResp {
        #[serde(rename = "sessionId")]
        session_id: String,
    }
    let result: SessionResp = conn
        .call("session/new", &serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(result.session_id, "test-session-ok");
    agent_task.await.unwrap();
}

#[tokio::test]
async fn call_propagates_jsonrpc_error_response() {
    let dir = make_temp_dir();
    let (mut conn, agent) = make_test_conn(dir.path().to_path_buf());

    let agent_task = tokio::spawn(async move {
        let (mut ar, mut aw) = tokio::io::split(agent);
        let mut reader = BufReader::new(&mut ar);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let req: Value = serde_json::from_str(line.trim()).unwrap();

        let error_resp = serde_json::json!({
            "jsonrpc": "2.0", "id": req["id"],
            "error": { "code": -32600, "message": "server exploded" }
        });
        let mut text = serde_json::to_string(&error_resp).unwrap();
        text.push('\n');
        aw.write_all(text.as_bytes()).await.unwrap();
        aw.flush().await.unwrap();
    });

    let err = conn
        .call::<_, Value>("initialize", &serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("server exploded"), "{err}");
    agent_task.await.unwrap();
}
