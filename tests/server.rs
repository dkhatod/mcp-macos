//! Contract test: the server speaks real MCP over a byte transport.
//!
//! Drives a full initialize → initialized → tools/list round trip against
//! `MacosServer` over an in-memory duplex stream (the same ndjson codec the
//! stdio transport uses; the real stdio binary is smoke-tested separately).

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}"#;
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
const LIST_TOOLS: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;

/// Reads one ndjson line with a deadline so a silent server fails loudly.
async fn read_line(
    reader: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
) -> serde_json::Value {
    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for a response")
        .expect("read failed");
    assert!(n > 0, "transport closed before a response arrived");
    serde_json::from_str(line.trim()).expect("response was not valid JSON")
}

#[tokio::test]
async fn server_initializes_and_lists_tools() {
    let dir = tempfile::tempdir().unwrap();
    let server = mcp_macos::MacosServer::new(dir.path().to_path_buf());

    let (client, server_io) = tokio::io::duplex(64 * 1024);
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = BufReader::new(reader);
    let handle = tokio::spawn(async move {
        // serve_server returns once initialization completes; the returned
        // RunningService IS the live session — dropping it would shut the
        // server down mid-test. waiting() resolves when the client drops.
        if let Ok(running) = rmcp::serve_server(server, server_io).await {
            let _ = running.waiting().await;
        }
    });
    // Let the server task start polling before we write (duplex race).
    tokio::time::sleep(Duration::from_millis(50)).await;

    writer.write_all(INITIALIZE.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    let v = read_line(&mut reader).await;
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["protocolVersion"], "2026-07-28");
    assert!(v["result"]["serverInfo"]["name"].is_string());

    writer.write_all(INITIALIZED.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.write_all(LIST_TOOLS.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    let v2 = read_line(&mut reader).await;
    assert_eq!(v2["id"], 2);
    assert!(
        v2["result"]["tools"].is_array(),
        "expected tools array in: {v2}"
    );

    // In-memory transport never signals EOF cleanly; kill the session task.
    drop(writer);
    handle.abort();
}

/// Doc-clarity surface (mcp-macos docs clarity layer): the `initialize`
/// response advertises non-empty server `instructions` and every discovered
/// tool ships a non-empty `description` that documents its gate/token flow.
#[tokio::test]
async fn server_instructions_and_tool_descriptions() {
    let dir = tempfile::tempdir().unwrap();
    let server = mcp_macos::MacosServer::new(dir.path().to_path_buf());

    let (client, server_io) = tokio::io::duplex(64 * 1024);
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = BufReader::new(reader);

    let handle = tokio::spawn(async move {
        // serve_server returns once initialization completes; the returned
        // RunningService IS the live session — keep it alive until teardown.
        if let Ok(running) = rmcp::serve_server(server, server_io).await {
            let _ = running.waiting().await;
        }
    });
    // Let the server task start polling before we write (duplex race).
    tokio::time::sleep(Duration::from_millis(50)).await;

    // initialize → captures the server-level instructions.
    writer.write_all(INITIALIZE.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();
    let init = read_line(&mut reader).await;
    assert_eq!(init["id"], 1);

    let instructions = init["result"]["instructions"]
        .as_str()
        .expect("initialize result must include an `instructions` string");
    assert!(!instructions.is_empty(), "instructions must not be empty");
    assert!(
        instructions.contains("soft-gated"),
        "instructions must document the soft-gate model: {instructions}"
    );
    assert!(
        instructions.contains("permissions_check"),
        "instructions must point clients to permissions_check: {instructions}"
    );
    assert!(
        instructions.contains("folders"),
        "instructions must document folder selection: {instructions}"
    );

    // tools/list → captures every advertised tool description.
    writer.write_all(INITIALIZED.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.write_all(LIST_TOOLS.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    let listed = read_line(&mut reader).await;
    assert_eq!(listed["id"], 2);
    let tools = listed["result"]["tools"]
        .as_array()
        .expect("tools/list must return a tools array");
    assert!(!tools.is_empty(), "expected at least one advertised tool");

    // 1. Every tool carries a non-empty description.
    for tool in tools {
        let name = tool["name"].as_str().expect("tool name");
        let desc = tool["description"]
            .as_str()
            .unwrap_or_else(|| panic!("tool '{name}' is missing a description"));
        assert!(
            !desc.is_empty(),
            "tool '{name}' description must not be empty"
        );
    }

    // 2. Gated send/write tools (plus 0.2.0's forward/reply) document the
    // token flow.
    for gated in [
        "mail_send",
        "messages_send",
        "calendar_create",
        "calendar_update",
        "mail_forward",
        "mail_reply",
    ] {
        let desc = tools
            .iter()
            .find(|t| t["name"].as_str() == Some(gated))
            .unwrap_or_else(|| panic!("gated tool '{gated}' was not advertised"))["description"]
            .as_str()
            .expect("description");
        assert!(
            desc.contains("confirmation_token"),
            "gated tool '{gated}' must document confirmation_token: {desc}"
        );
    }

    // 2b. 0.2.0 tools must be advertised (presence, not count).
    for required in ["mail_forward", "mail_reply", "mail_config"] {
        assert!(
            tools.iter().any(|t| t["name"].as_str() == Some(required)),
            "tools/list must advertise '{required}'"
        );
    }

    // 3. Ungated clipboard tools must not claim a token gate.
    for ungated in ["clipboard_set", "clipboard_get"] {
        let desc = tools
            .iter()
            .find(|t| t["name"].as_str() == Some(ungated))
            .unwrap_or_else(|| panic!("clipboard tool '{ungated}' was not advertised"))["description"]
            .as_str()
            .expect("description");
        assert!(
            !desc.contains("confirmation_token"),
            "ungated tool '{ungated}' must not mention confirmation_token: {desc}"
        );
    }

    // 4. The mail_config doctor documents itself as read-only.
    let config_desc = tools
        .iter()
        .find(|t| t["name"].as_str() == Some("mail_config"))
        .unwrap_or_else(|| panic!("mail_config was not advertised"))["description"]
        .as_str()
        .expect("description");
    assert!(
        config_desc.contains("Read-only"),
        "mail_config must describe itself as read-only: {config_desc}"
    );

    // In-memory transport never signals EOF cleanly; kill the session task.
    drop(writer);
    handle.abort();
}

/// Spec §11.1: a client can trim the tool set at startup; hidden groups are
/// absent from discovery and refuse calls.
#[tokio::test]
async fn enabled_tools_trim_discovery_and_calls() {
    let dir = tempfile::tempdir().unwrap();
    let enabled = mcp_macos::EnabledTools {
        mail: true,
        messages: false,
        calendar: false,
        notifications: false,
        clipboard: false,
    };
    let server = mcp_macos::MacosServer::new_with_tools(
        dir.path().to_path_buf(),
        enabled,
        mcp_macos::policy::MailPolicy::default(),
        mcp_macos::policy::EffectiveScope::open(),
    );

    let (client, server_io) = tokio::io::duplex(64 * 1024);
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = BufReader::new(reader);

    let handle = tokio::spawn(async move {
        if let Ok(running) = rmcp::serve_server(server, server_io).await {
            let _ = running.waiting().await;
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    writer.write_all(INITIALIZE.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();
    let _ = read_line(&mut reader).await;

    writer.write_all(INITIALIZED.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.write_all(LIST_TOOLS.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();
    let v = read_line(&mut reader).await;

    let names: Vec<String> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"mail_search".to_string()));
    assert!(names.contains(&"permissions_check".to_string()));
    assert!(
        !names.iter().any(|n| n.starts_with("messages_")
            || n.starts_with("calendar_")
            || n.starts_with("notifications_")
            || n.starts_with("clipboard_")),
        "disabled groups leaked: {names:?}"
    );

    // Calling a disabled tool must refuse without executing.
    writer
        .write_all(
            br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"clipboard_get","arguments":{}}}"#,
        )
        .await
        .unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();
    let v2 = read_line(&mut reader).await;
    let text = v2["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("disabled"),
        "expected refusal payload, got: {v2}"
    );

    drop(writer);
    handle.abort();
}
