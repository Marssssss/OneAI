# OneAI 距离一流 Agent 框架的差距分析 — 代码级核查

**日期**: 2026-07-21
**方法**: 对 `crates/` 全栈做代码级核查（非读文档），覆盖五大子系统，对照 2024–2026 agent loop engineering 文献（ReAct / Reflexion / Plan-and-Act / Letta-MemGPT / A-MEM / Generative Agents / SWE-agent / Temporal-DBOS durable execution / LangGraph）。
**目的**: 查明 OneAI 距离"高效、稳定、能力强大、易于集成"四目标的真实差距，并给出优先级路线图。

---

## 总体结论

OneAI 的**结构宽度**（DomainPack 7 层、范式、StateGraph、多 agent 引擎、A2A、WASM、MCP、eval、Studio）在开源 agent 框架中罕见，是护城河。但**主执行路径的工程深度**——自纠正、预算护栏、可观测性、恢复、durable 执行——多处停在"脚手架已立、未通电"的状态。

**最致命的发现：存在系统性的"文档/记忆 vs. 实际代码"漂移。** README、CLAUDE.md、项目记忆描述的若干"已实现"机制，在主执行路径上是死代码或 stub。这些是距离"稳定、可靠"框架最致命的距离——因为用户与开发者会信任它们已生效，构成虚假安全感。

---

## 一、致命漂移：文档说是、代码不是（Hot-path dead/stub 清单）

全部有 `file:line` 实证。

| 机制 | 文档/记忆声称 | 代码实情 | 位置 |
|---|---|---|---|
| **三层解析器** | constrained→fuzzy→self-correct 是 README 头牌防御 | `AgentLoop::parse_decision` 完全绕过 `ThreeLayerParser`，直接 `serde_json::from_str(args).unwrap_or_else(\|_\| json!({}))`，畸形参数被静默丢弃，不回喂模型 | `agent_loop.rs:2530,2542`；parser 字段 `#[allow(dead_code)]` 见 `:4120` |
| **TokenBudget 终止** | "终止由 TokenBudget 治理，非硬编码 max_iterations" | 主循环从不检查/扣减任何 token 预算；唯一硬上限是 `hard_max_iterations`（默认 200），设 `None`→`usize::MAX`（无上限）→ 失控模型烧钱无上限 | `agent_loop.rs:1090,657`；`budget.rs:478` |
| **ContextManager + 4 裁剪策略** | TruncateOldest/ImportanceRanked/CompressMiddle/SmartSummary | 主循环完全绕过 `ContextManager`（仅 team/swarm 用）；4 策略中 3 个无运行时实现，真实压缩只走 `ContextCompressor` 的 keep_recent | `context_manager.rs`；`session.rs:873-892` |
| **OTEL 可观测性** | OtlpCollector + OtelMetricsProvider + AgentLoop trace | `OtlpCollector` 只把 span 存进本地 Vec，不真正导出 OTLP；`OtelMetricsProvider` 是裸 AtomicU64 且从未被 AgentLoop/AppBuilder 实例化；`tracing` crate 仅当日志宏用，无 `#[instrument]` | `otel_exporter.rs:282-301`；`otel_metrics.rs` |
| **RecoveryManager** | 工具失败→Retry/Rollback/FallbackRoute | 结果只作为 system message "告知"模型，不真正重试/回滚/路由；Rollback 引用的 checkpoint 系统已被删除 | `error_recovery.rs:272`；`agent_loop.rs:2143-2167` |
| **同族模型降级** | DegradationRule (Opus→Sonnet→Haiku) | 配置+预设齐全，pool fallback 循环从不调用 `route_for_degradation` | `provider_pool.rs`；`smart_router.rs:497` |
| **A2A Server** | A2AServerHost + 路由 + 处理器 | 无 HTTP server（`host.run(port)` 不存在），`cmd_a2a_serve` 只 `ctrl_c().await`；`handle_send_task` 返回占位响应，不跑 AgentLoop | `server.rs:15`；`cmd_a2a.rs:57`；`handler.rs:98` |
| **Team/Swarm/Handoff** | 4+3+3 策略引擎 | 引擎有单测但 CLI/AppBuilder 不可达，CLI 注释自认"未接入"；生产多智能体路径只有 `delegate` | `cmd_team.rs:128`；`cmd_swarm.rs:141` |
| **压缩抽取事实** | "压缩即不丢"、§12.1 已修 | `extract_and_archive` 直连 `archive.upsert`，绕过 `archive_facts`→ 压缩抽取的事实永不嵌入、永不落 SQLite，重启即失、语义不可达。记忆系统最严重单点 | `compression.rs:322-323` vs `manager.rs:338-348` |
| **Custom 边条件** | StateGraph 自定义边 | `Custom` 变体 log warn 返回 false，无注册机制 | `state_executor.rs:743` |
| **Layer1 约束解码 / Layer3 自纠正** | 三层防御完整 | Layer1 = `StubConstrainedDecoder`（`is_available()==false`）；Layer3 `max_retries` 默认 3 但循环体硬编码只做 1 次 | `constrained.rs:31`；`fallback.rs:45-46` |

