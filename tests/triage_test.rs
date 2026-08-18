//! Behavioral test wrapper for triage mode — Get-Me-To-Zero (kata 5np4).
//!
//! `tests/triage_test.cjs` drives the REAL handleNormalModeKey (with the real
//! emailAction/toggleUnread/removal machinery underneath) and asserts the
//! advance-after-action semantics: T enters over the unread queue in order,
//! e/#/u act-and-advance to the next UNREAD, j skips, the last email lands on
//! the zero state, a failed action reverts and STAYS, Escape exits clean, the
//! queue tolerates vanished ids, the palette gates on unread presence, and
//! 100 keystrokes over a 1,000-email list stay under 200ms total.
//!
//! Mirrors `tests/palette_test.rs`: shells out to `node --test`, skipping
//! (not failing) when node is unavailable.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn triage_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "triage_test.cjs"].iter().collect();
    assert!(
        test_js.exists(),
        "tests/triage_test.cjs must exist (next to this file)"
    );

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping triage behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "triage behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
