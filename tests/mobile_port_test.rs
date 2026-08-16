//! Runs the mobile desktop-port behavior suite when Node is available.
//!
//! The JavaScript suite evals the real escaping, attachment, calendar and
//! read-toggle helpers out of static/mobile/app.js. It complements the
//! byte-identical port pins in routes.rs: those catch drift in the functions
//! mobile copies verbatim, while these cover the ones that legitimately
//! diverge from desktop and so have nothing to be pinned against. kata r29v.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn mobile_port_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "mobile_port_test.cjs"]
        .iter()
        .collect();
    assert!(test_js.exists(), "mobile port Node test must exist");

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping mobile port behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    assert!(
        output.status.success(),
        "mobile port behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
