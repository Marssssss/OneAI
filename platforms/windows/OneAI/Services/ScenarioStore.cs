// ScenarioStore — CRUD + persistence for Agents & Scenarios, plus the built-in
// preset scenarios (面试演练 / 语言伙伴 / 辩论 / 写作工坊 / 头脑风暴). Port of
// macOS Sources/AgentStore.swift. Persists to %LOCALAPPDATA%\OneAI\
// oneai_scenarios.json (unpackaged apps can't use ApplicationData) so
// user-edited scenarios survive restarts.

using System.Collections.ObjectModel;
using System.Text.Json;
using System.Text.Json.Serialization;
using OneAI.ViewModels;

namespace OneAI.Services;

/// <summary>On-disk wrapper: a schema version + the scenario list. Bumping
/// <see cref="SchemaVersion"/> re-seeds the built-in presets (preserving
/// user-added custom scenarios) so structural preset changes (new fields,
/// debrief config) reach users whose disk already holds an older file.</summary>
public class ScenarioStoreData
{
    [JsonPropertyName("version")] public int Version { get; set; }
    [JsonPropertyName("scenarios")] public List<Scenario> Scenarios { get; set; } = new();
}

public class ScenarioStore : ObservableObject
{
    /// <summary>Bump when the preset structure changes — triggers a preset
    /// re-seed on load. v6: presets are now locale-aware (zh / en); an
    /// English-system user re-seeds to the English preset set (names, prompts,
    /// approve marker).</summary>
    private const int SchemaVersion = 6;

    public ObservableCollection<Scenario> Scenarios { get; } = new();

    private static string FileUrl => AppPaths.ScenariosPath;

    public ScenarioStore()
    {
        Load();
        if (Scenarios.Count == 0)
        {
            foreach (var s in PresetsFor(AppLocaleHelper.Current)) Scenarios.Add(s);
            Save();
        }
    }

    // ── CRUD ───────────────────────────────────────────────────────────
    public void Upsert(Scenario scenario)
    {
        var existing = Scenarios.FirstOrDefault(s => s.Id == scenario.Id);
        if (existing is null) Scenarios.Add(scenario);
        else
        {
            int idx = Scenarios.IndexOf(existing);
            Scenarios[idx] = scenario;
        }
        Save();
    }

    public void Delete(Scenario scenario)
    {
        for (int i = 0; i < Scenarios.Count; i++)
            if (Scenarios[i].Id == scenario.Id) { Scenarios.RemoveAt(i); break; }
        Save();
    }

    // ── Persistence ─────────────────────────────────────────────────────
    private void Load()
    {
        string? json = null;
        try { json = File.ReadAllText(FileUrl); } catch { /* first run */ }
        if (string.IsNullOrEmpty(json)) return;

        try
        {
            var wrapped = JsonSerializer.Deserialize<ScenarioStoreData>(json);
            if (wrapped is { })
            {
                Scenarios.Clear();
                foreach (var s in wrapped.Scenarios) Scenarios.Add(s);
                if (wrapped.Version < SchemaVersion) ReseedPresets();
                return;
            }
        }
        catch { /* fall through to legacy bare-array */ }

        // Legacy format: bare [Scenario] (pre-wrapper). Re-seed to migrate.
        try
        {
            var legacy = JsonSerializer.Deserialize<List<Scenario>>(json);
            if (legacy is { })
            {
                Scenarios.Clear();
                foreach (var s in legacy) Scenarios.Add(s);
                ReseedPresets();
            }
        }
        catch { /* corrupt — leave empty, presets seeded by caller check */ }
    }

    private void Save()
    {
        var wrapped = new ScenarioStoreData { Version = SchemaVersion };
        foreach (var s in Scenarios) wrapped.Scenarios.Add(s);
        try { File.WriteAllText(FileUrl, JsonSerializer.Serialize(wrapped)); } catch { }
    }

    /// <summary>Replace every built-in preset (id starts with "preset-") with
    /// the current code-defined version, leaving user-added custom scenarios
    /// untouched.</summary>
    private void ReseedPresets()
    {
        var customs = Scenarios.Where(s => !s.Id.StartsWith("preset-")).ToList();
        Scenarios.Clear();
        foreach (var p in PresetsFor(AppLocaleHelper.Current)) Scenarios.Add(p);
        foreach (var c in customs) Scenarios.Add(c);
        Save();
    }

