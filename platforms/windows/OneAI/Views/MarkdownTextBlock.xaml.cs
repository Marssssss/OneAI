using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Media;
using OneAI.Services;

namespace OneAI.Views;

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
            if (seg.Kind == MdSegKind.Code)
            {
                var border = new Border
                {
                    Background = (Brush)Application.Current.Resources["CardBackgroundFillColorDefaultBrush"],
                    CornerRadius = new CornerRadius(8),
                    Padding = new Thickness(10),
                };
                var sp = new StackPanel { Spacing = 4 };
                if (!string.IsNullOrEmpty(seg.Lang))
                    sp.Children.Add(new TextBlock
                    {
                        Text = seg.Lang,
                        FontFamily = new FontFamily("Consolas"),
                        FontSize = 11,
                        Foreground = (Brush)Application.Current.Resources["TextFillColorTertiaryBrush"],
                    });
                sp.Children.Add(new TextBlock
                {
                    Text = seg.Text,
                    FontFamily = new FontFamily("Consolas"),
                    FontSize = 13,
                    TextWrapping = TextWrapping.Wrap,
                    IsTextSelectionEnabled = true,
                });
                border.Child = sp;
                Root.Children.Add(border);
            }
            else
            {
                var tb = new RichTextBlock { TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true };
                var p = new Paragraph();
                foreach (var line in seg.Text.Replace("\r", "").Split('\n'))
                {
                    var display = MarkdownHelper.BulletPrefix(line);
                    AppendInline(p, display);
                    p.Inlines.Add(new LineBreak());
                }
                // Trim trailing empty line break
                if (p.Inlines.Count > 0) p.Inlines.RemoveAt(p.Inlines.Count - 1);
                tb.Blocks.Add(p);
                Root.Children.Add(tb);
            }
        }
    }

    private static void AppendInline(Paragraph p, string s)
    {
        foreach (var seg in MarkdownHelper.InlineSplit(s))
        {
            var run = new Run { Text = seg.Text };
            if (seg.Kind == MarkdownHelper.Inline.Bold) p.Inlines.Add(new Bold { Inlines = { run } });
            else if (seg.Kind == MarkdownHelper.Inline.Code)
            {
                run.FontFamily = new FontFamily("Consolas");
                p.Inlines.Add(run);
            }
            else p.Inlines.Add(run);
        }
    }
}
