// ArtifactStore — shared canvas state (port of macOS ArtifactCanvas.swift's
// ArtifactStore). Code blocks >600 chars (or anything the user clicks "在画布
// 打开" on) are promoted to a canvas tab so long content doesn't挤占对话流.
// ChatView renders the docked ArtifactCanvas panel when Visible.

using System.Collections.ObjectModel;
using OneAI.ViewModels;

namespace OneAI.Services;

public class Artifact : ObservableObject
{
    public string Title { get; set; } = "代码";
    public string Lang { get; set; } = "";
    public string Content { get; set; } = "";
}

public class ArtifactStore : ObservableObject
{
    public ObservableCollection<Artifact> Artifacts { get; } = new();

    private Artifact? _selected;
    public Artifact? Selected { get => _selected; set => SetProperty(ref _selected, value); }

    private bool _visible;
    public bool Visible { get => _visible; set => SetProperty(ref _visible, value); }

    public void Open(string lang, string content)
    {
        var existing = Artifacts.FirstOrDefault(a => a.Content == content && a.Lang == lang);
        if (existing != null)
        {
            Selected = existing;
        }
        else
        {
            var title = string.IsNullOrEmpty(lang) ? "代码" : lang;
            var a = new Artifact { Title = title, Lang = lang, Content = content };
            Artifacts.Add(a);
            Selected = a;
        }
        Visible = true;
    }

    public void Close(Artifact artifact)
    {
        Artifacts.Remove(artifact);
        if (Selected == artifact) Selected = Artifacts.FirstOrDefault();
        if (Artifacts.Count == 0) Visible = false;
    }
}
