//! Behavioral test for the desktop email-iframe content-sizing (kata ceph).
//!
//! `tests/email_iframe_test.cjs` extracts the REAL `sizeIframeToContent` from
//! `static/app.js`, stands up a mock DOM in Node, and asserts the runtime
//! behavior — not just the code shape — so a layout race can't regress
//! silently. Specifically: a late-loading image must grow the iframe to full
//! content height even when the email's body is height-pinned by sender CSS
//! (html,body{height:100%}), which defeats the ResizeObserver (it watches
//! body's border-box, which never changes under height:100%). That is the
//! remaining "top ~10% stuck" gap in ceph.
//!
//! Mirrors `tests/scripts_test.rs`: shells out to `node --test`, and skips
//! (does not fail) if node is unavailable, so CI images without node don't
//! break. The string-invariant Rust tests in `src/routes.rs` pin the fix's
//! code shape regardless of node availability.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn email_iframe_sizing_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "email_iframe_test.cjs"]
        .iter()
        .collect();
    assert!(
        test_js.exists(),
        "tests/email_iframe_test.cjs must exist (next to this file)"
    );

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping email-iframe behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "email-iframe behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
