#!/usr/bin/env bash
# Behavioral test: open_webapp must invoke the OS default browser by
# calling `open "$URL"` with no app-preference arguments. Anything else
# (e.g. `open -na "/Applications/Google Chrome.app" --args --app=URL`)
# overrides the user's macOS default and is a regression.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Stub `open`: record argv (one line per arg) and exit.
cat > "$TMP/open" <<'STUB'
#!/usr/bin/env bash
: > "$OPEN_ARGS_FILE"
for a in "$@"; do
    printf '%s\n' "$a" >> "$OPEN_ARGS_FILE"
done
STUB
chmod +x "$TMP/open"

export PATH="$TMP:$PATH"
export OPEN_ARGS_FILE="$TMP/args"
export SUPERVILLAIN_LAUNCHER_SOURCE_ONLY=1

# shellcheck disable=SC1091
source "$REPO/scripts/supervillain-launcher.sh"

open_webapp

expected="$URL"
actual="$(cat "$OPEN_ARGS_FILE")"

if [[ "$actual" != "$expected" ]]; then
    echo "FAIL: open_webapp should invoke the OS default browser"
    echo "  expected argv:"
    printf '    %s\n' "$expected"
    echo "  actual argv:"
    printf '    %s\n' $actual
    exit 1
fi

echo "PASS: open_webapp respects the OS default browser"