> 含义：在"高效、稳定、可靠"三轴上，OneAI 主执行路径目前**没有**真正生效的自纠正、没有真正的预算护栏、没有真正的可观测性导出、没有真正的恢复。这些恰是 loop-engineering 文献反复强调的"agent loop 鲁棒性脊梁"（ReAct/Reflexion 的错误回喂、SWE-agent 的 action 修复循环、Plan-and-Act 的预算约束）。

---

## 二、按子系统细化的差距

### 2.1 AgentLoop & 范式（核心引擎）

**已实现优点**：4 分支决策（DirectAnswer/ToolCalls/Delegate/SwitchParadigm）真实；流式有 60s idle timeout（解决了 macOS 卡死）+ 双重 `tokio::select!` 取消；多委托 Kahn wave 调度真实且按输入序回填（确定性 feed-back）+ 环检测；working_state 投影零 IO 热路径 + append-only JSONL + 跨 session 发现 + git 对账。

**差距**：
- **畸形输出不回喂**：malformed tool args 被 `unwrap_or(json!({}))` 吞掉（`agent_loop.rs:2542`）。Reflexion/SWE-agent 的核心教训是"把错误显式回喂给模型让其自纠正"，OneAI 反向操作。
- **非 RateLimit 的 provider 错误直接终止循环**（`agent_loop.rs:1411-1424`），无重试无退避。一次 5xx/网络抖动杀掉整个 run。
- **PostToolUse hook `tool_name` 硬编码 `""`**（`:2100`），PostToolUse 结果明确"不改变工具输出"（`:2109`）——hook 机制形同虚设。
- **`apply_paradigm_switch` 删掉所有 system message**（`:3148`），连带丢失 runtime_context（日期/web 指引）和 domain 注入的系统上下文。
- **`AgentLoopGraphActionExecutor` 持有但不用** parser/hook_registry/recovery_manager（`:4120` 注释自认）→ 内联范式切换走 StateGraph 时不触发 hook、不用解析器、不恢复——一致性洞。
- **预算不一致**：StateGraphExecutor 用 `unwrap_or(50)`，主循环默认 200，都可被覆盖为 None。
- **空响应重试丢失 `prompt_cache_policy` metadata**（`:1618`），重试打不到 prompt cache。

### 2.2 工具 / 权限 / Skill / MCP

**优点**：并行工具执行真实（`join_all`）；权限三分级 + 5 决策点 InteractionGate；MCP 经 rmcp 真实接入且 MCP 工具被 `PermissionAwareTool` 同等门控。

**差距（安全护栏严重不足）**：
- **无 ToolExecutor 级输出尺寸上限**——只有各工具 ad-hoc 自截（shell 100KB、read 1MB…）。一个无自截的 MCP/自定义工具的长输出会在压缩触发前撑爆 context。
- **`ShellTool::new()` 默认 regex-only**，`default_sandbox_backend` 从不自动接入——生产默认无 Seatbelt/Docker 真隔离。
- **shell 黑名单正则浅薄**：`rm -rf /` 能挡，但 `rm -rf ~`/`find / -delete`/base64 payload/`curl|sh` 全过。
- **文件工具的 `..` 检测是子串匹配非规范路径**，`foo/..bar` 误杀、符号链接逃逸不挡；`apply_patch`/`write_file` 的遍历守卫未确认。
- **`ToolExecutor` 完全无视 `PermissionProfile`**（`executor.rs:206-212`，只看 `risk_level`）→ workflow 步骤绕过 domain 的 `deny_by_default`。两条权限路径分叉。
- **`PermissionProfile::resolve` 代码顺序与文档注释相反**（auto_approve 在 permission_overrides 之前）。
- **无权限决策审计日志**（只有 best-effort `tracing::warn!`）。
- **`ThresholdInteractionGate` 仍按 legacy `RiskLevel` 而非 `PermissionLevel`**。
- **范式 tool_filter 硬编码且与注册集漂移**：`write_file`/`delete_file`/`web_search` 不在任何范式 filter 里。
- **Skill `SkillSelector` embedding 模式是死代码**（`#[allow(dead_code)]`），语义选择从未工作；skill 无版本/依赖/参数 schema；无 trust 边界（`~/.claude/skills/evil` 静默运行）。
- **MCP 工具权限无法按 server 粒度覆盖**，同名工具在 HashMap 里 last-wins 冲突。
- **`mcp_tools.rs` 是 `mcp_real.rs` 的死桩副本**。
- **缺工具**：无沙箱代码执行（code-interpreter）、无 computer-use/Playwright、无 git 工具、`apply_patch` 无 backup/undo（失败的多文件 patch 留半改状态）、无 mkdir/move/copy 结构化 FS 工具（逼模型退回 shell）。

