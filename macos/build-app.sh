#!/bin/zsh
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
APP_DIR="${MACEVERY_APP_DIR:-$ROOT_DIR/.build/MacEvery.app}"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
MODULE_CACHE="$ROOT_DIR/.build/macevery-swift-module-cache"
CARGO_OUTPUT="${CARGO_TARGET_DIR:-$ROOT_DIR/target}"
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

cp "$CARGO_OUTPUT/release/macevery" "$MACOS_DIR/macevery"
cp "$ROOT_DIR/macos/Info.plist" "$CONTENTS_DIR/Info.plist"

# Development installs keep the release version and identify their exact source.
SOURCE_COMMIT="$(git rev-parse HEAD)"
SOURCE_SHORT="$(git rev-parse --short HEAD)"
SOURCE_DIRTY="$(git status --porcelain --untracked-files=normal)"
BUILD_TIME="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
APP_VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$CONTENTS_DIR/Info.plist")"
BUILD_LABEL="$APP_VERSION-g$SOURCE_SHORT"
if [[ -n "$SOURCE_DIRTY" ]]; then
  BUILD_LABEL="$BUILD_LABEL-dirty"
fi
/usr/libexec/PlistBuddy -c "Add :MacEveryGitCommit string $SOURCE_COMMIT" "$CONTENTS_DIR/Info.plist"
/usr/libexec/PlistBuddy -c "Add :MacEveryBuildTime string $BUILD_TIME" "$CONTENTS_DIR/Info.plist"
/usr/libexec/PlistBuddy -c "Add :MacEveryBuildLabel string $BUILD_LABEL" "$CONTENTS_DIR/Info.plist"

chmod +x "$MACOS_DIR/MacEveryApp" "$MACOS_DIR/macevery"

if command -v codesign >/dev/null 2>&1; then
  if [[ -n "${MACEVERY_CODESIGN_IDENTITY:-}" ]]; then
    codesign \
      --force \
      --options runtime \
      --timestamp \
      --entitlements "$ROOT_DIR/macos/Entitlements.plist" \
      --sign "$MACEVERY_CODESIGN_IDENTITY" \
      "$MACOS_DIR/macevery"
    codesign \
      --force \
      --options runtime \
      --timestamp \
      --entitlements "$ROOT_DIR/macos/Entitlements.plist" \
      --sign "$MACEVERY_CODESIGN_IDENTITY" \
      "$MACOS_DIR/MacEveryApp"
    codesign \
      --force \
      --options runtime \
      --timestamp \
      --entitlements "$ROOT_DIR/macos/Entitlements.plist" \
      --sign "$MACEVERY_CODESIGN_IDENTITY" \
      "$APP_DIR"
    codesign --verify --deep --strict --verbose=2 "$APP_DIR"
  else
    codesign --force --deep --sign - "$APP_DIR" >/dev/null 2>&1 || true
  fi
fi

echo "$APP_DIR"
