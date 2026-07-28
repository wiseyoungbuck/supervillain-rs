//! Behavioral test for the command-palette context pass (kata sefy).
//!
//! `tests/palette_test.cjs` extracts the REAL `getCommands` (and its per-view
//! builder `commandsForView`) from `static/app.js`, stubs `state` + `visibleRows`,
//! and asserts the runtime action set per view — not just the code shape — so a
//! context regression can't hide behind a matching code form. Specifically:
//! compose offers Send + Close Draft; detail with a calendar event offers an
//! RSVP action (and without one offers none — the calendar gate mirrors the
//! y/n/m keybindings); list with no selection omits Reply; detail ranks Reply
//! above Compose.
//!
//! Mirrors `tests/email_iframe_test.rs`: shells out to `node --test`, and skips
//! (does not fail) if node is unavailable, so CI images without node don't
//! break. The string-invariant Rust tests in `src/routes.rs` pin the fix's code
//! shape regardless of node availability.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn palette_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "palette_test.cjs"].iter().collect();
    assert!(
        test_js.exists(),
        "tests/palette_test.cjs must exist (next to this file)"
    );

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping command-palette behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "command-palette behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
