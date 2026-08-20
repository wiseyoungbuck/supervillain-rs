//! Runs the desktop send-path behavior harness when Node is available.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn send_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "send_test.cjs"].iter().collect();
    assert!(test_js.exists(), "tests/send_test.cjs must exist");

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping send behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .expect("failed to spawn Node for send behavior tests");
    assert!(
        output.status.success(),
        "send behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
