//! mcp-macos library: five self-contained Apple tool groups behind one MCP
//! server.
//!
//! Layout: each tool group lives in its own module (`mail`, `messages`, …)
//! as a `*Toolset<T: AppleTransport>` that is fully testable against
//! [`personai_core::macos::MockTransport`]. The MCP surface is
//! [`MacosServer`] — thin `#[tool]` methods that lock [`ServerState`] and
//! delegate to the toolsets.
//!
//! See `docs/development.md` for how to add a tool group.

use std::sync::Arc;

use rmcp::tool_router;
use tokio::sync::Mutex;

/// Mutable server state shared by all tools.
///
/// The MCP handler methods take `&self`; everything that must mutate
/// (transports, gates) lives here behind an async mutex, because tool bodies
/// await subprocesses while holding the lock. Each field is one tool group;
/// groups never touch each other.
#[derive(Default)]
pub struct ServerState {}

/// The MCP server handle passed to every tool method.
pub struct MacosServer {
    pub state_dir: std::path::PathBuf,
    /// Read once the first tool group registers (Mail). The `expect` errors
    /// as soon as this is actually used — remove it then.
    #[expect(dead_code)]
    inner: Arc<Mutex<ServerState>>,
}

#[tool_router(server_handler)]
impl MacosServer {
    /// Build a server rooted at `state_dir`.
    ///
    /// Tool groups register themselves here as they are implemented; the
    /// soft-gate token store lives at `<state_dir>/tokens.json`.
    pub fn new(state_dir: std::path::PathBuf) -> Self {
        Self {
            state_dir,
            inner: Arc::new(Mutex::new(ServerState {})),
        }
    }
}