    // ── Speaker metadata (for rendering speaker names/colors in a running
    //    conversation). Returns (name, color, avatar). ──────────────────
    public static (string Name, string Color, string Avatar) SpeakerMeta(string speakerId, Scenario? scenario)
    {
        if (speakerId == "user" || string.IsNullOrEmpty(speakerId))
            return (Loc.Str("you"), "#8A8A8A", "");
        if (scenario?.AgentById(speakerId) is { } a)
            return (a.Name, a.Color, a.Avatar);
        return (speakerId, "#8A8A8A", "");
    }

    // ── Built-in presets ───────────────────────────────────────────────
    // IDs are stable so a user can edit a preset (it overwrites in place via
    // Upsert). The locale selects which language variant of the names / prompts
    // / markers ships. `zh` is the historical Chinese set; `en` ships English
    // names + English persona prompts + an English review-loop marker
    // ("approved" ↔ "定稿") so an English-locale scenario drives the LLM in
    // English end-to-end (the engine locale in ScenarioSpecDto.Locale matches
    // — see AppLocaleHelper / Scenario.SpecDto).
    public static Scenario[] PresetsFor(AppLocale locale) =>
        locale == AppLocale.En ? EnPresets : ZhPresets;

    // Prompts ported verbatim from macOS AgentStore.swift zhPresets.
    public static Scenario[] ZhPresets => new[]
    {
        new Scenario
        {
            Id = "preset-interview",
            Name = "面试演练",
            Icon = "🎤",
            Agents = new()
            {
                new Agent
                {
                    Id = "interviewer", Name = "面试官", Role = "提问", Color = "#4D6BFE", Avatar = "👨‍💼",
                    SystemPrompt = "你是一名资深技术面试官。你的任务是就用户应聘的岗位提出有深度、循序渐进的问题。每次只问一个问题，等用户回答后再追问或换方向。不要替用户回答，不要给出指导性评价——那是指导员的工作。语气专业、克制。",
                },
                new Agent
                {
                    Id = "coach", Name = "指导员", Role = "点评", Color = "#3B8C5A", Avatar = "🎯",
                    SystemPrompt = "你是一名面试指导教练。在用户每次回答后，你给出针对性点评：哪里回答得好、哪里不足、可以怎样改进，并给出一个简短的「行动建议」。点评要具体、可执行。不要替用户回答面试官的问题。若【场景背景】中提供了候选人的项目经历，请结合其项目内容给出项目级、有针对性的建议（这些信息面试官看不到，仅你用于点评）。",
                },
            },
            TurnPolicy = TurnPolicy.Scripted,
            ScriptOrder = new() { "coach", "interviewer" },
            OpenerAgentId = "interviewer",
            OpenerLine = "我们开始面试吧。请先做个简短的自我介绍。",
            TopicFields = new()
            {
                new TopicField { Id = "position", Label = "应聘岗位", Placeholder = "如:前端工程师 3 年" },
                new TopicField { Id = "company", Label = "目标公司", Placeholder = "如:字节跳动" },
                new TopicField { Id = "level", Label = "职位级别", Placeholder = "如:社招 P5" },
                // 项目经历只注入指导员的背景（visibleTo:["coach"]），面试官看不到、
                // 也不会据此提问，但指导员能据此给出项目级建议。
                new TopicField { Id = "projects", Label = "项目经历", Placeholder = "如:电商订单中台,负责库存与支付模块;可写多条", VisibleTo = new() { "coach" } },
            },
            Debrief = new DebriefConfig
            {
                ButtonLabel = "结束面试",
                SummaryPrompt = "（面试结束）请以指导员身份,对候选人本次面试的整体表现进行全场总结:亮点、不足、可改进之处,并给出后续学习与练习建议。",
                DebriefMemberId = "coach",
            },
        },
        new Scenario
        {
            Id = "preset-language-partner",
            Name = "语言伙伴",
            Icon = "🌐",
            Agents = new()
            {
                new Agent
                {
                    Id = "partner", Name = "语言伙伴", Role = "陪练", Color = "#B68C2E", Avatar = "🗣",
                    SystemPrompt = "你是一名外语陪练伙伴。与用户进行自然对话，根据用户水平调整难度，适时温和地纠正用词与语法错误，并给出更地道的说法。一次只推进话题一步。请使用【场景背景】中“语言·话题”所指定的语言与用户交谈；若用户未指定语言，默认用英语。",
                },
            },
            TurnPolicy = TurnPolicy.RoundRobin,
            OpenerAgentId = "partner",
            OpenerLine = "请按背景中指定的语言与话题自然开场，与用户聊起来。",
            TopicFields = new() { new TopicField { Id = "topic", Label = "语言·话题", Placeholder = "如:中文·旅行" } },
        },
        new Scenario
        {
            Id = "preset-debate",
            Name = "辩论赛",
            Icon = "⚖️",
            Agents = new()
            {
                new Agent { Id = "pro", Name = "正方辩手", Role = "支持", Color = "#4D6BFE", Avatar = "👍", SystemPrompt = "你是正方辩手，从支持立场出发进行论证，观点鲜明、论据有力。" },
                new Agent { Id = "con", Name = "反方辩手", Role = "反对", Color = "#E5484D", Avatar = "👎", SystemPrompt = "你是反方辩手，从反对立场出发进行论证，针锋相对、有理有据。" },
                new Agent { Id = "moderator", Name = "主持人", Role = "调度", Color = "#8A8A8A", Avatar = "⚖️", SystemPrompt = "你是辩论主持人。首轮请点明今日辩题并邀请正方先开始立论；其后每轮只回复下一个发言者的角色 id（pro/con/user），不要回复其他内容，并确保双方均衡发言。" },
            },
            TurnPolicy = TurnPolicy.Moderator,
            ModeratorId = "moderator",
            OpenerAgentId = "moderator",
            OpenerLine = "请开场:点明今日辩题,邀请正方先开始立论。",
            TopicFields = new() { new TopicField { Id = "motion", Label = "辩论主题", Placeholder = "如:AI 是否会取代人类" } },
        },
        new Scenario
        {
            Id = "preset-writing-workshop",
            Name = "写作工坊",
            Icon = "✏️",
            Agents = new()
            {
                new Agent
                {
                    Id = "writer", Name = "写手", Role = "起草", Color = "#4D6BFE", Avatar = "✍️",
                    SystemPrompt = "你是写手，根据用户主题起草初稿，注重结构与表达。当编辑给出修改意见时，请据此修改你的稿件，并输出完整稿件，不要只描述改动。",
                },
                new Agent
                {
                    Id = "editor", Name = "编辑", Role = "润色", Color = "#3B8C5A", Avatar = "📝",
                    SystemPrompt = "你是编辑，对写手的稿件给出具体、可执行的修改建议并说明理由。每轮审阅后必须明确表态：若稿件已达到可定稿的质量，请在回复中包含「定稿」二字以示通过；否则指出需修改之处，交回写手继续修改。不要替写手重写全文。",
                },
            },
            TurnPolicy = TurnPolicy.Scripted,
            ScriptOrder = new() { "writer", "editor" },
            // 写手起草 → 编辑审阅 → 写手修改 → 编辑复审 → …，直到编辑在回复中
            // 包含「定稿」或达到最多 3 轮（含首轮），防止无限修改。
            TopicFields = new() { new TopicField { Id = "topic", Label = "写作主题", Placeholder = "如:一篇关于秋天的散文" } },
            ReviewLoop = new ReviewLoopConfig { ReviewerId = "editor", ApproveMarker = "定稿", MaxRounds = 3 },
        },
        new Scenario
        {
            Id = "preset-brainstorm",
            Name = "头脑风暴",
            Icon = "💡",
            Agents = new()
            {
                new Agent { Id = "ideator", Name = "创意官", Role = "发散", Color = "#B68C2E", Avatar = "💡", SystemPrompt = "你是创意官，围绕用户主题快速产出多样、不落俗套的点子，每次给 3 条并简述理由。" },
                new Agent { Id = "critic", Name = "评审", Role = "收敛", Color = "#3B8C5A", Avatar = "✅", SystemPrompt = "你是评审，对创意官的点子挑出风险与可行性问题，并圈出最有潜力的一条。" },
            },
            TurnPolicy = TurnPolicy.Scripted,
            ScriptOrder = new() { "ideator", "critic" },
            OpenerAgentId = "ideator",
            OpenerLine = "请围绕今天的主题,给出第一批点子,每条简述理由。",
            TopicFields = new() { new TopicField { Id = "topic", Label = "头脑风暴主题", Placeholder = "如:提升产品留存的点子" } },
        },
    };