### 2.3 记忆 / RAG / 嵌入 / 工作状态

**优点**：3 层 Letta 式 + Mem0 冲突更新（`_superseded_history` 审计 + 软失效）+ SQLite 镜像一致；FastEmbed 是**真实现**（记忆里"stub"条目已过期）+ auto 探测 + adapter registry + 无 key 优雅退化关键词召回；working-state 是最自洽的子系统。

**差距（vs Letta/MemGPT/A-MEM/Generative Agents/Zep-Graphiti）**：
- **压缩抽取事实 write-only**（见漂移表）——最大单点，被动抽取（长会话事实主要来源）恰好是坏的那条路径。
- **反思非递归**：`reflect_with_prior` 只喂"最近 3 条"情景事实，不按相关性检索；Generative Agents 的"洞察引用洞察"反思树未实现。
- **无时序知识图 / 实体中心 / bi-temporal**（vs Zep/Graphiti 的边有效期、episode→derived-fact 溯源）。
- **无衰减/遗忘策略**：事实永久累积，`temporal_score_fact` 只在检索降权从不删除。
- **无检索校准 / 弃权阈值**：`abstention` 有评测但不强制。
- **RAG 无持久化**（DocumentIndex 是内存 HashMap）、**无真正混合融合**（vector 与 keyword 是两条独立路径，非 memory 那套 search_hybrid）、**无 BM25**（只是词频）、**CrossEncoder 文档写了枚举里没有**。
- **`RefreshPolicy` 是装饰**：assembler 不 gate，OnResume/Periodic 只是 promise，正确性依赖每个 `ContextSource::load()` 自缓存。
- **遗留死代码** `ltm_entries`/`stm_entries`/`ShortTermMemory`/`LongTermMemory`/`EmbeddedVectorStore` 仍被 `pub use`（P3-1 API 稳定性理由），但 `delete_conversation` 还在 touch `stm_entries`——维护陷阱。
- **`chat --resume` 未实现**（`cmd_session` 只打印计数）。

### 2.4 Workflow / StateGraph / 多智能体 / A2A / 解析器

**优点**：WorkflowDag（声明式、并行 wave、HITL 审批、模板插值、retry）真实；StateGraph 与 AgentLoop 经 `run_with_state_graph` 闭环；GroupChat 共享 transcript 原语真实且被 macOS 场景 UI 用。

**差距**：
- **无 durable execution**（vs Temporal/DBOS）：StateGraph walk 状态从不中途持久化，无 replay、无崩溃恢复。interrupt checkpoint 只记成 opaque string，**无 resume API**；`Revise` 反馈被显式丢弃（"stateless graph path can't loop on feedback → deny"）。
- **StateGraph 不支持并行分支**（单 walker，非 frontier）；map-reduce/Send fan-out 无。
- **确定性破坏**：DAG level 计算遍历 `HashMap`（`dag.rs:162`），跨运行 level 划分都可能变。
- **Team/Swarm/Handoff 不可达**（见漂移表）；`max_concurrent` 信号量从未实现（`team.rs:491` `_max_concurrent`）；swarm 质量估计是字符串长度启发式；debate 是拼接字符串非结构化论证图。
- **无共享 blackboard / 消息总线**：所有跨 agent 通信要么共享 transcript、要么单向依赖摘要注入、要么 relay 前缀。
- **A2A 半实现**：client 侧 SSE 解析真；server 侧 `sendSubscribe` 是占位；push notification 结构体存在但永不投递且 capability 硬编码 false（虚假广告：card 声明 `streaming:true` 但 server 不能流）；server 无 auth 校验；无 `tasks/resubscribe`；TaskStore 仅内存。
- **解析器**：Layer2 fuzzy 是手搓括号平衡+正则（不处理串内括号/转义引号/尾逗号/注释），远逊 `serde-json-repair`/`json5`/`partial-json`；JSON Schema 校验是手搓子集（无 `$ref`/`oneOf`/`format`/`pattern`），代码注释自认"production 应用 jsonschema crate"。

