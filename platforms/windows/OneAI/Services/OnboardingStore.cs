// First-run onboarding persistence (issue #33) — a one-way "dismissed" flag
// stored as JSON under %LOCALAPPDATA%\OneAI, mirroring ProviderStore. Once the
// user dismisses the "添加一个 API Key 开始使用" banner (× or by opening
// settings), it never shows again so it doesn't nag on every launch.

using System;
using System.IO;
using System.Text.Json;

namespace OneAI.Services;

public static class OnboardingStore
{
    private sealed class State
    {
        public bool Dismissed { get; set; }
    }

    public static bool LoadDismissed()
    {
        try
        {
            var json = File.ReadAllText(AppPaths.OnboardingStatePath);
            var s = JsonSerializer.Deserialize<State>(json);
            if (s is { }) return s.Dismissed;
        }
        catch { /* first run / corrupt — fall through to default */ }
        return false;
    }

    public static void SaveDismissed(bool dismissed)
    {
        try
        {
            File.WriteAllText(
                AppPaths.OnboardingStatePath,
                JsonSerializer.Serialize(new State { Dismissed = dismissed }));
        }
        catch { /* best-effort */ }
    }
}
