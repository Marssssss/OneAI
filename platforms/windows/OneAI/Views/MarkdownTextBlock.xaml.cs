using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Media;
using OneAI.Services;

namespace OneAI.Views;

/// <summary>Renders block + inline markdown (port of macOS MarkdownText view).
/// Blocks: code fence (with copy + "open in canvas" buttons when long),
/// heading, blockquote, ordered/unordered list, table, paragraph. Rebuilt in
/// code-behind from <see cref="MarkdownHelper.Split"/> — no RichText deps.
///
/// Two rendering invariants:
/// 1. <b>Single selectable prose run</b> — consecutive prose segments
///    (paragraph/heading/blockquote/bullet/ordered) render as Paragraphs inside
///    ONE <see cref="RichTextBlock"/> so drag-selection spans the whole prose
///    (WinUI selection crosses Paragraphs within a single RichTextBlock, but
///    NOT across separate controls). Code fences and tables stay as their own
///    cards (each has a copy button; the whole-answer context menu copies all).
/// 2. <b>Incremental rebuild</b> — <see cref="Render"/> caches the last segment
///    list; when only the last segment's text grew (count + prefix unchanged,
///    the common streaming case) it rebuilds ONLY the last child instead of
///    clearing + rebuilding every child. This is the Windows analog of macOS's
///    per-block <c>.equatable()</c> (re-parse only the in-progress block), so
///    live-streaming markdown doesn't flood the UI thread on long replies.</summary>
public sealed partial class MarkdownTextBlock : UserControl
{
    public static readonly DependencyProperty MarkdownProperty =
        DependencyProperty.Register(nameof(Markdown), typeof(string), typeof(MarkdownTextBlock),
            new PropertyMetadata("", OnChanged));

    public string Markdown
    {
        get => (string)GetValue(MarkdownProperty);
        set => SetValue(MarkdownProperty, value);
    }

    /// <summary>Set by ChatView: invoked when the user clicks "在画布打开" on a
    /// long code block, promoting it to the docked ArtifactCanvas. Static
    /// because code blocks live inside a DataTemplate (per-instance wiring from
    /// XAML isn't practical) — all instances share the one ChatView store.</summary>
    public static OneAI.Services.ArtifactStore? ArtifactStore { get; set; }

    /// <summary>Threshold above which a code block shows the "open in canvas" button.</summary>
    private const int ArtifactThreshold = 600;
    private static readonly Brush Transparent = new SolidColorBrush(Microsoft.UI.Colors.Transparent);

    /// <summary>Flattened segment list from the previous render (for the
    /// incremental-rebuild prefix comparison).</summary>
    private List<MdSeg>? _lastSegs;
    /// <summary>Segments each Root child was built from, parallel to
    /// <see cref="Root.Children"/>. Lets the incremental path rebuild just the
    /// last child from its segment group.</summary>
    private List<List<MdSeg>>? _childSegGroups;

    public MarkdownTextBlock()
    {
        InitializeComponent();
    }

    private static void OnChanged(DependencyObject d, DependencyPropertyChangedEventArgs e) =>
        ((MarkdownTextBlock)d).Render();

    private void Render()
    {
        var segs = MarkdownHelper.Split(Markdown ?? "");

        // Incremental: during streaming the markdown grows token-by-token; the
        // common case is "only the last segment's text grew, count + prefix
        // unchanged". Rebuild ONLY the last child instead of clearing + rebuilding
        // every child — re-parse work is bounded to the in-progress block.
        if (_lastSegs != null && _childSegGroups != null
            && segs.Count > 0 && segs.Count == _lastSegs.Count
            && PrefixEqual(_lastSegs, segs))
        {
            var lastGroup = _childSegGroups[^1];
            lastGroup[^1] = segs[^1];   // the changed segment, in place
            Root.Children.RemoveAt(Root.Children.Count - 1);
            Root.Children.Add(BuildGroup(lastGroup));
            _lastSegs = segs;
            return;
        }

        // Full rebuild.
        Root.Children.Clear();
        _childSegGroups = new();
        foreach (var group in GroupSegments(segs))
        {
            _childSegGroups.Add(group);
            Root.Children.Add(BuildGroup(group));
        }
        _lastSegs = segs;
    }

    /// <summary>True when every segment EXCEPT the last is equal between the two
    /// lists (record equality on <see cref="MdSeg"/>, incl. the <see cref="MdTableSeg"/>
    /// subclass). True ⇒ only the last segment may have changed ⇒ the incremental
    /// path can rebuild just the last child.</summary>
    private static bool PrefixEqual(List<MdSeg> a, List<MdSeg> b)
    {
        for (int i = 0; i < a.Count - 1; i++)
            if (!a[i].Equals(b[i])) return false;
        return true;
    }

