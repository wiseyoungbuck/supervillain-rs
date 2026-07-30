//! Behavioral browser-bundle tests for Unicode attachment filenames (kata sbrd).
//!
//! The Node suite extracts the real encoder from both `static/app.js` and
//! `static/mobile/app.js` and checks Japanese, emoji, and accented Latin names.
//! As with the other JS behavior suites, CI hosts without Node skip this tier;
//! Rust contract tests in `src/routes.rs` still pin both call sites.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn attachment_filename_browser_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "attachment_filename_test.cjs"]
        .iter()
        .collect();
    assert!(test_js.exists(), "attachment filename Node test must exist");

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping attachment filename browser tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "attachment filename browser tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
