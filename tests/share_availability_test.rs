//! Behavioral test for Share Availability compose insertion (kata mtqp).
//!
//! `tests/share_availability_test.cjs` extracts the REAL `availabilityText`
//! and `insertAtCursor` from `static/app.js` and asserts the exact text block
//! built from a mocked /api/calendar/free-slots response (contiguous slots
//! merged, grouped per day) and the exact compose-body value after cursor
//! insertion.
//!
//! Mirrors `tests/palette_test.rs`: shells out to `node --test`, and skips
//! (does not fail) if node is unavailable.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn share_availability_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "share_availability_test.cjs"]
        .iter()
        .collect();
    assert!(
        test_js.exists(),
        "tests/share_availability_test.cjs must exist (next to this file)"
    );

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping share-availability behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "share-availability behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
