// Lightweight markdown renderer (port of Android splitMarkdown/buildInline).
// Fenced code blocks + inline `code` + **bold** + bullet list prefixes.
// No external deps. Uses only built-in AttributedString attributes (.font,
// .backgroundColor) so it is stable on macOS 13.

import SwiftUI

enum MdSeg {
    case prose(String)
    case code(lang: String, code: String)
}

func splitMarkdown(_ src: String) -> [MdSeg] {
    var out: [MdSeg] = []
    let lines = src.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
    var buf: [String] = []
    func flush() {
        guard !buf.isEmpty else { return }
        let body = buf.joined(separator: "\n")
        out.append(.prose(body))
        buf.removeAll()
    }
    var i = 0
    while i < lines.count {
        let l = lines[i]
        if l.hasPrefix("```") {
            flush()
            let lang = l.dropFirst(3).trimmingCharacters(in: .whitespaces)
            var code: [String] = []
            i += 1
            while i < lines.count && !lines[i].hasPrefix("```") {
                code.append(lines[i]); i += 1
            }
            i += 1 // skip closing fence
            out.append(.code(lang: lang, code: code.joined(separator: "\n")))
        } else {
            buf.append(l); i += 1
        }
    }
    flush()
    return out
}

/// Build an AttributedString for a prose block: inline `code`, **bold**,
/// and `- `/`* ` bullet prefixes. Each source line becomes one line.
func buildInline(_ src: String, codeBg: Color) -> AttributedString {
    var result = AttributedString()
    for rawLine in src.split(separator: "\n", omittingEmptySubsequences: false) {
        let trimmed = String(rawLine).trimmingCharacters(in: CharacterSet(charactersIn: "\n"))
        if trimmed.isEmpty { result.append(AttributedString("\n")); continue }
        let (prefix, body): (String, String)
        if trimmed.hasPrefix("- ") || trimmed.hasPrefix("* ") {
            prefix = "•  "; body = String(trimmed.dropFirst(2))
        } else {
            prefix = ""; body = trimmed
        }
        if !prefix.isEmpty { result.append(AttributedString(prefix)) }
        result.append(inlineFormat(body, codeBg: codeBg))
        result.append(AttributedString("\n"))
    }
    return result
}

private func inlineFormat(_ s: String, codeBg: Color) -> AttributedString {
    var result = AttributedString()
    var i = s.startIndex
    while i < s.endIndex {
        if s[i...].hasPrefix("**") {
            let start = s.index(i, offsetBy: 2)
            if let end = s[start...].firstRange(of: "**")?.lowerBound {
                var seg = AttributedString(String(s[start..<end]))
                seg.font = .body.bold()
                result.append(seg)
                i = s.index(end, offsetBy: 2)
                continue
            }
        } else if s[i] == "`" {
            let next = s.index(after: i)
            if let end = s[next...].firstIndex(of: "`") {
                var seg = AttributedString(String(s[next..<end]))
                seg.font = .system(.body, design: .monospaced)
                seg.backgroundColor = codeBg
                result.append(seg)
                i = s.index(after: end)
                continue
            }
        }
        result.append(AttributedString(String(s[i])))
        i = s.index(after: i)
    }
    return result
}
