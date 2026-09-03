// DTOs the C# app exchanges with the engine over the 3-symbol bus pump.
//
// Inbound directives the app builds live in BusPump (System.Text.Json.Nodes);
// this file decodes the OUTBOUND EngineYield shapes (serde tag "kind",
// snake_case — see crates/oneai-bus/src/protocol.rs) into the flat event/item
// models the ViewModel renders.
//
// ChatEvent.FromBusYield maps bus yields onto the legacy event "Type" strings
// ChatViewModel.HandleEvent switches on (StreamChunk/Thinking/ToolCall/
// ToolResult/DirectAnswer/Complete/Error), so the render path is unchanged.

using System.Text.Json;
using System.Text.Json.Serialization;
using System.Collections.Generic;

namespace OneAI.Native;

public class ChatEvent
{
    public string Type { get; set; } = "";
    public string? Text { get; set; }
    public string? FinalText { get; set; }
    public string? Message { get; set; }
    public string? Id { get; set; }
    public string? Name { get; set; }
    public string? ArgsJson { get; set; }
    public string? CallId { get; set; }
    public string? ToolName { get; set; }
    public string? Content { get; set; }
    public bool Success { get; set; }
    /// <summary>Member id that produced this event (group-chat only; null for
    /// single-agent). The VM routes the event to that member's bubble.</summary>
    public string? Speaker { get; set; }

    /// <summary>Map one bus yield onto zero or more renderable events.
    /// Lifecycle/control kinds (approval_request, token_usage, speaker_turn,
    /// session_*, turn bookkeeping) are handled by the VM directly and return
    /// an empty list here.</summary>
    public static List<ChatEvent> FromBusYield(BusYield y)
    {
        var list = new List<ChatEvent>();
        var j = y.Json;
        string? speaker = StrOrNull(j, "speaker");
        switch (y.Kind)
        {
            case "stream_chunk":
                list.Add(new ChatEvent { Type = "StreamChunk", Text = StrOrNull(j, "text"), Speaker = speaker });
                break;
            case "thinking":
                list.Add(new ChatEvent { Type = "Thinking", Text = StrOrNull(j, "text"), Speaker = speaker });
                break;
            case "direct_answer":
                list.Add(new ChatEvent { Type = "DirectAnswer", Text = StrOrNull(j, "text"), Speaker = speaker });
                break;
            case "tool_calls":
                // One renderable event per call (the VM dedups by call id).
                if (j.TryGetProperty("calls", out var calls) && calls.ValueKind == JsonValueKind.Array)
                {
                    foreach (var c in calls.EnumerateArray())
                    {
                        list.Add(new ChatEvent
                        {
                            Type = "ToolCall",
                            Id = StrOrNull(c, "id"),
                            Name = StrOrNull(c, "name"),
                            ArgsJson = c.TryGetProperty("args", out var a) ? a.GetRawText() : null,
                            Speaker = speaker,
                        });
                    }
                }
                break;
            case "tool_result":
                list.Add(new ChatEvent
                {
                    Type = "ToolResult",
                    CallId = StrOrNull(j, "call_id"),
                    ToolName = StrOrNull(j, "tool_name"),
                    Content = j.TryGetProperty("output", out var o) ? StrOrNull(o, "content") : null,
                    Success = j.TryGetProperty("output", out var o2) && o2.TryGetProperty("success", out var s) && s.ValueKind == JsonValueKind.True,
                    Speaker = speaker,
                });
                break;
            case "turn_complete":
                list.Add(new ChatEvent
                {
                    Type = "Complete",
                    FinalText = j.TryGetProperty("summary", out var sm) ? StrOrNull(sm, "final_answer") : null,
                });
                break;
            case "error":
                list.Add(new ChatEvent { Type = "Error", Message = StrOrNull(j, "message") });
                break;
            // approval_request / token_usage / speaker_turn / session_* /
            // iteration_start / tool_intent / delegate_* / context_* / etc.
            // are not chat-surface events — the VM consumes the ones it needs.
        }
        return list;
    }

