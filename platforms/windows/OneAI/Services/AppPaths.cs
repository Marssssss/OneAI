// Filesystem app-data locations for the unpackaged WinUI 3 app.
//
// The app ships unpackaged (`WindowsPackageType=None`), which means it has NO
// package identity — so `Windows.Storage.ApplicationData.Current` (LocalSettings
// / LocalFolder) throws "The process has no package identity" at runtime.
// Persist under %LOCALAPPDATA%\OneAI instead, which is the standard writable
// per-user location for unpackaged desktop apps and survives restarts.

using System;
using System.IO;

namespace OneAI.Services;

public static class AppPaths
{
    public const string AppName = "OneAI";

    /// <summary>`%LOCALAPPDATA%\OneAI`, created on first access.</summary>
    public static string AppDataDir
    {
        get
        {
            var baseDir = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
            var dir = string.IsNullOrEmpty(baseDir)
                ? Path.Combine(AppContext.BaseDirectory, "data")
                : Path.Combine(baseDir, AppName);
            Directory.CreateDirectory(dir);
            return dir;
        }
    }

    /// <summary>Provider config (kind/model/apiKey/baseUrl) as JSON.</summary>
    public static string ProviderConfigPath => Path.Combine(AppDataDir, "provider.json");

    /// <summary>First-run onboarding dismissal state (issue #33) as JSON.
    /// A one-way flag: once the user dismisses the "add an API key" banner,
    /// it never shows again.</summary>
    public static string OnboardingStatePath => Path.Combine(AppDataDir, "onboarding.json");

    /// <summary>
    /// Canonical OneAI session/Memory db — `%USERPROFILE%\.oneai\oneai.db`,
    /// the same file the Rust <c>SqliteSessionStore::with_defaults()</c>
    /// resolves (sqlite_store.rs reads <c>HOME</c> then <c>USERPROFILE</c>)
    /// and that <c>oneai web</c> / the TUI write to. Keeping the FFI app (and,
    /// later, the sidecar via <c>ONEAI_DB_PATH</c>) on this path means every
    /// client shares one backend DB: a session saved anywhere surfaces in
    /// every other client's sidebar. Previously
    /// <c>%LOCALAPPDATA%\OneAI\oneai.db</c>, which diverged from the canonical
    /// default and so never appeared in the webUI/TUI session list. WAL +
    /// busy_timeout make the cross-process sharing safe.
    /// </summary>
    public static string DbPath
    {
        get
        {
            var home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
            var dir = string.IsNullOrEmpty(home)
                ? Path.Combine(AppContext.BaseDirectory, "data")
                : Path.Combine(home, ".oneai");
            Directory.CreateDirectory(dir);
            return Path.Combine(dir, "oneai.db");
        }
    }

    /// <summary>User-edited + preset scenarios (schema-versioned wrapper).</summary>
    public static string ScenariosPath => Path.Combine(AppDataDir, "oneai_scenarios.json");
}
