#!/usr/bin/env bash
# Behavioral test: the launcher must export SUPERVILLAIN_BIND (defaulting
# to the LAN/tailnet opt-in the binary itself no longer makes — roborev
# 273/279) and derive PORT from it, so overriding the bind address keeps
# the is-running/port-poll checks pointed at the right port.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
LAUNCHER="$REPO/scripts/supervillain-launcher.sh"
UPGRADE="$REPO/scripts/upgrade.sh"

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

# Case 3: URL host mirrors the binary's browser_url(): a wildcard bind is
# reachable at loopback, a specific host (incl. bracketed IPv6) is only
# listening on itself — opening loopback there hits a dead port.
out="$(
    env -u SUPERVILLAIN_BIND SUPERVILLAIN_LAUNCHER_SOURCE_ONLY=1 bash -c \
        "source '$LAUNCHER'; printf '%s' \"\$URL\""
)"
[[ "$out" == "http://127.0.0.1:8000" ]] ||
    fail "wildcard bind should open loopback, got: $out"

out="$(
    SUPERVILLAIN_BIND='[::1]:9000' SUPERVILLAIN_LAUNCHER_SOURCE_ONLY=1 bash -c \
        "source '$LAUNCHER'; printf '%s %s' \"\$PORT\" \"\$URL\""
)"
[[ "$out" == "9000 http://[::1]:9000" ]] ||
    fail "bracketed IPv6 bind should derive port and open the bound host, got: $out"

out="$(
    SUPERVILLAIN_BIND=100.64.1.5:8000 SUPERVILLAIN_LAUNCHER_SOURCE_ONLY=1 bash -c \
        "source '$LAUNCHER'; printf '%s' \"\$URL\""
)"
[[ "$out" == "http://100.64.1.5:8000" ]] ||
    fail "specific-host bind should open the bound host, got: $out"

# Case 4: a value with no numeric port must fail loudly, naming the
# variable — otherwise the scripts silently poll a nonsense port and
# report a bogus 15s startup failure.
if out="$(
    SUPERVILLAIN_BIND=0.0.0.0 SUPERVILLAIN_LAUNCHER_SOURCE_ONLY=1 bash -c \
        "source '$LAUNCHER'" 2>&1
)"; then
    fail "portless SUPERVILLAIN_BIND should be rejected, got success with: $out"
fi
[[ "$out" == *SUPERVILLAIN_BIND* ]] ||
    fail "rejection message should name SUPERVILLAIN_BIND, got: $out"

# Case 5: leading-zero ports are normalized — ss reports the listener as
# :8000, so an unnormalized 08000 would never match and the poll would
# report a bogus 15s startup failure.
out="$(
    SUPERVILLAIN_BIND=0.0.0.0:08000 SUPERVILLAIN_LAUNCHER_SOURCE_ONLY=1 bash -c \
        "source '$LAUNCHER'; printf '%s %s' \"\$PORT\" \"\$URL\""
)"
[[ "$out" == "8000 http://127.0.0.1:8000" ]] ||
    fail "leading-zero port should normalize to 8000, got: $out"

# Case 6: out-of-range ports fail here with a clear message instead of
# only at bind time.
if out="$(
    SUPERVILLAIN_BIND=0.0.0.0:99999 SUPERVILLAIN_LAUNCHER_SOURCE_ONLY=1 bash -c \
        "source '$LAUNCHER'" 2>&1
)"; then
    fail "out-of-range port should be rejected by the launcher, got success with: $out"
fi
[[ "$out" == *SUPERVILLAIN_BIND* ]] ||
    fail "launcher range rejection should name SUPERVILLAIN_BIND, got: $out"

# Case 7: upgrade.sh applies the same validation (its check runs before
# any side effects, so --dry-run is safe belt-and-braces).
if out="$(SUPERVILLAIN_BIND=0.0.0.0 "$UPGRADE" --dry-run 2>&1)"; then
    fail "upgrade.sh should reject a portless SUPERVILLAIN_BIND, got success with: $out"
fi
[[ "$out" == *SUPERVILLAIN_BIND* ]] ||
    fail "upgrade.sh rejection should name SUPERVILLAIN_BIND, got: $out"

if out="$(SUPERVILLAIN_BIND=0.0.0.0:99999 "$UPGRADE" --dry-run 2>&1)"; then
    fail "upgrade.sh should reject an out-of-range port, got success with: $out"
fi
[[ "$out" == *SUPERVILLAIN_BIND* ]] ||
    fail "upgrade.sh range rejection should name SUPERVILLAIN_BIND, got: $out"

echo "PASS: PORT/URL derivation, normalization, and bind validation hold in launcher and upgrade.sh"
