//! Server-surface contracts for the Messages index tools: group trimming,
//! parameter validation. Sync/search delegation is covered by the unit and
//! live suites.

use mcp_macos::{MacosServer, MessagesSearchParams, MessagesSyncParams};
use rmcp::handler::server::wrapper::Parameters;

fn server(tools: &str) -> MacosServer {
    let dir = tempfile::tempdir().unwrap();
    std::mem::forget(dir);
    let enabled = mcp_macos::EnabledTools::parse(tools).unwrap();
    MacosServer::new_with_tools(
        std::path::PathBuf::from("/tmp/mcp-msg-test-state"),
        enabled,
        mcp_macos::policy::MailPolicy::default(),
        mcp_macos::policy::EffectiveScope::open(),
    )
}

#[tokio::test]
async fn messages_index_tools_honor_group_trimming() {
    let s = server("mail");
    for call in [
        serde_json::json!({}),
        serde_json::json!({"query": "x"}),
        serde_json::json!({}),
        serde_json::json!({}),
    ] {
        let out = match call.as_object().unwrap().get("query") {
            Some(_) => {
                let p: MessagesSearchParams = serde_json::from_value(call).unwrap();
                s.messages_search(Parameters(p)).await
            }
            None => {
                let p: MessagesSyncParams = serde_json::from_value(call).unwrap();
                s.messages_sync(Parameters(p)).await
            }
        };
        assert!(out.contains("disabled"), "{out}");
    }
}

#[tokio::test]
async fn messages_search_rejects_blank_query() {
    let s = server("messages");
    let p: MessagesSearchParams =
        serde_json::from_value(serde_json::json!({"query": "   "})).unwrap();
    let out = s.messages_search(Parameters(p)).await;
    assert!(out.contains("query"), "{out}");
}
