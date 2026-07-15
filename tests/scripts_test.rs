//! Wires `scripts/tests/*.sh` into `cargo test` so the launcher and
//! check-and-update behavior tests can't bit-rot in isolation. The bash
//! runners stub `git`/`cargo`/`curl`/`supervillain` via PATH, so this needs
//! nothing beyond bash itself (no network, no real builds).
//!
//! If bash is unavailable (e.g. weird CI image), the test reports it but
//! doesn't fail — the bash runners remain the authoritative entry point;
//! this just makes the default workflow run them.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn launcher_script_behavior_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let tests_dir: PathBuf = [manifest_dir, "scripts", "tests"].iter().collect();

    if Command::new("bash").arg("--version").output().is_err() {
        eprintln!("bash not on PATH; skipping scripts behavior tests");
        return;
    }

    let mut scripts: Vec<PathBuf> = std::fs::read_dir(&tests_dir)
        .expect("scripts/tests must exist")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "sh"))
        .collect();
    scripts.sort();
    assert!(
        !scripts.is_empty(),
        "scripts/tests contains no .sh files — the glob is broken, not the tests"
    );

    for script in scripts {
        let output = Command::new("bash")
            .arg(&script)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn bash for {}: {e}", script.display()));
        if !output.status.success() {
            panic!(
                "{} failed\n--- stdout ---\n{}--- stderr ---\n{}",
                script.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
