#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# OneAI macOS app build script
#
# Compiles the SwiftUI macOS app (platforms/macos/Sources/*.swift) together
# with the raw UniFFI Swift binding (platforms/apple/swift/oneai.swift) and
# links the universal macOS staticlib (platforms/apple/lib/liboneai.a) — NO
# Xcode required, only the Command Line Tools + the artifacts staged by
# scripts/build_apple.sh. Produces a runnable OneAI.app bundle.
#
# Usage:
#   ./scripts/build_apple.sh            # stage liboneai.a + headers + binding first
#   ./platforms/macos/build_macos.sh    # then this
#   ./platforms/macos/build_macos.sh --debug
# ──────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ONEAI_ROOT="${ONEAI_ROOT:-$(cd "$SCRIPT_DIR/../.." && pwd)}"
MACOS_DIR="$ONEAI_ROOT/platforms/macos"
APPLE_DIR="$ONEAI_ROOT/platforms/apple"

PROFILE="release"
if [[ "${1:-}" == "--debug" ]]; then PROFILE="debug"; fi

SWIFT_OPTS=(-O)
if [[ "$PROFILE" == "debug" ]]; then SWIFT_OPTS=(-Onone -g); fi

A="$APPLE_DIR/lib/liboneai.a"
[[ -f "$A" ]] || { echo "ERROR: $A missing — run ./scripts/build_apple.sh first"; exit 1; }
BINDING="$ONEAI_ROOT/bindings/swift/OneAI.swift"
[[ -f "$BINDING" ]] || { echo "ERROR: binding missing — run ./scripts/generate_bindings.sh swift"; exit 1; }

SOURCES=("$BINDING")
while IFS= read -r f; do SOURCES+=("$f"); done < <(find "$MACOS_DIR/Sources" -name '*.swift' | sort)

BUILD_DIR="$MACOS_DIR/build"
rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR/OneAI.app/Contents/MacOS" "$BUILD_DIR/headers"

# Stage a headers dir with module.modulemap (renamed from oneaiFFI.modulemap)
# so `import oneaiFFI` resolves. Source: bindings/swift (committed).
cp "$ONEAI_ROOT/bindings/swift/oneaiFFI.h"         "$BUILD_DIR/headers/oneaiFFI.h"
cp "$ONEAI_ROOT/bindings/swift/oneaiFFI.modulemap" "$BUILD_DIR/headers/module.modulemap"

APP_EXE="$BUILD_DIR/OneAI.app/Contents/MacOS/OneAI"
mkdir -p "$(dirname "$APP_EXE")"
SDK="$(xcrun --show-sdk-path --sdk macosx)"

# Universal (arm64 + x86_64) executable. liboneai.a is already universal
# (staged by scripts/build_apple.sh), so we compile Swift per-arch and
# `lipo -create` a fat binary. Either arch failing is non-fatal — the other
# slice still ships a working app.
MAC_ARCHS=(arm64 x86_64)
SLICES=()
for arch in "${MAC_ARCHS[@]}"; do
  echo "── Compiling ${#SOURCES[@]} Swift sources [$PROFILE / $arch]"
  if swiftc \
        -target "${arch}-apple-macos13" \
        "${SWIFT_OPTS[@]}" \
        -sdk "$SDK" \
        -I "$BUILD_DIR/headers" \
        -L "$APPLE_DIR/lib" -loneai \
        -lz -lresolv -lc++ \
        -framework AppKit -framework SwiftUI -framework Foundation \
        -framework SystemConfiguration -framework CoreFoundation -framework Security -framework CFNetwork \
        -framework Speech -framework AVFoundation \
        -module-name OneAI \
        -emit-executable \
        "${SOURCES[@]}" \
        -o "$BUILD_DIR/OneAI.$arch" >"$BUILD_DIR/swiftc-$arch.log" 2>&1; then
    SLICES+=("$BUILD_DIR/OneAI.$arch")
  else
    echo "  ⚠ $arch slice failed (see $BUILD_DIR/swiftc-$arch.log):"
    tail -40 "$BUILD_DIR/swiftc-$arch.log"
  fi
done

if [[ ${#SLICES[@]} -eq 0 ]]; then
  echo "ERROR: no architecture compiled successfully" >&2
  exit 1
fi
if [[ ${#SLICES[@]} -eq 1 ]]; then
  cp "${SLICES[0]}" "$APP_EXE"
else
  lipo -create "${SLICES[@]}" -output "$APP_EXE"
fi
rm -f "$BUILD_DIR/OneAI."*

cp "$MACOS_DIR/Info.plist" "$BUILD_DIR/OneAI.app/Contents/Info.plist"

# App icon (.icns) — staged at platforms/macos/Resources/OneAI.icns by the
# icon-build step (iconutil from OneAI_icon.png). Falls back to the generic
# macOS icon if absent so the build never hard-fails on a missing asset.
mkdir -p "$BUILD_DIR/OneAI.app/Contents/Resources"
if [[ -f "$MACOS_DIR/Resources/OneAI.icns" ]]; then
  cp "$MACOS_DIR/Resources/OneAI.icns" "$BUILD_DIR/OneAI.app/Contents/Resources/OneAI.icns"
else
  echo "  (no OneAI.icns — icon falls back to default; generate via iconutil)"
fi

# — Distribution zip (unsigned) —————————————————————————————
# The app is NOT code-signed or notarized. Users must right-click → Open on
# first launch to bypass Gatekeeper's "unidentified developer" quarantine.
APP_VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$MACOS_DIR/Info.plist" 2>/dev/null || echo "1.0.0")"
ZIP_NAME="OneAI-${APP_VERSION}-macos.zip"

echo ""
echo "── Packaging $ZIP_NAME (unsigned — right-click → Open on first launch)"
( cd "$BUILD_DIR" && rm -f "$ZIP_NAME" && zip -rSYq "$ZIP_NAME" OneAI.app -x '*.DS_Store' )

echo ""
echo "── Built $BUILD_DIR/OneAI.app  (+ $BUILD_DIR/$ZIP_NAME)"
echo "   Run:     open $BUILD_DIR/OneAI.app"
echo "   Distrib: $BUILD_DIR/$ZIP_NAME  (unsigned; Gatekeeper: right-click → Open)"
echo "   Slices:  $(lipo -archs "$APP_EXE" 2>/dev/null || echo '?')"
