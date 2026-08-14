//! Runs the explicit email-refresh behavior suite when Node is available.
//!
//! The JavaScript suite executes the real URL builders and desktop loader from
//! both shipped bundles. It is deliberately separate from the Playwright tier:
//! these tests are fast enough to run on every `cargo test`, while the browser
//! spec covers the rendered row appearing after a refresh request.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn email_refresh_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "email_refresh_test.cjs"]
        .iter()
        .collect();
    assert!(test_js.exists(), "email refresh Node test must exist");

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping email refresh behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    assert!(
        output.status.success(),
        "email refresh behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
