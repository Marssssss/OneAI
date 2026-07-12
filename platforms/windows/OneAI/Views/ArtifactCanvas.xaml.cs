using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using OneAI.Services;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage.Pickers;
using Windows.Storage;

namespace OneAI.Views;

public sealed partial class ArtifactCanvas : UserControl
{
    public ArtifactStore Store { get; }

    public ArtifactCanvas(ArtifactStore store)
    {
        Store = store;
        InitializeComponent();
        Store.PropertyChanged += (_, e) =>
        {
            if (e.PropertyName == nameof(Store.Selected)) _ = DispatcherQueue.TryEnqueue(RefreshContent);
            else if (e.PropertyName == nameof(Store.Artifacts)) _ = DispatcherQueue.TryEnqueue(RefreshContent);
        };
        Loaded += (_, _) => RefreshContent();
    }

    private void RefreshContent()
    {
        var sel = Store.Selected ?? (Store.Artifacts.Count > 0 ? Store.Artifacts[0] : null);
        Content.Text = sel?.Content ?? "";
        LangLabel.Text = sel?.Lang ?? "";
    }

    private void Copy_Click(object sender, RoutedEventArgs e)
    {
        if (Store.Selected is { } a)
        {
            var dp = new DataPackage { RequestedOperation = DataPackageOperation.Copy };
            dp.SetText(a.Content);
            Clipboard.SetContent(dp);
        }
    }

    private async void Export_Click(object sender, RoutedEventArgs e)
    {
        if (Store.Selected is not { } a || App.MainWindowRef is not { } win) return;
        var picker = new FileSavePicker();
        picker.SuggestedStartLocation = PickerLocationId.DocumentsLibrary;
        picker.SuggestedFileName = SuggestedFilename(a);
        if (!string.IsNullOrEmpty(ExtFor(a.Lang)))
            picker.FileTypeChoices.Add(a.Title, new List<string> { "." + ExtFor(a.Lang) });
        else
            picker.FileTypeChoices.Add("文本", new List<string> { ".txt" });
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(win));
        var file = await picker.PickSaveFileAsync();
        if (file != null) await FileIO.WriteTextAsync(file, a.Content);
    }

    private static string SuggestedFilename(Artifact a)
    {
        var ext = ExtFor(a.Lang);
        var dot = a.Title.IndexOf('.');
        var name = dot > 0 ? a.Title.Substring(0, dot) : a.Title;
        return ext.Length == 0 ? name : $"{name}.{ext}";
    }

    private static string ExtFor(string lang) => lang.ToLowerInvariant() switch
    {
        "rust" or "rs" => "rs",
        "swift" => "swift",
        "python" or "py" => "py",
        "javascript" or "js" => "js",
        "typescript" or "ts" => "ts",
        "shell" or "bash" or "sh" => "sh",
        "json" => "json",
        "yaml" or "yml" => "yml",
        "markdown" or "md" => "md",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        "go" => "go",
        "java" => "java",
        "kotlin" or "kt" => "kt",
        _ => lang.Length == 0 ? "" : lang,
    };
}
