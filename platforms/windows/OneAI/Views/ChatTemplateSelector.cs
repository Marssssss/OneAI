// DataTemplateSelector — picks the user vs assistant bubble by ChatItem.Kind.
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using OneAI.ViewModels;

namespace OneAI.Views;

public class ChatTemplateSelector : DataTemplateSelector
{
    public DataTemplate? UserTemplate { get; set; }
    public DataTemplate? AssistantTemplate { get; set; }

    protected override DataTemplate? SelectTemplateCore(object item, DependencyObject container)
    {
        if (item is ChatItem c)
            return c.Kind == ChatKind.User ? UserTemplate : AssistantTemplate;
        return base.SelectTemplateCore(item, container);
    }
}
