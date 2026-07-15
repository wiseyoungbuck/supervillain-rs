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

# Run a command with a hard 5s cap. Pre---build-id binaries ignore the flag
# and START THE SERVER, which would hang the caller's command substitution
# forever; the cap turns that into "unknown build id" (= stale, rebuild).
# GNU timeout is everywhere on Linux; perl covers stock macOS.
_supervillain_with_timeout() {
    if command -v timeout &>/dev/null; then
        timeout 5 "$@"
    elif command -v perl &>/dev/null; then
        perl -e 'alarm shift; exec @ARGV' 5 "$@"
    else
        "$@"
    fi
}

check_and_update() {
    local repo
    repo="$(_supervillain_resolve_repo_dir)"
    # `.git` is a directory in a normal clone, a file in a git worktree.
    [[ -n "$repo" && -e "$repo/.git" ]] || return 0

    # Best-effort upstream check (needs network). Offline or fetch error?
    # Skip silently — the local binary-vs-HEAD check below still runs.
    local installed=false
    if git -C "$repo" fetch --quiet 2>/dev/null; then
        local local_rev upstream_rev
        if local_rev="$(git -C "$repo" rev-parse HEAD 2>/dev/null)" &&
            upstream_rev="$(git -C "$repo" rev-parse '@{u}' 2>/dev/null)" &&
            [[ "$local_rev" != "$upstream_rev" ]]; then
            echo "Supervillain: upstream ahead — rebuilding from $repo..."
            cargo install --path "$repo"
            installed=true
        fi
    fi
    $installed && return 0

    # Installed binary vs repo HEAD (no network needed). Merging a fix does
    # NOT deploy it — the binary embeds its build id (and all static assets)
    # at compile time, so a binary older than HEAD keeps serving old code
    # until reinstalled (kata tgax). An empty id (binary missing, or too old
    # to know --build-id) also counts as stale.
    local repo_head installed_id
    repo_head="$(git -C "$repo" rev-parse --short HEAD 2>/dev/null)" || return 0
    [[ -n "$repo_head" ]] || return 0
    installed_id="$(_supervillain_with_timeout supervillain --build-id 2>/dev/null)" || installed_id=""
    if [[ "$installed_id" != "$repo_head" ]]; then
        echo "Supervillain: installed binary (${installed_id:-unknown}) != repo HEAD ($repo_head) — rebuilding from $repo..."
        cargo install --path "$repo"
    fi
    return 0
}

# Symmetry hook with the launchers' source-only convention. Written as
# an if-block (not `[[ … ]] && return 0`) so the file's final exit status
# is 0 — otherwise sourcing under `set -e` would abort the caller.
if [[ "${SUPERVILLAIN_CHECK_SOURCE_ONLY:-0}" == "1" ]]; then
    return 0
fi
