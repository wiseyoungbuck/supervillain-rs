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
    // Width pinned to 12: git's default abbreviation grows with the object
    // count, so an unpinned --short could disagree with the launch scripts'
    // `rev-parse --short=12 HEAD` for the same commit, forcing a spurious
    // rebuild+restart on every launch until reinstalled.
    let build_id = git_stdout(&["rev-parse", "--short=12", "HEAD"]);
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
    // In a worktree, <gitdir>/HEAD above is this worktree's own symbolic ref
    // file — it only changes on a branch switch, not on every commit — and
    // refs/heads lives in the COMMON git dir shared across all worktrees, so
    // consecutive commits made in THIS worktree touch neither path and cargo
    // keeps serving a stale SUPERVILLAIN_BUILD_ID (roborev 302, fix 3).
    // logs/HEAD is the per-worktree reflog: git appends to it on every
    // commit, checkout, and ref update made in this specific worktree, so
    // watching it too means a fresh commit here always gets a fresh build
    // id. Emitted unconditionally even when logs/HEAD doesn't exist yet
    // (e.g. a fresh repo, or reflogs disabled via core.logAllRefUpdates=false)
    // — that's not a harmless no-op: cargo treats a missing rerun-if-changed
    // path as permanently dirty, so this build script re-runs on every build
    // until the file appears. Accepted trade-off, not a bug — this build
    // script and the env it emits are cheap/unchanged when nothing actually
    // moved, so the extra reruns aren't worth guarding against.
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("logs").join("HEAD").display()
    );
}
