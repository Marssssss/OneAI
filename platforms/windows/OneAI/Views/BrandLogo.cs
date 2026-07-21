using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using Windows.UI;

namespace OneAI.Views;

/// <summary>The OneAI wordmark as extruded 3D pixel tiles — the WinUI counterpart
/// of the macOS `BrandLogo` (platforms/macos/Sources/Views.swift) and the TUI's
/// colorful block-art brand. Each filled cell is a raised tile (top-lit gradient
/// face) so the logo reads as dimensional blocks rather than flat letters. Same
/// 7×5 per-character bitmap and the same per-character hues (O/n/e/A/I =
/// coral/teal/muted-blue/sage/gold) so the brand stays consistent across
/// surfaces. Set <see cref="Scale"/> larger for the welcome screen, smaller for
/// the top bar.</summary>
public sealed class BrandLogo : Canvas
{
    public static readonly DependencyProperty ScaleProperty =
        DependencyProperty.Register(nameof(Scale), typeof(double), typeof(BrandLogo),
            new PropertyMetadata(5.0, OnScaleChanged));

    /// <summary>Edge length of one pixel tile (width). Height = Scale * Aspect.</summary>
    public double Scale
    {
        get => (double)GetValue(ScaleProperty);
        set => SetValue(ScaleProperty, value);
    }

    // Per-character gradient hues for "OneAI" — mirrors macOS Brand.charColors.
    private static readonly uint[] CharColors = { 0xD07C7C, 0x62B0BC, 0x6EA0C8, 0x96C47A, 0xD6B660 };

    // 5 chars × 5 rows × 7 cols. Each char's leading column is empty, giving
    // natural intra-word spacing (mirrors the macOS/TUI pattern verbatim).
    private static readonly bool[][][] Patterns =
    [
        // O
        [[false,true,true,true,true,true,true],
         [false,true,true,false,false,true,true],
         [false,true,true,false,false,true,true],
         [false,true,true,false,false,true,true],
         [false,true,true,true,true,true,true]],
        // n
        [[false,true,true,true,true,true,true],
         [false,true,true,false,false,true,true],
         [false,true,true,false,false,true,true],
         [false,true,true,false,false,true,true],
         [false,true,true,false,false,true,true]],
        // e
        [[false,true,true,true,true,true,true],
         [false,true,true,false,false,false,false],
         [false,true,true,true,true,false,false],
         [false,true,true,false,false,false,false],
         [false,true,true,true,true,true,true]],
        // A
        [[false,false,false,true,true,false,false],
         [false,false,true,true,true,true,false],
         [false,true,true,true,true,true,true],
         [false,true,true,false,false,true,true],
         [false,true,true,false,false,true,true]],
        // I
        [[false,true,true,true,true,true,true],
         [false,false,false,true,true,false,false],
         [false,false,false,true,true,false,false],
         [false,false,false,true,true,false,false],
         [false,true,true,true,true,true,true]],
    ];

    private const double Aspect = 1.4;   // tile height / width — squares each letter
    private const double GapFactor = 0.2; // gap between tiles, as a fraction of Scale

    public BrandLogo()
    {
        Loaded += (_, _) => Redraw();
        SizeChanged += (_, _) => Redraw();
    }

    private static void OnScaleChanged(DependencyObject d, DependencyPropertyChangedEventArgs e)
    {
        if (d is BrandLogo b) b.Redraw();
    }

    private void Redraw()
    {
        Children.Clear();
        double cell = Scale;
        if (cell <= 0) return;
        double h = cell * Aspect;
        double gap = Math.Max(1.0, cell * GapFactor);
        double stepX = cell + gap;
        double stepY = h + gap;
        double r = cell * 0.22;

        for (int ch = 0; ch < 5; ch++)
        {
            // Inter-char gap: 7 cols * stepX + one extra gap before the next char.
            double charX = ch * (7 * stepX + gap);
            var baseColor = FromHex(CharColors[ch]);
            var face = FaceBrush(baseColor);
            for (int row = 0; row < 5; row++)
            for (int col = 0; col < 7; col++)
            {
                if (!Patterns[ch][row][col]) continue;
                double x = charX + col * stepX;
                double y = row * stepY;
                var tile = new Rectangle
                {
                    Width = cell,
                    Height = h,
                    RadiusX = r,
                    RadiusY = r,
                    Fill = face,
                };
                // Soft drop shadow to read as a raised block (lightweight — no
                // DropShadow XAML since this stays static after first render).
                Canvas.SetLeft(tile, x);
                Canvas.SetTop(tile, y);
                Children.Add(tile);
            }
        }

        // Size the Canvas so layout (alignment / the welcome screen's centering)
        // works — a Canvas otherwise has no intrinsic content size.
        double totalW = 5 * (7 * stepX + gap) - gap;
        double totalH = 5 * stepY - gap;
        Width = totalW;
        Height = totalH;
    }

    /// <summary>Top-lit vertical gradient face for a tile: a lighter top stop →
    /// the base hue → a darker bottom stop. Approximates the macOS
    /// `mixedLight()→base→darker` face without a per-tile color-space blend.</summary>
    private static LinearGradientBrush FaceBrush(Windows.UI.Color base_)
    {
        var lighter = Windows.UI.Color.FromArgb(255,
            (byte)Math.Min(255, base_.R + (255 - base_.R) * 35 / 100),
            (byte)Math.Min(255, base_.G + (255 - base_.G) * 35 / 100),
            (byte)Math.Min(255, base_.B + (255 - base_.B) * 35 / 100));
        var darker = Windows.UI.Color.FromArgb(255,
            (byte)(base_.R * 55 / 100),
            (byte)(base_.G * 55 / 100),
            (byte)(base_.B * 55 / 100));
        var b = new LinearGradientBrush
        {
            StartPoint = new Windows.Foundation.Point(0, 0),
            EndPoint = new Windows.Foundation.Point(0, 1),
        };
        b.GradientStops.Add(new GradientStop { Color = lighter, Offset = 0 });
        b.GradientStops.Add(new GradientStop { Color = base_, Offset = 0.5 });
        b.GradientStops.Add(new GradientStop { Color = darker, Offset = 1 });
        return b;
    }

    private static Windows.UI.Color FromHex(uint v) =>
        Windows.UI.Color.FromArgb(255, (byte)(v >> 16), (byte)(v >> 8), (byte)v);
}