    /// <summary>Group consecutive PROSE segments (paragraph/heading/blockquote/
    /// bullet/ordered) into one group → rendered as a single selectable
    /// RichTextBlock so drag-selection spans the whole prose run. Code fences and
    /// tables each form their own singleton group → their own card.</summary>
    private static List<List<MdSeg>> GroupSegments(List<MdSeg> segs)
    {
        var groups = new List<List<MdSeg>>();
        List<MdSeg>? prose = null;
        foreach (var seg in segs)
        {
            if (seg.Kind is MdSegKind.Code or MdSegKind.Table)
            {
                prose = null;   // structural block flushes the prose run
                groups.Add(new List<MdSeg> { seg });
            }
            else
            {
                if (prose == null) { prose = new List<MdSeg>(); groups.Add(prose); }
                prose.Add(seg);
            }
        }
        return groups;
    }

    /// <summary>Build the UIElement for one segment group. A singleton code/table
    /// group → its card; a prose group → one selectable RichTextBlock.</summary>
    private UIElement BuildGroup(List<MdSeg> group)
    {
        if (group.Count == 1)
        {
            var seg = group[0];
            if (seg.Kind == MdSegKind.Code) return BuildCode(seg.Lang, seg.Text);
            if (seg.Kind == MdSegKind.Table && seg is MdTableSeg t) return BuildTable(t);
        }
        return BuildProse(group);
    }

    /// <summary>One selectable RichTextBlock holding a Paragraph per prose
    /// segment, so selection spans the whole prose run. A 6px top margin on
    /// every paragraph after the first preserves the inter-paragraph spacing
    /// the old per-paragraph-RichTextBlock layout got from the StackPanel.</summary>
    private UIElement BuildProse(List<MdSeg> group)
    {
        var rtb = new RichTextBlock { TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true };
        foreach (var seg in group)
        {
            Paragraph p = seg.Kind switch
            {
                MdSegKind.Heading => BuildParagraph(seg.Text, fontSize: HeadingSize(int.Parse(seg.Lang)), bold: true),
                MdSegKind.Blockquote => BuildQuoteParagraph(seg.Text),
                MdSegKind.BulletList => BuildListParagraph(seg.Text.Split('\n'), ordered: false),
                MdSegKind.OrderedList => BuildListParagraph(seg.Text.Split('\n'), ordered: true),
                _ => BuildParagraph(seg.Text, fontSize: 14, bold: false),
            };
            if (rtb.Blocks.Count > 0) p.Margin = new Thickness(0, 6, 0, 0);
            rtb.Blocks.Add(p);
        }
        return rtb;
    }

    private static int HeadingSize(int level) => level switch
    {
        1 => 22, 2 => 20, 3 => 17, _ => 15,
    };

    /// <summary>Append the inline-split runs of <paramref name="text"/> to a
    /// paragraph. <paramref name="forceBold"/> wraps every run in Bold (used for
    /// headings); otherwise Bold/Code markers come from <see cref="MarkdownHelper.InlineSplit"/>.</summary>
    private static void AppendInlines(Paragraph p, string text, bool forceBold)
    {
        foreach (var seg in MarkdownHelper.InlineSplit(text))
        {
            var run = new Run { Text = seg.Text };
            if (forceBold || seg.Kind == MarkdownHelper.Inline.Bold)
                p.Inlines.Add(new Bold { Inlines = { run } });
            else if (seg.Kind == MarkdownHelper.Inline.Code)
            {
                run.FontFamily = new FontFamily("Consolas");
                p.Inlines.Add(run);
            }
            else p.Inlines.Add(run);
        }
    }

    private static Paragraph BuildParagraph(string text, double fontSize, bool bold)
    {
        var p = new Paragraph { FontSize = fontSize };
        AppendInlines(p, text, bold);
        return p;
    }

    /// <summary>A blockquote as one italic paragraph (no left bar — it would
    /// break the single selectable RichTextBlock run). Lines separated by
    /// LineBreak, bold/code inlines preserved.</summary>
    private static Paragraph BuildQuoteParagraph(string text)
    {
        var p = new Paragraph { FontStyle = Windows.UI.Text.FontStyle.Italic };
        var lines = text.Replace("\r", "").Split('\n');
        for (int i = 0; i < lines.Length; i++)
        {
            if (i > 0) p.Inlines.Add(new LineBreak());
            AppendInlines(p, lines[i], false);
        }
        return p;
    }

