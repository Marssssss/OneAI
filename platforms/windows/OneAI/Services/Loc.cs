// Loc — single-mechanism string localization for the Windows app.
//
// One static `Str(key)` that returns the zh or en string for the effective
// locale (AppLocaleHelper.Current). Used directly from .cs (Loc.Str("copy"))
// and from XAML via x:Bind to a static call with a literal arg
// (Content="{x:Bind Loc.Str('copy'), Mode=OneTime}"). Avoids .resw / PRI
// generation for an unpackaged WinUI3 app (which has no package identity to
// back a PRI resource map by default) and keeps zh/en in one auditable table.
//
// zh is the historical text (preserved for Chinese-locale users); en is the
// translation that ships for English-locale users. Add keys here as chrome
// strings are migrated from hard-coded literals.

namespace OneAI.Services;

public static class Loc
{
    /// <summary>Localized string for the effective locale, by key.</summary>
    public static string Str(string key)
    {
        bool en = AppLocaleHelper.Current == AppLocale.En;
        return (en ? En : Zh).TryGetValue(key, out var v) ? v
             : Zh.TryGetValue(key, out var z) ? z
             : key; // unknown key → return the key itself (never empty)
    }

    // ── Chinese (historical) ────────────────────────────────────────────
    private static readonly Dictionary<string, string> Zh = new()
    {
        ["copy"] = "复制",
        ["copied"] = "已复制",
        ["open_on_canvas"] = "在画布打开",
        ["new_chat"] = "新对话",
        ["new_scenario"] = "新场景",
        ["delete_chat"] = "删除会话",
        ["delete_chat_confirm"] = "确定删除这个会话?历史无法恢复。",
        ["delete"] = "删除",
        ["cancel"] = "取消",
        ["retry"] = "重试",
        ["regenerate"] = "重新生成",
        ["share"] = "分享",
        ["edit_and_resend"] = "编辑并重发",
        ["thinking"] = "思考中…",
        ["thought"] = "已深度思考",
        ["scenario_failed"] = "场景启动失败: ",
        ["speaking"] = "正在发言…",
        ["your_turn"] = "轮到你 — 发送你的回答",
        ["just_now"] = "刚刚",
        ["minutes_ago"] = "{0} 分钟前",
        ["hours_ago"] = "{0} 小时前",
        ["days_ago"] = "{0} 天前",
        ["msg_count_dot"] = "{0} 条 · {1}",
        ["text_file"] = "文本",
        ["you"] = "你",
        ["scenarios"] = "场景",
        ["recent_chats"] = "最近会话",
        ["actions"] = "操作",
        ["settings"] = "设置",
        ["open_settings"] = "打开设置",
        ["close"] = "关闭",
        ["save"] = "保存",
        ["n_agents"] = "{0} 个智能体",
        ["n_msgs"] = "{0} 条",
        ["load_older"] = "加载更早消息",
        ["back_to_bottom"] = "回到底部",
        ["provider_settings"] = "Provider 设置",
        ["code"] = "代码",
        ["starter_summarize"] = "帮我总结一段笔记的核心要点",
        ["starter_rust_json"] = "用 Rust 写一个读取 JSON 的命令行小工具",
        ["starter_agent_rag"] = "解释一下 Agent 与 RAG 的区别",
        ["starter_rewrite"] = "把这段话改写得更简洁专业",
    };

    // ── English ──────────────────────────────────────────────────────────
    private static readonly Dictionary<string, string> En = new()
    {
        ["copy"] = "Copy",
        ["copied"] = "Copied",
        ["open_on_canvas"] = "Open on canvas",
        ["new_chat"] = "New chat",
        ["new_scenario"] = "New scenario",
        ["delete_chat"] = "Delete chat",
        ["delete_chat_confirm"] = "Delete this chat? History cannot be recovered.",
        ["delete"] = "Delete",
        ["cancel"] = "Cancel",
        ["retry"] = "Retry",
        ["regenerate"] = "Regenerate",
        ["share"] = "Share",
        ["edit_and_resend"] = "Edit & resend",
        ["thinking"] = "Thinking…",
        ["thought"] = "Thought",
        ["scenario_failed"] = "Scenario failed to start: ",
        ["speaking"] = "is speaking…",
        ["your_turn"] = "Your turn — send your answer",
        ["just_now"] = "just now",
        ["minutes_ago"] = "{0} min ago",
        ["hours_ago"] = "{0} hours ago",
        ["days_ago"] = "{0} days ago",
        ["msg_count_dot"] = "{0} msgs · {1}",
        ["text_file"] = "Text",
        ["you"] = "You",
        ["scenarios"] = "Scenarios",
        ["recent_chats"] = "Recent chats",
        ["actions"] = "Actions",
        ["settings"] = "Settings",
        ["open_settings"] = "Open settings",
        ["close"] = "Close",
        ["save"] = "Save",
        ["n_agents"] = "{0} agents",
        ["n_msgs"] = "{0} msgs",
        ["load_older"] = "Load older messages",
        ["back_to_bottom"] = "Back to bottom",
        ["provider_settings"] = "Provider settings",
        ["code"] = "code",
        ["starter_summarize"] = "Summarize the key points of a note for me",
        ["starter_rust_json"] = "Write a Rust CLI tool that reads JSON",
        ["starter_agent_rag"] = "Explain the difference between an Agent and RAG",
        ["starter_rewrite"] = "Rewrite this more concisely and professionally",
    };
}
