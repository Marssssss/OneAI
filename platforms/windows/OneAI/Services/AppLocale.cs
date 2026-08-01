// AppLocale — the Windows app's effective UI + engine language.
//
// Single source of truth read by both the group-chat engine locale (folded
// into the ScenarioSpecDto JSON → parse_scenario → ChatLocale) and the
// bilingual scenario presets (ScenarioStore.PresetsFor). The Settings panel's
// language picker sets `Override` (and, for chrome/.resw, the matching
// ApplicationLanguages.PrimaryLanguageOverride) — so a user override reaches
// the engine, the presets, and the localized chrome together. With no override
// set, the locale follows the system UI culture (which is also what .resw
// resource resolution uses by default).

using System.Globalization;

namespace OneAI.Services;

/// <summary>The app's effective language: zh or en.</summary>
public enum AppLocale
{
    Zh,
    En,
}

public static class AppLocaleHelper
{
    /// <summary>Explicit user override ("zh"/"en"), or null = follow system.
    /// Set by the Settings language picker (layer-2 chrome wiring).</summary>
    public static string? Override { get; set; }

    /// <summary>The effective locale: explicit override if set, otherwise the
    /// system UI culture. `zh` for any Chinese variant, `en` for everything
    /// else (the English fallback).</summary>
    public static AppLocale Current
    {
        get
        {
            if (Override is { } o)
            {
                if (o.StartsWith("zh", StringComparison.OrdinalIgnoreCase)) return AppLocale.Zh;
                if (o.StartsWith("en", StringComparison.OrdinalIgnoreCase)) return AppLocale.En;
            }
            string name = CultureInfo.CurrentUICulture.Name;
            return name.StartsWith("zh", StringComparison.OrdinalIgnoreCase) ? AppLocale.Zh : AppLocale.En;
        }
    }

    /// <summary>The c_facade `locale` JSON value ("en"/"zh").</summary>
    public static string? LocaleCode => Current switch
    {
        AppLocale.En => "en",
        _ => "zh",
    };
}
