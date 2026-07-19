# OneAI 上下文管理机制白皮书

> 版本：对应代码库 `0.2.0` / 1.0.0 线。本文基于对 `crates/oneai-core`（`budget.rs`/`context_manager.rs`/`context_accounting.rs`/`token_counter.rs`/`model_context.rs`）、`crates/oneai-agent`（`agent_loop.rs`/`context_assembler.rs`/`sub_agent.rs`）、`crates/oneai-memory`（`compression.rs`/`core_memory_source.rs`）、`crates/oneai-domain`（`context_source.rs`/`compression_template.rs`）、`crates/oneai-provider`（`anthropic.rs`）源码的逐文件审阅撰写，所有机制均标注 `file:line` 以便核对。姊妹篇《记忆机制白皮书》(`docs/memory-mechanism.md`) 聚焦记忆三层与召回，本文聚焦**上下文窗口的装配、预算、解析、裁剪、压缩、锚定、缓存与多代理隔离**。

---

## 0. 一句话概括

OneAI 的上下文管理是一个 **「持久日志/瞬时装配分离 + 抗压缩重注入 + token-预算驱动终止 + 三层模型上下文解析 + 四策略裁剪 + 压缩耦合抽取 + 缓存感知装配」** 的引擎：`state.conversation` 是唯一持久日志，每轮在其克隆上**瞬时重建**装配（ContextSource 块 + 锚定块），任何被压缩掉的状态靠下一轮重注入而非靠压缩器保留；终止由 `TokenBudget` 决定而非硬编码 `max_iterations`；模型上下文窗口经 L1 用户配置 > L2 服务商 API 探测 > L3 内置库三层解析；超限时按四策略裁剪，且压缩被耦合进事实抽取闭环——「压缩即丢失」被堵死。

---

## 1. 核心范式：持久/瞬时分离（durable / ephemeral separation）

这是整个上下文管理设计的**第一性原理**，理解它才能理解其余一切。

| 层 | 内容 | 是否写回 | 是否参与压缩 |
|---|---|---|---|
| **持久日志** `state.conversation` | system prompt、用户任务、assistant 回复、tool 结果——`AgentLoop` 每轮追加、持久化、被压缩 | ✅ 写回 | ✅ 压缩器作用对象 |
| **瞬时装配** `conv_for_inference` | 持久日志克隆 + ContextSource 缓存块 + 锚定块（TaskAnchor/PlanProgress/skill menu） | ❌ 从不写回 | ❌ 压缩器只看持久日志 |

`context_assembler.rs:72-101` 的文档注释把这一范式讲得很清楚：

> `state.conversation` 是持久日志；`assemble()` 产出的是**每轮新鲜、瞬时的装配**——持久日志克隆加上每个 `ContextSource` 的缓存内容——推理请求用它。因为装配每轮重建且从不写回持久日志，**锚定状态（env sensing、core memory、task anchor）靠重注入存活于压缩之后**，而非寄望压缩器保留它。压缩器只看瞬时装配；它摘要掉的任何东西下一轮都会被恢复。

### 这带来了三个直接后果

1. **抗压缩不靠压缩器**：被压缩器摘要掉的 ContextSource 块 / TaskAnchor / PlanProgress，下一轮 `assemble()` + `inject_pinned_blocks()` 重新注入。`context_assembler.rs:90-101` 的 `assemble()` 注释明确——「epoch/baseline 区分不再门控 *注入*，只有 `refresh_sources` 用它决定是否重调 `load()`。这就是让该块抗压缩的原因：它每轮重现，不管压缩器对上一份装配做了什么。」
2. **RefreshPolicy 只管 `load()` 是否重调，不管是否注入**：`context_assembler.rs:140-146` `refresh_sources()` 每轮对所有 source 重调 `load()` 更新缓存，缓存内容再由 `assemble()` 注入。旧的 `OnceAtStart`/`OnChange`「跳过注入」优化只在「注入累积进持久日志」的旧模型下才有意义——在瞬时模型下会让一个 source 第一轮后消失，所以被废弃（`context_assembler.rs:86-89` 测试 `every_source_reinjected_every_turn_regardless_of_policy` 锁死此行为）。
3. **压缩只压持久日志**：当装配后的请求会溢出，压缩的是**持久日志**而非瞬时装配——这样 `discarded_messages` 是真实转录、持久日志保持有界、锚定块在压缩后的持久日志上再重建（`agent_loop.rs:1041-1077`）。

> 历史教训（`agent_loop.rs:1056-1058`）：早期版本在「非压缩轮」把 `assembled` 丢弃、请求直接用裸持久日志，导致 ContextSource 注入在正常轮**从未到达模型**——已修复，现在每轮都走完整的 assemble → inject → fit-check → (compress durable → re-assemble → re-inject)。

---

## 2. 每轮装配管线（per-iteration assembly）

`AgentLoop` 主循环每轮的上下文装配步骤（`agent_loop.rs:1041-1200`），严格按序：

