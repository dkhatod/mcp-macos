//! macOS clipboard via `pbcopy`/`pbpaste` — no AppleScript needed.
//!
//! Auto-tier (no gate). Synchronous on purpose: both binaries are fast and
//! the MCP tool surface wraps them in an async handler anyway.

use std::process::{Command, Stdio};

/// Clipboard read/write over the system pasteboard binaries.
pub struct ClipboardToolset;

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("clipboard unavailable (macOS only): {0}")]
    Io(#[from] std::io::Error),
    #[error("pbcopy failed")]
    SetFailed,
}

impl ClipboardToolset {
    pub fn new() -> Self {
        Self
    }

    /// Reads UTF-8 text from the clipboard.
    pub fn get(&self) -> Result<String, ClipboardError> {
        let out = Command::new("pbpaste").output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Writes UTF-8 text to the clipboard.
    pub fn set(&self, text: &str) -> Result<(), ClipboardError> {
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin was piped")
            .write_all(text.as_bytes())?;
        let status = child.wait()?;
        if status.success() {
            Ok(())
        } else {
            Err(ClipboardError::SetFailed)
        }
    }
}

impl Default for ClipboardToolset {
    fn default() -> Self {
        Self::new()
    }
}
