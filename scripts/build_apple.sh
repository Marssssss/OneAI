#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# OneAI Apple build script (macOS + iOS)
#
# Cross-compiles liboneai.a (staticlib) for Apple targets and stages the
# artifacts the native apps consume:
#   platforms/apple/lib/liboneai.a      — universal macOS staticlib (always)
#   platforms/apple/headers/            — oneaiFFI.h + module.modulemap (always)
#   platforms/apple/swift/oneai.swift   — raw UniFFI Swift binding (always)
#   platforms/apple/OneAI.xcframework   — iOS+macOS slices (only if Xcode present)
#
# The always-staged artifacts let the macOS app (platforms/macos) build with
# plain `swiftc` — NO Xcode required, only the Command Line Tools + the rust
# aarch64-apple-darwin/x86_64-apple-darwin targets. The xcframework (for the
# Xcode-based iOS app and an Xcode-built macOS app) is created only when
# `xcodebuild -version` succeeds; otherwise iOS/xcframework are skipped with
# a note (install Xcode from the Mac App Store to enable iOS).
#
# Usage:
#   ./scripts/build_apple.sh            # release
#   ./scripts/build_apple.sh --debug
#
# Prerequisites:
#   - rust apple targets:
#       rustup target add aarch64-apple-darwin x86_64-apple-darwin \
#                          aarch64-apple-ios aarch64-apple-ios-sim   # iOS needs Xcode
# ──────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ONEAI_ROOT="${ONEAI_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}"
APPLE_DIR="$ONEAI_ROOT/platforms/apple"
SWIFT_BINDINGS="$ONEAI_ROOT/bindings/swift"

PROFILE="release"
if [[ "${1:-}" == "--debug" ]]; then PROFILE="debug"; fi

MAC_TRIPLES=(aarch64-apple-darwin x86_64-apple-darwin)
IOS_DEVICE_TRIPLE=aarch64-apple-ios
IOS_SIM_TRIPLES=(aarch64-apple-ios-sim)

# Does a real Xcode exist? (CLT alone cannot build iOS or xcframeworks.)
have_xcode() { xcodebuild -version >/dev/null 2>&1; }

build_target() {
    local triple="$1"
    echo "── Building liboneai.a for $triple [$PROFILE]"
    cargo build "--$PROFILE" -p oneai-uniffi --target "$triple"
}

stage_a() {
    local triple="$1" name="$2"
    local src="$ONEAI_ROOT/target/$triple/$PROFILE/liboneai.a"
    [[ -f "$src" ]] || { echo "ERROR: $src not built"; exit 1; }
    mkdir -p "$APPLE_DIR/build/$name"
    cp "$src" "$APPLE_DIR/build/$name/liboneai.a"
}

# ─── macOS: build + lipo universal ────────────────────────────────────
for t in "${MAC_TRIPLES[@]}"; do build_target "$t"; stage_a "$t" "macos-$t"; done
echo "── lipo → universal macOS liboneai.a"
mkdir -p "$APPLE_DIR/lib"
lipo -create \
    "$APPLE_DIR/build/macos-${MAC_TRIPLES[0]}/liboneai.a" \
    "$APPLE_DIR/build/macos-${MAC_TRIPLES[1]}/liboneai.a" \
    -output "$APPLE_DIR/lib/liboneai.a"

# ─── Stage headers + raw Swift binding (the link surface for swiftc/Xcode) ──
# The binding + FFI header live in bindings/swift/ (committed source, like the
# Kotlin binding). We only stage a *renamed* modulemap (module.modulemap, from
# oneaiFFI.modulemap) into a scratch headers dir — clang looks for
# module.modulemap to resolve `import oneaiFFI`.
mkdir -p "$APPLE_DIR/headers"
cp "$SWIFT_BINDINGS/oneaiFFI.h"         "$APPLE_DIR/headers/oneaiFFI.h"
cp "$SWIFT_BINDINGS/oneaiFFI.modulemap" "$APPLE_DIR/headers/module.modulemap"

# ─── iOS + xcframework (only with Xcode) ──────────────────────────────
if have_xcode; then
    build_target "$IOS_DEVICE_TRIPLE"; stage_a "$IOS_DEVICE_TRIPLE" "ios"
    for t in "${IOS_SIM_TRIPLES[@]}"; do build_target "$t"; done
    if [[ ${#IOS_SIM_TRIPLES[@]} -eq 1 ]]; then
        stage_a "${IOS_SIM_TRIPLES[0]}" "ios-sim"
    else
        mkdir -p "$APPLE_DIR/build/ios-sim"
        lipo -create $(printf "%s/liboneai.a\n" "${IOS_SIM_TRIPLES[@]/#/$APPLE_DIR/build/ios-sim-}") \
            -output "$APPLE_DIR/build/ios-sim/liboneai.a"
    fi
    echo "── Creating $APPLE_DIR/OneAI.xcframework"
    rm -rf "$APPLE_DIR/OneAI.xcframework"
    xcodebuild -create-xcframework \
        -library "$APPLE_DIR/build/macos/liboneai.a"   -headers "$APPLE_DIR/headers" \
        -library "$APPLE_DIR/build/ios/liboneai.a"     -headers "$APPLE_DIR/headers" \
        -library "$APPLE_DIR/build/ios-sim/liboneai.a" -headers "$APPLE_DIR/headers" \
        -output "$APPLE_DIR/OneAI.xcframework"
    echo "   ✓ xcframework created (iOS + macOS slices)"
else
    echo "── Xcode not found (only Command Line Tools) — skipping iOS + xcframework."
    echo "   macOS artifacts staged. Install Xcode to enable the iOS app."
fi

# Keep the build/ scratch dir out of the way; the committed surface is lib/headers/swift.
rm -rf "$APPLE_DIR/build"

echo ""
echo "── Done. macOS artifacts in $APPLE_DIR/{lib,headers}"
echo "   Next: ./platforms/macos/build_macos.sh   # builds OneAI.app via swiftc"
if have_xcode; then
    echo "   iOS:  open platforms/ios (xcodegen + xcodebuild) — uses OneAI.xcframework"
fi
