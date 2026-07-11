#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# OneAI HarmonyOS build script
#
# Cross-compiles liboneai.so (cdylib) for HarmonyOS (OpenHarmony) ABIs via the
# OHOS NDK clang, and stages it into the DevEco project's native libs dir for
# the NAPI module (platforms/harmony/entry/src/main/cpp/libs/<abi>/) to link.
#
# Run on a machine with DevEco Studio's Native SDK installed:
#   rustup target add aarch64-linux-ohos x86_64-linux-ohos
#   export OHOS_NDK_HOME=/path/to/harmony/.../native     # contains llvm/bin
#   ./scripts/build_harmony.sh
#
# NOTE: uniffi-bindgen 0.32 has NO C++ generator. The HarmonyOS bridge is a
# hand-written NAPI (Node-API) C++ module that either (a) drives the C FFI
# header bindings/swift/oneaiFFI.h directly, or (b) calls a small `extern "C"`
# JSON facade added to the Rust side. See bindings/cpp/README.md. This script
# only builds the liboneai.so that the NAPI module links against.
# ──────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ONEAI_ROOT="${ONEAI_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}"
HARMONY_DIR="$ONEAI_ROOT/platforms/harmony"
CPP_LIBS="$HARMONY_DIR/entry/src/main/cpp/libs"

# OHOS NDK — DevEco ships a clang under <native>/llvm/bin.
OHOS_NDK_HOME="${OHOS_NDK_HOME:-}"
if [[ -z "$OHOS_NDK_HOME" || ! -x "$OHOS_NDK_HOME/llvm/bin/clang" ]]; then
    echo "ERROR: set OHOS_NDK_HOME to DevEco's native dir (contains llvm/bin/clang)."
    echo "       e.g. export OHOS_NDK_HOME=\"\$HOME/Library/Huawei/Sdk/.../native\""
    exit 1
fi
CLANG="$OHOS_NDK_HOME/llvm/bin/clang"

# ABIs: rust triple -> harmony ABI dir. HarmonyOS 6.0.1 supports arm64-v8a
# (device) + x86_64 (emulator) only — armeabi-v7a is rejected.
ABIS=(
  "aarch64-linux-ohos:arm64-v8a"
  "x86_64-linux-ohos:x86_64"
)

for entry in "${ABIS[@]}"; do
    triple="${entry%%:*}"
    abi="${entry##*:}"
    echo "── Building liboneai.a for $abi ($triple)"
    # oneai-staticlib (crate-type=staticlib) → liboneai.a, linked statically into
    # liboneai_napi.so by CMakeLists. (Build only on demand; debug builds don't
    # emit this fat archive.) OHOS clang as the cargo linker.
    export "CARGO_TARGET_$(echo "$triple" | tr 'a-z-' 'A-Z_')_LINKER"="$CLANG"
    export "CC_$triple"="$CLANG"
    export "CXX_$triple"="${CLANG}++"
    export "CARGO_TARGET_$(echo "$triple" | tr 'a-z-' 'A-Z_')_RUSTFLAGS"="-C link-arg=--target=$triple -C link-arg=--sysroot=$OHOS_NDK_HOME/sysroot"
    cargo build --release -p oneai-staticlib --target "$triple"
    mkdir -p "$CPP_LIBS/$abi"
    cp "$ONEAI_ROOT/target/$triple/release/liboneai.a" "$CPP_LIBS/$abi/liboneai.a"
done

echo ""
echo "── Done. liboneai.a staged under $CPP_LIBS"
echo "   Next: in DevEco Studio, build platforms/harmony (hvigorw assembleHap)"
