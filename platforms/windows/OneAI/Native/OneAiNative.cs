// OneAI native interop — P/Invoke into oneai_native.dll, the collapsed
// 3-symbol bus pump (crates/oneai-uniffi/src/c_facade.rs). Renamed from the
// crate's oneai.dll to avoid an NTFS case-insensitive collision with the
// managed assembly OneAI.dll in the output dir.
//
// The ENTIRE surface is three symbols:
//   int32_t     oneai_submit_directive(const char* json);
//   const char* oneai_poll_yield(void);   // null = none; valid until next poll
//   int32_t     oneai_shutdown(void);
//
// Everything else rides JSON Directive (inbound) / EngineYield (outbound) —
// the oneai-bus protocol, serde tag "kind", snake_case (see
// crates/oneai-bus/src/protocol.rs). Strings cross as UTF-8 so CJK
// (thinking text, answers) round-trips correctly.
//
// Ownership: oneai_poll_yield returns a pointer into a THREAD-LOCAL buffer —
// valid only until the next oneai_poll_yield on the SAME thread. Copy it
// immediately (YieldPtrToString does) and NEVER free it; there is no
// oneai_free_string. Poll from exactly one thread (see BusPump).

using System;
using System.Runtime.InteropServices;

namespace OneAI.Native;

internal static class OneAiNative
{
    private const string Dll = "oneai_native";

    // Submit one Directive as a NUL-terminated UTF-8 JSON line. The
    // marshaller pins/copies the string and frees its own buffer on return —
    // no manual cleanup needed on the input side.
    [DllImport(Dll, EntryPoint = "oneai_submit_directive", CallingConvention = CallingConvention.Cdecl)]
    public static extern int SubmitDirective([MarshalAs(UnmanagedType.LPUTF8Str)] string json);

    // Poll the next EngineYield as one NUL-terminated UTF-8 JSON line, or
    // IntPtr.Zero when none is pending. The pointer aliases a thread-local
    // buffer — copy it right away (YieldPtrToString) and never free it.
    [DllImport(Dll, EntryPoint = "oneai_poll_yield", CallingConvention = CallingConvention.Cdecl)]
    public static extern IntPtr PollYield();

    // Shut the engine down (Directive::Shutdown + abort pump + drop state).
    [DllImport(Dll, EntryPoint = "oneai_shutdown", CallingConvention = CallingConvention.Cdecl)]
    public static extern int Shutdown();

    // ── oneai_submit_directive return codes (c_facade.rs) ─────────────
    public const int Ok = 0;
    public const int NullInput = 1;
    public const int BadJson = 2;
    public const int AlreadyBuilt = 3;
    public const int BuildFailed = 4;
    public const int NotInitialized = 5;
    public const int BusSubmitFailed = 6;
    public const int PanicCaught = 7;

    /// <summary>Copy the thread-local yield buffer into a managed string.
    /// Returns null for a null pointer (no yield pending). Does NOT free the
    /// native buffer — it is owned by the facade and reused on the next
    /// poll of the same thread.</summary>
    public static string? YieldPtrToString(IntPtr p) =>
        p == IntPtr.Zero ? null : Marshal.PtrToStringUTF8(p);

    /// <summary>Human-readable label for a oneai_submit_directive code.</summary>
    public static string SubmitCodeMessage(int code) => code switch
    {
        Ok => "ok",
        NullInput => "null/invalid input",
        BadJson => "bad JSON",
        AlreadyBuilt => "engine already built (shutdown first)",
        BuildFailed => "engine build failed",
        NotInitialized => "engine not initialized (submit init first)",
        BusSubmitFailed => "bus submit failed",
        PanicCaught => "internal panic caught at the FFI boundary",
        _ => $"unknown code {code}",
    };
}
