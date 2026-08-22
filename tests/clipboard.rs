//! Clipboard toolset tests. pbcopy/pbpaste exist only on macOS, so the
//! round trip is macOS-gated; Linux CI covers the graceful-error path.

use mcp_macos::clipboard::ClipboardToolset;

#[cfg(target_os = "macos")]
#[test]
fn get_set_roundtrip() {
    let ts = ClipboardToolset::new();
    // Preserve the user's clipboard across the test.
    let saved = ts.get().ok();
    ts.set("clip-test-123").unwrap();
    assert_eq!(ts.get().unwrap(), "clip-test-123");
    if let Some(saved) = saved {
        let _ = ts.set(&saved);
    }
}

/// On non-macOS (Linux CI) pbpaste is absent: get must fail with a clear
/// error rather than panic.
#[cfg(not(target_os = "macos"))]
#[test]
fn get_fails_gracefully_without_pbpaste() {
    let ts = ClipboardToolset::new();
    assert!(ts.get().is_err());
}