```
┌──────────────────────────────────────────────────────────────────────┐
│ 1. refresh_sources()            每轮重调所有 ContextSource.load() 刷新缓存  │ agent_loop.rs:1060-1062
│ 2. assemble(state)              持久日志克隆 + 注入 ContextSource 缓存块   │ agent_loop.rs:1070
│ 3. inject_pinned_blocks()       注入锚定块(见 §6)                         │ agent_loop.rs:1071
│ 4. needs_compression(conv)?     按 token 预算检查是否溢出                  │ agent_loop.rs:1073
│    ├─ 否：用当前 conv_for_inference                                      │
│    └─ 是：compress(state.conversation) 压持久日志(见 §5)                 │ agent_loop.rs:1074
│           → 重新 assemble + inject_pinned_blocks onto 压缩后持久日志    │ agent_loop.rs:1075-1076
│ 5. sync plan_state → metadata   活计划写回持久日志 metadata(抗压缩+恢复)  │ agent_loop.rs:1082-1090
│ 6. build InferenceRequest       + paradigm-aware tool defs                │ agent_loop.rs:1100-1119
│ 7. PreInfer gate                可临时注入/重写请求(见 §7)                  │ agent_loop.rs:1144-1181
│ 8. ContextAccounting::account   全装配+tool defs 的逐类 token 分解(见 §4)  │ agent_loop.rs:1195-1200
│ 9. infer (streaming or not)                                              │ agent_loop.rs:1222-1234
└──────────────────────────────────────────────────────────────────────┘
```

### 2.1 装配的内容顺序（注入到 `conv_for_inference`）

1. **持久日志全量**（`state.conversation.clone()`，`context_assembler.rs:91`）——system prompt + 历史 turns。
2. **ContextSource 缓存块**（`inject_sources`，`context_assembler.rs:108-130`）：按 `priority()` 升序注入，每个非空缓存内容包成 `[Context: {key}] {content}` 的 system 消息。predicate 恒为 `true`——所有缓存的 source 每轮都注入。
3. **锚定块**（`inject_pinned_blocks`，`agent_loop.rs:3625-3652`）：
   - `[Task Anchor] (do not compress — original task)`——原始任务原文 + 可选 distilled intent（`context_assembler.rs:164` `task_anchor_block`）；
   - `[Plan & Progress] (do not compress — live task list)`——当有活计划时，渲染 ✅/🔄/⏳ checklist（`context_assembler.rs:179` `plan_progress_block`）；
   - skill menu（Tier1 始终在）+ active skill 全量 prompt_template（当 `inject_skills` 为真）。
4. **运行时上下文块**（`runtime_context_block`，`context_assembler.rs:198-212`）：当前日期时间 + 时效性问题必须先 `web_search`/`web_fetch` 的指导，追加到 system prompt（session 启动时 `agent_loop.rs:908/936`）。**追加到 system prompt 而非临时 system 消息**，因为 system prompt 抗压缩性更好（`context_assembler.rs:194-197`）。

### 2.2 env-diff 检测归属

环境感知（git status、文件树、工作目录、当前日期）**完全由 `oneai-domain` 的 `ContextSource` 实现拥有**——`ContextAssembler` 自身不跑 git/filesystem 探针（`context_assembler.rs:17-21`）。这让 env sensing 可插拔、受 RefreshPolicy 治理、跨 DomainPack 可组合，而非一条硬编码的并行路径。例如 `GitStatusSource` 是 `OnChange`：git 状态变了，`load()` 返回新内容，下一轮装配注入完整 git 块。

---

## 3. token 预算驱动终止（而非 max_iterations）

### 3.1 TokenBudget

`budget.rs:326` ——会话/子代理的总 token 预算：

| 字段 | 含义 |
|---|---|
| `total: u32` | 总预算（可 `from_context_window` = 0.8×窗口） |
| `consumed: u32` | 已消耗（prompt + completion + tool 结果） |

关键方法：`remaining()`(:355)、`record_usage(prompt, completion)`(:360)、`can_support_iteration(estimated_cost)`(:365)、`estimated_remaining_iterations(per_iter_cost)`(:370)。

### 3.2 终止语义

`AgentLoopConfig` 有 `hard_max_iterations: Some(200)`（`agent_loop.rs:621`）作为**安全护栏**，但首要终止条件是 `TokenBudget`——`can_support_iteration` 不够即停。文档明确（`budget.rs:323-324`）：「当 `remaining()` 跌破 `min_iteration_cost`，loop 应终止。」这取代了硬编码的 `max_iterations`，让长任务自然受预算约束、短任务不因人为迭代上限被截断。

### 3.3 BudgetAllocation（按源比例分配）

`budget.rs:383` ——把预算按比例分给不同上下文源：

| 源 | 默认占比 |
|---|---|
| system_prompt | 10% |
| recent_turns | 30% |
| tool_results | 25%（最大，因 tool 输出可能极长） |
| skills | 10% |
| retrieved | 15% |
| overhead | 10% |

`CompressionPriority`（`budget.rs:461`）定义超预算时的裁剪优先级：`ToolResults`(1) → `OlderTurns`(2) → `Retrieved`(3) → `Skills`(4) → `RecentTurns`(5, 最后才动)。

