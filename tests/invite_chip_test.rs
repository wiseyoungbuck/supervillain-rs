//! Runtime contract for inbox calendar-invite chips (kata trbx).
//!
//! `tests/invite_chip_test.cjs` extracts the real desktop and mobile
//! `renderInviteChip` functions and verifies their output for every RSVP state,
//! updates, and non-invite calendar methods. Mirrors `palette_test.rs` and skips
//! only when Node is unavailable; routes.rs string contracts still pin wiring.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn invite_chip_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "invite_chip_test.cjs"]
        .iter()
        .collect();
    assert!(
        test_js.exists(),
        "tests/invite_chip_test.cjs must exist (next to this file)"
    );

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping invite-chip behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "invite-chip behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
