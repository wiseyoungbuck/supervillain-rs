#!/usr/bin/env bash
# check_and_update — if the repo at $SUPERVILLAIN_REPO_DIR is behind its
# upstream, rebuild & reinstall the binary via `cargo install --path`.
#
# Designed to be sourced by launcher scripts. Never aborts the launcher:
# any failure (no repo, no network, fetch error) is swallowed so the user
# still gets their app even when offline.

# Resolve repo dir: explicit env var wins; otherwise fall back to a stamp
# file written by the installer.
_supervillain_resolve_repo_dir() {
    if [[ -n "${SUPERVILLAIN_REPO_DIR:-}" ]]; then
        echo "$SUPERVILLAIN_REPO_DIR"
        return 0
    fi
    local stamp="${XDG_CONFIG_HOME:-$HOME/.config}/supervillain/repo"
    [[ -f "$stamp" ]] && cat "$stamp"
}

check_and_update() {
    local repo
    repo="$(_supervillain_resolve_repo_dir)"
    # `.git` is a directory in a normal clone, a file in a git worktree.
    [[ -n "$repo" && -e "$repo/.git" ]] || return 0

    # Best-effort fetch. Offline? Skip silently — the user's already-built
    # binary is still good enough to launch.
    git -C "$repo" fetch --quiet 2>/dev/null || return 0

    local local_rev upstream_rev
    local_rev="$(git -C "$repo" rev-parse HEAD 2>/dev/null)" || return 0
    upstream_rev="$(git -C "$repo" rev-parse '@{u}' 2>/dev/null)" || return 0
    [[ "$local_rev" == "$upstream_rev" ]] && return 0

    echo "Supervillain: upstream ahead — rebuilding from $repo..."
    cargo install --path "$repo"
}

# Symmetry hook with the launchers' source-only convention. Written as
# an if-block (not `[[ … ]] && return 0`) so the file's final exit status
# is 0 — otherwise sourcing under `set -e` would abort the caller.
if [[ "${SUPERVILLAIN_CHECK_SOURCE_ONLY:-0}" == "1" ]]; then
    return 0
fi
