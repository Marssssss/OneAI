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

    /// <summary>SQLite session/Memory db consumed by the Rust core.</summary>
    public static string DbPath => Path.Combine(AppDataDir, "oneai.db");

    /// <summary>User-edited + preset scenarios (schema-versioned wrapper).</summary>
    public static string ScenariosPath => Path.Combine(AppDataDir, "oneai_scenarios.json");
}
