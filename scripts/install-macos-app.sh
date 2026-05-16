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
cp "${REPO_DIR}/scripts/supervillain-launcher.sh" "$MACOS_DIR/supervillain-launcher"
chmod +x "$MACOS_DIR/supervillain-launcher"

# Stamp the repo path so the launcher can find scripts/check-and-update.sh
# and rebuild from source when upstream moves ahead.
STAMP_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/supervillain"
mkdir -p "$STAMP_DIR"
printf '%s\n' "$REPO_DIR" > "$STAMP_DIR/repo"
echo "Stamped repo path at $STAMP_DIR/repo"

echo "Installed ${APP_DIR}"
echo "You can now launch Supervillain from Spotlight or /Applications."