### 3.4 ContextBudgetManager

`budget.rs:488` ——预算检查与自动压缩的编排器，注入 `AgentLoop` 的装配步骤（`budget.rs:482-487` 用法示例）：

- `needs_compression(conv)`(:576)：用 `TokenCounter`（若配了）或压缩器启发式（~4 chars/token）估 token，超 `budget.total` 即需压缩；
- `compress(conv)`(:597)：**三步管线**——
  1. `estimate_source_tokens`(:647) 按源估算；
  2. 若 tool_results 超 allocation → `truncate_tool_results`(:688) **无损截断 tier**——每个 `ToolResult` 块按预算折算成字符上限截头 + 追加 `[...output truncated — use memory_search for the full output]` 指针，告诉模型去 `memory_search` 取全量（`budget.rs:705`）；
  3. `compressor.compress()` 摘要旧段；
  4. 若配了 `DiscardedSink`，把 `discarded_messages` 落库为原始转录快照（`budget.rs:616-623`，C2 兜底）——「压缩即不丢」。

`with_token_counter(tc, model)`(:552) 接入模型感知 token 计数（见 §4），取代压缩器的 ~4 chars/token 启发式，CJK 文本估算更准；`with_discarded_sink`(:565) 接入原始转录归档。

---

## 4. 模型上下文三层解析 + token 计数

### 4.1 三层解析（opencode 式）

`model_context.rs` ——模型上下文窗口大小的**单一真相源**，严格优先级（`model_context.rs:3-20`）：

| 层 | 来源 | ContextSource 标签 |
|---|---|---|
| **L1 用户** | `ONEAI_CONTEXT_WINDOW` 环境变量（全局最高） / `ContextManagerConfig.profiles` 每模型 profile / `ModelConfig.extra["context_window"]` 每服务商模型 override | `UserEnv` / `UserProfile` / `UserProviderExtra` |
| **L2 服务商 API** | `LlmProvider::probe_context_window()`——Ollama `/api/show`、Anthropic `/v1/models/{id}`、Gemini `models.get`、OpenAI-compat best-effort；结果缓存 | `ProviderApi` |
| **L3 内置库** | `BUILTIN_MODEL_CONTEXT` 静态表（`model_context.rs:61-93`，Anthropic/OpenAI/Gemini/GLM/DeepSeek/Qwen/Llama 全覆盖，specific→general 排序）；仍未知则 `infer_context_window_for_tokenizer` 名字模式启发式 | `BuiltinLibrary` / `NameHeuristic` |

**两条解析路径**（`model_context.rs:16-20, 243-295`）：

- `resolve_cached(model)`（同步，:248）：L1 → probe cache → L3。**绝不发网络请求**——安全地用在同步 `TokenCounter::context_window_size` 里。probe 结果由异步预热/agent-loop 路径预先缓存。
- `resolve_with_provider(model, provider)`（异步，:270）：L1 → live L2 probe（写缓存）→ L3。供异步 trim 路径与 CLI `token probe` 用。

这套设计**镜像 opencode 的 `BUILTIN_MODEL_CONTEXT` + 三层解析**，同时契合 OneAI 的同步 `TokenCounter` trait 契约：探测是 warm-up 时 opt-in，同步路径只读缓存。

### 4.2 HeuristicTokenCounter（按服务商、CJK 感知）

`token_counter.rs:475` ——在无服务商专属 tokenizer 库时的合理估算：

- **按服务商家族**（`ProviderTokenizerType`，:173）：OpenAI tiktoken/BPE、Anthropic 自有、Google SentencePiece、Ollama 随模型、Generic 兜底。每家族不同 chars/token（:222-241）：

  | 类型 | 英文 CPT | CJK CPT |
  |---|---|---|
  | OpenAI | 4.0 | 2.0 |
  | Anthropic | 3.8 | 1.8 |
  | Google/Ollama/Generic | 4.0 | 2.0 |

- **CJK 感知**（`LanguageType::detect`，:280）：用 Unicode 范围判 CJK/Latin/Mixed，CJK 占比 >30% 视为 CJK 主导，混合文本走 50/50 加权（`chars_per_token_for_text`，:448）。GLM 也归类为 Ollama 式（中文导向分词，:205）。
- **每消息 overhead**（:354）：role 标记、分隔符、格式化——OpenAI 4 tok/msg、Anthropic 6 tok/msg、system prompt overhead 8-10、tool 定义 overhead 6-8。朴素 ~4 chars/token 启发式忽略这些。
- 估算对英文通常在 ±10% 内（:474 注释）。

`count_conversation_tokens`(:572) 逐块计数 Text/ToolCall/ToolResult/Image(170)/Thinking/File(50) + per-msg overhead + system overhead。

### 4.3 ContextFitResult（是否装得下）

`token_counter.rs:90` ——装配检查结果：`fits`（是否 ≤ 窗口×阈值）、`total_tokens`、`context_window`、`remaining_tokens`、`overflow_tokens`、`utilization_pct`。阈值默认 0.8（留 20% 给新 token，:73-78）。供 SmartRouter 上下文感知路由与 ContextManager 裁剪用。

