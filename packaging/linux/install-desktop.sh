#!/usr/bin/env bash
# packaging/linux/install-desktop.sh
# Installs desktop file and icons into standard XDG locations.
set -euo pipefail

PREFIX="${1:-$HOME/.local}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

mkdir -p "$PREFIX/share/applications"
mkdir -p "$PREFIX/share/icons/hicolor/256x256/apps"
mkdir -p "$PREFIX/share/icons/hicolor/512x512/apps"

cp "$SCRIPT_DIR/rdownloader.desktop" "$PREFIX/share/applications/"
if [ -f "$REPO_ROOT/packaging/icons/rdownloader-256.png" ]; then
    cp "$REPO_ROOT/packaging/icons/rdownloader-256.png" "$PREFIX/share/icons/hicolor/256x256/apps/rdownloader.png"
fi
if [ -f "$REPO_ROOT/packaging/icons/rdownloader.png" ]; then
    cp "$REPO_ROOT/packaging/icons/rdownloader.png" "$PREFIX/share/icons/hicolor/512x512/apps/rdownloader.png"
fi

echo "Installed desktop file and icons to $PREFIX/share"
