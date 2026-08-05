//! Runs the refill-vs-optimistic-removal behavior harness (kata jg51)
//! when Node is available.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn refill_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "refill_test.cjs"].iter().collect();
    assert!(test_js.exists(), "tests/refill_test.cjs must exist");

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping refill behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .expect("failed to spawn Node for refill behavior tests");
    assert!(
        output.status.success(),
        "Refill behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
