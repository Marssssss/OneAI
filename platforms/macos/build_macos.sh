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

echo "── Compiling ${#SOURCES[@]} Swift sources [$PROFILE]"
swiftc \
  -target arm64-apple-macos13 \
  "${SWIFT_OPTS[@]}" \
  -sdk "$(xcrun --show-sdk-path --sdk macosx)" \
  -I "$BUILD_DIR/headers" \
  -L "$APPLE_DIR/lib" -loneai \
  -lz -lresolv -lc++ \
  -framework AppKit -framework SwiftUI -framework Foundation \
  -framework SystemConfiguration -framework CoreFoundation -framework Security -framework CFNetwork \
  -framework Speech -framework AVFoundation \
  -module-name OneAI \
  -emit-executable \
  "${SOURCES[@]}" \
  -o "$BUILD_DIR/OneAI.app/Contents/MacOS/OneAI" 2>&1 | tail -60

cp "$MACOS_DIR/Info.plist" "$BUILD_DIR/OneAI.app/Contents/Info.plist"

echo ""
echo "── Built $BUILD_DIR/OneAI.app"
echo "   Run: open $BUILD_DIR/OneAI.app"