    /// English-locale preset set — names, persona prompts, topic-field labels,
    /// opener lines, debrief text, and the review-loop marker ("approved")
    /// all in English. Structure (ids, colors, icons/avatars, turn policies,
    /// script orders) is identical to ZhPresets; only the human-/LLM-facing
    /// text is translated. The "approved" marker pairs with ChatLocale.en
    /// engine prompts (the editor is told to emit "approved"; the engine
    /// matches it substring-wise to approve).</summary>
    public static Scenario[] EnPresets => new[]
    {
        new Scenario
        {
            Id = "preset-interview",
            Name = "Interview Practice",
            Icon = "🎤",
            Agents = new()
            {
                new Agent
                {
                    Id = "interviewer", Name = "Interviewer", Role = "Asks questions", Color = "#4D6BFE", Avatar = "👨‍💼",
                    SystemPrompt = "You are a senior technical interviewer. Your job is to ask in-depth, progressive questions about the position the candidate is applying for. Ask only one question at a time; follow up or change direction only after the candidate answers. Do not answer for the candidate, and do not give coaching feedback — that is the coach's job. Keep a professional, measured tone.",
                },
                new Agent
                {
                    Id = "coach", Name = "Coach", Role = "Feedback", Color = "#3B8C5A", Avatar = "🎯",
                    SystemPrompt = "You are an interview coach. After each of the candidate's answers, give targeted feedback: what was strong, what was weak, how to improve, plus a short, actionable \"next step\". Be specific and practical. Do not answer the interviewer's questions for the candidate. If the candidate's project experience is provided in [Scenario Background], tie your advice to those projects at a project-specific level (the interviewer does not see this — you use it only for coaching).",
                },
            },
            TurnPolicy = TurnPolicy.Scripted,
            // candidate answers → coach feedback → interviewer follow-up
            ScriptOrder = new() { "coach", "interviewer" },
            OpenerAgentId = "interviewer",
            OpenerLine = "Let's begin the interview. Please give a brief self-introduction.",
            TopicFields = new()
            {
                new TopicField { Id = "position", Label = "Position", Placeholder = "e.g. Frontend Engineer, 3 years" },
                new TopicField { Id = "company", Label = "Target company", Placeholder = "e.g. ByteDance" },
                new TopicField { Id = "level", Label = "Level", Placeholder = "e.g. Mid-level" },
                // Project experience is injected only into the coach's background
                // (VisibleTo: ["coach"]) — the interviewer never sees it and won't
                // ask about it, but the coach can give project-specific advice.
                new TopicField { Id = "projects", Label = "Project experience", Placeholder = "e.g. E-commerce order platform, owned inventory & payment modules; multiple allowed", VisibleTo = new() { "coach" } },
            },
            Debrief = new DebriefConfig
            {
                ButtonLabel = "End interview",
                SummaryPrompt = "(Interview over.) As the coach, give an overall summary of the candidate's performance in this interview: highlights, weaknesses, areas to improve, and follow-up learning and practice recommendations.",
                DebriefMemberId = "coach",
            },
        },
        new Scenario
        {
            Id = "preset-language-partner",
            Name = "Language Partner",
            Icon = "🌐",
            Agents = new()
            {
                new Agent
                {
                    Id = "partner", Name = "Language Partner", Role = "Practice", Color = "#B68C2E", Avatar = "🗣",
                    SystemPrompt = "You are a foreign-language practice partner. Hold a natural conversation with the user, adjust difficulty to their level, gently correct wording and grammar mistakes as they come up, and offer more idiomatic phrasing. Advance the topic one step at a time. Speak the language specified under \"Language · Topic\" in [Scenario Background]; if the user didn't specify a language, default to English.",
                },
            },
            TurnPolicy = TurnPolicy.RoundRobin,
            OpenerAgentId = "partner",
            OpenerLine = "Open naturally in the language and topic specified in the background, and start chatting with the user.",
            TopicFields = new() { new TopicField { Id = "topic", Label = "Language · Topic", Placeholder = "e.g. Chinese · Travel" } },
        },
        new Scenario
        {
            Id = "preset-debate",
            Name = "Debate",
            Icon = "⚖️",
            Agents = new()
            {
                new Agent { Id = "pro", Name = "Pro Debater", Role = "For", Color = "#4D6BFE", Avatar = "👍", SystemPrompt = "You are the pro debater. Argue from the supporting position — sharp viewpoints, strong supporting reasoning." },
                new Agent { Id = "con", Name = "Con Debater", Role = "Against", Color = "#E5484D", Avatar = "👎", SystemPrompt = "You are the con debater. Argue from the opposing position — counter the pro side point for point, with solid reasoning." },
                new Agent { Id = "moderator", Name = "Moderator", Role = "Facilitator", Color = "#8A8A8A", Avatar = "⚖️", SystemPrompt = "You are the debate moderator. In the first round, state today's motion and invite the pro side to open; thereafter, each round reply only with the next speaker's role id (pro/con/user) — nothing else — and keep both sides balanced." },
            },
            TurnPolicy = TurnPolicy.Moderator,
            ModeratorId = "moderator",
            OpenerAgentId = "moderator",
            OpenerLine = "Please open: state today's motion and invite the pro side to begin.",
            TopicFields = new() { new TopicField { Id = "motion", Label = "Debate motion", Placeholder = "e.g. Will AI replace humans" } },
        },
        new Scenario
        {
            Id = "preset-writing-workshop",
            Name = "Writing Workshop",
            Icon = "✏️",
            Agents = new()
            {
                new Agent
                {
                    Id = "writer", Name = "Writer", Role = "Drafts", Color = "#4D6BFE", Avatar = "✍️",
                    SystemPrompt = "You are the writer. Draft an initial piece based on the user's topic, focused on structure and expression. When the editor gives revision feedback, revise your draft accordingly and output the complete draft — don't just describe the changes.",
                },
                new Agent
                {
                    Id = "editor", Name = "Editor", Role = "Refines", Color = "#3B8C5A", Avatar = "📝",
                    SystemPrompt = "You are the editor. Give specific, actionable revision suggestions on the writer's draft and explain your reasoning. After each review you must state a clear verdict: if the draft has reached a quality ready to finalize, include the word \"approved\" in your reply to signal approval; otherwise point out what needs changing and hand it back to the writer. Do not rewrite the whole piece for the writer.",
                },
            },
            TurnPolicy = TurnPolicy.Scripted,
            ScriptOrder = new() { "writer", "editor" },
            // writer drafts → editor reviews → writer revises → editor re-reviews → …,
            // until the editor includes "approved" or the 3-round cap (incl. the
            // first pass) is reached, preventing infinite revision.
            TopicFields = new() { new TopicField { Id = "topic", Label = "Writing topic", Placeholder = "e.g. An essay about autumn" } },
            ReviewLoop = new ReviewLoopConfig { ReviewerId = "editor", ApproveMarker = "approved", MaxRounds = 3 },
        },
        new Scenario
        {
            Id = "preset-brainstorm",
            Name = "Brainstorm",
            Icon = "💡",
            Agents = new()
            {
                new Agent { Id = "ideator", Name = "Ideator", Role = "Diverge", Color = "#B68C2E", Avatar = "💡", SystemPrompt = "You are the ideator. Quickly produce diverse, unconventional ideas around the user's topic — give 3 each time with a brief rationale." },
                new Agent { Id = "critic", Name = "Critic", Role = "Converge", Color = "#3B8C5A", Avatar = "✅", SystemPrompt = "You are the critic. Surface risks and feasibility issues with the ideator's ideas, and flag the single most promising one." },
            },
            TurnPolicy = TurnPolicy.Scripted,
            ScriptOrder = new() { "ideator", "critic" },
            OpenerAgentId = "ideator",
            OpenerLine = "Please give the first batch of ideas on today's topic, with a brief rationale for each.",
            TopicFields = new() { new TopicField { Id = "topic", Label = "Brainstorm topic", Placeholder = "e.g. Ideas to improve user retention" } },
        },
    };
}
