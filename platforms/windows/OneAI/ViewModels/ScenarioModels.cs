// Rich multi-agent scenario models — port of macOS Sources/Models.swift.
// Agent / Scenario / TopicField / DebriefConfig / ReviewLoopConfig are the
// editable UI models persisted to JSON by ScenarioStore; SpecView projects
// them down to the FFI wire DTOs (Native/ScenarioSpecDto) the c_facade
// `oneai_create_group_session` consumes. An Agent is an AI persona — the
// user is an implicit extra participant, never stored as an Agent.

using System.Text.Json.Serialization;
using OneAI.Native;

namespace OneAI.ViewModels;

/// <summary>Turn policy for a scenario — mirrors the engine TurnPolicy.</summary>
public enum TurnPolicy
{
    Scripted,     // fixed order after each user input
    RoundRobin,   // members cycle in list order
    Moderator,    // a moderator member picks the next speaker
}

public static class TurnPolicyExt
{
    public static string Label(this TurnPolicy t) => t switch
    {
        TurnPolicy.Scripted => "脚本式",
        TurnPolicy.RoundRobin => "轮询",
        TurnPolicy.Moderator => "主持人选择",
        _ => t.ToString(),
    };
    /// <summary>The string the FFI scenario_json turn_policy expects.</summary>
    public static string SpecValue(this TurnPolicy t) => t switch
    {
        TurnPolicy.Scripted => "scripted",
        TurnPolicy.RoundRobin => "roundrobin",
        TurnPolicy.Moderator => "moderator",
        _ => "scripted",
    };
}

/// <summary>An AI persona in a scenario.</summary>
public class Agent
{
    public string Id { get; set; } = "";
    public string Name { get; set; } = "";
    /// <summary>Short role label.</summary>
    public string Role { get; set; } = "";
    public string SystemPrompt { get; set; } = "";
    /// <summary>Model name. null ⇒ inherit the app's configured model.</summary>
    public string? Model { get; set; }
    /// <summary>Hex accent color, e.g. "#4D6BFE".</summary>
    public string Color { get; set; } = "#4D6BFE";
    /// <summary>Icon glyph name (Segoe Fluent Icons / emoji).</summary>
    public string Avatar { get; set; } = "";
    // Provider overrides — null ⇒ inherit the app's configured provider.
    public string? Kind { get; set; }
    public string? ApiKey { get; set; }
    public string? BaseUrl { get; set; }

    /// <summary>Build the FFI AgentSpecDto, inheriting provider config from the
    /// app settings where the agent leaves it null. When <paramref name="background"/>
    /// is non-empty it is appended to the persona system prompt as scenario
    /// background — so every member KNOWS the topic (the interviewer asks
    /// targeted questions about the position; the coach critiques in that
    /// context) rather than asking the user to supply it.</summary>
    public AgentSpecDto SpecDto(string defaultKind, string defaultApiKey, string defaultBaseUrl,
                                string defaultModel, string background)
    {
        var prompt = string.IsNullOrEmpty(background) ? SystemPrompt : $"{SystemPrompt}\n\n{background}";
        return new AgentSpecDto
        {
            Id = Id,
            Name = Name,
            SystemPrompt = prompt,
            Kind = string.IsNullOrEmpty(Kind) ? defaultKind : Kind!,
            Model = string.IsNullOrEmpty(Model) ? defaultModel : Model!,
            ApiKey = string.IsNullOrEmpty(ApiKey) ? (string.IsNullOrEmpty(defaultApiKey) ? null : defaultApiKey) : ApiKey,
            BaseUrl = string.IsNullOrEmpty(BaseUrl) ? (string.IsNullOrEmpty(defaultBaseUrl) ? null : defaultBaseUrl) : BaseUrl,
            Color = Color,
            Avatar = Avatar,
        };
    }
}

/// <summary>One input field the user fills before starting a scenario
/// (e.g. "应聘岗位"). A scenario with topic fields non-empty prompts the
/// inline intake page on start; the collected values are baked into every
/// member's system prompt as background and into the session title.
///
/// <see cref="VisibleTo"/> controls per-member visibility: null means the
/// value is folded into ALL members' background (default — e.g. the
/// interview's "应聘岗位" is shared context). A non-empty array restricts
/// the value to only those member ids (e.g. the interviewee's "项目经历"
/// is ["coach"] so the coach can give specific advice but the interviewer
/// never sees it and can't ask about it).</summary>
public class TopicField
{
    public string Id { get; set; } = "";
    public string Label { get; set; } = "";
    public string? Placeholder { get; set; }
    /// <summary>Member ids allowed to see this field's value in their system
    /// prompt. null = all members. Non-empty = only those members.</summary>
    public List<string>? VisibleTo { get; set; }

    /// <summary>One-line hint for the intake form: blank when visible to all,
    /// else "· 仅 {ids} 可见" (member ids; the editor uses member names but the
    /// intake field only has ids — readable enough for the single-member case
    /// the presets use, e.g. projects → coach).</summary>
    public string VisibilityHint =>
        (VisibleTo is null || VisibleTo.Count == 0) ? "" : "· 仅 " + string.Join("/", VisibleTo) + " 可见";
}

