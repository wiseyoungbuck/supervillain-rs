//! Runtime contract for the contact insights sidebar (kata wcsg).
//!
//! `tests/contact_sidebar_test.cjs` extracts the real desktop
//! `contactSidebarHtml` / `loadContactInsights` functions and verifies their
//! rendered output, endpoint contract, caching, and render budget. Mirrors
//! `invite_chip_test.rs` and skips only when Node is unavailable.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn contact_sidebar_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "contact_sidebar_test.cjs"]
        .iter()
        .collect();
    assert!(
        test_js.exists(),
        "tests/contact_sidebar_test.cjs must exist (next to this file)"
    );

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping contact-sidebar behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "contact-sidebar behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
