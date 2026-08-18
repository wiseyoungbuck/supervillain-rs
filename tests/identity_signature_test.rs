//! Runtime contract for per-identity signatures (kata zqrn).
//!
//! `tests/identity_signature_test.cjs` extracts the real signature helpers
//! from BOTH bundles, pins them byte-identical across surfaces, and verifies
//! the observable compose-body behavior: a From change swaps only the
//! signature block, preserving user edits. Mirrors `invite_chip_test.rs`;
//! skips only when Node is unavailable.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn identity_signature_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [manifest_dir, "tests", "identity_signature_test.cjs"]
        .iter()
        .collect();
    assert!(
        test_js.exists(),
        "tests/identity_signature_test.cjs must exist (next to this file)"
    );

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping identity-signature behavior tests");
        return;
    }

    let output = Command::new("node")
        .arg("--test")
        .arg(&test_js)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    if !output.status.success() {
        panic!(
            "identity-signature behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
