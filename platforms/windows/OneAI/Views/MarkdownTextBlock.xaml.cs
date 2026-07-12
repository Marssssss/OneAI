using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Media;
using OneAI.Services;

namespace OneAI.Views;

/// <summary>Renders block + inline markdown (port of macOS MarkdownText view).
/// Blocks: code fence (with copy + "open in canvas" buttons when long),
/// heading, blockquote, ordered/unordered list, table, paragraph. Rebuilt in
/// code-behind from <see cref="MarkdownHelper.Split"/> — no RichText deps.</summary>
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
    private static readonly Brush Transparent = new SolidColorBrush(Windows.UI.Colors.Transparent);

    public MarkdownTextBlock()
    {
        InitializeComponent();
    }

    private static void OnChanged(DependencyObject d, DependencyPropertyChangedEventArgs e) =>
        ((MarkdownTextBlock)d).Render();

    private void Render()
    {
        Root.Children.Clear();
        foreach (var seg in MarkdownHelper.Split(Markdown ?? ""))
        {
            switch (seg.Kind)
            {
                case MdSegKind.Code:
                    Root.Children.Add(BuildCode(seg.Lang, seg.Text));
                    break;
                case MdSegKind.Heading:
                    Root.Children.Add(BuildParagraph(seg.Text, fontSize: HeadingSize(int.Parse(seg.Lang)), bold: true));
                    break;
                case MdSegKind.Blockquote:
                    Root.Children.Add(BuildQuote(seg.Text));
                    break;
                case MdSegKind.BulletList:
                    Root.Children.Add(BuildList(seg.Text.Split('\n'), ordered: false));
                    break;
                case MdSegKind.OrderedList:
                    Root.Children.Add(BuildList(seg.Text.Split('\n'), ordered: true));
                    break;
                case MdSegKind.Table:
                    if (seg is MdTableSeg t) Root.Children.Add(BuildTable(t));
                    break;
                default: // Paragraph
                    Root.Children.Add(BuildParagraph(seg.Text, fontSize: 14, bold: false));
                    break;
            }
        }
    }

    private static int HeadingSize(int level) => level switch
    {
        1 => 22, 2 => 20, 3 => 17, _ => 15,
    };

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

    private UIElement BuildParagraph(string text, double fontSize, bool bold)
    {
        var tb = new RichTextBlock { TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true };
        var p = new Paragraph { FontSize = fontSize };
        foreach (var seg in MarkdownHelper.InlineSplit(text))
        {
            var run = new Run { Text = seg.Text };
            if (bold) p.Inlines.Add(new Bold { Inlines = { run } });
            else if (seg.Kind == MarkdownHelper.Inline.Code)
            {
                run.FontFamily = new FontFamily("Consolas");
                p.Inlines.Add(run);
            }
            else p.Inlines.Add(run);
        }
        tb.Blocks.Add(p);
        return tb;
    }

    private UIElement BuildQuote(string text)
    {
        var grid = new Grid { ColumnDefinitions = { new ColumnDefinition { Width = GridLength.Auto }, new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) } } };
        var bar = new Border { Background = (Brush)Application.Current.Resources["AccentFillColorDefaultBrush"], Width = 3, CornerRadius = new CornerRadius(1.5) };
        Grid.SetColumn(bar, 0);
        grid.Children.Add(bar);
        var tb = new RichTextBlock { TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true, Margin = new Thickness(8, 0, 0, 0) };
        var p = new Paragraph { FontStyle = Windows.UI.Text.FontStyle.Italic };
        foreach (var line in text.Replace("\r", "").Split('\n'))
        {
            foreach (var seg in MarkdownHelper.InlineSplit(line))
            {
                var run = new Run { Text = seg.Text };
                if (seg.Kind == MarkdownHelper.Inline.Bold) p.Inlines.Add(new Bold { Inlines = { run } });
                else if (seg.Kind == MarkdownHelper.Inline.Code) { run.FontFamily = new FontFamily("Consolas"); p.Inlines.Add(run); }
                else p.Inlines.Add(run);
            }
            p.Inlines.Add(new LineBreak());
        }
        tb.Blocks.Add(p);
        Grid.SetColumn(tb, 1);
        grid.Children.Add(tb);
        return grid;
    }

    private UIElement BuildList(string[] items, bool ordered)
    {
        var sp = new StackPanel { Spacing = 3, Margin = new Thickness(0, 0, 0, 0) };
        for (int i = 0; i < items.Length; i++)
        {
            if (string.IsNullOrWhiteSpace(items[i])) continue;
            var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
            row.Children.Add(new TextBlock { Text = ordered ? $"{i + 1}." : "•", FontSize = 14 });
            var tb = new RichTextBlock { TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true };
            var p = new Paragraph { FontSize = 14 };
            foreach (var seg in MarkdownHelper.InlineSplit(items[i]))
            {
                var run = new Run { Text = seg.Text };
                if (seg.Kind == MarkdownHelper.Inline.Bold) p.Inlines.Add(new Bold { Inlines = { run } });
                else if (seg.Kind == MarkdownHelper.Inline.Code) { run.FontFamily = new FontFamily("Consolas"); p.Inlines.Add(run); }
                else p.Inlines.Add(run);
            }
            tb.Blocks.Add(p);
            row.Children.Add(tb);
            sp.Children.Add(row);
        }
        return sp;
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
                Child = new TextBlock { Text = t.Header[c], FontWeight = Windows.UI.Text.FontWeights.SemiBold, FontSize = 13, TextWrapping = TextWrapping.Wrap },
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