    private static string? StrOrNull(JsonElement parent, string prop) =>
        parent.TryGetProperty(prop, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;
}

/// <summary>Sidebar row for a saved conversation. Decoded from the
/// <c>session_list</c> yield's <c>sessions</c> array (millis shape, same as the
/// app-server session/list RPC).</summary>
public class SessionInfo
{
    public string Id { get; set; } = "";
    public string? Title { get; set; }
    public ulong MessageCount { get; set; }
    public long CreatedAtMs { get; set; }
    public long UpdatedAtMs { get; set; }
    public bool Archived { get; set; }
    public string? Workspace { get; set; }

    public static List<SessionInfo> ParseSessionList(JsonElement sessionListYield)
    {
        var result = new List<SessionInfo>();
        if (!sessionListYield.TryGetProperty("sessions", out var arr) || arr.ValueKind != JsonValueKind.Array)
            return result;
        foreach (var s in arr.EnumerateArray())
        {
            result.Add(new SessionInfo
            {
                Id = s.TryGetProperty("id", out var id) && id.ValueKind == JsonValueKind.String ? id.GetString()! : "",
                Title = s.TryGetProperty("title", out var t) && t.ValueKind == JsonValueKind.String ? t.GetString() : null,
                MessageCount = s.TryGetProperty("message_count", out var mc) && mc.TryGetUInt64(out var mcVal) ? mcVal : 0,
                CreatedAtMs = s.TryGetProperty("created_at_ms", out var ca) && ca.TryGetInt64(out var caVal) ? caVal : 0,
                UpdatedAtMs = s.TryGetProperty("updated_at_ms", out var ua) && ua.TryGetInt64(out var uaVal) ? uaVal : 0,
                Archived = s.TryGetProperty("archived", out var ar) && ar.ValueKind == JsonValueKind.True,
                Workspace = s.TryGetProperty("workspace", out var ws) && ws.ValueKind == JsonValueKind.String ? ws.GetString() : null,
            });
        }
        return result;
    }

    // UI helpers (bound from XAML — x:Bind can't null-coalesce inline).
    public string DisplayTitle => string.IsNullOrEmpty(Title) ? OneAI.Services.Loc.Str("new_chat") : Title!;
    public string Summary => string.Format(OneAI.Services.Loc.Str("msg_count_dot"), MessageCount, RelativeTime(UpdatedAtMs));
    private static string RelativeTime(long epochMsLocal)
    {
        var now = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        long mins = (now - epochMsLocal) / 60000;
        if (mins < 1) return OneAI.Services.Loc.Str("just_now");
        if (mins < 60) return string.Format(OneAI.Services.Loc.Str("minutes_ago"), mins);
        if (mins < 60 * 24) return string.Format(OneAI.Services.Loc.Str("hours_ago"), mins / 60);
        if (mins < 60 * 24 * 7) return string.Format(OneAI.Services.Loc.Str("days_ago"), mins / (60 * 24));
        var t = DateTimeOffset.FromUnixTimeMilliseconds(epochMsLocal).LocalDateTime;
        return t.ToString("MM-dd HH:mm");
    }
}

/// <summary>A replayed conversation message, decoded from the core
/// <c>Message</c> shape carried by the <c>session_loaded</c> yield:
/// <c>{role, content:[{type:"text",text:..},..], metadata:{speaker:..}}</c>.
/// Flattened to (role, joined text, speaker) so the LoadSession bubble-fold
/// logic stays unchanged.</summary>
public class ChatMessage
{
    public string Role { get; set; } = "";
    public string Text { get; set; } = "";
    /// <summary>Speaker id for assistant messages in a group-chat conversation
    /// (null for single-agent). Replayed into the bubble on session load.</summary>
    public string? Speaker { get; set; }

