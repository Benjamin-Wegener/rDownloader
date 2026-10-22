#!/usr/bin/env bash
# packaging/macos/bundle.sh
# Bundles target/release/rdownloader into a macOS .app bundle.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

TARGET_DIR="${1:-$REPO_ROOT/target/release}"
OUTPUT_DIR="${2:-$REPO_ROOT/target/bundle/osx}"
APP_NAME="rDownloader.app"
APP_DIR="$OUTPUT_DIR/$APP_NAME"

echo "Building macOS application bundle: $APP_DIR"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"

# Copy binary
if [ -f "$TARGET_DIR/rdownloader" ]; then
    cp "$TARGET_DIR/rdownloader" "$APP_DIR/Contents/MacOS/rdownloader"
    chmod +x "$APP_DIR/Contents/MacOS/rdownloader"
elif [ -f "$REPO_ROOT/target/debug/rdownloader" ]; then
    echo "Warning: target/release/rdownloader not found, using debug binary"
    cp "$REPO_ROOT/target/debug/rdownloader" "$APP_DIR/Contents/MacOS/rdownloader"
    chmod +x "$APP_DIR/Contents/MacOS/rdownloader"
else
    echo "Error: rdownloader executable not found in $TARGET_DIR or target/debug" >&2
    exit 1
fi

# Copy Info.plist
cp "$SCRIPT_DIR/Info.plist" "$APP_DIR/Contents/Info.plist"

# Copy Icon
if [ -f "$REPO_ROOT/packaging/icons/rdownloader.icns" ]; then
    cp "$REPO_ROOT/packaging/icons/rdownloader.icns" "$APP_DIR/Contents/Resources/rdownloader.icns"
else
    echo "Warning: rdownloader.icns not found, skipping icon"
fi

echo "APPL????" > "$APP_DIR/Contents/PkgInfo"

echo "Successfully created $APP_DIR"