    /// <summary>A list as one paragraph: each item prefixed ("•  " / "N.  "),
    /// items separated by LineBreak. Folding into the prose run keeps selection
    /// continuous across list items.</summary>
    private static Paragraph BuildListParagraph(string[] items, bool ordered)
    {
        var p = new Paragraph { FontSize = 14 };
        int n = 0;
        foreach (var raw in items)
        {
            if (string.IsNullOrWhiteSpace(raw)) continue;
            if (p.Inlines.Count > 0) p.Inlines.Add(new LineBreak());
            p.Inlines.Add(new Run { Text = ordered ? $"{++n}.  " : "•  " });
            AppendInlines(p, raw, false);
        }
        return p;
    }

    private UIElement BuildCode(string lang, string code)
    {
        var border = new Border
        {
            Background = (Brush)Application.Current.Resources["CardBackgroundFillColorDefaultBrush"],
            BorderBrush = (Brush)Application.Current.Resources["ControlStrokeColorDefaultBrush"],
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(8),
            Padding = new Thickness(10),
        };
        var sp = new StackPanel { Spacing = 4 };
        // Header row: lang label + copy + (when long) open-in-canvas buttons.
        var header = new Grid { ColumnDefinitions = { new ColumnDefinition { Width = GridLength.Auto }, new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) }, new ColumnDefinition { Width = GridLength.Auto }, new ColumnDefinition { Width = GridLength.Auto } } };
        if (!string.IsNullOrEmpty(lang))
        {
            header.Children.Add(new TextBlock
            {
                Text = lang, FontFamily = new FontFamily("Consolas"), FontSize = 11,
                Foreground = (Brush)Application.Current.Resources["TextFillColorTertiaryBrush"],
                VerticalAlignment = VerticalAlignment.Center,
            });
        }
        var copyBtn = new Button
        {
            Content = "复制", Padding = new Thickness(8, 2, 8, 2), FontSize = 11,
            Background = Transparent, BorderThickness = new Thickness(0),
        };
        copyBtn.Click += (_, _) =>
        {
            var dp = new Windows.ApplicationModel.DataTransfer.DataPackage { RequestedOperation = Windows.ApplicationModel.DataTransfer.DataPackageOperation.Copy };
            dp.SetText(code);
            Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(dp);
            copyBtn.Content = "已复制";
            _ = DispatcherQueue.TryEnqueue(async () => { await Task.Delay(1200); copyBtn.Content = "复制"; });
        };
        Grid.SetColumn(copyBtn, 2);
        header.Children.Add(copyBtn);
        if (code.Length > ArtifactThreshold)
        {
            var openBtn = new Button
            {
                Content = "在画布打开", Padding = new Thickness(8, 2, 8, 2), FontSize = 11,
                Background = Transparent, BorderThickness = new Thickness(0),
            };
            openBtn.Click += (_, _) => ArtifactStore?.Open(lang, code);
            Grid.SetColumn(openBtn, 3);
            header.Children.Add(openBtn);
        }
        sp.Children.Add(header);
        sp.Children.Add(new TextBlock
        {
            Text = code, FontFamily = new FontFamily("Consolas"), FontSize = 13,
            TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true,
        });
        border.Child = sp;
        return border;
    }

    private UIElement BuildTable(MdTableSeg t)
    {
        var grid = new Grid();
        // Columns
        for (int c = 0; c < t.Header.Count; c++)
            grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        // Header row
        grid.RowDefinitions.Add(new RowDefinition());
        for (int c = 0; c < t.Header.Count; c++)
        {
            var cell = new Border
            {
                Background = (Brush)Application.Current.Resources["ControlFillColorDefaultBrush"],
                Padding = new Thickness(6),
                Child = new TextBlock { Text = t.Header[c], FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, FontSize = 13, TextWrapping = TextWrapping.Wrap },
            };
            Grid.SetRow(cell, 0); Grid.SetColumn(cell, c);
            grid.Children.Add(cell);
        }
        // Data rows
        for (int r = 0; r < t.Rows.Count; r++)
        {
            grid.RowDefinitions.Add(new RowDefinition());
            for (int c = 0; c < t.Rows[r].Count && c < t.Header.Count; c++)
            {
                var cell = new Border
                {
                    BorderBrush = (Brush)Application.Current.Resources["ControlStrokeColorDefaultBrush"],
                    BorderThickness = new Thickness(1, 1, 0, 0),
                    Padding = new Thickness(6),
                    Child = new TextBlock { Text = t.Rows[r][c], FontSize = 13, TextWrapping = TextWrapping.Wrap },
                };
                Grid.SetRow(cell, r + 1); Grid.SetColumn(cell, c);
                grid.Children.Add(cell);
            }
        }
        return grid;
    }
}
