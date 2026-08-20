//! Behavioral test wrapper for the Move to Folder / Apply Label picker
//! (kata e993).
//!
//! `tests/move_picker_test.cjs` extracts the REAL picker functions from
//! `static/app.js`, stubs `state`/`els`/`document`, and asserts observable
//! outcomes: the rendered picker markup (current-mailbox exclusion, Gmail
//! label wording, fhtz escaping), keyboard navigation, the
//! POST /emails/{id}/move request contract with optimistic removal and
//! failure revert, undo-to-source-mailbox, palette gating, and the
//! 500-mailbox render budget.
//!
//! Mirrors `tests/palette_test.rs`: shells out to `node --test`, and skips
//! (does not fail) if node is unavailable, so CI images without node don't
//! break.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn move_picker_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "move_picker_test.cjs"]
        .iter()
        .collect();
    assert!(
        test_js.exists(),
        "tests/move_picker_test.cjs must exist (next to this file)"
    );

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping move-picker behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "move-picker behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
