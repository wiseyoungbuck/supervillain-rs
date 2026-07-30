//! Runs the Remind Me client behavior harness when Node is available.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn reminder_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "reminder_test.cjs"]
        .iter()
        .collect();
    assert!(test_js.exists(), "tests/reminder_test.cjs must exist");

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping Remind Me behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .expect("failed to spawn Node for Remind Me behavior tests");
    assert!(
        output.status.success(),
        "Remind Me behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
