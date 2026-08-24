//! Release-hygiene contract: the `#[tool_handler(version = …)]` literal in
//! src/lib.rs must track Cargo.toml. It silently drifted once (reported
//! 0.1.7 while shipping 0.1.8), which made clients unable to tell what
//! they were talking to.

use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn cargo_version() -> String {
    let cargo = std::fs::read_to_string(manifest_dir().join("Cargo.toml")).unwrap();
    cargo
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("version = \"")
                .map(|rest| rest.trim_end_matches('"').to_string())
        })
        .expect("version line in Cargo.toml")
}

#[test]
fn handler_version_literal_tracks_cargo_version() {
    let ver = cargo_version();
    let lib = std::fs::read_to_string(manifest_dir().join("src/lib.rs")).unwrap();
    assert!(
        lib.contains(&format!("version = \"{ver}\"")),
        "#[tool_handler] version literal drifted from Cargo.toml ({ver})"
    );
}

#[test]
fn changelog_documents_current_version() {
    let ver = cargo_version();
    if ver.rsplit('.').next() == Some("0") {
        return; // dev-cycle versions need no entry yet
    }
    let log = std::fs::read_to_string(manifest_dir().join("CHANGELOG.md")).unwrap();
    assert!(
        log.contains(&format!("## [{ver}]")),
        "CHANGELOG.md missing an entry for {ver}"
    );
}
