// Lightweight markdown → XAML-friendly Inlines. Ports Android splitMarkdown:
// fenced code blocks become plain monospace blocks; inline `code` / **bold**
// / bullet prefixes are applied. Kept minimal (no RichText dependency).

using System.Collections.Generic;
using System.Text;
using System.Text.RegularExpressions;

namespace OneAI.Services;

public enum MdSegKind { Prose, Code }
public record MdSeg(MdSegKind Kind, string Text, string Lang);

public static class MarkdownHelper
{
    public static List<MdSeg> Split(string src)
    {
        var out_ = new List<MdSeg>();
        var buf = new StringBuilder();
        var lines = src.Replace("\r", "").Split('\n');
        void Flush()
        {
            if (buf.Length > 0) { out_.Add(new MdSeg(MdSegKind.Prose, buf.ToString(), "")); buf.Clear(); }
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
            else
            {
                buf.AppendLine(l);
            }
        }
        Flush();
        return out_;
    }

    // Inline highlight markers for the XAML to apply: returns segments tagged
    // Bold / Code. The ChatPage renders these as Runs in a TextBlock.
    public enum Inline { Plain, Bold, Code }
    public record InlineSeg(Inline Kind, string Text);

    private static readonly Regex BoldRe = new(@"\*\*(.+?)\*\*", RegexOptions.Compiled);
    private static readonly Regex CodeRe = new(@"`([^`]+)`", RegexOptions.Compiled);

    public static List<InlineSeg> InlineSplit(string s)
    {
        // Tokenise **bold** and `code` left-to-right.
        var segs = new List<InlineSeg>();
        int i = 0;
        while (i < s.Length)
        {
            // bold
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