    public static List<ChatMessage> ParseSessionLoaded(JsonElement sessionLoadedYield)
    {
        var result = new List<ChatMessage>();
        if (!sessionLoadedYield.TryGetProperty("messages", out var arr) || arr.ValueKind != JsonValueKind.Array)
            return result;
        foreach (var m in arr.EnumerateArray())
        {
            string role = m.TryGetProperty("role", out var r) && r.ValueKind == JsonValueKind.String ? r.GetString()! : "";
            // Flatten the text content blocks (skip tool_call/tool_result/image
            // blocks — the bubble replay only renders text, same as before).
            var texts = new List<string>();
            if (m.TryGetProperty("content", out var content) && content.ValueKind == JsonValueKind.Array)
            {
                foreach (var b in content.EnumerateArray())
                {
                    if (b.TryGetProperty("type", out var ty) && ty.ValueKind == JsonValueKind.String && ty.GetString() == "text"
                        && b.TryGetProperty("text", out var tx) && tx.ValueKind == JsonValueKind.String)
                    {
                        var t = tx.GetString();
                        if (!string.IsNullOrEmpty(t)) texts.Add(t!);
                    }
                }
            }
            string? speaker = null;
            if (m.TryGetProperty("metadata", out var meta) && meta.ValueKind == JsonValueKind.Object
                && meta.TryGetProperty("speaker", out var sp) && sp.ValueKind == JsonValueKind.String)
            {
                speaker = sp.GetString();
            }
            result.Add(new ChatMessage { Role = role, Text = string.Join("\n", texts), Speaker = speaker });
        }
        return result;
    }
}

// Serialized to JSON for the BusPump.Init config (Directive::Init.config).
// Snake_case keys match oneai-bus BusEngineConfig.
public class ProviderConfig
{
    [JsonPropertyName("kind")] public string Kind { get; set; } = "openai";
    [JsonPropertyName("api_key")] public string? ApiKey { get; set; }
    [JsonPropertyName("base_url")] public string? BaseUrl { get; set; }
    [JsonPropertyName("model")] public string Model { get; set; } = "gpt-4o-mini";
    [JsonPropertyName("host")] public string? Host { get; set; }
    [JsonPropertyName("port")] public ushort? Port { get; set; }
    [JsonPropertyName("db_path")] public string? DbPath { get; set; }
    [JsonPropertyName("default_tools")] public bool DefaultTools { get; set; } = true;

    public string ToJson() => JsonSerializer.Serialize(this);
}

// ── Scenario wire DTOs (→ start_group_chat Directive scenario) ───────────
// Snake_case keys match oneai-bus BusGroupScenario. The rich UI models
// (Agent/Scenario/...) in ViewModels/ScenarioModels.cs project down to these.

public class AgentSpecDto
{
    [JsonPropertyName("id")] public string Id { get; set; } = "";
    [JsonPropertyName("name")] public string Name { get; set; } = "";
    [JsonPropertyName("system_prompt")] public string SystemPrompt { get; set; } = "";
    [JsonPropertyName("kind")] public string Kind { get; set; } = "openai";
    [JsonPropertyName("model")] public string Model { get; set; } = "";
    [JsonPropertyName("api_key")] public string? ApiKey { get; set; }
    [JsonPropertyName("base_url")] public string? BaseUrl { get; set; }
    [JsonPropertyName("color")] public string? Color { get; set; }
    [JsonPropertyName("avatar")] public string? Avatar { get; set; }
}

public class ReviewLoopSpecDto
{
    [JsonPropertyName("reviewer_id")] public string ReviewerId { get; set; } = "";
    [JsonPropertyName("approve_marker")] public string ApproveMarker { get; set; } = "";
    [JsonPropertyName("max_rounds")] public ulong MaxRounds { get; set; } = 1;
}

public class ScenarioSpecDto
{
    [JsonPropertyName("members")] public List<AgentSpecDto> Members { get; set; } = new();
    [JsonPropertyName("turn_policy")] public string TurnPolicy { get; set; } = "scripted";
    [JsonPropertyName("script_order")] public List<string>? ScriptOrder { get; set; }
    [JsonPropertyName("moderator_id")] public string? ModeratorId { get; set; }
    [JsonPropertyName("opener_agent_id")] public string? OpenerAgentId { get; set; }
    [JsonPropertyName("opener_line")] public string? OpenerLine { get; set; }
    [JsonPropertyName("title")] public string? Title { get; set; }
    [JsonPropertyName("review_loop")] public ReviewLoopSpecDto? ReviewLoop { get; set; }
    /// <summary>Engine prompt language ("en"/"zh"). Mirrors the bus BusLocale
    /// field. Pairs with an English approve_marker on the review loop.</summary>
    [JsonPropertyName("locale")] public string? Locale { get; set; }

    public string ToJson() => JsonSerializer.Serialize(this);
}
