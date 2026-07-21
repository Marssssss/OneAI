// Markdown renderer — block-level parser + Foundation inline markdown.
// Blocks: code fence, heading, blockquote, ordered/unordered list, table,
// paragraph. Inline (bold/italic/code/links/strikethrough) is delegated to
// `AttributedString(markdown:)` (macOS 13+ Extended syntax) so we don't
// hand-roll regexes. No external deps.

import SwiftUI

enum MdBlock: Equatable {
    case heading(level: Int, text: String)
    case paragraph(String)
    case code(lang: String, code: String)
    case blockquote(String)
    case bulletList(items: [String])
    case orderedList(items: [String])
    case table(header: [String], rows: [[String]])
}

/// Split raw markdown into block-level segments (code fences are extracted
/// verbatim; the rest are classified by leading markers). Falls back to
/// `.paragraph` for anything unrecognized.
func splitMarkdown(_ src: String) -> [MdBlock] {
    var out: [MdBlock] = []
    let lines = src.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
    var i = 0
    var para: [String] = []
    func flushPara() {
        guard !para.isEmpty else { return }
        out.append(.paragraph(para.joined(separator: "\n")))
        para.removeAll()
    }
    while i < lines.count {
        let l = lines[i]
        // Code fence
        if l.hasPrefix("```") {
            flushPara()
            let lang = l.dropFirst(3).trimmingCharacters(in: .whitespaces)
            var code: [String] = []
            i += 1
            while i < lines.count && !lines[i].hasPrefix("```") {
                code.append(lines[i]); i += 1
            }
            i += 1
            out.append(.code(lang: lang, code: code.joined(separator: "\n")))
            continue
        }
        // Heading
        if let level = headingLevel(l) {
            flushPara()
            out.append(.heading(level: level, text: l.dropFirst(level).trimmingCharacters(in: .whitespaces)))
            i += 1; continue
        }
        // Blockquote — merge consecutive `>` lines.
        if l.hasPrefix(">") {
            flushPara()
            var quote: [String] = []
            while i < lines.count && lines[i].hasPrefix(">") {
                quote.append(lines[i].dropFirst().trimmingCharacters(in: .whitespaces))
                i += 1
            }
            out.append(.blockquote(quote.joined(separator: "\n")))
            continue
        }
        // Table — a `|`-delimited line followed by a separator line `|---|`.
        if l.contains("|"), i + 1 < lines.count, isTableSeparator(lines[i+1]) {
            flushPara()
            let header = splitTableRow(l)
            i += 2
            var rows: [[String]] = []
            while i < lines.count && lines[i].contains("|") {
                rows.append(splitTableRow(lines[i])); i += 1
            }
            out.append(.table(header: header, rows: rows))
            continue
        }
        // Blank line → paragraph break
        if l.trimmingCharacters(in: .whitespaces).isEmpty {
            flushPara(); i += 1; continue
        }
        para.append(l); i += 1
    }
    flushPara()

    // Post-pass: group consecutive paragraph lines that are list items into
    // list blocks for cleaner rendering.
    out = groupLists(out)
    return out
}

private func headingLevel(_ l: String) -> Int? {
    var n = 0
    for ch in l { if ch == "#" { n += 1 } else { break } }
    return (1...6).contains(n) ? n : nil
}

private func isTableSeparator(_ l: String) -> Bool {
    let s = l.trimmingCharacters(in: .whitespaces)
    return s.contains("-") && s.contains("|")
}

private func splitTableRow(_ l: String) -> [String] {
    // Split on `|`, trim, drop the empty cells produced by leading/trailing `|`.
    var parts = l.split(separator: "|", omittingEmptySubsequences: false)
        .map { $0.trimmingCharacters(in: .whitespaces) }
    if parts.count > 1, parts.first?.isEmpty == true { parts.removeFirst() }
    if parts.count > 1, parts.last?.isEmpty == true { parts.removeLast() }
    return parts
}

/// Coalesce consecutive `- `/`* `/`1. ` paragraph lines into list blocks.
private func groupLists(_ blocks: [MdBlock]) -> [MdBlock] {
    var out: [MdBlock] = []
    var bullets: [String] = []
    var ordered: [String] = []
    func flushBullets() { if !bullets.isEmpty { out.append(.bulletList(items: bullets)); bullets.removeAll() } }
    func flushOrdered() { if !ordered.isEmpty { out.append(.orderedList(items: ordered)); ordered.removeAll() } }

    for b in blocks {
        if case .paragraph(let p) = b {
            let lines = p.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
            var matchedAny = false
            for line in lines {
                if line.hasPrefix("- ") || line.hasPrefix("* ") {
                    flushOrdered(); bullets.append(String(line.dropFirst(2))); matchedAny = true
                } else if let num = orderedListPrefix(line) {
                    flushBullets(); ordered.append(String(line.dropFirst(num))); matchedAny = true
                } else {
                    if !bullets.isEmpty { flushBullets() }
                    if !ordered.isEmpty { flushOrdered() }
                    out.append(.paragraph(line))
                }
            }
            if !matchedAny { /* already appended */ }
        } else {
            flushBullets(); flushOrdered(); out.append(b)
        }
    }
    flushBullets(); flushOrdered()
    return out
}

private func orderedListPrefix(_ l: String) -> Int? {
    let s = l
    var dotIdx: String.Index?
    for idx in s.indices {
        if s[idx].isNumber { continue }
        if s[idx] == "." { dotIdx = idx; break }
        return nil
    }
    guard let d = dotIdx, s.distance(from: s.startIndex, to: d) >= 1 else { return nil }
    let after = s.index(after: d)
    // `after` may be `endIndex` (the line is a partial "N." mid-stream — the
    // dot is the last char, nothing after it yet). Subscripting `s[after]`
    // then traps (String index out of range). Treat "no char after the dot"
    // as "not (yet) a list item".
    guard after < s.endIndex, s[after].isWhitespace else { return nil }
    return s.distance(from: s.startIndex, to: after)
}

/// Build an AttributedString for inline markdown via Foundation's parser
/// (Extended: bold/italic/code/links/strikethrough). Falls back to plain text.
func buildInline(_ src: String, codeBg: Color) -> AttributedString {
    var options = AttributedString.MarkdownParsingOptions()
    options.interpretedSyntax = .inlineOnlyPreservingWhitespace
    options.allowsExtendedAttributes = true
    if let attr = try? AttributedString(markdown: src, options: options) {
        return attr
    }
    return AttributedString(src)
}