### 4.4 ContextAccounting（逐类 token 分解）

`context_accounting.rs:31` ——把上下文窗口占用**按类别分解**：system prompt / user / assistant / tool_call / tool_result / thinking / image / file，各自 token + 占比 + 可视化条。供 TUI sidebar `📝~ctx N%` 与 `/context` 命令，**两者同源**用 `HeuristicTokenCounter` 保证一致（:9, :82-166）。`agent_loop.rs:1195-1200` 每轮用真实模型名（如 `glm-5.1` 而非 provider 类型名）算 accounting 喂给 observer。

### 4.5 SmartRouter 的 token 计数

`HeuristicTokenCounter` 的 `context_window_size`(:629) 当挂了 resolver 时委托给它（L1→cache→L3，:634-636）。SmartRouter 用它判断「这模型装不装得下当前对话」做路由决策——context-aware routing。

---

## 5. 四策略裁剪 + 压缩管线

OneAI 有**两套**裁剪/压缩实现，分工不同：

| 实现 | crate | 角色 | 何时用 |
|---|---|---|---|
| **ContextManager**（4 策略） | `oneai-core` | SmartRouter 路由到特定模型后，确保对话装得下该模型窗口的**即时裁剪** | SmartRouter/CLI token 路径 |
| **ContextCompressor**（LLM 摘要 + 抽取） | `oneai-memory` | AgentLoop 预算超限时的**摘要压缩** + 压缩耦合事实抽取闭环 | AgentLoop 主循环 |

### 5.1 ContextManager 的四策略（`context_manager.rs:46`）

`ContextTrimmingStrategy` ——质量/成本/可靠性权衡：

| 策略 | 做法 | 需 LLM | 默认 |
|---|---|---|---|
| **TruncateOldest**(:56) | 留最近 N 轮（默认 6≈3 轮交互）+ system + **首条 user 钉住**，旧轮截成 200 字 stub，长 tool_result 截 2000 字 | ❌ | ✅ 默认 |
| **ImportanceRanked**(:73) | 按重要度排序：system 永留 > 近轮完整 > tool_result 截断 > 旧轮摘要；保留有用 tool_result | ❌ | |
| **CompressMiddle**(:90) | 留 first N + last N，中间压成单条摘要（长会话友好） | ❌ | |
| **SmartSummary**(:112) | LLM 生成结构化 handoff（Goal/Progress/Key Decisions/Critical Files/Next Steps）+ 首条 user 钉住 + 近 N 轮 | ✅ | 需 summarizer |

**关键修复**（`context_manager.rs:535-552`）：旧版 `SmartSummary` **永远静默退化**成 TruncateOldest，从不生成 handoff。现 `with_summarizer`(:439) 接入 LLM 后真正摘要；无 summarizer 时退化成「首条 user 钉住的 TruncateOldest」**并 log**（不再静默）。

**Q2 硬保证——首条 user 钉住**（`context_manager.rs:599, :1287` 测试）：原始任务是压缩时最该保的上下文，被识别一次后视同 system/近轮——即使落入「旧段」也不被压成 200 字 stub。`TruncationCompressor`（`budget.rs:121`）与 `ContextCompressor`（`memory/compression.rs:159`）都镜像此保证。

### 5.2 ContextWindowProfile（每模型窗口画像）

`context_manager.rs:193` ——`model_name` + `context_window_tokens` + `max_output_tokens` + `recommended_utilization`(默认 0.8) + `trimming_strategy`。`effective_limit`(:264) = 窗口×利用率。`default_profiles`(:242) 内置 12 模型画像。`profile_for_model`(:467) 当挂了 resolver 时窗口数走三层解析（:470-476）覆盖静态画像。

### 5.3 ContextCompressor 的压缩管线（`memory/compression.rs:141`）

`compress()` 步骤（详见姊妹篇 §4.2）：

1. 保留最近 N 轮（`keep_recent_turns`，默认 6）；
2. **钉住首条 user 消息原文**（Q2/Q3 硬保证，:159）——放在 summary 与 recent tail 之间；
3. 每条将被摘要掉的旧消息做**无损截断**（`MAX_OLDER_MSG_CHARS=2000`，超长截头 + 指向 `memory_search`，:187）；
4. LLM 按领域 `CompressionTemplate`（`with_template`，:62）摘要旧段；
5. **压缩耦合抽取**：在 `discarded_messages` 上跑 `FactExtractor.extract`（`extract_and_archive`，:306），按领域 `extraction_schema` 抽原子事实 conflict-resolve 进 archival tier——**压缩掉的信息不丢失，变成长期记忆**；
6. `discarded_messages` 经 `DiscardedSink` 落库为原始转录快照（C2 兜底）。

整个抽取 **fail-safe**（:327）——坏抽取只 `tracing::warn!`，绝不打断压缩。

