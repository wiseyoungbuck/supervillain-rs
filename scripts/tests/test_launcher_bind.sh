#!/usr/bin/env bash
# Behavioral test: the launcher must export SUPERVILLAIN_BIND (defaulting
# to the LAN/tailnet opt-in the binary itself no longer makes — roborev
# 273/279) and derive PORT from it, so overriding the bind address keeps
# the is-running/port-poll checks pointed at the right port.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
LAUNCHER="$REPO/scripts/supervillain-launcher.sh"

fail() {
    echo "FAIL: $1"
    exit 1
}

# Case 1: no SUPERVILLAIN_BIND in the environment → launcher opts in to
# 0.0.0.0:8000 and PORT follows.
out="$(
    env -u SUPERVILLAIN_BIND SUPERVILLAIN_LAUNCHER_SOURCE_ONLY=1 bash -c \
        "source '$LAUNCHER'; printf '%s %s' \"\$SUPERVILLAIN_BIND\" \"\$PORT\""
)"
[[ "$out" == "0.0.0.0:8000 8000" ]] ||
    fail "default should export SUPERVILLAIN_BIND=0.0.0.0:8000 with PORT=8000, got: $out"

# Case 2: caller override → respected, and PORT derived from it so
# port_listening polls the actual port.
out="$(
    SUPERVILLAIN_BIND=127.0.0.1:9000 SUPERVILLAIN_LAUNCHER_SOURCE_ONLY=1 bash -c \
        "source '$LAUNCHER'; printf '%s %s' \"\$SUPERVILLAIN_BIND\" \"\$PORT\""
)"
[[ "$out" == "127.0.0.1:9000 9000" ]] ||
    fail "override should be respected with PORT derived, got: $out"

echo "PASS: launcher exports SUPERVILLAIN_BIND and derives PORT from it"
