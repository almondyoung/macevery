#!/bin/zsh
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
APP_DIR="$ROOT_DIR/.build/MacEvery.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
MODULE_CACHE="$ROOT_DIR/.build/macevery-swift-module-cache"
ARCH="${MACEVERY_APP_ARCH:-$(uname -m)}"
TARGET="${MACEVERY_SWIFT_TARGET:-${ARCH}-apple-macos13.0}"

cd "$ROOT_DIR"
cargo build --release

mkdir -p "$MACOS_DIR" "$RESOURCES_DIR" "$MODULE_CACHE"

swift \
  -module-cache-path "$MODULE_CACHE" \
  "$ROOT_DIR/macos/generate-icon.swift" \
  "$RESOURCES_DIR/MacEveryIcon.icns"

swiftc \
  -O \
  -parse-as-library \
  -module-cache-path "$MODULE_CACHE" \
  -target "$TARGET" \
  -o "$MACOS_DIR/MacEveryApp" \
  "$ROOT_DIR/macos/MacEveryApp.swift"

cp "$ROOT_DIR/target/release/macevery" "$MACOS_DIR/macevery"
cp "$ROOT_DIR/macos/Info.plist" "$CONTENTS_DIR/Info.plist"

chmod +x "$MACOS_DIR/MacEveryApp" "$MACOS_DIR/macevery"

if command -v codesign >/dev/null 2>&1; then
  codesign --force --deep --sign - "$APP_DIR" >/dev/null 2>&1 || true
fi

echo "$APP_DIR"
