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

@main
struct OneAIApp: App {
    var body: some Scene {
        WindowGroup {
            ChatScreen()
                .frame(minWidth: 720, minHeight: 480)
        }
        .windowStyle(.titleBar)
        .defaultSize(width: 960, height: 640)
    }
}