/// <summary>Optional "debrief" phase config. After the user triggers it (a
/// top-bar button), the turn policy switches to a scripted order containing
/// only <see cref="DebriefMemberId"/> (e.g. coach), and the
/// <see cref="SummaryPrompt"/> is sent to that member for a full-session
/// summary. The user can then keep asking that member follow-up questions —
/// the other members (e.g. the interviewer) no longer participate.</summary>
public class DebriefConfig
{
    public string ButtonLabel { get; set; } = "结束";
    public string SummaryPrompt { get; set; } = "";
    public string DebriefMemberId { get; set; } = "";
}

/// <summary>Optional review-revise loop (writing workshop: writer drafts →
/// editor reviews → writer revises → editor re-reviews → … until the editor
/// approves or max_rounds is reached). The reviewer's persona prompt must
/// instruct it to emit <see cref="ApproveMarker"/> when satisfied. null =
/// single pass, no loop.</summary>
public class ReviewLoopConfig
{
    public string ReviewerId { get; set; } = "";
    public string ApproveMarker { get; set; } = "";
    public int MaxRounds { get; set; } = 1;
}

/// <summary>A multi-agent scenario — a cast of personas + a turn policy.</summary>
public class Scenario
{
    public string Id { get; set; } = "";
    public string Name { get; set; } = "";
    /// <summary>Icon glyph name for the sidebar.</summary>
    public string Icon { get; set; } = "";
    public List<Agent> Agents { get; set; } = new();
    public TurnPolicy TurnPolicy { get; set; } = TurnPolicy.Scripted;
    /// <summary>Scripted — member ids after each user input.</summary>
    public List<string>? ScriptOrder { get; set; }
    /// <summary>Moderator — member id that picks next.</summary>
    public string? ModeratorId { get; set; }
    /// <summary>Who opens; null = user first.</summary>
    public string? OpenerAgentId { get; set; }
    public string? OpenerLine { get; set; }
    /// <summary>Topic-intake form fields. Non-empty ⇒ intake page on start.</summary>
    public List<TopicField>? TopicFields { get; set; }
    public DebriefConfig? Debrief { get; set; }
    public ReviewLoopConfig? ReviewLoop { get; set; }

    /// <summary>Build the FFI ScenarioSpecDto, inheriting provider config per
    /// agent. topicValues (keyed by field id) is rendered into a per-member
    /// background block ("【场景背景】\n岗位: X\n…") folded into each member's
    /// system prompt, and into the session title ("场景名·v1·v2…").
    ///
    /// Per-member visibility: a field with VisibleTo is only folded into the
    /// background of the listed members (e.g. the interviewee's "项目经历"
    /// with VisibleTo=["coach"] reaches the coach but NOT the interviewer).
    /// Fields with VisibleTo==null reach everyone.</summary>
    public ScenarioSpecDto SpecDto(string defaultKind, string defaultApiKey, string defaultBaseUrl,
                                   string defaultModel, Dictionary<string, string>? topicValues)
    {
        var fields = TopicFields ?? new();
        // Pre-render the per-field (label, value) pairs, dropping blanks.
        var pairs = new List<(TopicField field, string value)>();
        foreach (var f in fields)
        {
            var v = (topicValues?.GetValueOrDefault(f.Id) ?? "").Trim();
            if (!string.IsNullOrEmpty(v)) pairs.Add((f, v));
        }
        // Title suffix uses every non-blank value regardless of visibility.
        var titleParts = pairs.Select(p => p.value).ToList();
        var title = titleParts.Count == 0 ? Name : $"{Name}·" + string.Join("·", titleParts);

        var members = Agents.Select(agent =>
        {
            // Background for THIS member: only fields it's allowed to see.
            var visible = pairs.Where(p =>
            {
                if (p.field.VisibleTo is null || p.field.VisibleTo.Count == 0) return true; // null → all
                return p.field.VisibleTo.Contains(agent.Id);
            }).ToList();
            var lines = visible.Select(p => $"{p.field.Label}: {p.value}").ToList();
            var background = lines.Count == 0 ? "" : "【场景背景】\n" + string.Join("\n", lines);
            return agent.SpecDto(defaultKind, defaultApiKey, defaultBaseUrl, defaultModel, background);
        }).ToList();

        ReviewLoopSpecDto? rl = null;
        if (ReviewLoop is { } r)
            rl = new ReviewLoopSpecDto { ReviewerId = r.ReviewerId, ApproveMarker = r.ApproveMarker, MaxRounds = (ulong)r.MaxRounds };

        return new ScenarioSpecDto
        {
            Members = members,
            TurnPolicy = TurnPolicy.SpecValue(),
            ScriptOrder = ScriptOrder,
            ModeratorId = ModeratorId,
            OpenerAgentId = OpenerAgentId,
            OpenerLine = OpenerLine,
            Title = title,
            ReviewLoop = rl,
        };
    }

    /// <summary>Resolve an agent by id (speaker-name + color lookup).</summary>
    public Agent? AgentById(string id) => Agents.FirstOrDefault(a => a.Id == id);
}
