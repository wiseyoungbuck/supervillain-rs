// Embeds a per-build identifier so the mobile service worker's CACHE_NAME
// changes on every deploy, not just when CARGO_PKG_VERSION is bumped.
// scripts/upgrade.sh redeploys per-commit; CARGO_PKG_VERSION rarely moves,
// so without this consecutive deploys would share one cache (kata: roborev
// 284) and clients would never pick up the new app shell.
use std::process::Command;

fn main() {
    let build_id = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=SUPERVILLAIN_BUILD_ID={build_id}");
    // Re-run when HEAD moves to a new commit or a branch/ref is updated,
    // so a fresh commit always gets a fresh build id without a `cargo clean`.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
}
