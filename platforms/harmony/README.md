# OneAI HarmonyOS app (ArkTS / ArkUI + NAPI)

Native HarmonyOS chat app — the HarmonyOS port of `platforms/android` /
`platforms/macos` / `platforms/windows`. Consumes the Rust `oneai-uniffi`
core through the **same `extern "C"` JSON facade** used by Windows
(`crates/oneai-uniffi/src/c_facade.rs`, header `bindings/c/oneai_c.h`), wrapped
in a NAPI (Node-API) C++ module so ArkTS can call it.

## Why NAPI + C facade

`uniffi-bindgen` 0.32 has no ArkTS/Node generator (kotlin/swift/python/ruby
only). So we reuse the hand-rolled C facade (the deterministic route, verified
by `cargo test -p oneai-uniffi c_facade` on macOS) and wrap it with NAPI.
ArkTS imports `liboneai_napi.so` and calls typed functions; the streaming
callback round-trips via `napi_threadsafe_function` (fires on a tokio worker
thread, dispatched on the ArkTS thread).

## Build (on a machine with DevEco Studio + the HarmonyOS Native SDK)

```bash
# 1. Cross-compile liboneai.so for the OHOS ABIs + stage it where CMake finds it.
export OHOS_NDK_HOME=/path/to/harmony/native   # contains llvm/bin/clang
./scripts/build_harmony.sh
# (requires: rustup target add aarch64-linux-ohos x86_64-linux-ohos)

# 2. Open platforms/harmony in DevEco Studio (or `hvigorw assembleHap`).
#    DevEco's CMake builds liboneai_napi.so (entry/src/main/cpp/napi_init.cpp)
#    and links liboneai.so (staged at entry/src/main/cpp/libs/<abi>/).

# 3. Run on an emulator or signed device.
```

## Architecture

```
entry/src/main/
  cpp/
    napi_init.cpp                      # NAPI module: wraps oneai_* C symbols; async run_task
                                       #   via napi_async_work + threadsafe_function
    CMakeLists.txt                     # links liboneai.so + ohos NDK hilog
    libs/<abi>/liboneai.so             # staged by build_harmony.sh (gitignored)
    types/liboneai_napi/Index.d.ts     # ArkTS type declarations for the import
  ets/
    pages/ChatPage.ets                 # @Entry: top bar + message List + input + drawer + settings overlays
    viewmodel/ChatViewModel.ets        # holds handles + provider (preferences) + drives NAPI + applyEvent
    viewmodel/ChatModels.ets           # ChatEntry / AssistantItem / ToolStep
    utils/OneAiNative.ets              # import 'liboneai_napi.so' + JSON parse helpers
    entryability/EntryAbility.ets      # loads pages/ChatPage
  module.json5 / resources/base/profile/main_pages.json
build-profile.json5 / oh-package.json5 (project + entry) / hvigorfile.ts
```

## Feature parity (with Android S5 / macOS / Windows)

Drawer session list (new/switch/delete), streaming chat (onEvent fires on the
ArkTS thread via threadsafe_function → mutates @State directly), thinking card
(collapsible, "思考中…" → "已深度思考"), tool steps (`✓/✗/⚙ name(args)` +
truncated result), plain-text + fenced-code markdown (v1), blinking cursor,
retry-on-error, dark theme (follows system via ArkUI color mode), first-run
hint, stop button → `sessionInterrupt`. Provider settings persisted in
`@ohos.data.preferences`; SQLite db at `context.filesDir/oneai.db`. Save
rebuilds the app (history kept).

## Caveats — this is unverified on this Mac

- This machine has no DevEco Studio / OHOS NDK, so **none of this builds here**.
  The Rust foundation it depends on IS verified: the C facade's 4 unit tests
  pass and its 14 `oneai_*` symbols export from the cdylib (see `a1848f8`).
  Build/iterate on your DevEco box.
- Likely adjustment spots: DevEco version-specific `build-profile.json5` /
  `oh-package.json5` schema, ArkUI V1/V2 decorator choices in `ChatPage.ets`,
  the `OHOS_ARCH_ABI` CMake variable name (set by the NDK toolchain — verify
  in DevEco's CMake output), and the threadsafe-function lifecycle.
- Markdown is plain-text + fenced code only (no inline bold/code Runs) — v1;
  extend `AssistantBubble` if you want parity with the macOS/Windows inline
  rendering.