> 接线：`ContextCompressorTrait`（`budget.rs:31`）是依赖反转 trait——`oneai-core` 定义、`oneai-memory::ContextCompressor` 实现（`compression.rs:341`），`ContextBudgetManager` 接受任何实现。这让 core 不依赖 memory。

---

## 6. 抗压缩锚定注入（anti-compression pinning）

瞬时重注入模型让三类「绝不能被压缩掉」的状态靠**每轮重注入**存活：

| 锚定块 | 内容 | 来源 | 文件 |
|---|---|---|---|
| `[Task Anchor]` | 原始任务原文 + 可选 distilled intent；metadata 亦镜像 `task_anchor` | `task_anchor_block` | `context_assembler.rs:164` |
| `[Plan & Progress]` | 活计划的 ✅/🔄/⏳ checklist；同步进 `metadata["plan_state"]` | `plan_progress_block` + `agent_loop.rs:1082-1090` | `context_assembler.rs:179` |
| `[Core Memory]` + `[Recalled Context]` | 常驻策展事实 + 每轮召回；`RefreshPolicy::EveryIteration` 重注入 | `CoreMemorySource` | `core_memory_source.rs:80-91` |

**Task Anchor 双保险**（`context_assembler.rs:155-163`）：既每轮瞬时注入 pinned block，又镜像进 `Conversation::metadata["task_anchor"]`——每个压缩器都逐字拷贝 metadata（`budget.rs:200`、`compression.rs:248`），所以即使首条 user 消息本身被摘要掉，metadata 里的 task_anchor 仍在。

**Plan State 双保险**（`agent_loop.rs:1082-1090`）：活计划同步进 `metadata["plan_state"]`，`from_conversation` 恢复——既抗压缩又抗会话重载。

**CoreMemorySource 抗压缩**（`core_memory_source.rs:86-96`）：`refresh_policy() = EveryIteration` 是抗压缩的关键——压缩器丢旧轮（只留 `keep_recent_turns`），但下一轮 `assemble()` 重新注入 core 块。对比旧设计：一次性「Previous conversation context」system 消息埋历史里会被摘要抹去。`priority() = 10` 高优先级先注入。

---

## 7. 缓存感知装配 + 交互门上下文控制

### 7.1 prompt 缓存（Anthropic `cache_control: ephemeral`）

`anthropic.rs:179-262` ——静态上下文打 `cache_control: ephemeral` 断点，避免每轮重发：

- system prompt 块打断点（:192）；
- 最后一个 tool 定义打断点（:240-262）——这创造一个缓存边界，让 tool defs + system 稳定前缀命中缓存。
- `InferenceRequest.metadata["prompt_cache_policy"]`（`agent_loop.rs:1116-1117` 透传）控制：`Off` 时剥掉所有断点（基线测量），默认 `Auto` 开（:183）。

这与瞬时装配范式**协同**：锚定块每轮重注入看似「重复发」，但稳定前缀（system + 工具定义）被缓存命中，只有变化部分真正计费——`stream-macOS-mainqueue-flooding` 等记忆里记录的「节流只压 publish 没压派发」教训也呼应：重注入的 token 成本被 prompt cache 大幅摊薄。

### 7.2 交互门对上下文的控制

`InteractionGate` 的 5 个决策点里，两个直接改写上下文（`agent_loop.rs:1144-1181`）：

- **PreInfer**：应用层可 `ProceedWith{InjectSystemMessage}` **临时注入**（不写持久日志，防累积，:1156-1160）、`ReplaceRequest` 重写、`Revise{feedback}` 把反馈**既作持久 user turn 又加入本轮请求**（:1166-1172）、`Abort`。
- **PostInfer**：可校验/过滤/替换响应，或要求 feedback-grounded 重试。

`Revise` 的双写设计是上下文管理的细节：反馈既是持久轮（下一轮装配含它）又在本轮请求里（模型当轮就看到）。

---

## 8. 多代理上下文隔离（sub-agent / delegate / handoff）

### 8.1 sub-agent 只回摘要（context isolation）

`sub_agent.rs:8` 原则：「sub-agent 只把**摘要**返回主代理，完整对话不带回」。`SubAgentSummary`（:113）的 `summary` 字段（:127）是「蒸馏摘要，非完整输出」。这让深度任务分解可行——子代理在隔离上下文里做大量工作，主代理上下文只膨胀一条摘要。

### 8.2 并行多委托 + Kahn wave 调度

`parallel_executor.rs` + `scope_state.rs` 实现一轮多 `delegate` batch + Kahn 拓扑排序 wave 调度（记忆 `parallel-multi-delegation.md`）：

- **无依赖委托并行**执行；
- **有依赖委托串行**，且上游摘要自动注入下游子代理上下文（依赖感知）；
- 环检测。

摘要注入是上下文管理在多代理层的延伸：上游 sub-agent 的 `SubAgentSummary` 被注入下游的初始上下文，让依赖链上的信息流动而不爆主上下文。

### 8.3 范式切换改写上下文

