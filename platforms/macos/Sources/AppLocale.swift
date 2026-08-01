// AppLocale — the app's effective UI + engine language.
//
// Single source of truth read by both the group-chat engine locale
// (passed to ScenarioSpecView.locale → ChatLocaleView) and the bilingual
// scenario presets (AgentStore.presets(locale:)). The Settings panel's
// language picker writes the `oneai_language` UserDefaults key (and, for
// SwiftUI Text chrome, the matching `AppleLanguages` override) — so a user
// override reaches the engine, the presets, and the localized chrome together.
// With no override set, the locale follows the system preferred languages
// (which is also what SwiftUI Text + Bundle.main use by default).

import Foundation

enum AppLocale: String, Codable, CaseIterable {
    case zh, en

    /// The effective locale: explicit user override (`oneai_language`) if set,
    /// otherwise the system preferred language. `zh` for any Chinese variant,
    /// `en` for everything else (the English fallback).
    static var current: AppLocale {
        if let raw = UserDefaults.standard.string(forKey: "oneai_language"),
           let loc = AppLocale(rawValue: raw) {
            return loc
        }
        let preferred = Locale.preferredLanguages.first ?? ""
        return preferred.hasPrefix("zh") ? .zh : .en
    }

    /// The engine `ChatLocaleView` for `ScenarioSpecView.locale`.
    var chatLocaleView: ChatLocaleView {
        switch self {
        case .en: return .en
        case .zh: return .zh
        }
    }
}
