//! Behavioral test for the escape primitives (kata hp8w / yane).
//!
//! `tests/escape_test.cjs` extracts the REAL `escapeHtml`/`escapeAttr` from
//! `static/app.js`, stands up a mock DOM in Node, and asserts the runtime
//! behavior — not just the code shape — so an escape primitive that silently
//! does nothing can't pass. Specifically: a script-tag payload must be
//! entity-encoded by escapeHtml (hp8w class), and a quote-breakout payload
//! must have both `"` and `'` encoded by escapeAttr (yane class — escapeHtml
//! alone leaves quotes raw, which is why the attribute call site must use
//! escapeAttr).
//!
//! Mirrors `tests/scripts_test.rs`: shells out to `node --test`, and skips
//! (does not fail) if node is unavailable, so CI images without node don't
//! break. The string-invariant Tier-1 tests in `src/routes.rs` pin the fix's
//! code shape regardless of node availability.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn escape_primitive_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "escape_test.cjs"].iter().collect();
    assert!(
        test_js.exists(),
        "tests/escape_test.cjs must exist (next to this file)"
    );

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping escape behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "escape behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
