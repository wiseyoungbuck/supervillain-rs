//! Runs the offline-mail behavior suite when Node is available (kata 2chc).
//!
//! The JavaScript suite extracts the real snapshot writer, restore, offline
//! gate and init() from the shipped mobile bundle and drives them against a
//! fake localStorage and mock DOM — fast enough for every `cargo test`, unlike
//! the Playwright tier. Airplane-mode verification on the actual phone is a
//! separate, manual gate (see the ticket's verification section).

use std::path::PathBuf;
use std::process::Command;

#[test]
fn offline_cache_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "offline_cache_test.cjs"]
        .iter()
        .collect();
    assert!(test_js.exists(), "offline cache Node test must exist");

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping offline cache behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    assert!(
        output.status.success(),
        "offline cache behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
