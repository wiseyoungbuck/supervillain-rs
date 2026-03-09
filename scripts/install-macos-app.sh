#!/usr/bin/env bash
set -euo pipefail

# Build and install Supervillain as a macOS .app bundle in /Applications

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="Supervillain"
APP_DIR="/Applications/${APP_NAME}.app"
CONTENTS_DIR="${APP_DIR}/Contents"
MACOS_DIR="${CONTENTS_DIR}/MacOS"
RESOURCES_DIR="${CONTENTS_DIR}/Resources"

echo "Building supervillain..."
cargo install --path "$REPO_DIR"

BINARY_PATH="$(which supervillain)"
if [[ -z "$BINARY_PATH" ]]; then
    echo "Error: supervillain binary not found after install" >&2
    exit 1
fi

echo "Creating ${APP_DIR}..."
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR"

# Generate .icns from the 512px PNG
ICON_SRC="${REPO_DIR}/static/icon-512.png"
if [[ -f "$ICON_SRC" ]]; then
    ICONSET_DIR=$(mktemp -d)/supervillain.iconset
    mkdir -p "$ICONSET_DIR"
    sips -z 16 16     "$ICON_SRC" --out "$ICONSET_DIR/icon_16x16.png"      > /dev/null
    sips -z 32 32     "$ICON_SRC" --out "$ICONSET_DIR/icon_16x16@2x.png"   > /dev/null
    sips -z 32 32     "$ICON_SRC" --out "$ICONSET_DIR/icon_32x32.png"      > /dev/null
    sips -z 64 64     "$ICON_SRC" --out "$ICONSET_DIR/icon_32x32@2x.png"   > /dev/null
    sips -z 128 128   "$ICON_SRC" --out "$ICONSET_DIR/icon_128x128.png"    > /dev/null
    sips -z 256 256   "$ICON_SRC" --out "$ICONSET_DIR/icon_128x128@2x.png" > /dev/null
    sips -z 256 256   "$ICON_SRC" --out "$ICONSET_DIR/icon_256x256.png"    > /dev/null
    sips -z 512 512   "$ICON_SRC" --out "$ICONSET_DIR/icon_256x256@2x.png" > /dev/null
    sips -z 512 512   "$ICON_SRC" --out "$ICONSET_DIR/icon_512x512.png"    > /dev/null
    cp "$ICON_SRC" "$ICONSET_DIR/icon_512x512@2x.png"
    iconutil -c icns "$ICONSET_DIR" -o "$RESOURCES_DIR/supervillain.icns"
    rm -rf "$(dirname "$ICONSET_DIR")"
    echo "Generated app icon."
fi

# Info.plist
cat > "$CONTENTS_DIR/Info.plist" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Supervillain</string>
    <key>CFBundleDisplayName</key>
    <string>Supervillain</string>
    <key>CFBundleIdentifier</key>
    <string>com.supervillain.mail</string>
    <key>CFBundleVersion</key>
    <string>0.2.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.2.0</string>
    <key>CFBundleExecutable</key>
    <string>supervillain-launcher</string>
    <key>CFBundleIconFile</key>
    <string>supervillain</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>LSUIElement</key>
    <false/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

# Launcher script — starts the server and opens as a native webapp
cat > "$MACOS_DIR/supervillain-launcher" << 'LAUNCHER'
#!/usr/bin/env bash

BINARY="${HOME}/.cargo/bin/supervillain"
PORT=8000
URL="http://127.0.0.1:${PORT}"
LOG_FILE="${TMPDIR:-/tmp}/supervillain.log"

if [[ ! -x "$BINARY" ]]; then
    osascript -e 'display alert "Supervillain not found" message "Run: cargo install --path /path/to/supervillain-rs" as critical'
    exit 1
fi

port_listening() {
    /usr/sbin/lsof -i ":${PORT}" -sTCP:LISTEN &>/dev/null
}

# Open as a standalone webapp window (no browser chrome).
# Tries Chrome first, then Edge, then falls back to default browser.
open_webapp() {
    local chrome="/Applications/Google Chrome.app"
    local edge="/Applications/Microsoft Edge.app"
    if [[ -d "$chrome" ]]; then
        open -na "$chrome" --args --app="$URL"
    elif [[ -d "$edge" ]]; then
        open -na "$edge" --args --app="$URL"
    else
        open "$URL"
    fi
}

# If already running, just open a webapp window
if port_listening; then
    open_webapp
    exit 0
fi

# Start the server
"$BINARY" --no-browser > "$LOG_FILE" 2>&1 &
SERVER_PID=$!

# Wait for server to be ready
for _ in $(seq 1 30); do
    # Check the process is still alive
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        osascript -e 'display alert "Supervillain crashed on startup" message "Check '"$LOG_FILE"' for details." as critical'
        exit 1
    fi
    if port_listening; then
        open_webapp
        wait $SERVER_PID
        exit 0
    fi
    sleep 0.5
done

osascript -e 'display alert "Supervillain failed to start" message "Check '"$LOG_FILE"' for details." as critical'
kill $SERVER_PID 2>/dev/null || true
exit 1
LAUNCHER
chmod +x "$MACOS_DIR/supervillain-launcher"

echo "Installed ${APP_DIR}"
echo "You can now launch Supervillain from Spotlight or /Applications."