`meta_tool.rs` 的模型驱动 `switch_paradigm` / `delegate`：`apply_paradigm_switch` + `AgentLoopGraphActionExecutor` 内联升级范式（system prompt + tool filter，见 `paradigm-delegate-metatool.md`）。范式切换即上下文重组——Plan/Reflect/Explore 各有不同 system prompt 与工具子集，上下文随范式变。

---

## 9. 在长程任务中的闭环（痛点 × 机制）

| 长程上下文痛点 | OneAI 机制 | 位置 |
|---|---|---|
| 上下文超限 | token-budget 触发压缩（非 max_iter），留最近 6 轮 | `agent_loop.rs:1073`, `budget.rs:576` |
| 窗口数不准（新模型/未知模型） | 三层解析 L1>L2>L3，sync 不发网络 | `model_context.rs:248` |
| token 估算偏差（CJK） | 按服务商 + CJK 感知 HeuristicTokenCounter | `token_counter.rs:475` |
| 压缩丢信息 | 压缩耦合 FactExtractor 抽取落档 + 原始转录快照兜底 | `compression.rs:306`, `budget.rs:616` |
| 原始 Goal 被摘要掉 | 首条 user 钉住（Q2/Q3）+ metadata 镜像 | `context_manager.rs:599`, `compression.rs:159` |
| 计划/进度被压缩 | PlanProgress 每轮重注入 + metadata 同步 | `agent_loop.rs:1082-1090` |
| 早期约束被长上下文稀释 | CoreMemorySource EveryIteration 重注入 + 约束沉淀 | `core_memory_source.rs:89` |
| 长输出淹没上下文 | 无损截断 tier（截头 + memory_search 指针） | `budget.rs:688`, `compression.rs:187` |
| 重注入的 token 成本 | prompt cache ephemeral 断点摊薄稳定前缀 | `anthropic.rs:179-262` |
| 多代理上下文爆炸 | sub-agent 只回摘要 + 并行依赖感知摘要注入 | `sub_agent.rs:8` |
| 装配轮被丢弃（历史 bug） | 每轮完整 assemble→inject→fit→compress | `agent_loop.rs:1056-1058` |

---

## 10. 与业界前沿对标

### 10.1 总览对标表

| 设计轴 | OneAI 现状 | 业界参照 | 评价 |
|---|---|---|---|
| **持久/瞬时分离** | `state.conversation` 持久 + `conv_for_inference` 每轮瞬时重建，从不写回 | Claude Code 的「context edit」、MemGPT OS 式分页 | ✅ 领先：重注入范式让抗压缩不依赖压缩器 |
| **抗压缩锚定** | TaskAnchor/PlanProgress/CoreMemory 每轮重注入 + metadata 双保险 | Aider repo-map 每轮重注入、Cursor 上下文钉选 | ✅ 对齐且有 metadata 双保险更稳 |
| **终止语义** | TokenBudget 驱动，hard_max_iterations 仅护栏 | 各框架普遍 max_iterations 或 token-cap | ✅ 自然预算约束 |
| **模型上下文解析** | 三层 L1>L2>L3，sync resolve_cached 不发网络 | opencode `BUILTIN_MODEL_CONTEXT` 三层 | ✅ 直接对齐 opencode |
| **token 计数** | 按服务商 + CJK 感知 + per-msg overhead 启发式 | tiktoken/真实 tokenizer；LangChain len-token counter | 🟡 启发式 ±10%，CJK 友好；缺真实 tokenizer |
| **裁剪策略** | 4 策略（Truncate/Importance/CompressMiddle/SmartSummary） | LangChain `trim_messages`（token/message/selector）；LlamaIndex postprocessor | ✅ 策略丰富，SmartSummary 真生成 handoff |
| **压缩管线** | LLM 摘要 + 无损截断 tier + 首条 user 钉住 + 压缩耦合抽取 | Claude Code auto-compact；MemGPT 摘要压缩 | ✅ 压缩耦合抽取是 OneAI 特色 |
| **缓存感知** | Anthropic ephemeral 断点 + policy 开关 | Anthropic prompt caching 官方指南；各框架 cache_control | ✅ 对齐官方，policy 可关 |
| **多代理隔离** | sub-agent 只回摘要 + 并行依赖感知注入 | Claude Code subagents 隔离上下文；LangGraph state channels | ✅ 对齐，Kahn wave 依赖注入领先 |
| **交互层上下文控制** | PreInfer 临时注入/Revise 双写 | 各框架多靠回调 | ✅ 临时/持久分离清晰 |
| **上下文会计** | ContextAccounting 逐类分解，sidebar 与 /context 同源 | Claude Code `/context`、Aider /tokens | ✅ 对齐 |

### 10.2 前沿研究/产品速记（便于深读对标）

