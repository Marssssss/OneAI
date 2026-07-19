# Agent Working State 固定与跨 Session 接续方案参考文档

> 用途：作为自研 Agent（支持自定义场景切换）制定 working state 持久化与跨 session 续接方案的参考依据。
> 
> 涵盖：记忆分层架构、检索策略选型、working state 持久化策略、跨 session 续接机制、场景切换的 profile 设计。
> 
> 整理自对 Claude Code / opencode / agent-session-resume / RALPH Loop / 12 Factor Agents / event-sourcing 模式及 arXiv:2605.15184 实证的调研。

---

## 0. 文档导航

- [1. 问题定义与边界](#1-问题定义与边界)
- [2. 核心概念纠正](#2-核心概念纠正)
- [3. 记忆分层架构](#3-记忆分层架构)
- [4. 检索策略选型](#4-检索策略选型)
- [5. Working State 持久化策略](#5-working-state-持久化策略)
- [6. 跨 Session 续接机制](#6-跨-session-续接机制)
- [7. 核心架构原则](#7-核心架构原则)
- [8. 四个必须处理的失败模式](#8-四个必须处理的失败模式)
- [9. 场景切换的 Profile 设计](#9-场景切换的-profile-设计)
- [10. 落地架构建议](#10-落地架构建议)
- [11. 实证参考与数据校准](#11-实证参考与数据校准)
- [12. 决策清单](#12-决策清单)

---

## 1. 问题定义与边界

### 1.1 要解决的问题

1. **Working state 固定**：如何让"用户目标、任务列表、当前进度、关键决策"这类运行时生成的状态在 session 内稳定地 pinned 在上下文中，不被 compaction/rot 抹掉。
2. **跨 session 续接**：用户在未完成任务列表时退出当前 session，新建 session 后如何继续未完成任务，不丢上下文、不重复已完成工作、不覆盖无关变更。

### 1.2 约束与目标

- Agent 需支持**自定义场景切换**（编码 / 对话助手 / 多场景），记忆与持久化策略需随场景切换。
- 不绑定单一 provider（参考 Claude Code、Codex、Cursor、opencode 的跨 agent handoff 实践）。
- 需抗崩溃（进程异常退出不丢进度）、抗 stale（外部世界在 session 间变化）、抗 context rot（长 session 上下文退化）。

---

## 2. 核心概念纠正

> 这部分纠正三个常见但致命的概念混淆。方案设计前必须先对齐这些概念，否则会在错误的层级做配置。

### 2.1 "Pinned" 是上下文属性，不是存储属性

**范畴错误**："pinned 的记忆是否需要持久化"——pinned 描述的是"此刻在 context window 里"，与"是否持久"是两个正交维度。

| | 持久（durable source） | 非持久（runtime-only） |
|---|---|---|
| **Pinned** | CLAUDE.md（文件，注入 prompt） | todo list、当前步、本 session 决策 |
| **非 Pinned** | journal、历史 transcript | working memory 滑窗 |

**正确问法**：不是"pinned 要不要持久化"，而是"pinned 内容的 **source** 是不是持久的"。
- CLAUDE.md / persona：source 本就是磁盘文件，pinned 只是投递机制——持久化免费。
- todo list / 当前进度 / 本 session 决策：source 是运行时生成的，pinned 只意味着"现在在窗口里"——退出 session 就死，**必须显式持久化**。

### 2.2 "Resume 同一 session" ≠ "新建 session 续接"

这是两个不同的场景，机制完全不同：

- **Resume 同 session**（Claude Code 的 `--resume` / `--continue`）：session ID 不变，往同一个 JSONL 追加。重开旧 session，不是新建。
- **新建 session 续接**：新 session 不会读旧 session 的 transcript。要续接，**必须有一份独立于 session transcript 之外的、新 session 启动时能读到的持久物**。

Claude Code 默认机制只覆盖前者。后者是 Claude Code 的缺口，也是 `agent-session-resume` skill、TASKS.md 工作流、event-sourcing 模式存在的原因。

### 2.3 "关键词 vs embedding" 是错误的切分轴

编码 agent 并非"只用关键词"——实际是分层混用：
- **Pinned 层**（CLAUDE.md、todo）：不检索，常驻 system prompt。
- **召回层**：grep/读文件是对**代码这个外部 substrate** 的检索，不是对"记忆库"的检索。
- opencode 的 `agent-session-resume` 插件本身**同时用两种**：pinned memory blocks（markdown，注入 prompt，lexical）+ journal（append-only，`all-MiniLM-L6-v2` 本地 embedding 做语义检索）。

助手 agent 也并非"必须 embedding"。arXiv:2605.15184 在 LongMemEval（多轮长对话记忆，恰是助手场景）上实测：**inline 投递下 grep 在每一个 harness-model 组合上都胜过 vector**。原因：答案大量 license on 字面 span（日期、计数、偏好原话），lexical 无瓶颈捞出；dense 反而拉入 topical false friend。

**正确切分轴**：检索方法由**记忆的 substrate 性质**决定，不是由"是不是助手"决定。

---

## 3. 记忆分层架构

### 3.1 五层模型

| 层 | 内容 | 检索 | 投递 | 持久性 |
|---|---|---|---|---|
| **Pinned** | 目标、active todo、当前步、关键决策、persona | 不检索，常驻 | system prompt 注入 | source 决定 |
| **Working** | 最近 N 轮、活跃文件、未解决错误 | 滑窗 + compaction 摘要 | inline | runtime（需显式持久化） |
| **Episodic** | 带 timestamp 的历史 session/动作日志 | hybrid（lexical 查事件，embedding 查"我们决定过 X"） | 按需 | 持久（事件日志） |
| **Semantic** | 蒸馏事实、偏好、决策 | hybrid，按 substrate 加权 | 按需 | 持久（蒸馏 store） |
| **External** | 代码/DB/文档等外部 store | **用该 store 的原生 API**（代码=grep+tree-sitter，DB=SQL） | 按需 | 外部系统 |

### 3.2 关键洞察

- **Pinned vs Retrieved 是一阶轴**，比"关键词 vs embedding"更根本。先分 pinned/retrieved，再在 retrieved 内部分 lexical/semantic。
- **代码本身是一个巨大的、外部已索引好的、符号化的记忆库**。文件路径、git、AST、LSP 符号表都是现成地址系统。grep 是这个 substrate 的原生 API。给代码做 embedding 是在已有完美索引的东西上叠有损索引。
- 人类助手 agent 没有外部结构化 store，记忆是无地址、会改写、靠 paraphrase 的对话流，所以**需要 embedding 来人造结构**。

### 3.3 Substrate 决定检索方法

| Substrate | 稳定地址 | 标识符即语义 | paraphrase 常见 | 最优检索 |
|---|---|---|---|---|
| 代码 | 有（路径+符号） | 是（变量名=意图） | 几乎不 | lexical + 结构化（AST/tree-sitter） |
| 自然语言对话 | 无 | 否 | 是 | embedding 有价值 |
| 蒸馏笔记/决策 | 看存储方式 | 半结构化 | 中等 | hybrid |

**结论**：这不是 tradeoff，是不同的记忆物质决定不同的最优检索。硬给代码上 embedding，或硬给对话上 grep，都是 substrate 错配。

---

## 4. 检索策略选型

### 4.1 比切"关键词 vs embedding"更根本的五条轴

这五条轴**联合**起作用，不能孤立选某一轴。

**轴 A：Pinned vs Retrieved（一阶轴）**
- 先分 pinned/retrieved，再在 retrieved 内分 lexical/semantic。
- Claude Code 的 todo 是 pinned（不检索），grep 是 retrieved-lexical，opencode 的 journal 是 retrieved-semantic。

**轴 B：Substrate 类型**（见 3.3）——决定检索方法。

**轴 C：任务类型**
- 字面 span 恢复（"X 定义在哪"、"用户那天说了什么日期"）→ lexical 占优
- 概念融合（"整体 auth 怎么工作的"、"我们之前对架构的倾向"）→ semantic 占优
- 综合/摘要（"总结过去一周的决策"）→ hybrid 或时序检索

**轴 D：检索模式 × harness × 投递路径 × backbone × 噪声——联合系统**
- **投递路径**会反转 grep/vector 优劣：programmatic（结果写文件，agent 再读）模式下，vector 在 10 个 harness-model 组合里有 5 个反超 grep。同一索引、同一模型，只是结果走 inline 还是走文件，排序就翻。
- **Backbone 强弱**影响巨大：弱模型（如 Haiku）在 dense retrieval 上掉得比 grep 多——dense 需要迭代 query 精炼和 reranker-aware 阅读，弱模型做不好。"默认用 vector"要以 backbone 强度为条件。
- **Harness 不是被动的**：同一个 Claude Opus 4.6，换 harness（Chronos vs Claude Code）带来的精度差，和换 retriever 差不多大。"retrieval in Table 1 is really retrieval-plus-orchestration."

**轴 E：写路径（extraction / consolidation / decay）——retrieval 是下游**
- **Extraction**：什么时候、由谁（模型自抽 vs harness 抽）、抽什么。写策略决定什么可被检索——切到 embedding 召回，但 extraction 从没写过可被 paraphrase 的内容，embedding 也白搭。
- **Consolidation**：合并、去重、跨 session 摘要。embedding 对编码 agent 也有用——不是给代码用，是给 episodic/决策层用。
- **Decay/forgetting**：pinned 永不衰减（会 stale），retrieved 隐式衰减（不被召回=被遗忘）。需显式 recency/relevance 衰减。
- **Reconciliation**：记忆冲突时谁赢（用户改了偏好），这是时序推理问题。

### 4.2 检索策略推荐

1. **默认用 hybrid（RRF）**，而非 either/or。lexical 和 dense 检索出的相关文档高度互补，RRF 合并两者排名不需要分数校准，是最便宜的提升。把"关键词模式 / embedding 模式"二分开关删掉，换成"lexical 权重 / dense 权重"两个连续值。
2. **代码场景**：structural 为主（grep + tree-sitter，零索引成本、永远新鲜），embedding 为辅（仅用于概念查询）。注意 churn：代码变得快，用 Merkle-tree diff 只 re-embed 变更文件。
3. **Routing 按 (store, query type)**，不按 scenario label。scenario label 是粗粒度代理。让 routing 层看 query 类型（有没有具体标识符？是"在哪"还是"怎么"）+ 目标 store，再决定 lexical/dense/hybrid 权重。
4. **可给 agent 两种工具让它自己选**（agent-mediated hybrid）：agent 同时有 grep 和 semantic_search 工具，自己按 query 选。**但注意**：弱模型选得差，这条路要以 backbone 强度为条件。强模型可放权，弱模型用静态 routing。
5. **投递路径要可配，且和检索方法联动**：inline 适合小结果集 + 强模型；file-based 适合大结果集但要求 agent 能可靠完成 read-integrate-retry 循环。投递路径要和检索方法联合调，不能单独切。

---

## 5. Working State 持久化策略

### 5.1 四种策略对比

| 策略 | 持久物 | 续接机制 | 优点 | 失败模式 |
|---|---|---|---|---|
| **A. Resume 同 session** | JSONL transcript | parent-UUID 链重建 + stateRestore 重建 todos | 全保真、零信息丢失 | 只在同 agent/同安装内；长 session 有 context rot；compaction 后是 lossy 版本；**不是"新 session"** |
| **B. Handoff 文档** | 一份 handoff.md（goal/done/open/next） | 新 agent 启动时读 | 跨 agent；fresh context 无 rot；紧凑 | **lossy**（摘要不是全量）；**要求旧 agent 退出前主动写**——崩溃就没 handoff；新 agent 要重新推导大量上下文 |
| **C. 持久 working-state 文件** | TASKS.md / progress.txt / ERRORS.md，git 提交 | 新 session 启动像读 CLAUDE.md 一样读它 | 崩溃也保得住（每步 git commit）；人可读可改；跨 session 天然可用 | 需保持同步（drift 风险）；只和写入的东西一样好；无自动重建 |
| **D. Event-sourced** | append-only 事件日志 | 新 session replay 事件，derive state | 全审计、time-travel、天然 pause/resume、并发安全 | 实现复杂；要外部存储；schema 演进要小心；事件日志要 compaction |

### 5.2 现实映射

- Claude Code 默认是 **A**（`--resume`/`--continue`，session ID 不变，往同一 JSONL 追加）。
- `agent-session-resume` skill 是 **B**（产出 handoff checkpoint，跨 Claude Code/Codex/Cursor/opencode）。
- RALPH Loop / understandingdata 那篇是 **C**（TASKS.md + progress.txt + git commit）。
- 12 Factor Agents（Factors 5、6）推 **D**（event-sourced state）。

### 5.3 策略组合的必要性

B 有个致命前提：**要求旧 agent 在退出前主动产出 handoff**。如果用户直接关掉终端、或进程崩溃，B 就瞎了。所以 B 必须叠加 C 或 D 才能抗崩溃——这也是为什么 `agent-session-resume` 的作者下一步做了 Kontinuo："checkpoints the next agent just reads"（C/D 思路），而不是"从 transcript 重建"（B 思路）。

**推荐组合**：A（同 session resume）+ C/D（跨 session 续接的持久 source）。B 作为跨 agent handoff 的可选增强，但依赖 C/D 兜底崩溃场景。

---

## 6. 跨 Session 续接机制

### 6.1 Claude Code 的实际做法（策略 A 的参考实现）

来自 Claude Code 源码 deep dive（`sessionStorage.ts` 5106 行 + `sessionRestore.ts` 552 行等，约 7600 行）：

- **存储格式**：每个 session 一个 append-only JSONL，路径 `~/.claude/projects/{sanitized-cwd}/{session-id}.jsonl`。
- **Entry 类型**：`user/assistant/system/attachment`（transcript）、`summary`（compaction 摘要）、`custom-title/ai-title`、`last-prompt`、`tag`、`agent-name/agent-color/agent-setting`、`mode`、`worktree-state`、`pr-link`、`file-history-snapshot`、`attribution-snapshot`、`content-replacement`、`marble-origami-commit/snapshot`（context collapse）、`queue-operation`。
- **Parent-UUID 链**：transcript 消息通过 `parentUuid → uuid` 形成链表，支持 branching（fork session 共享链前缀）、side-chains（subagent transcript）、compaction boundaries（null parentUuid 截断链）。
- **Resume pipeline**：`loadConversationForResume` → `readTranscriptFile`（chunked read）→ `parseJSONL` → `applyPreservedSegmentRelinks` + `applySnipRemovals` → `findLatestMessage` → `buildConversationChain`（从 leaf 走到 root 再 reverse）→ `recoverOrphanedParallelToolResults` → `deserializeMessagesWithInterruptDetection`。
- **State reconstruction**：`sessionRestore.ts` 专门负责"worktree, agent, mode, attribution, **todos**"——**todo list 被持久化进 JSONL、resume 时重建**。
- **Interruption detection**：`detectTurnInterruption()` 检测是否中途被打断（mid-tool execution / prompt without response / attachment without response），注入"Continue"。
- **Lite metadata**：`--resume` picker 只读 64KB head+tail，提取 firstPrompt/createdAt/cwd/gitBranch/sessionId（head）和 customTitle/aiTitle/lastPrompt/tag/summary（tail）。`reAppendSessionMetadata()` 在 compaction/exit/resume 时把 metadata 重写到 EOF，保证总在 tail 窗口内。
- **Dual write paths**：async queue（正常运行，100ms coalescing）+ sync direct（exit cleanup、metadata re-append）。
- **Crash safety**：append-only，partial final write 在 reload 时被忽略；每条 entry 自包含 JSON。

### 6.2 新建 session 续接的机制（策略 B/C/D）

Claude Code 默认覆盖不到。需要 session transcript 之外的持久物：

**B. Handoff 文档（agent-session-resume skill 的做法）**
- 旧 agent 产出 handoff checkpoint：prior goal、已完成、仍 open、next action。
- 新 agent 启动时读 handoff，reconstruct 原始目标、completed work、decisions、stopping point。
- 提取显式和隐式 task，分类为 DONE / PARTIALLY DONE / NOT DONE。
- 从第一个未完成步骤继续，不重复已完成工作。
- 跨 agent 支持（Claude Code / Codex / Cursor / Antigravity / OpenCode / GitHub Copilot）。
- **局限**：依赖旧 agent 主动写；崩溃就瞎。

**C. 持久 working-state 文件（RALPH Loop 的做法）**
- 文件集：`TASKS.md`（任务队列+完成状态）、`progress.txt`（session 日志+近期活动）、`ERRORS.md`（持久错误记忆）、`LEARNINGS.md`（积累洞察）、`features.json`（里程碑跟踪）。
- 每步：load state from files → parse current task → execute → persist state back → git commit。
- Git 作为 durability layer：每个 significant action 后 `git add + commit`；崩溃恢复用 `git log` / `git diff`。
- **优点**：人可读可改、版本控制、抗崩溃、无外部依赖。
- **局限**：无并发处理、查询能力限于 grep、状态重建需解析、无事务保证。

**D. Event-sourced（12 Factor Agents Factors 5、6）**
- 不存当前 state，存产生 state 的事件序列。State 是 derived。
- 事件类型：`task_started` / `tool_called` / `tool_result` / `approval_requested` / `approval_granted/denied` / `error` / `task_completed` / `paused` / `resumed` / `human_feedback`。
- Checkpoint/resume：`launch` → `run` → 遇 approval 则 `pause` + 持久化 + 退出进程 → webhook 触发 `resume`。
- 每次工具调用后 checkpoint（不是只在结尾）。
- **优点**：完整审计、time-travel debugging、天然 pause/resume、并发安全（append-only）。
- **局限**：实现复杂、需外部存储、schema 演进要小心、存储需求高。

---

## 7. 核心架构原则

### 7.1 总原则

> **Pinned 内容应是持久 state 的 projection，不是 state 本身。**

这是 event-sourcing 的精髓（understandingdata 那篇 Pitfall 1 原话："State is DERIVED from events, never stored directly"）。

- **持久的是**：events（task_created / task_completed / decision_made）+ reference files（CLAUDE.md）。
- **Pinned 是**：每次 session 启动时，从持久 source 重新 project 出来的视图。

### 7.2 推论

- **"pinned 要不要持久化"** → 不持久化 pinned 本身（它是 derived view），持久化它的 source（events + reference files）。
- **"新 session 怎么续接"** → 新 session 启动时，从持久 source 重新 project 出 pinned working state，注入 context。等价于"CLAUDE.md 注入"这个机制，只是 source 是 working-state 文件/事件日志而非静态指令。
- **等价于把 working state 当成 CLAUDE.md 来对待**——一个启动时注入的持久文件——但带生命周期（任务开始时创建、任务完成时删除/归档）。

### 7.3 实施要点

- **只存 events，derive state**：不要同时存 events 和 derived state（会 drift）。pinned todo 是 `deriveState(events)` 的结果，每次启动重算。
- **事件日志 compaction**：日志长了把旧 events 折叠成一个 snapshot event + 保留近期 events。
- **Event 要带足够 context**：`tool_called` 事件要含 params + reason，不能只含 tool name。
- **Version event schema**：`schemaVersion` 字段 + migration 函数。
- **Failure 是 first-class event**：错误要进事件日志，不能只 console.error。

---

## 8. 四个必须处理的失败模式

### 8.1 崩溃/异常退出（B 策略的死穴）

- **问题**：依赖"退出前写 handoff"的策略，在进程崩溃/用户直接关终端时全瞎。
- **解法**：**持续持久化，而非退出时才写**。
    - Claude Code：每轮 append JSONL（append-only，崩溃最多丢最后一行不完整 entry）。
    - C 策略：每个 significant action 后 `git commit`。
    - D 策略：每次工具调用后 checkpoint。
    - **绝不能依赖"退出前写 handoff"**。

### 8.2 Stale checkpoint（working state 和外部世界脱节）

- **问题**：session 之间外部世界可能变化。编码场景特别严重：git 前进过、文件被外部编辑、别的 agent 改过同一份代码。pinned 的"当前步"可能引用已不存在的状态。
- **解法**：resume 时**对齐外部 ground truth**。
    - understandingdata 那篇"Handle Stale Checkpoints"：checkpoint 太旧就 re-verify external state。
    - 编码场景具体：resume 时先跑 `git status` / `git log` / 读关键文件，把 pinned working state 和实际仓库状态对账，冲突时以外部为准并修正 working state，记一条 reconciliation event。
    - 对话助手场景：无外部 ground truth，跳过此步，但 working state 必须更厚。

### 8.3 Context rot（A 策略的隐患）

- **问题**：长 session 即使 resume，上下文也是 compacted（lossy）版本。注意力稀释、context rot 导致长 horizon 任务退化。
- **解法**：长任务用 fresh agent + handoff（B/C/D）而非 resume（A）。
    - RALPH Loop 故意 spawn fresh agent 规避 rot——但 fresh agent 没记忆，需要 C/D 桥接。
    - **根本 tradeoff**：resume = 全保真但有 rot；fresh+handoff = 干净 context 但 lossy。任务越长越该选后者。

### 8.4 Derived state drift（C/D 的陷阱）

- **问题**：如果同时存 events 和 derived state（既存事件日志又存当前 todo 快照），两者会 drift。
- **解法**：**只存 events，state 永远从 events 派生**。
    - pinned todo 是 `deriveState(events)` 的结果，每次启动重算。
    - 事件日志长了就 compaction：把旧 events 折叠成一个 snapshot event + 保留近期 events。
    - **Pitfall**：不要 `db.save({ events, state })`——state 会和 events 不一致。

---

## 9. 场景切换的 Profile 设计

### 9.1 Profile 是联合配置，不是单一切换

"随场景切换策略"太浅，因为只动了 retrieval 一个字段。真正该切的是一个**记忆 profile**，是五条轴的联合 bundle：

```
profile = {
    激活的层: [pinned, working, episodic, semantic, external],
    extraction: {触发条件, 抽取者, 抽什么, 写到哪层},
    storage: {每层的 substrate + 索引方式},
    retrieval: {每层的检索方法 + hybrid 权重 + routing 策略},
    delivery: {inline | file-based, 按层},
    decay: {每层的衰减策略},
    pinned_template: 常驻上下文的模板,
    persistence: {working-state 文件 + 事件日志的策略},
    ground_truth_reconciliation: {resume 时对账外部 state 的策略}
}
```

"场景"只是选 profile 的 key，但 profile 内部是五条轴 + 持久化维度的联合调参。

### 9.2 编码场景 profile（示例）

- **激活层**：全部五层，External 层为主（代码是主记忆库）。
- **Extraction**：稀疏（CLAUDE.md 人写、auto-memory 稀疏）；episodic 记录任务边界事件。
- **Storage**：External=代码本身（grep+tree-sitter 索引）；Episodic=事件日志；Semantic=蒸馏决策文件。
- **Retrieval**：External 用原生 API（grep+AST），Semantic 用 hybrid（RRF），权重偏 lexical。
- **Delivery**：inline 为主（结果集小），大结果集走 file-based。
- **Persistence**：C 策略为主（TASKS.md + git commit），事件日志为辅。working state 可以薄——很多状态可从代码 re-derive。
- **Ground truth reconciliation**：强（git status/log 对账，冲突以代码为准）。

### 9.3 对话助手场景 profile（示例）

- **激活层**：Pinned + Working + Episodic + Semantic，无 External。
- **Extraction**：激进（每轮抽事实/决策/偏好，写入 Semantic）。
- **Storage**：Episodic=事件日志（厚）；Semantic=蒸馏 store（embedding 索引）。
- **Retrieval**：Semantic 用 hybrid，权重偏 dense（paraphrase 常见）；Episodic 用 hybrid（lexical 查事件，embedding 查概念）。
- **Delivery**：inline。
- **Persistence**：D 策略为主（事件日志全），working state 必须厚（无外部 ground truth 可 re-derive）。
- **Ground truth reconciliation**：无外部 ground truth，跳过；但需 reconciliation 处理记忆冲突（用户改了偏好，时序推理决定谁赢）。

### 9.4 切换的粒度

- **不要按 scenario label 切检索方法**：scenario label 是粗粒度代理。真正决定检索方法的是 (store, query type, backbone)。
- **按 (store, query type) routing**：让 routing 层看 query 类型（有没有具体标识符？是"在哪"还是"怎么"）+ 目标 store，再决定 lexical/dense/hybrid 权重。
- **Backbone 强度作为条件**：强模型可放权（agent-mediated hybrid，自己选工具）；弱模型用静态 routing。

---

## 10. 落地架构建议

### 10.1 分五层，每层独立 store + retrieval adapter

| 层 | 内容 | 检索 | 投递 |
|---|---|---|---|
| Pinned | 目标、active todo、当前步、关键决策、persona | 不检索，常驻 | system prompt 注入 |
| Working | 最近 N 轮、活跃文件、未解决错误 | 滑窗 + compaction 摘要 | inline |
| Episodic | 带 timestamp 的历史 session/动作日志 | hybrid（lexical 查事件，embedding 查"我们决定过 X"） | 按需 |
| Semantic | 蒸馏事实、偏好、决策 | hybrid，按 substrate 加权 | 按需 |
| External | 代码/DB/文档等外部 store | **用该 store 的原生 API**（代码=grep+tree-sitter，DB=SQL） | 按需 |

### 10.2 Pinned 层拆成两个 source

- **静态 reference**（persona、CLAUDE.md 类）→ 持久文件，启动注入。
- **动态 working state**（goal、todo、当前步、决策）→ 持久化为一个 **working-state 文件**（如 `.agent/WORK_IN_PROGRESS.md`）+ 一个 **append-only 事件日志**。启动时从事件日志 derive 出当前 working state，注入 pinned。

### 10.3 Working-state 文件像 CLAUDE.md 一样启动注入，但带生命周期

- 任务开始创建、每步更新、任务完成归档删除。
- 新 session 天然能续接，不依赖 resume 旧 session。

### 10.4 每步 checkpoint，不依赖退出时写

- 每个 significant action（任务完成、决策做出、文件修改）后：append 事件 + 更新 working-state 文件 + （编码场景）git commit。
- 崩溃最多丢最后一步。

### 10.5 Resume 时对账外部 ground truth

- 启动时先读外部 state（代码场景：git；对话场景：无外部 state 可跳过）。
- 再和 working-state 文件对账，冲突时修正 working state 并记一条 reconciliation event。

### 10.6 Extraction 作为一等公民、可配置

给 extraction 一个独立 pipeline，scenario 可配：
- 触发条件（每轮 vs 每任务 vs 手动）
- 抽取者（小模型 vs 主模型 vs 规则）
- 抽取目标（事实 vs 决策 vs 偏好）
- 写入哪层

**这是最容易被忽略但最决定性的部分**——读路径再花哨，写路径没写对东西都白搭。

### 10.7 持久化策略随场景切换

- 编码场景：外部 ground truth 强（git），working state 可以更薄，很多状态可从代码 re-derive → C 策略为主，事件日志为辅。
- 对话助手场景：无外部 ground truth，working state 必须更厚、事件日志更全 → D 策略为主。
- 这是"场景切 profile"在持久化维度的体现，不是只切检索方法。

---

## 11. 实证参考与数据校准

### 11.1 arXiv:2605.15184 — *Is Grep All You Need? How Agent Harnesses Reshape Agentic Search*

PwC，2026.05，LongMemEval 116 题，跨 Chronos / Claude Code / Codex / Gemini CLI，inline vs programmatic。

**核心发现**：
- Inline 投递下，**grep 在每一个 harness-model 组合上都胜过 vector**。最大差距 Chronos+Gemini Flash-Lite 86.2% vs 62.9%，最小差距 Claude Code+Opus 76.7% vs 75.0%。
- Programmatic（file-based）投递会反转：vector 在 10 个 harness-model 组合里有 5 个反超 grep。
- 同一个 Claude Opus 4.6，换 harness（Chronos vs Claude Code）精度差和换 retriever 差不多大。
- 弱模型（Haiku）在 dense retrieval 上掉得比 grep 多。
- Crossover 取决于 harness+backbone 而非语料大小——"语料大了就该上 embedding"是错的，crossover 不单调。
- 结论：retrieval mechanics + harness orchestration + delivery path 是联合系统，不是独立设计选择。

**对本方案的校准**：检索方法不能孤立选；投递路径要和检索方法联合调；弱模型用静态 routing。

### 11.2 embedding-vs-grep-experiment — henry-dowling

20 题 × 100 合成语料，Claude Sonnet，TF-IDF 代理 embedding。

**核心发现**：
- Embedding 便宜 ~35%、快 ~32%，原因是**少 round trip**（agentic loop 每轮重发全量历史，少一轮=省一大笔）。
- 不是单次检索更便宜——embedding 单次返回文本反而多 3x（grep 4.3 calls/324 tokens vs embedding 1.5 calls/1048 tokens）。
- **局限**：用 TF-IDF 代理真 embedding、合成语料、**没评正确性**。

**对本方案的校准**：cost 和 accuracy 是两个轴，grep 可以更准且更贵，embedding 可以更便宜且更不准。这两篇不矛盾。

### 11.3 opencode-agent-memory — joshuadavidthomas

Letta-style editable memory blocks for OpenCode。

**核心发现**：
- 一个插件内同时用 pinned blocks（markdown，注入 prompt，lexical）+ journal（append-only，`all-MiniLM-L6-v2` 本地 embedding 语义检索）。
- 三个默认 block：`persona`（global）、`human`（global）、`project`（project）。
- Memory tools：`memory_list` / `memory_set` / `memory_replace`。
- Journal tools：`journal_write` / `journal_search`（语义）/ `journal_read`。
- Block format：markdown + YAML frontmatter（label/description/limit/read_only）。

**对本方案的校准**：分层混用是已被验证的工程实践，不是理论。

### 11.4 Claude Code session persistence — openedclaude deep dive

约 7600 行源码分析。

**核心发现**：
- Append-only JSONL，parent-UUID 链表，支持 branching/side-chains/compaction boundaries。
- `sessionRestore.ts` 专门重建 todos（working state 持久化在 transcript 里）。
- 64KB head/tail lite read 用于 session picker。
- Dual write paths（async queue + sync direct）。
- Crash safety：append-only，partial final write 被忽略。

**对本方案的校准**：策略 A 的工业级实现细节；todo 持久化进 transcript 是可行路径，但只覆盖同 session resume。

### 11.5 agent-session-resume — hacktivist123

跨 agent handoff skill（Claude Code / Codex / Cursor / Antigravity / OpenCode / GitHub Copilot）。

**核心发现**：
- 产出 handoff checkpoint：prior goal / done / open / next action。
- 新 agent 读 handoff，reconstruct + 分类 task（DONE/PARTIALLY DONE/NOT DONE）+ 从第一个未完成步继续。
- 作者下一步做 Kontinuo："checkpoints the next agent just reads"（C/D 思路），承认"从 transcript 重建"是 hard mode。

**对本方案的校准**：策略 B 的参考实现；B 必须叠加 C/D 才能抗崩溃。

### 11.6 understandingdata — Agent Memory Patterns

三层记忆 tier（session / file-based / event-sourced）+ RALPH Loop + 12 Factor Agents。

**核心发现**：
- Tier 1 session state：runtime，丢失于进程终止。
- Tier 2 file-based：TASKS.md/progress.txt/ERRORS.md + git，抗崩溃，人可读。
- Tier 3 event-sourced：append-only 事件日志，state 是 derived，全审计/time-travel/pause-resume/并发安全。
- Best practices：每次工具调用后 checkpoint；events 是 source of truth，不存 derived state；event 含足够 context；处理 stale checkpoint；version event schema。
- Common pitfalls：存 derived state（drift）、missing failure events、unbounded event logs、blocking on human approval。

**对本方案的校准**：策略 C/D 的实施细节和反模式；"state is DERIVED from events, never stored directly"是核心原则的出处。

---

## 12. 决策清单

制定方案时需逐项决策的点：

### 12.1 记忆架构
- [ ] 五层是否都激活？哪些场景裁掉某些层？
- [ ] Pinned 层的静态 reference 和动态 working state 是否分开 source？
- [ ] 每层的 substrate 是什么？检索方法是否匹配 substrate？

### 12.2 检索策略
- [ ] 是否默认 hybrid（RRF）而非 either/or？
- [ ] Routing 是按 (store, query type) 还是按 scenario label？
- [ ] Backbone 强度是否作为 routing 条件？弱模型是否用静态 routing？
- [ ] 投递路径（inline/file-based）是否和检索方法联合调？
- [ ] 代码场景是否以 structural（grep+tree-sitter）为主、embedding 为辅？

### 12.3 Working state 持久化
- [ ] 选哪种策略组合？（推荐 A + C/D）
- [ ] Working-state 文件路径和格式？（如 `.agent/WORK_IN_PROGRESS.md`）
- [ ] 事件日志格式和 schema version？
- [ ] Checkpoint 粒度？（每步 vs 每任务）
- [ ] 是否只存 events、derive state（避免 drift）？
- [ ] 事件日志 compaction 策略？

### 12.4 跨 session 续接
- [ ] 新 session 启动时从哪些持久 source re-project pinned？
- [ ] 是否对账外部 ground truth？编码场景的 git 对账策略？
- [ ] 对话场景的记忆冲突 reconciliation 策略？
- [ ] Handoff 文档（策略 B）是否作为可选增强？崩溃兜底是否靠 C/D？

### 12.5 Extraction（写路径）
- [ ] 触发条件（每轮/每任务/手动）？
- [ ] 抽取者（小模型/主模型/规则）？
- [ ] 抽取目标（事实/决策/偏好）？
- [ ] 写入哪层？

### 12.6 场景切换
- [ ] 定义了哪些 profile？每个 profile 的五轴 + 持久化维度配置？
- [ ] Profile 切换的触发机制？（用户显式切换 / 自动检测场景）
- [ ] 切换时 working state 如何迁移？

### 12.7 失败模式覆盖
- [ ] 崩溃：是否持续持久化而非退出时才写？
- [ ] Stale：是否 resume 时对账外部 ground truth？
- [ ] Context rot：长任务是否用 fresh agent + handoff 而非 resume？
- [ ] Drift：是否只存 events、derive state？

---

## 附录 A：术语表

| 术语 | 定义 |
|---|---|
| Pinned | 此刻在 context window 里，常驻 system prompt |
| Retrieved | 需要时通过工具召回 |
| Substrate | 记忆的物质载体（代码/对话/蒸馏笔记） |
| Working state | 运行时生成的状态（goal/todo/当前步/决策） |
| Reference file | 静态持久文件（CLAUDE.md/persona） |
| Event sourcing | 存事件序列，state 是 derived |
| RRF | Reciprocal Rank Fusion，合并 lexical 和 dense 排名 |
| Context rot | 长 session 上下文退化 |
| Handoff | 跨 agent/session 的交接文档 |
| Ground truth reconciliation | resume 时对账外部真实状态 |

## 附录 B：参考来源

1. arXiv:2605.15184 — *Is Grep All You Need? How Agent Harnesses Reshape Agentic Search*（PwC, 2026.05）
2. henry-dowling/embedding-vs-grep-experiment（GitHub）
3. joshuadavidthomas/opencode-agent-memory（GitHub）
4. openedclaude/claude-reviews-claude Chapter 09 — Session Persistence
5. hacktivist123/agent-session-resume（GitHub）+ Kontinuo
6. understandingdata.com/posts/agent-memory-patterns
7. Claude Code Docs — Memory / Sessions / Todo Tracking
8. opencode DeepWiki — Session Management / Context Management
9. 12 Factor Agents（HumanLayer）— Factors 5 & 6
10. particula.tech — Semantic Code Search vs Grep for Coding Agents（Merkle-tree diff re-embedding）

---

*文档版本：v1.0 | 整理日期：2026-07-19*