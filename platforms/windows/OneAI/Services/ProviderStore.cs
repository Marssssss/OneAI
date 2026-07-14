// Provider config persistence — a JSON file under %LOCALAPPDATA%\OneAI
// (the counterpart of Android SharedPreferences / macOS UserDefaults).
//
// The app is unpackaged, so `Windows.Storage.ApplicationData.Current` is
// unavailable (no package identity). Use a plain JSON file instead.

using System;
using System.IO;
using System.Text.Json;
using OneAI.Native;

namespace OneAI.Services;

public static class ProviderStore
{
    public static ProviderConfig Load()
    {
        try
        {
            var json = File.ReadAllText(AppPaths.ProviderConfigPath);
            var c = JsonSerializer.Deserialize<ProviderConfig>(json);
            if (c is { }) return c;
        }
        catch { /* first run / corrupt — fall through to defaults */ }
        return new ProviderConfig();
    }

    public static void Save(ProviderConfig c)
    {
        try { File.WriteAllText(AppPaths.ProviderConfigPath, JsonSerializer.Serialize(c)); }
        catch { /* best-effort */ }
    }

    public static void ApplyPreset(ProviderConfig c, string newKind)
    {
        if (newKind == c.Kind) return;
        c.Kind = newKind;
        switch (newKind)
        {
            case "openai":    c.Model = "gpt-4o-mini"; c.BaseUrl = ""; break;
            case "anthropic": c.Model = "claude-sonnet-4-6"; c.BaseUrl = ""; break;
            case "ollama":    c.Model = "llama3"; c.BaseUrl = "127.0.0.1:11434"; break;
        }
    }

    public static string DbPath => AppPaths.DbPath;
}
