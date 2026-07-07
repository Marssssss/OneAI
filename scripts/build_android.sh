#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# OneAI Android build script
#
# Cross-compiles liboneai.so for 4 Android ABIs via cargo-ndk, then stages
# the .so files + the UniFFI-generated Kotlin bindings into the Gradle
# project under platforms/android so `./gradlew assembleDebug` can package
# them into the APK.
#
# Usage:
#   ./scripts/build_android.sh           # build all ABIs (release)
#   ./scripts/build_android.sh --debug   # debug build
#
# Prerequisites:
#   - Android NDK (brew install --cask android-ndk, or Android Studio SDK Manager)
#   - cargo-ndk            (cargo install cargo-ndk)
#   - rust android targets (rustup target add aarch64-linux-android ...)
# ──────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ONEAI_ROOT="${ONEAI_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}"
ANDROID_DIR="$ONEAI_ROOT/platforms/android"
JNILIBS="$ANDROID_DIR/app/src/main/jniLibs"
KT_BINDINGS_SRC="$ONEAI_ROOT/bindings/kotlin/uniffi/oneai/oneai.kt"
KT_BINDINGS_DST="$ANDROID_DIR/app/src/main/kotlin/uniffi/oneai/oneai.kt"

PROFILE="release"
if [[ "${1:-}" == "--debug" ]]; then
    PROFILE="debug"
fi

# ─── Locate the NDK ───────────────────────────────────────────────────
NDK_DIR=""
for candidate in \
    "${NDK_HOME:-}" \
    "${ANDROID_NDK_HOME:-}" \
    "/opt/homebrew/share/android-ndk" \
    "/usr/local/share/android-ndk" \
    "$(ls -d "$HOME/Library/Android/sdk/ndk"/* 2>/dev/null | tail -1)"; do
    # Strip whitespace from command-substitution output.
    candidate="$(echo "$candidate" | xargs)"
    if [[ -n "$candidate" && -d "$candidate/toolchains/llvm" ]]; then
        NDK_DIR="$candidate"
        break
    fi
done

if [[ -z "$NDK_DIR" ]]; then
    echo "ERROR: Android NDK not found."
    echo "       Set NDK_HOME, or install via: brew install --cask android-ndk"
    exit 1
fi
export NDK_HOME="$NDK_DIR"
export ANDROID_NDK_HOME="$NDK_DIR"
echo "── NDK: $NDK_DIR"

# ─── ABI map: rust triple → android ABI dir ───────────────────────────
ABIS=(
    "aarch64-linux-android:arm64-v8a"
    "armv7-linux-androideabi:armeabi-v7a"
    "x86_64-linux-android:x86_64"
    "i686-linux-android:x86"
)

# ─── Cross-compile each ABI ───────────────────────────────────────────
for entry in "${ABIS[@]}"; do
    triple="${entry%%:*}"
    abi="${entry##*:}"
    echo "── Building liboneai.so for $abi ($triple) [$PROFILE]"
    cargo ndk -t "$triple" build "--$PROFILE" -p oneai-uniffi
    mkdir -p "$JNILIBS/$abi"
    cp "$ONEAI_ROOT/target/$triple/$PROFILE/liboneai.so" "$JNILIBS/$abi/liboneai.so"
done

# ─── Stage generated Kotlin bindings into the Gradle source set ──────
echo "── Staging Kotlin bindings → $KT_BINDINGS_DST"
mkdir -p "$(dirname "$KT_BINDINGS_DST")"
cp "$KT_BINDINGS_SRC" "$KT_BINDINGS_DST"

# ─── Ensure local.properties points at the Android SDK ───────────────
# (machine-specific, gitignored — generated so ./gradlew works out of the box)
ANDROID_SDK_DIR="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
if [[ -d "$ANDROID_SDK_DIR" ]]; then
    echo "sdk.dir=$ANDROID_SDK_DIR" > "$ANDROID_DIR/local.properties"
fi

echo ""
echo "── Done. .so staged in $JNILIBS, bindings in $KT_BINDINGS_DST"
echo "   Next: cd $ANDROID_DIR && ./gradlew assembleDebug"
