#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# OneAI Binding Generation Script
#
# Generates Kotlin, Swift, C++, and C# foreign-language bindings from
# the oneai-uniffi crate using the uniffi-bindgen tool.
#
# Usage:
#   ./scripts/generate_bindings.sh [language]
#   ./scripts/generate_bindings.sh          # Generate all languages
#   ./scripts/generate_bindings.sh kotlin   # Generate Kotlin only
#   ./scripts/generate_bindings.sh swift    # Generate Swift only
#   ./scripts/generate_bindings.sh cpp      # Generate C++ only
#   ./scripts/generate_bindings.sh csharp   # Generate C# only
#
# Prerequisites:
#   1. cargo install uniffi-bindgen
#   2. Build the oneai-uniffi cdylib:
#      cargo build --release -p oneai-uniffi
#
# Environment:
#   ONEAI_ROOT     — Project root directory (default: script location)
#   LIB_DIR        — Directory containing the compiled .dylib/.so/.dll
#                    (default: target/release)
# ──────────────────────────────────────────────────────────────────────

set -euo pipefail

# ─── Configuration ────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ONEAI_ROOT="${ONEAI_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}"
LIB_DIR="${LIB_DIR:-$ONEAI_ROOT/target/release}"
BINDINGS_DIR="$ONEAI_ROOT/bindings"
CRATE_NAME="oneai"

# Detect the compiled library file
detect_library() {
    local lib_file=""
    if [[ -f "$LIB_DIR/lib${CRATE_NAME}.dylib" ]]; then
        lib_file="$LIB_DIR/lib${CRATE_NAME}.dylib"
    elif [[ -f "$LIB_DIR/lib${CRATE_NAME}.so" ]]; then
        lib_file="$LIB_DIR/lib${CRATE_NAME}.so"
    elif [[ -f "$LIB_DIR/${CRATE_NAME}.dll" ]]; then
        lib_file="$LIB_DIR/${CRATE_NAME}.dll"
    else
        echo "ERROR: Compiled library not found in $LIB_DIR"
        echo "       Run: cargo build --release -p oneai-uniffi"
        exit 1
    fi
    echo "$lib_file"
}

# ─── Language Generators ──────────────────────────────────────────────
#
# UniFFI 0.32 library mode: `--library <lib>` reads the metadata embedded by
# the proc-macros directly from the compiled cdylib — no .udl file needed.
# (Plain `generate <path>` would treat <path> as a UDL file and fail.)

generate_kotlin() {
    local lib_file="$1"
    local out_dir="$BINDINGS_DIR/kotlin"
    echo "── Generating Kotlin bindings → $out_dir"
    uniffi-bindgen generate --library "$lib_file" --language kotlin --out-dir "$out_dir" --no-format
    echo "   ✓ Kotlin bindings generated"
}

generate_swift() {
    local lib_file="$1"
    local out_dir="$BINDINGS_DIR/swift"
    echo "── Generating Swift bindings → $out_dir"
    uniffi-bindgen generate --library "$lib_file" --language swift --out-dir "$out_dir"
    echo "   ✓ Swift bindings generated"
}

generate_cpp() {
    local lib_file="$1"
    local out_dir="$BINDINGS_DIR/cpp"
    echo "── Generating C++ bindings → $out_dir"
    uniffi-bindgen generate --library "$lib_file" --language cpp --out-dir "$out_dir"
    echo "   ✓ C++ bindings generated"
}

generate_csharp() {
    local lib_file="$1"
    local out_dir="$BINDINGS_DIR/csharp"
    echo "── Generating C# bindings → $out_dir"
    uniffi-bindgen generate --library "$lib_file" --language csharp --out-dir "$out_dir"
    echo "   ✓ C# bindings generated"
}

# ─── Build Library ────────────────────────────────────────────────────

build_library() {
    echo "── Building oneai-uniffi cdylib..."
    cargo build --release -p oneai-uniffi
    echo "   ✓ Library built"
}

# ─── Main ─────────────────────────────────────────────────────────────

main() {
    local language="${1:-all}"

    # Ensure uniffi-bindgen is available
    if ! command -v uniffi-bindgen &>/dev/null; then
        echo "ERROR: uniffi-bindgen not found"
        echo "       Install with: cargo install uniffi-bindgen"
        exit 1
    fi

    # Build if library not present
    local lib_file
    lib_file="$(detect_library)" || {
        build_library
        lib_file="$(detect_library)"
    }

    echo "── OneAI Binding Generation ───────────────"
    echo "   Library: $lib_file"
    echo "   Output:  $BINDINGS_DIR"
    echo ""

    case "$language" in
        kotlin)  generate_kotlin "$lib_file" ;;
        swift)   generate_swift "$lib_file" ;;
        cpp)     generate_cpp "$lib_file" ;;
        csharp)  generate_csharp "$lib_file" ;;
        all)
            generate_kotlin "$lib_file"
            generate_swift "$lib_file"
            generate_cpp "$lib_file"
            generate_csharp "$lib_file"
            ;;
        *)
            echo "ERROR: Unknown language '$language'"
            echo "       Supported: kotlin, swift, cpp, csharp, all"
            exit 1
            ;;
    esac

    echo ""
    echo "── Done! Bindings generated in $BINDINGS_DIR ──"
}

main "$@"