### 2.5 Provider / 上下文 / Token / 可观测性

**优点**：ProviderPool fallback 链 + FallbackEvent + 断路器真实；SmartRouter 多因子（latency/quality，USD 已彻底删除）真实；3 层模型上下文解析（用户配置 > provider probe > builtin）+ `probe_context_window` 四家都实现；Anthropic `cache_control:ephemeral` 真且 `cache_read_tokens` 回读到 trace；UsageTracker + SqliteUsageTracker 真接线。

**差距**：
- **Gemini/Ollama 无 provider 级 retry**（只有 OpenAI/Anthropic 调 `send_with_retry`）——一次 429 直冲 AgentLoop 的粗粒度 5s 重试。
- **退避无 jitter**（纯指数 `initial*factor^attempt`）→ 同步客户端 retry-storm 风险。
- **无真 tokenizer**：全栈 `tiktoken`/`tokenizers`/`hf-hub` 0 命中，全是 4 chars/token 启发式；CJK 比例是粗平均。`truncate_tool_results` 用裸 `chars×4`，忽略已挂载的 TokenCounter——准确分母+近似分子，溢出触发可差 10–20%，CJK 重时可能在压缩触发前真撑爆窗口。
- **OpenAI provider 跳过 `ContentBlock::Image`/`File`/`Thinking`**（`openai.rs:148`）却声称 `supports_multimodal:true`——视觉仅 Anthropic。
- **无 Batch API、无 OpenAI/Gemini prompt caching 标记**。
- **`recent_fallback_count` 文档说"last 24 hours"实为 lifetime `total_count()`**——误导运维。
- **pool 级路由 context-blind**：`route_for_pool` 被以空 task desc + 硬编码 "react" 调用。
- **FallbackLog/SmartRoutingLog 内存 capped 1000，无持久化**——fallback/路由决策无跨重启审计。
- **分布式 trace 不传播**：sub_agent/A2A 全无 `trace_context`/traceparent，子 agent 是 trace 孤岛。

---

## 三、对标 loop-engineering 文献的定位

文献（ReAct、Reflexion、Plan-and-Act、SWE-agent、Generative Agents、Letta/MemGPT、A-MEM、LangGraph、Temporal/DBOS）收敛出的"成熟 agent loop"必备特征，对照 OneAI 现状：

| 文献共识的成熟特征 | OneAI 现状 |
|---|---|
| 错误显式回喂→模型自纠正（Reflexion/SWE-agent） | ❌ 畸形参数静默丢弃 |
| 真正的 token/步预算护栏 | ❌ TokenBudget 是 vaporware，仅迭代数硬上限 |
| 约束解码/结构化输出（provider 原生或 grammar） | ⚠️ 仅 provider 原生 tool-call，Layer1 stub |
| 可观测性导出到标准后端（OTLP/Jaeger） | ❌ OTLP stub，metrics 未接线 |
| 工具失败的真重试/回滚（不是只告知） | ❌ Recovery 仅 informational |
| Durable workflow + replay/resume（Temporal/DBOS） | ❌ 无中途持久化、无 resume API |
| 活跃自管理的记忆（A-MEM 反思-合并-链接-衰减） | ⚠️ 骨架在但压缩路径写穿、反思非递归、无衰减 |
| 多智能体共享状态/blackboard | ❌ 仅 transcript/单向注入 |
| 沙箱化代码执行 + computer-use | ❌ 无独立 code-exec 工具，shell 是 Full 权限 |
| Prompt caching 全 provider | ⚠️ 仅 Anthropic |

---

## 四、优先级路线图

按"稳定性 > 可靠 > 能力 > 易集成"排序，且优先修"已宣称但没真生效"的（这些是最危险的，因为用户和开发者会信任它们工作）。

