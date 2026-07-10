// Provider config persistence — Windows ApplicationData LocalSettings
// (the counterpart of Android SharedPreferences / macOS UserDefaults).

using System;
using Windows.Storage;
using OneAI.Native;

namespace OneAI.Services;

public static class ProviderStore
{
    private const string Container = "oneai_provider";
    private static ApplicationDataContainer Settings =>
        ApplicationData.Current.LocalSettings.CreateContainer(Container, ApplicationDataCreateDisposition.Always);

    public static ProviderConfig Load()
    {
        var c = new ProviderConfig
        {
            Kind = (string?)Settings.Values["kind"] ?? "openai",
            Model = (string?)Settings.Values["model"] ?? "gpt-4o-mini",
            ApiKey = (string?)Settings.Values["apiKey"] ?? "",
            BaseUrl = (string?)Settings.Values["baseUrl"] ?? "",
        };
        return c;
    }

    public static void Save(ProviderConfig c)
    {
        Settings.Values["kind"] = c.Kind;
        Settings.Values["model"] = c.Model;
        Settings.Values["apiKey"] = c.ApiKey ?? "";
        Settings.Values["baseUrl"] = c.BaseUrl ?? "";
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

    public static string DbPath =>
        System.IO.Path.Combine(ApplicationData.Current.LocalFolder.Path, "oneai.db");
}
