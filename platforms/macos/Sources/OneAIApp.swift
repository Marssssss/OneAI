// OneAI macOS chat — native port of platforms/android MainActivity.kt (S5).
//
// SwiftUI App lifecycle (no Xcode project needed — built by build_macos.sh
// via swiftc + the universal liboneai.a staged by scripts/build_apple.sh).
// Dark theme follows the system appearance; every color is adaptive.

import SwiftUI
import AppKit

// ── Adaptive Doubao-ish palette (light + dark, follows system) ──────────
// NSColor dynamic providers give automatic light/dark switching on macOS 13.
private func adaptive(light: UInt32, dark: UInt32) -> Color {
    Color(NSColor(name: nil) { appearance in
        let v = appearance.bestMatch(from: [.darkAqua, .vibrantDark]) != nil ? dark : light
        let r = CGFloat((v >> 16) & 0xff) / 255
        let g = CGFloat((v >> 8) & 0xff) / 255
        let b = CGFloat(v & 0xff) / 255
        return NSColor(srgbRed: r, green: g, blue: b, alpha: 1)
    })
}

enum Theme {
    static let background   = adaptive(light: 0xF7F7F8, dark: 0x0F0F10)
    static let surface      = adaptive(light: 0xFFFFFF, dark: 0x1C1C1E)
    static let surfaceVar   = adaptive(light: 0xEFEFF1, dark: 0x2A2A2C)
    static let onBg         = adaptive(light: 0x1A1A1A, dark: 0xECECEC)
    static let onSurfaceVar = adaptive(light: 0x8A8A8A, dark: 0x9A9A9A)
    static let primary      = adaptive(light: 0x4D6BFE, dark: 0x6B8BFF)
    static let primaryCont   = adaptive(light: 0xE7F0FF, dark: 0x1E2A4A)
    static let secondaryCont = adaptive(light: 0xF2F4FB, dark: 0x23242B)
    static let tertiary     = adaptive(light: 0x3B8C5A, dark: 0x4CAF50)
    static let errorC       = adaptive(light: 0xE5484D, dark: 0xFF6B6E)
}

// ── OneAI brand palette (mirrors the TUI's per-character gradient — see
// examples/cli/src/tui/render/brand.rs + theme.rs). Same hues so the macOS app
// and the TUI read as one brand; the macOS version renders each filled pixel as
// an extruded 3D tile instead of a flat block.
enum Brand {
    /// Per-character gradient colors for "OneAI": O, n, e, A, I.
    static let charColors: [UInt32] = [
        0xD07C7C,  // O — coral
        0x62B0BC,  // n — teal
        0x6EA0C8,  // e — muted blue
        0x96C47A,  // A — sage
        0xD6B660,  // I — gold
    ]
    /// Solid brand color for a character index (used as the tile's base face).
    static func color(_ idx: Int) -> Color { Color(hex: String(charColors[idx], radix: 16)) }
}

// ── Color(hex:) — parse "#RRGGBB" / "RRGGBB" hex strings (per-agent colors) ─
extension Color {
    init(hex: String) {
        var s = hex.trimmingCharacters(in: .whitespaces)
        if s.hasPrefix("#") { s.removeFirst() }
        var v: UInt64 = 0x888888
        Scanner(string: s).scanHexInt64(&v)
        let r = CGFloat((v >> 16) & 0xff) / 255
        let g = CGFloat((v >> 8) & 0xff) / 255
        let b = CGFloat(v & 0xff) / 255
        self = Color(NSColor(srgbRed: r, green: g, blue: b, alpha: 1))
    }
}

// ── pointerCursor() — show the pointing-hand cursor over a clickable view.
// SwiftUI's `.cursor(.pointingHand)` is macOS 14+ only; this builds the same
// affordance on macOS 13 via NSCursor push/pop on hover, so every clickable
// row/button reads as interactive (otherwise a plain Button gives no hover
// cue and users don't know it's clickable).
private struct PointerHoverModifier: ViewModifier {
    func body(content: Content) -> some View {
        content.onHover { inside in
            if inside { NSCursor.pointingHand.push() }
            else { NSCursor.pop() }
        }
    }
}
extension View {
    func pointerCursor() -> some View { modifier(PointerHoverModifier()) }
}
@main
struct OneAIApp: App {
    var body: some Scene {
        WindowGroup {
            ChatScreen()
                .frame(minWidth: 720, minHeight: 480)
        }
        // hiddenTitleBar drops the (empty, space-wasting) native title bar;
        // the traffic-light buttons still float over the sidebar top-left.
        // The in-app header carries the OneAI title + slogan instead.
        .windowStyle(.hiddenTitleBar)
        .defaultSize(width: 960, height: 640)
    }
}

// ── Explicit app font sizes ─────────────────────────────────────────────
// macOS SwiftUI does NOT scale semantic fonts (.body/.caption/…) via
// dynamicTypeSize the way iOS does, so the app sizes text explicitly. Tune
// these to change sizes app-wide. (Code/monospaced sizes are bumped in place
// at their call sites.)
extension Font {
    static let oTitle2          = Font.system(size: 22, weight: .bold)
    static let oTitle3          = Font.system(size: 19, weight: .bold)
    static let oHeadline        = Font.system(size: 17, weight: .semibold)
    static let oBody            = Font.system(size: 14)
    static let oBodyItalic      = Font.system(size: 14).italic()
    static let oSubheadline     = Font.system(size: 14)
    static let oSubheadlineBold = Font.system(size: 14, weight: .bold)
    static let oFootnote        = Font.system(size: 13)
    static let oCaption         = Font.system(size: 12)
    static let oCaptionBold     = Font.system(size: 12, weight: .bold)
    static let oCaption2        = Font.system(size: 11)
}