### P0 把已宣称的脊梁通电（最高优先，修"虚假安全感"）
1. **接线三层解析器到 `parse_decision`**：畸形 tool args 走 fuzzy→self-correct 并回喂模型（而不是 `unwrap_or(json!({}))`）。"稳定"单点最大杠杆。
2. **实现 TokenBudget 真终止**：主循环每轮检查 `budget.remaining()`，超阈值 break。默认 `hard_max_iterations=None` 必须改为有预算或硬上限兜底。
3. **修压缩抽取事实路径**：让 `extract_and_archive` 走 `MemoryManager::archive_facts`（嵌入+落 SQLite），兑现 §12.1 的真实承诺。
4. **真接 OTEL**：`OtlpCollector` 接真 `opentelemetry-otlp` exporter；`OtelMetricsProvider` 接入 AgentLoop；给 sub_agent/A2A 传 `trace_context`/traceparent 实现分布式追踪。
5. **RecoveryManager 真生效**：工具失败时程序性重试（带 jitter 退避），不只是塞 system message。

### P1 安全护栏补齐
6. **退避加 jitter**；Gemini/Ollama 接 `send_with_retry`。
7. **ToolExecutor 级输出尺寸上限**（统一的 tool-result 截断守卫，而非各工具 ad-hoc）。
8. **`ShellTool::new()` 默认接 `default_sandbox_backend`**；黑名单改用规范化命令解析；文件工具 `..` 改 canonical-path 校验。
9. **统一权限路径**：`ToolExecutor` 接 `PermissionProfile`（修 workflow 绕过 deny 的洞）；加权限决策审计日志；`ThresholdInteractionGate` 迁到 `PermissionLevel`。
10. **`apply_paradigm_switch` 只删范式 prompt**，保留 runtime_context 与 domain system block。

### P2 能力补齐（对标 SWE-agent/computer-use/Letta）
11. **沙箱代码执行工具**（独立于 shell，Standard 权限，有输出上限+超时）。
12. **`apply_patch` 加 backup/undo 栈**（多文件 patch 失败可回滚）。
13. **真 tokenizer**（tiktoken-rs / huggingface-tokenizers），让 `truncate_tool_results` 真用 TokenCounter。
14. **StateGraph resume API** + 中途持久化 walk state（向 durable execution 走第一步）。
15. **A2A server 接 axum** + `handle_send_task` 真跑 AgentLoop（补 streaming/push/auth）。
16. **记忆衰减 + 递归反思**：importance 阈值驱逐 archival；反思按相关性检索 prior 而非最近 N 条。

### P3 易集成 / 收尾
17. **Team/Swarm/Handoff 接入 AppBuilder**（CLI 注释自认未接，补工厂方法）。
18. **删死代码**：`mcp_tools.rs`、`ltm_entries`/`ShortTermMemory`/`LongTermMemory`（或在 P3-1 稳定性承诺下显式 deprecate 标注）。
19. **SkillSelector embedding 真工作** + skill 版本/依赖/trust 边界。
20. **`Custom` 边条件注册机制**；StateGraph 并行分支；WorkflowDag 改 BTreeMap 恢复确定性。

---

## 附：核查覆盖的文件

- `crates/oneai-agent/src/agent_loop.rs`、`context_assembler.rs`、`streaming.rs`、`meta_tool.rs`、`plan_agent.rs`、`plan_state.rs`、`reflection_agent.rs`、`sub_agent.rs`、`error_recovery.rs`、`structured_output.rs`、`hooks.rs`、`async_task_runner.rs`、`skill_tool.rs`
- `crates/oneai-tool/src/{registry,executor,interaction_gate,tool_interfaces,sandbox,apply_patch,local_tools,mcp_real,mcp_tools}.rs`
- `crates/oneai-core/src/{types,traits,platform,budget,token_counter,model_context,context_manager,usage,provider_pool,smart_router,swarm}.rs`
- `crates/oneai-memory/src/{manager,compression,fact_store,memory_tools,short_term,long_term,core_memory,core_memory_source}.rs`
- `crates/oneai-rag/src/{embedding,index,retrieval}.rs`
- `crates/oneai-persistence/src/{working_state_store,sqlite_store,usage_tracker,checkpoint}.rs`
- `crates/oneai-workflow/src/{config,dag,compiler,executor,validator,state_graph,state_executor}.rs`
- `crates/oneai-a2a/src/{card,client,server,router,handler,task_store,transport,types}.rs`
- `crates/oneai-parser/src/{constrained,fuzzy,fallback,three_layer}.rs`
- `crates/oneai-provider/src/{provider_pool,smart_router,retry,openai,anthropic,gemini,ollama}.rs`
- `crates/oneai-trace/src/{otel_exporter,otel_metrics}.rs`
- `crates/oneai-app/src/{builder,session}.rs`
- `examples/cli/src/{cmd_a2a,cmd_session,cmd_team,cmd_swarm,cmd_handoff}.rs`
