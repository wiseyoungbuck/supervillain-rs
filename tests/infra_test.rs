//! Wires `infra/tests/test_bootstrap.sh` into `cargo test` so the IaC
//! script's behavior tests can't bit-rot in isolation. The bash runner
//! mocks `gcloud` via PATH and asserts on the call log + script output,
//! so this needs nothing beyond bash itself (no gcloud, no network).
//!
//! If the bash side is unavailable (e.g. weird CI image without bash),
//! the test reports it but doesn't fail — the bash runner remains the
//! authoritative entry point; this just makes the default workflow run it.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn infra_bootstrap_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let script: PathBuf = [manifest_dir, "infra", "tests", "test_bootstrap.sh"]
        .iter()
        .collect();

    if Command::new("bash").arg("--version").output().is_err() {
        eprintln!("bash not on PATH; skipping infra bootstrap behavior tests");
        return;
    }

    let output = Command::new("bash")
        .arg(&script)
        .output()
        .expect("failed to spawn bash for infra/tests/test_bootstrap.sh");

    if !output.status.success() {
        panic!(
            "infra/tests/test_bootstrap.sh failed\n--- stdout ---\n{}--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
