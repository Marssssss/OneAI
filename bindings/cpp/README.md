# C++ bindings — status

`uniffi-bindgen` 0.32 (the version OneAI uses) does **not** ship a C++
generator — `--language` accepts only `kotlin, swift, python, ruby`.

`legacy/oneai_sdk.hpp` is a hand-written SDK wrapper from an earlier,
now-removed API surface (it references `auto_approval_gate`,
`OneAITools::calculator()`, `memory_manager_with_config(window, threshold)`).
It does **not** compile against the current Rust API and is kept only for
historical reference.

## Path for HarmonyOS (Phase 4)

HarmonyOS native modules are NAPI (Node-API, a C/C++ ABI). Two viable routes:

1. **Consume the C FFI header directly.** `bindings/swift/oneaiFFI.h` is a
   plain C header exposing the `ffi_oneai_*` symbols + `RustBuffer` protocol.
   A hand-written NAPI C++ module can `#include "oneaiFFI.h"`, link
   `liboneai.so`, and drive the C ABI. Downside: the full uniffi wire protocol
   (RustBuffer (de)serialization, the foreign-callback vtable for
   `ChatEventCallback`, async runtime) must be reimplemented in C++ — large and
   error-prone.

2. **Expose a thin hand-rolled C API from Rust.** Add a small
   `oneai-platform-harmony` (or `oneai-uniffi`) `extern "C"` facade with
   JSON-in/JSON-out functions (`oneai_create_app`, `oneai_run_task`,
   `oneai_interrupt`, ...) and a single `extern "C" fn(const char*)` event
   callback. The NAPI module only marshals JSON strings — no uniffi protocol to
   reimplement. This is the recommended route; it mirrors the existing
   `crates/oneai-platform-{ios,harmony}/src/callback_bridge.rs` C-callback
   pattern already in the repo.

Route 2 will be implemented when Phase 4 starts.
