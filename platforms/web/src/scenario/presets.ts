// Scenario presets — the built-in 5×2-locale (zh/en) starter library,
// mirroring macOS `AgentStore.presets(locale:)` field-for-field. These are the
// Web frontend's *local* defaults: ids prefixed `preset-` are NOT upserted to
// the shared server store (they're per-frontend, locale-bound). The shared
// `scenario/list` merge rule (see scenarioStore) drops server-side `preset-*`
// seeds so the richer local set wins; only customs travel to the server.
//
// Behavior (turn policy, script order, review loop, topic visibility) matches
// the native macOS/Windows ports exactly — only the human-/LLM-facing text is
// translated between the two locale blocks.

import type { BusScenario, BusLocale } from '../rpc/types'

export function presetsFor(locale: BusLocale): BusScenario[] {
  return locale === 'en' ? EN_PRESETS : ZH_PRESETS
}

const ZH_PRESETS: BusScenario[] = [
  {
    id: 'preset-interview',
    name: '面试演练',
    icon: '◆',
    members: [
      {
        id: 'interviewer',
        name: '面试官',
        role: '提问',
        system_prompt:
          '你是一名资深技术面试官。你的任务是就用户应聘的岗位提出有深度、循序渐进的问题。每次只问一个问题，等用户回答后再追问或换方向。不要替用户回答，不要给出指导性评价——那是指导员的工作。语气专业、克制。',
        kind: 'openai',
        model: '',
        color: '#4D6BFE',
        avatar: '◆',
      },
      {
        id: 'coach',
        name: '指导员',
        role: '点评',
        system_prompt:
          '你是一名面试指导教练。在用户每次回答后，你给出针对性点评：哪里回答得好、哪里不足、可以怎样改进，并给出一个简短的「行动建议」。点评要具体、可执行。不要替用户回答面试官的问题。若【场景背景】中提供了候选人的项目经历，请结合其项目内容给出项目级、有针对性的建议（这些信息面试官看不到，仅你用于点评）。',
        kind: 'openai',
        model: '',
        color: '#3B8C5A',
        avatar: '✓',
      },
    ],
    turn_policy: 'scripted',
    script_order: ['coach', 'interviewer'],
    opener_agent_id: 'interviewer',
    opener_line: '我们开始面试吧。请先做个简短的自我介绍。',
    topic_fields: [
      { id: 'position', label: '应聘岗位', placeholder: '如:前端工程师 3 年' },
      { id: 'company', label: '目标公司', placeholder: '如:字节跳动' },
      { id: 'level', label: '职位级别', placeholder: '如:社招 P5' },
      {
        id: 'projects',
        label: '项目经历',
        placeholder: '如:电商订单中台,负责库存与支付模块;可写多条',
        visible_to: ['coach'],
      },
    ],
    debrief: {
      button_label: '结束面试',
      summary_prompt:
        '（面试结束）请以指导员身份,对候选人本次面试的整体表现进行全场总结:亮点、不足、可改进之处,并给出后续学习与练习建议。',
      debrief_member_id: 'coach',
    },
    locale: 'zh',
  },
  {
    id: 'preset-language-partner',
    name: '语言伙伴',
    icon: '◯',
    members: [
      {
        id: 'partner',
        name: '语言伙伴',
        role: '陪练',
        system_prompt:
          '你是一名外语陪练伙伴。与用户进行自然对话，根据用户水平调整难度，适时温和地纠正用词与语法错误，并给出更地道的说法。一次只推进话题一步。请使用【场景背景】中“语言·话题”所指定的语言与用户交谈；若用户未指定语言，默认用英语。',
        kind: 'openai',
        model: '',
        color: '#B68C2E',
        avatar: '◯',
      },
    ],
    turn_policy: 'roundrobin',
    opener_agent_id: 'partner',
    opener_line: '请按背景中指定的语言与话题自然开场，与用户聊起来。',
    topic_fields: [
      { id: 'topic', label: '语言·话题', placeholder: '如:中文·旅行' },
    ],
    locale: 'zh',
  },
  {
    id: 'preset-debate',
    name: '辩论赛',
    icon: '⚖',
    members: [
      {
        id: 'pro',
        name: '正方辩手',
        role: '支持',
        system_prompt:
          '你是正方辩手，从支持立场出发进行论证，观点鲜明、论据有力。',
        kind: 'openai',
        model: '',
        color: '#4D6BFE',
        avatar: '▲',
      },
      {
        id: 'con',
        name: '反方辩手',
        role: '反对',
        system_prompt:
          '你是反方辩手，从反对立场出发进行论证，针锋相对、有理有据。',
        kind: 'openai',
        model: '',
        color: '#E5484D',
        avatar: '▼',
      },
      {
        id: 'moderator',
        name: '主持人',
        role: '调度',
        system_prompt:
          '你是辩论主持人。首轮请点明今日辩题并邀请正方先开始立论；其后每轮只回复下一个发言者的角色 id（pro/con/user），不要回复其他内容，并确保双方均衡发言。',
        kind: 'openai',
        model: '',
        color: '#8A8A8A',
        avatar: '⚖',
      },
    ],
    turn_policy: 'moderator',
    moderator_id: 'moderator',
    opener_agent_id: 'moderator',
    opener_line: '请开场:点明今日辩题,邀请正方先开始立论。',
    topic_fields: [
      { id: 'motion', label: '辩论主题', placeholder: '如:AI 是否会取代人类' },
    ],
    locale: 'zh',
  },
  {
    id: 'preset-writing-workshop',
    name: '写作工坊',
    icon: '✎',
    members: [
      {
        id: 'writer',
        name: '写手',
        role: '起草',
        system_prompt:
          '你是写手，根据用户主题起草初稿，注重结构与表达。当编辑给出修改意见时，请据此修改你的稿件，并输出完整稿件，不要只描述改动。',
        kind: 'openai',
        model: '',
        color: '#4D6BFE',
        avatar: '✎',
      },
      {
        id: 'editor',
        name: '编辑',
        role: '润色',
        system_prompt:
          '你是编辑，对写手的稿件给出具体、可执行的修改建议并说明理由。每轮审阅后必须明确表态：若稿件已达到可定稿的质量，请在回复中包含「定稿」二字以示通过；否则指出需修改之处，交回写手继续修改。不要替写手重写全文。',
        kind: 'openai',
        model: '',
        color: '#3B8C5A',
        avatar: '✓',
      },
    ],
    turn_policy: 'scripted',
    script_order: ['writer', 'editor'],
    topic_fields: [
      { id: 'topic', label: '写作主题', placeholder: '如:一篇关于秋天的散文' },
    ],
    review_loop: { reviewer_id: 'editor', approve_marker: '定稿', max_rounds: 3 },
    locale: 'zh',
  },
  {
    id: 'preset-brainstorm',
    name: '头脑风暴',
    icon: '✦',
    members: [
      {
        id: 'ideator',
        name: '创意官',
        role: '发散',
        system_prompt:
          '你是创意官，围绕用户主题快速产出多样、不落俗套的点子，每次给 3 条并简述理由。',
        kind: 'openai',
        model: '',
        color: '#B68C2E',
        avatar: '✦',
      },
      {
        id: 'critic',
        name: '评审',
        role: '收敛',
        system_prompt:
          '你是评审，对创意官的点子挑出风险与可行性问题，并圈出最有潜力的一条。',
        kind: 'openai',
        model: '',
        color: '#3B8C5A',
        avatar: '✓',
      },
    ],
    turn_policy: 'scripted',
    script_order: ['ideator', 'critic'],
    opener_agent_id: 'ideator',
    opener_line: '请围绕今天的主题,给出第一批点子,每条简述理由。',
    topic_fields: [
      { id: 'topic', label: '头脑风暴主题', placeholder: '如:提升产品留存的点子' },
    ],
    locale: 'zh',
  },
]

