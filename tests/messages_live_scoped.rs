//! LIVE scoped Messages verification — run MANUALLY with a contact's
//! handle(s); never runs in CI:
//!
//! ```sh
//! MCP_MESSAGES_LIVE_CHAT='+17038140603' \
//!   cargo test --test messages_live_scoped -- --ignored --nocapture
//! ```
//!
//! Privacy contract: pass exactly ONE chat identifier you are authorized to
//! inspect; this test reads only that chat (3 newest messages).

use mcp_macos::messages::MessagesToolset;
use personai_core::macos::JxaTransport;

fn live_chat() -> String {
    std::env::var("MCP_MESSAGES_LIVE_CHAT")
        .expect("set MCP_MESSAGES_LIVE_CHAT=<chat identifier> to run this test")
}

#[tokio::test]
#[ignore = "live chat.db required; opt in with MCP_MESSAGES_LIVE_CHAT"]
async fn scoped_read_returns_real_messages() {
    let mut ts = MessagesToolset::new(JxaTransport);
    let chat = live_chat();
    let out = ts
        .read(Some(chat.clone()), Some(3), 0)
        .await
        .unwrap_or_else(|e| panic!("scoped read failed for {chat}: {e}"));
    println!("payload: {out}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("payload must be valid JSON");
    assert!(v["total"].is_u64(), "total must be present");
    assert!(v["messages"].is_array());
}

#[tokio::test]
#[ignore = "live chat.db required; opt in with MCP_MESSAGES_LIVE_CHAT"]
async fn scoped_raw_transport_dump() {
    // Diagnostic companion: prints the exact stdout the mapper consumes.
    unsafe { std::env::set_var("MCP_MACOS_DEBUG_RAW", "1") };
    let mut ts = MessagesToolset::new(JxaTransport);
    let chat = live_chat();
    match ts.read(Some(chat), Some(1), 0).await {
        Ok(out) => println!(
            "OK payload head: {}",
            out.chars().take(300).collect::<String>()
        ),
        Err(e) => println!("ERR (raw dump above if emitted): {e}"),
    }
}
