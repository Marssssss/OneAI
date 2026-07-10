using Microsoft.UI.Xaml.Controls;
using OneAI.ViewModels;
using OneAI.Services;

namespace OneAI.Views;

public sealed partial class SettingsDialog : ContentDialog
{
    private ChatViewModel? _vm;

    public SettingsDialog()
    {
        InitializeComponent();
        Loaded += (_, _) => SyncFromVm();
    }

    public void SetVm(ChatViewModel vm) { _vm = vm; SyncFromVm(); }

    private void SyncFromVm()
    {
        if (_vm == null) return;
        var p = _vm.Provider;
        KindCombo.SelectedIndex = p.Kind switch { "openai" => 0, "anthropic" => 1, "ollama" => 2, _ => 0 };
        ModelBox.Text = p.Model;
        KeyBox.Password = p.ApiKey ?? "";
        BaseUrlBox.Text = p.BaseUrl ?? "";
    }

    private void Kind_Changed(object sender, SelectionChangedEventArgs e)
    {
        if (_vm == null || KindCombo.SelectedIndex < 0) return;
        string newKind = (string)KindCombo.SelectedItem;
        ProviderStore.ApplyPreset(_vm.Provider, newKind);
        ModelBox.Text = _vm.Provider.Model;
        BaseUrlBox.Text = _vm.Provider.BaseUrl ?? "";
    }

    protected override async void OnPrimaryButtonClick(ContentDialogButtonClickEventArgs args)
    {
        args.Cancel = true; // keep open until rebuild finishes
        if (_vm == null) return;
        _vm.Provider.Model = ModelBox.Text;
        _vm.Provider.ApiKey = KeyBox.Password;
        _vm.Provider.BaseUrl = BaseUrlBox.Text;
        _vm.SaveConfig();
        await _vm.RebuildApp();
        Hide();
    }
}