const EN_PRESETS: BusScenario[] = [
  {
    id: 'preset-interview',
    name: 'Interview Practice',
    icon: '◆',
    members: [
      {
        id: 'interviewer',
        name: 'Interviewer',
        role: 'Asks questions',
        system_prompt:
          'You are a senior technical interviewer. Your job is to ask in-depth, progressive questions about the position the candidate is applying for. Ask only one question at a time; follow up or change direction only after the candidate answers. Do not answer for the candidate, and do not give coaching feedback — that is the coach\'s job. Keep a professional, measured tone.',
        kind: 'openai',
        model: '',
        color: '#4D6BFE',
        avatar: '◆',
      },
      {
        id: 'coach',
        name: 'Coach',
        role: 'Feedback',
        system_prompt:
          'You are an interview coach. After each of the candidate\'s answers, give targeted feedback: what was strong, what was weak, how to improve, plus a short, actionable "next step". Be specific and practical. Do not answer the interviewer\'s questions for the candidate. If the candidate\'s project experience is provided in [Scenario Background], tie your advice to those projects at a project-specific level (the interviewer does not see this — you use it only for coaching).',
        kind: 'openai',
        model: '',
        color: '#3B8C5A',
        avatar: '✓',
      },
    ],
    turn_policy: 'scripted',
    script_order: ['coach', 'interviewer'],
    opener_agent_id: 'interviewer',
    opener_line: "Let's begin the interview. Please give a brief self-introduction.",
    topic_fields: [
      { id: 'position', label: 'Position', placeholder: 'e.g. Frontend Engineer, 3 years' },
      { id: 'company', label: 'Target company', placeholder: 'e.g. ByteDance' },
      { id: 'level', label: 'Level', placeholder: 'e.g. Mid-level' },
      {
        id: 'projects',
        label: 'Project experience',
        placeholder:
          'e.g. E-commerce order platform, owned inventory & payment modules; multiple allowed',
        visible_to: ['coach'],
      },
    ],
    debrief: {
      button_label: 'End interview',
      summary_prompt:
        "(Interview over.) As the coach, give an overall summary of the candidate's performance in this interview: highlights, weaknesses, areas to improve, and follow-up learning and practice recommendations.",
      debrief_member_id: 'coach',
    },
    locale: 'en',
  },
  {
    id: 'preset-language-partner',
    name: 'Language Partner',
    icon: '◯',
    members: [
      {
        id: 'partner',
        name: 'Language Partner',
        role: 'Practice',
        system_prompt:
          'You are a foreign-language practice partner. Hold a natural conversation with the user, adjust difficulty to their level, gently correct wording and grammar mistakes as they come up, and offer more idiomatic phrasing. Advance the topic one step at a time. Speak the language specified under "Language · Topic" in [Scenario Background]; if the user didn\'t specify a language, default to English.',
        kind: 'openai',
        model: '',
        color: '#B68C2E',
        avatar: '◯',
      },
    ],
    turn_policy: 'roundrobin',
    opener_agent_id: 'partner',
    opener_line:
      'Open naturally in the language and topic specified in the background, and start chatting with the user.',
    topic_fields: [
      { id: 'topic', label: 'Language · Topic', placeholder: 'e.g. Chinese · Travel' },
    ],
    locale: 'en',
  },
  {
    id: 'preset-debate',
    name: 'Debate',
    icon: '⚖',
    members: [
      {
        id: 'pro',
        name: 'Pro Debater',
        role: 'For',
        system_prompt:
          'You are the pro debater. Argue from the supporting position — sharp viewpoints, strong supporting reasoning.',
        kind: 'openai',
        model: '',
        color: '#4D6BFE',
        avatar: '▲',
      },
      {
        id: 'con',
        name: 'Con Debater',
        role: 'Against',
        system_prompt:
          'You are the con debater. Argue from the opposing position — counter the pro side point for point, with solid reasoning.',
        kind: 'openai',
        model: '',
        color: '#E5484D',
        avatar: '▼',
      },
      {
        id: 'moderator',
        name: 'Moderator',
        role: 'Facilitator',
        system_prompt:
          'You are the debate moderator. In the first round, state today\'s motion and invite the pro side to open; thereafter, each round reply only with the next speaker\'s role id (pro/con/user) — nothing else — and keep both sides balanced.',
        kind: 'openai',
        model: '',
        color: '#8A8A8A',
        avatar: '⚖',
      },
    ],
    turn_policy: 'moderator',
    moderator_id: 'moderator',
    opener_agent_id: 'moderator',
    opener_line: 'Please open: state today\'s motion and invite the pro side to begin.',
    topic_fields: [
      { id: 'motion', label: 'Debate motion', placeholder: 'e.g. Will AI replace humans' },
    ],
    locale: 'en',
  },
  {
    id: 'preset-writing-workshop',
    name: 'Writing Workshop',
    icon: '✎',
    members: [
      {
        id: 'writer',
        name: 'Writer',
        role: 'Drafts',
        system_prompt:
          'You are the writer. Draft an initial piece based on the user\'s topic, focused on structure and expression. When the editor gives revision feedback, revise your draft accordingly and output the complete draft — don\'t just describe the changes.',
        kind: 'openai',
        model: '',
        color: '#4D6BFE',
        avatar: '✎',
      },
      {
        id: 'editor',
        name: 'Editor',
        role: 'Refines',
        system_prompt:
          'You are the editor. Give specific, actionable revision suggestions on the writer\'s draft and explain your reasoning. After each review you must state a clear verdict: if the draft has reached a quality ready to finalize, include the word "approved" in your reply to signal approval; otherwise point out what needs changing and hand it back to the writer. Do not rewrite the whole piece for the writer.',
        kind: 'openai',
        model: '',
        color: '#3B8C5A',
        avatar: '✓',
      },
    ],
    turn_policy: 'scripted',
    script_order: ['writer', 'editor'],
    topic_fields: [
      { id: 'topic', label: 'Writing topic', placeholder: 'e.g. An essay about autumn' },
    ],
    review_loop: { reviewer_id: 'editor', approve_marker: 'approved', max_rounds: 3 },
    locale: 'en',
  },
  {
    id: 'preset-brainstorm',
    name: 'Brainstorm',
    icon: '✦',
    members: [
      {
        id: 'ideator',
        name: 'Ideator',
        role: 'Diverge',
        system_prompt:
          'You are the ideator. Quickly produce diverse, unconventional ideas around the user\'s topic — give 3 each time with a brief rationale.',
        kind: 'openai',
        model: '',
        color: '#B68C2E',
        avatar: '✦',
      },
      {
        id: 'critic',
        name: 'Critic',
        role: 'Converge',
        system_prompt:
          'You are the critic. Surface risks and feasibility issues with the ideator\'s ideas, and flag the single most promising one.',
        kind: 'openai',
        model: '',
        color: '#3B8C5A',
        avatar: '✓',
      },
    ],
    turn_policy: 'scripted',
    script_order: ['ideator', 'critic'],
    opener_agent_id: 'ideator',
    opener_line: 'Please give the first batch of ideas on today\'s topic, with a brief rationale for each.',
    topic_fields: [
      { id: 'topic', label: 'Brainstorm topic', placeholder: 'e.g. Ideas to improve user retention' },
    ],
    locale: 'en',
  },
]
