//! Behavioral test for the calendar peek render (kata j6e4).
//!
//! `tests/calendar_peek_test.cjs` extracts the REAL `calendarPeekHtml` (and
//! the helpers it composes with) from `static/app.js` and asserts the rendered
//! day/week HTML for mocked events: all-day vs timed placement, day-column
//! assignment, cross-midnight clamping, escaping, and the 200-event-week
//! perf budget.
//!
//! Mirrors `tests/palette_test.rs`: shells out to `node --test`, and skips
//! (does not fail) if node is unavailable, so CI images without node don't
//! break.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn calendar_peek_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "calendar_peek_test.cjs"]
        .iter()
        .collect();
    assert!(
        test_js.exists(),
        "tests/calendar_peek_test.cjs must exist (next to this file)"
    );

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping calendar peek behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "calendar peek behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
