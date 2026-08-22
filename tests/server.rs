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