- **Anthropic「context engineering」（2025-06）**：提出从「prompt engineering」转向「context engineering」——agent 的成败取决于每轮往上下文塞什么、扔什么。核心实践：① **just-in-time context**（按需取，而非全塞）；② **microagents/subagents**（隔离上下文做子任务，只回结论）；③ **auto-compaction**（超阈值摘要旧上下文）；④ **context window as budget**。OneAI 的瞬时重注入 + sub-agent 只回摘要 + token 预算 + 压缩管线**逐条对应**这套方法论。
- **Claude Code**：`/compact` 手动/自动压缩、subagent 隔离上下文、`context-edit` 动态改写、`/context` 显示占用。OneAI 的 `ContextAccounting` ↔ `/context`、`ContextCompressor` ↔ auto-compact、`sub_agent` 摘要 ↔ subagent 隔离。OneAI 额外把压缩**耦合进事实抽取**，比 Claude Code 的纯摘要更保信息。
- **MemGPT/Letta**：把 context window 当 RAM、OS 式分页 core↔archival。OneAI 的 `CoreMemory` 有 token 预算 + core↔archival 分页（enforce_budget 驱逐最久未更新非 pinned 事实到 archival），直接对应。但 OneAI 多了「瞬时重注入」——core 块每轮在装配层重注入，而非靠分页换入换出。
- **LangChain/LangGraph**：`trim_messages`（按 token/message、first/last selector）postprocessor、`ContextModule` 跨步上下文裁剪、长期记忆 store。OneAI 的 4 策略 ↔ `trim_messages` 的 selector；`ContextBudgetManager` 的按源比例分配 ↔ ContextModule 的预算分配思路。OneAI 缺 LangChain 的「跨步状态通道」细粒度裁剪。
- **Aider**：repo-map 每轮重注入（基于文件重要性排序），「持久/瞬时分离」的工程典范。OneAI 的 `ContextSource`（含 GitStatusSource 等领域 env 源）每轮重注入，理念同源；OneAI 把它泛化成可声明、受 RefreshPolicy 治理、跨 DomainPack 可组合的 trait。
- **「lost in the middle」**（Liu et al. 2024）：长上下文中间信息易被忽略。OneAI 的 `CompressMiddle` 策略（留首尾、压中间成摘要）与 `ImportanceRanked`（保留有用 tool_result 而非纯按时间）是对此的直接工程对策。
- **「just-in-time context」**（业界共识）：不必把所有可能相关信息塞进上下文，按需召回。OneAI 的 `[Recalled Context]` 每轮按 query 召回 top-k（而非全量灌入）+ `memory_search` 工具按需回溯原始转录，正是此范式。

### 10.3 OneAI 相对前沿的领先/持平/落后

- **领先**：① **持久/瞬时分离 + 重注入抗压缩**——抗压缩不靠压缩器保留、不累积、不漂移，比「摘要后塞回历史」的朴素做法更稳；② **压缩耦合抽取**——压缩掉的信息被抽取成可搜索长期记忆（Mem0/Letta 不原生有）；③ **模型上下文三层解析** sync 不发网络，契合同步 trait 契约；④ **声明式 DomainPack**——ContextSource/CompressionTemplate/MemoryProfile 一行切换上下文与压缩策略，比各框架写死更灵活。
- **持平**：token 预算驱动终止、4 裁剪策略、prompt cache ephemeral、sub-agent 上下文隔离、上下文会计。
- **落后**：① token 计数是**启发式**（±10%），非真实 tokenizer（tiktoken 等），CJK 已修但精确度有限；② 三因子召回权重/归一化硬编码（见姊妹篇 §12.4）；③ 无「跨步状态通道」式细粒度裁剪（LangGraph 风格）；④ 缺自动「上下文预热」——L2 provider probe 需 warm-up 显式触发，未在首次推理前自动就绪。

---

## 11. 缺口与改进方向

审阅中发现 4 个上下文管理层面的缺口（按影响排序；记忆层缺口见姊妹篇 §12）。

### 11.1 【中】token 计数为启发式，非真实 tokenizer

**事实**：`HeuristicTokenCounter`（`token_counter.rs:475`）用 chars/token 比例 + per-msg overhead 估算，英文 ±10%。`infer_context_window_for_tokenizer`（`token_counter.rs:687`）对未知模型靠名字模式（`glm-5`→203K 等）。

**影响**：预算检查与裁剪触发点有 ±10% 偏差，CJK/混合文本偏差更大；可能导致「以为装得下实则溢出」或「过早压缩」。

**修复方向**：对接 `tiktoken-rs`（OpenAI）/各服务商 token-count API（Anthropic `/v1/messages/count_tokens`、OpenAI token endpoint），按 provider 选真实 tokenizer；保留 `HeuristicTokenCounter` 作离线/无网络兜底。`ONEAI_CONTEXT_WINDOW` 已是用户 override 通道，可补 `ONEAI_TOKENIZER=real|heuristic` 开关。

### 11.2 【中】L2 provider probe 未自动预热

**事实**：`resolve_cached`（:248）只读 probe cache，不发起探测；L2 live probe 由 `AppSession::warm_model_context` / CLI `token probe` 显式异步触发（`model_context.rs:150-154`）。

**影响**：若 warm-up 未跑或失败，首次推理前 `context_window_size` 退回 L3 静态值，对非内置库的新模型（如刚发布的模型）可能窗口数不准，影响路由与裁剪决策。

