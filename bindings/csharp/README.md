# C# bindings — status

`uniffi-bindgen` 0.32 (the version OneAI uses) does **not** ship a C#
generator — `--language` accepts only `kotlin, swift, python, ruby`.

`legacy/OneAI.cs` is a hand-written SDK wrapper from an earlier, now-removed
API surface (it references `AutoApprovalGate`, `OneAITools.Calculator()`,
`MemoryManagerWithConfig(windowSize, thresholdTokens)`). It does **not**
compile against the current Rust API and is kept only for historical reference.

## Path for Windows (Phase 3)

Two viable routes to call `oneai.dll` from C#:

1. **Third-party C# generator.** `uniffi-bindgen-cs` (community crate,
   https://github.com/NordSecurity/uniffi-bindgen-cs) generates raw C# bindings
   from the same uniffi metadata the Kotlin/Swift generators use. If it builds
   cleanly against the OneAI cdylib, this is the least-effort route and
   produces a maintained, idiomatic C# API (like `bindings/kotlin`/`swift`).

2. **Hand-rolled C facade + P/Invoke.** Add a small `extern "C"` JSON facade in
   Rust (same approach as the HarmonyOS route in `bindings/cpp/README.md`) and
   P/Invoke it from C# with `[DllImport("oneai")]`. Only JSON strings cross the
   boundary; no uniffi protocol to reimplement.

Route 1 is preferred if `uniffi-bindgen-cs` cooperates; route 2 is the
deterministic fallback. Decided when Phase 3 starts.
