// Lightweight markdown → XAML-friendly blocks/Inlines. Port of macOS
// Sources/Markdown.swift. Blocks: code fence, heading, blockquote, ordered/
// unordered list, table, paragraph. Inline: `code` / **bold** / bullet prefix.
// Kept minimal (no RichText/markdown-community dependency).

using System.Collections.Generic;
using System.Text;
using System.Text.RegularExpressions;

namespace OneAI.Services;

public enum MdSegKind { Code, Heading, Blockquote, BulletList, OrderedList, Table, Paragraph }
public record MdSeg(MdSegKind Kind, string Text, string Lang);

public record MdTableSeg(List<string> Header, List<List<string>> Rows) : MdSeg(MdSegKind.Table, "", "");

public static class MarkdownHelper
{
    public static List<MdSeg> Split(string src)
    {
        var out_ = new List<MdSeg>();
        var buf = new StringBuilder();
        var lines = src.Replace("\r", "").Split('\n');
        void Flush()
        {
            if (buf.Length > 0)
            {
                // Group consecutive paragraph lines into list blocks where
                // possible; otherwise one paragraph block.
                AppendParagraph(buf.ToString(), out_);
                buf.Clear();
            }
        }
        for (int i = 0; i < lines.Length; i++)
        {
            var l = lines[i];
            if (l.StartsWith("```"))
            {
                Flush();
                var lang = l.Substring(3).Trim();
                var code = new StringBuilder();
                i++;
                while (i < lines.Length && !lines[i].StartsWith("```")) { code.AppendLine(lines[i]); i++; }
                // skip closing fence
                out_.Add(new MdSeg(MdSegKind.Code, code.ToString().TrimEnd('\n'), lang));
            }
            else if (HeadingLevel(l) is { } level && level > 0)
            {
                Flush();
                // A heading is a single line; the for loop's i++ consumes it.
                out_.Add(new MdSeg(MdSegKind.Heading, l.Substring(level).Trim(), level.ToString()));
            }
            else if (l.TrimStart().StartsWith(">"))
            {
                Flush();
                var quote = new StringBuilder();
                while (i < lines.Length && lines[i].TrimStart().StartsWith(">"))
                { quote.AppendLine(lines[i].TrimStart().Substring(1).Trim()); i++; }
                i--; // the for loop will advance past the last `>` line's successor
                out_.Add(new MdSeg(MdSegKind.Blockquote, quote.ToString().TrimEnd('\n'), ""));
            }
            else if (IsTableHeader(l, i + 1 < lines.Length ? lines[i + 1] : ""))
            {
                Flush();
                var header = SplitTableRow(l);
                i += 2; // skip header + separator
                var rows = new List<List<string>>();
                while (i < lines.Length && lines[i].Contains("|")) { rows.Add(SplitTableRow(lines[i])); i++; }
                i--; // for loop advances once more
                out_.Add(new MdTableSeg(header, rows));
            }
            else
            {
                buf.AppendLine(l);
            }
        }
        Flush();
        return out_;
    }

    // A heading line starts with 1..6 '#' then a space.
    private static int HeadingLevel(string l)
    {
        int n = 0;
        foreach (var ch in l) { if (ch == '#') n++; else break; }
        if (n < 1 || n > 6) return 0;
        return l.Length > n && l[n] == ' ' ? n : 0;
    }

    private static bool IsTableHeader(string l, string next)
    {
        if (!l.Contains("|")) return false;
        var s = next.Trim();
        return s.Contains("-") && s.Contains("|");
    }

    private static List<string> SplitTableRow(string l)
    {
        var parts = l.Split('|');
        var list = new List<string>();
        foreach (var p in parts) list.Add(p.Trim());
        // drop empty cells produced by leading/trailing '|'
        if (list.Count > 1 && list[0] == "") list.RemoveAt(0);
        if (list.Count > 1 && list[^1] == "") list.RemoveAt(list.Count - 1);
        return list;
    }

    // Group paragraph lines into list blocks + plain paragraphs.
    private static void AppendParagraph(string para, List<MdSeg> out_)
    {
        var bullets = new List<string>();
        var ordered = new List<string>();
        void FlushBullets() { if (bullets.Count > 0) { out_.Add(new MdSeg(MdSegKind.BulletList, string.Join("\n", bullets), "")); bullets.Clear(); } }
        void FlushOrdered() { if (ordered.Count > 0) { out_.Add(new MdSeg(MdSegKind.OrderedList, string.Join("\n", ordered), "")); ordered.Clear(); } }

        foreach (var raw in para.Replace("\r", "").Split('\n'))
        {
            var line = raw.TrimEnd();
            if (line.StartsWith("- ") || line.StartsWith("* "))
            {
                FlushOrdered();
                bullets.Add(line.Substring(2));
            }
            else if (OrderedPrefix(line) is { } rest)
            {
                FlushBullets();
                ordered.Add(rest);
            }
            else
            {
                FlushBullets(); FlushOrdered();
                if (!string.IsNullOrWhiteSpace(line))
                    out_.Add(new MdSeg(MdSegKind.Paragraph, line, ""));
            }
        }
        FlushBullets(); FlushOrdered();
    }

    // If line begins with "N. " return the rest after the prefix; else null.
    private static string? OrderedPrefix(string l)
    {
        int i = 0;
        while (i < l.Length && char.IsDigit(l[i])) i++;
        if (i == 0 || i >= l.Length || l[i] != '.') return null;
        return i + 1 < l.Length && l[i + 1] == ' ' ? l.Substring(i + 2) : null;
    }

    // Inline highlight markers for XAML: Bold / Code. The MarkdownTextBlock
    // renders these as Runs in a RichTextBlock paragraph.
    public enum Inline { Plain, Bold, Code }
    public record InlineSeg(Inline Kind, string Text);

    private static readonly Regex BoldRe = new(@"\*\*(.+?)\*\*", RegexOptions.Compiled);
    private static readonly Regex CodeRe = new(@"`([^`]+)`", RegexOptions.Compiled);

    public static List<InlineSeg> InlineSplit(string s)
    {
        var segs = new List<InlineSeg>();
        int i = 0;
        while (i < s.Length)
        {
            var m = BoldRe.Match(s, i);
            var mc = CodeRe.Match(s, i);
            int boldAt = m.Success ? m.Index : int.MaxValue;
            int codeAt = mc.Success ? mc.Index : int.MaxValue;
            if (boldAt == int.MaxValue && codeAt == int.MaxValue)
            {
                segs.Add(new InlineSeg(Inline.Plain, s.Substring(i)));
                break;
            }
            if (boldAt < codeAt)
            {
                if (boldAt > i) segs.Add(new InlineSeg(Inline.Plain, s.Substring(i, boldAt - i)));
                segs.Add(new InlineSeg(Inline.Bold, m!.Groups[1].Value));
                i = m.Index + m.Length;
            }
            else
            {
                if (codeAt > i) segs.Add(new InlineSeg(Inline.Plain, s.Substring(i, codeAt - i)));
                segs.Add(new InlineSeg(Inline.Code, mc!.Groups[1].Value));
                i = mc.Index + mc.Length;
            }
        }
        return segs;
    }

    public static string BulletPrefix(string line)
    {
        var t = line.TrimEnd();
        if (t.StartsWith("- ") || t.StartsWith("* ")) return "•  " + t.Substring(2);
        return t;
    }
}
