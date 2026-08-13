//! Behavioral test for Ctrl/Cmd+C copy out of the sandboxed email-body
//! iframe (kata 26gb).
//!
//! `tests/copy_selection_test.cjs` extracts the REAL
//! `copyEmailIframeSelection` / `execCommandCopyFallback` from
//! `static/app.js`, injects mock window/document/navigator, and asserts the
//! runtime behavior: an iframe selection is copied as plain text, a real
//! parent-document selection is never hijacked, a rejected clipboard write
//! falls back to the textarea/execCommand path, and the handleNormalModeKey
//! chord guard (Ctrl+C must not open compose) stays in place.
//!
//! Mirrors `tests/email_iframe_test.rs`: shells out to `node --test`, and
//! skips (does not fail) if node is unavailable, so CI images without node
//! don't break.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn copy_selection_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "copy_selection_test.cjs"]
        .iter()
        .collect();
    assert!(
        test_js.exists(),
        "tests/copy_selection_test.cjs must exist (next to this file)"
    );

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping copy-selection behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "copy-selection behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
