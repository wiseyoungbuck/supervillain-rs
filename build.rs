// Embeds a per-build identifier so the mobile service worker's CACHE_NAME
// changes on every deploy, not just when CARGO_PKG_VERSION is bumped.
// scripts/upgrade.sh redeploys per-commit; CARGO_PKG_VERSION rarely moves,
// so without this consecutive deploys would share one cache (kata: roborev
// 284) and clients would never pick up the new app shell.
use std::path::PathBuf;
use std::process::Command;

fn git_stdout(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn main() {
    let build_id = git_stdout(&["rev-parse", "--short", "HEAD"]);
    let build_id_unavailable = build_id.is_none();
    let build_id = build_id.unwrap_or_else(|| "unknown".to_string());

    if build_id_unavailable {
        println!(
            "cargo:warning=SUPERVILLAIN_BUILD_ID unknown (git unavailable); SW cache-busting degraded"
        );
    }

    println!("cargo:rustc-env=SUPERVILLAIN_BUILD_ID={build_id}");

    // Re-run when HEAD moves to a new commit or a branch/ref is updated, so a
    // fresh commit always gets a fresh build id without a `cargo clean`. In a
    // worktree checkout `.git` is a file (not a directory) pointing at the
    // real git dir nested under the main checkout's `.git/worktrees/<name>`,
    // so `.git/HEAD`/`.git/refs` don't exist as paths relative to this crate
    // — resolve the actual git dir instead of assuming `.git/`, falling back
    // to the plain-repo layout if git isn't available.
    let git_dir = git_stdout(&["rev-parse", "--git-dir"])
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".git"));

    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!("cargo:rerun-if-changed={}", git_dir.join("refs").display());
}