**修复方向**：在 `AppSession.run` 首轮前自动 `resolve_with_provider`（已有 provider），失败 fail-open 到 L3，避免依赖外部显式 warm-up。

### 11.3 【低】缺「跨步状态通道」细粒度裁剪

LangGraph 的 `ContextModule`/state channels 可按步保留/裁剪特定状态键，OneAI 的裁剪粒度是「消息级」（system/近轮/tool_result/旧轮），无法对「某个工具的累积状态」单独裁剪。

**修复方向**：在 `ContextSource` 层引入「带预算的累积源」——source 自身管理 token 预算与裁剪，而非全量重注入。当前 `CoreMemory` 已有 token 预算 + 驱逐，可推广到其他长寿命源。

### 11.4 【低】CompressMiddle / ImportanceRanked 未接 ContextBudgetManager 主路径

`ContextBudgetManager.compress`（`budget.rs:597`）走 `ContextCompressorTrait`（memory 的 LLM 摘要），而 core 的 4 策略 `ContextManager` 主要服务 SmartRouter 路由。两者策略空间未统一——例如 AgentLoop 主循环无法声明式选用 `CompressMiddle` 而非 LLM 摘要。

**修复方向**：让 `ContextBudgetManager` 接受 `ContextTrimmingStrategy`，按域 `MemoryProfile`/`CompressionTemplate` 声明压缩策略（LLM 摘要 / CompressMiddle / ImportanceRanked），与裁剪策略统一在 DomainPack 层声明。

---

## 12. 小结：OneAI 上下文管理的定位

OneAI 的上下文管理**在工程闭环上达到一线 agent 框架水平**，在两个维度上领先：

- **领先**：① 持久/瞬时分离 + 重注入抗压缩——抗压缩不靠压缩器、不累积、不漂移，是对「context engineering」范式的彻底贯彻；② 压缩耦合抽取——压缩掉的信息变可搜索长期记忆，堵死「压缩即丢失」；③ 模型上下文三层解析 sync 不发网络 + 声明式 DomainPack 让上下文/压缩策略一行切换。
- **持平**：token 预算驱动终止、4 裁剪策略、prompt cache ephemeral、sub-agent 上下文隔离 + 并行依赖感知摘要注入、上下文会计逐类分解、首条 user/PlanState 双保险锚定。
- **落后**：token 计数为启发式非真实 tokenizer（§11.1，最该先修）；L2 probe 未自动预热（§11.2）；无跨步状态通道细粒度裁剪（§11.3）；压缩与裁剪策略空间未统一（§11.4）。

一句话：**OneAI 把「上下文作为抗压缩、预算驱动、压缩无损、声明式可切换的瞬时装配」做对了，与 Anthropic 2025「context engineering」方法论逐条对应**——补齐真实 tokenizer（§11.1）与 L2 自动预热（§11.2）后，上下文窗口的「装得下、裁得准、压不丢、记得回」四要素才完整兑现设计意图。

---

### 附：关键文件索引

| 关注点 | 文件 |
|---|---|
| 瞬时装配器 | `crates/oneai-agent/src/context_assembler.rs:46` |
| 装配/压缩每轮管线 | `crates/oneai-agent/src/agent_loop.rs:1041-1077` |
| 锚定块注入 | `crates/oneai-agent/src/agent_loop.rs:3625` |
| 上下文会计 | `crates/oneai-agent/src/agent_loop.rs:1195-1200` |
| 预算管理器 | `crates/oneai-core/src/budget.rs:488` |
| 无损截断 tier | `crates/oneai-core/src/budget.rs:688` |
| 压缩 trait（依赖反转） | `crates/oneai-core/src/budget.rs:31` |
| 4 裁剪策略 + ContextManager | `crates/oneai-core/src/context_manager.rs:46` |
| 模型上下文三层解析 | `crates/oneai-core/src/model_context.rs:159` |
| 内置模型库 | `crates/oneai-core/src/model_context.rs:61` |
| 启发式 token 计数 | `crates/oneai-core/src/token_counter.rs:475` |
| ContextFitResult | `crates/oneai-core/src/token_counter.rs:90` |
| 上下文会计类型 | `crates/oneai-core/src/context_accounting.rs:31` |
| LLM 压缩器 + 抽取闭环 | `crates/oneai-memory/src/compression.rs:26` |
| 抗压缩注入源 | `crates/oneai-memory/src/core_memory_source.rs:31` |
| RefreshPolicy + ContextSource | `crates/oneai-domain/src/context_source.rs:30` |
| 压缩模板 | `crates/oneai-domain/src/compression_template.rs` |
| prompt 缓存断点 | `crates/oneai-provider/src/anthropic.rs:179-262` |
| sub-agent 摘要隔离 | `crates/oneai-agent/src/sub_agent.rs:8` |
| 并行依赖感知调度 | `crates/oneai-agent/src/parallel_executor.rs` |
| 范式/委托 meta-tool | `crates/oneai-agent/src/meta_tool.rs` |
| 姊妹篇 | `docs/memory-mechanism.md` |
