//! Behavioral test wrapper for bulk selection (kata pakx).
//!
//! `tests/bulk_ops_test.cjs` drives the REAL handleNormalModeKey with x /
//! Shift+X / Escape / e / u / s / v keystroke sequences and asserts selection
//! state, the batch request contracts over the existing per-email endpoints,
//! single-entry batch undo restoring to the source mailbox, partial-failure
//! revert, selection lifecycle across mailbox/split/account switches, palette
//! gating, rendered selection markup, and the 1,000-email/500-selected
//! renderEmailList budget.
//!
//! Mirrors `tests/palette_test.rs`: shells out to `node --test`, skipping
//! (not failing) when node is unavailable.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn bulk_ops_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "bulk_ops_test.cjs"].iter().collect();
    assert!(
        test_js.exists(),
        "tests/bulk_ops_test.cjs must exist (next to this file)"
    );

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping bulk-ops behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "bulk-ops behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
