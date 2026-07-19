# OneAI 记忆机制白皮书

> 版本：对应代码库 `0.2.0` / 1.0.0 线。本文基于对 `crates/oneai-memory`、`oneai-rag`、`oneai-persistence`、`oneai-domain`、`oneai-app` 源码的逐文件审阅撰写，所有机制均标注 `file:line` 以便核对。文末与业界前沿记忆系统（Mem0 / Letta / Generative Agents / Zep-Graphiti / A-MEM / Cognee）对标。

---

## 0. 一句话概括

OneAI 的记忆系统是一个 **「Letta 式三层 + Mem0 式冲突更新 + Generative-Agents 式三因子召回 + 压缩耦合抽取」** 的声明式记忆引擎：工作记忆单源化在 `Conversation` 上，长期记忆以原子 `MemoryFact` 为单位、按 `(user_id, subject, predicate)` 冲突更新，每轮用「相关度 + 近因 + 重要度」召回注入，被压缩丢弃的轮次会被 LLM 抽取成事实落档——「压缩即丢失」被闭环堵死。整个记忆行为由 DomainPack 第 7 层 `MemoryProfile` 声明，一行 `AppBuilder::domain_pack(...)` 切换。

---

## 1. 架构总览：分层与数据流

```
                       ┌─────────────────────────────────────────────┐
                       │            AgentLoop (oneai-agent)           │
                       │   每轮迭代：infer → 解析 → 工具/委托/范式   │
                       └───────────────┬─────────────────────────────┘
                                       │ 每轮重新 assemble 上下文
                                       ▼
            ┌──────────────────────────────────────────────────────────┐
            │  ContextAssembler  (oneai-agent/context_assembler.rs)  │
            │  注入源：ContextSource trait，按 epoch/refresh_policy   │
            └───────────────┬──────────────────────────────┬─────────┘
                            │                                │
            ┌───────────────▼──────────┐      ┌──────────────▼──────────┐
            │  CoreMemorySource         │      │  领域 ContextSource    │
            │  (EveryIteration,抗压缩)  │      │  (env diff / skill…)   │
            │  · [Core Memory] 常驻块  │      └─────────────────────────┘
            │  · [Recalled Context]    │
            └───────────────┬──────────┘
                            │ set_recall(facts) 每轮由 AppSession 写入
                            │
       ┌────────────────────▼──────────────────────────────────────┐
       │                  MemoryManager (统一入口)                 │
       │   oneai-memory/src/manager.rs                            │
       ├──────────────────────────────────────────────────────────┤
       │  core_memory  (CoreMemory, Letta core, token 预算)      │
       │  fact_archive (MemoryFactStore, archival, 全量事实)     │
       │  reflection?  (MemoryReflection, 会话末 episodic 提炼) │
       │  persistence? (MemoryPersistence → SQLite)             │
       │  embedding?   (EmbeddingService → 语义召回)            │
       └──────────────────────────────────────────────────────────┘
                            │                │                  │
              ┌─────────────▼─┐    ┌─────────▼────────┐  ┌──────▼─────────┐
              │ MemoryFact    │    │ ContextCompressor│  │ SqliteSession  │
              │ (oneai-core)  │    │ + FactExtractor  │  │ Store          │
              │ atomic fact   │    │ 压缩耦合抽取      │  │ memories 表    │
              └───────────────┘    └──────────────────┘  └────────────────┘
```

**三个 crate 的职责分工：**

| crate | 角色 | 关键文件 |
|---|---|---|
| `oneai-memory` | 记忆引擎本体：三层、抽取、召回、反思、自管理工具 | `manager.rs:655`, `fact_store.rs`, `core_memory.rs`, `compression.rs`, `fact_extraction.rs`, `reflection.rs`, `core_memory_source.rs`, `memory_tools.rs` |
| `oneai-rag` | 嵌入服务（OpenAI/Anthropic/Ollama/FastEmbed）+ 自动嵌入文档索引 | `embedding.rs:1258` |
| `oneai-persistence` | SQLite 持久化：会话/LTM/事实/用量 + 渐进 Checkpoint | `sqlite_store.rs:1302` |
| `oneai-domain` | `MemoryProfile`（DomainPack 第 7 层）声明式记忆策略 | `memory_profile.rs:246` |
| `oneai-core` | 共享类型：`MemoryFact`/`FactType`/`RecallConfig`/`MemoryScope` + `MemoryPersistence`/`EmbeddingService`/`DiscardedSink` trait | `types.rs:1200`, `traits.rs:479` |

---

## 2. 记忆的三层结构（Letta 式）

OneAI 把记忆显式分为三个 tier，这是对 Letta/MemGPT「core / archival / recall」三层模型的直接映射，但有一个关键修正：**工作记忆单源化（M1）**。

### 2.1 工作记忆（Working Memory）—— 单源在 `Conversation`

> 历史教训：早期实现里 STM/LTM 是两套平行的 `MemoryEntry` 存储，会出现「压缩了但 STM 没同步」的漂移。重构后（M1）工作记忆**唯一原始日志是 `Conversation`**，legacy STM/LTM 的 `MemoryEntry` 存储已移除。

- 工作记忆 = `AppSession.conversation`（`crates/oneai-app/src/session.rs`），`AgentLoop` 每轮在其上追加/压缩。
- `MemoryManager` 的文档注释明确写道：`Working memory is single-sourced on the Conversation (M1); the legacy STM/LTM MemoryEntry stores have been removed.`（`manager.rs:5-7`）

### 2.2 Core 层（常驻、有预算、agent 自管理）

`CoreMemory`（`core_memory.rs:193`）——包了一个 `MemoryFactStore` + token 预算：

- **常驻注入**：每轮由 `CoreMemorySource`（`EveryIteration`）重注入，且**抗压缩**（见 §4.1）。
- **token 预算**：`budget_tokens`（默认 2048，由 `MemoryProfile.core_budget_tokens` 声明）。超预算时驱逐「最久未更新的非 pinned 事实」到 archival 层（`core_memory.rs:69 enforce_budget`），形成 **core ↔ archival 分页闭环**。
- **pinned**：agent 可 pin 关键事实，永不被预算驱逐（`core_memory.rs:46 pin`）。
- **自管理**：agent 通过 `core_memory_edit` 工具直接策展（增/改），见 §6。

### 2.3 Archival 层（全量事实、按需召回）

`MemoryFactStore`（`fact_store.rs:35`）——存储全量原子 `MemoryFact`，按需三因子召回。这是长期记忆的**规范容器**：core 层和 archival 层都是它的实例（`manager.rs` 中 `core_memory` 与 `fact_archive` 各持一个）。

### 2.4 Recall 层（原始日志回溯）

Recall 不是独立存储，而是**持久化的会话快照**：被压缩丢弃的原始 `Message` 会经 `archive_discarded_snapshot` 以 `"{session}::discarded::{uuid}"` 为 id 落库（`manager.rs:381`），保留为可恢复、可审计、可按需 `memory_search` 回溯的真值。这是「压缩即不丢」的原始转录兜底（C2）。

---

## 3. 原子事实模型 `MemoryFact` 与 Mem0 式冲突更新

### 3.1 事实的结构

`MemoryFact`（`oneai-core/src/types.rs`，约 1233+）是长期记忆的单位，字段：

| 字段 | 含义 |
|---|---|
| `id` | 事实唯一 id |
| `user_id` | **跨会话命名空间**（habits 作用域） |
| `session_id` | **本会话命名空间**（episodic 作用域） |
| `fact_type: FactType` | 类别标签，受领域 `extraction_schema` 约束（coding: `user_tooling_pref`/`decision`/`open_task`/`critical_file`；research: `source`/`claim`/`open_question`/`user_interest`） |
| `subject` / `predicate` / `content` | 三元组：主体-谓词-值，如 `user.package_manager` / `prefers` / `pnpm` |
| `importance: f32` | 重要度 [0,1]，召回排序用 |
| `embedding: Option<Vec<f32>>` | 语义向量（**当前恒为 None，见 §8 缺口**） |
| `created_at` / `updated_at` / `version` | 时间戳与版本号 |

### 3.2 冲突更新（Mem0 invariant）

冲突键 = `(user_id, subject, predicate)`。`MemoryFactStore::upsert`（`fact_store.rs:67`）的逻辑：

- 命中已有同键事实 → **就地更新** `content`/`embedding`/`metadata`/`fact_type`/`updated_at`，`version + 1`，返回 `Updated { previous_version }`；
- 否则插入，`version` 归一为 1，返回 `Inserted`。

这意味着：agent 学到「用户从 npm 改用 pnpm」时，**旧事实被更新而非追加**，长期记忆不会随会话累积漂移成自相矛盾。SQLite 后端用 `ON CONFLICT(user_id, subject, predicate) DO UPDATE ... version = memories.version + 1` 镜像同一不变量（`sqlite_store.rs:713`，配合 `CREATE UNIQUE INDEX idx_memories_key ON memories(user_id, subject, predicate)`，`sqlite_store.rs:124`）——**运行时与持久层冲突语义一致**。

> 与 Mem0 的细微差别见 §9.1：Mem0 用 LLM 对每条事实判 `ADD/UPDATE/DELETE/NONE`；OneAI 用确定性结构键做 `update-vs-insert`，不区分「相关但已变（merge）」与「矛盾（delete）」，也不做 DELETE。

### 3.3 双命名空间

- `user_id`（跨会话 habits）+ `session_id`（本会话 episodic）。`load_persisted_facts`（`manager.rs:316`）在 resume 时先按空 session_id 拉全量用户习惯，再拉本会话 episodic，都 upsert 进 archival tier。
- 持久化在统一 `memories` 表，CLI `oneai memory search <kw> --user <id>` / `list --user <id>` 命名空间化跨会话记忆。

---

## 4. 压缩耦合抽取：堵死「压缩即丢失」

这是 OneAI 记忆系统**最具特色**的设计，也是与朴素 RAG「召回即一切」的根本区别。

### 4.1 抗压缩注入（CoreMemorySource）

`CoreMemorySource`（`core_memory_source.rs`）实现 `ContextSource`，两个关键属性：

- `refresh_policy() = EveryIteration`（`core_memory_source.rs:89`）——**每轮重注入**。`ContextCompressor` 压缩时会丢弃旧轮次（只留 `keep_recent_turns`），但下一轮 `assemble()` 会重新注入 core 块，所以 **core 记忆不会被摘要掉**。对比旧设计：一个一次性的「Previous conversation context」system 消息埋在历史里，会被摘要抹去。
- `priority() = 10`（`core_memory_source.rs:94`）——高优先级，先于领域 env 源注入。

它产出两段：`[Core Memory]`（常驻策展事实）+ `[Recalled Context]`（每轮召回，由 `set_recall` 写入，`core_memory_source.rs:48`）。

### 4.2 压缩 → 抽取 → 归档闭环

`ContextCompressor`（`compression.rs:26`）在 `compress` 时（`compression.rs:141`）：

1. 保留最近 N 轮（`keep_recent_turns`，默认 6）；
2. **钉住首条 user 消息原文**（Q2/Q3 硬保证，`compression.rs:159`）——原始 Goal 不会被摘要掉，压缩后放在 summary 与 recent tail 之间；
3. 对每条将被摘要掉的旧消息做**无损截断**（`MAX_OLDER_MSG_CHARS=2000`，超长 tool_result 截头 + 指向 `memory_search`，`compression.rs:187`）；
4. LLM 按领域 `CompressionTemplate` 摘要旧段；
5. **关键**：在被丢弃的 `discarded_messages` 上跑 `FactExtractor.extract`（`compression.rs:306 extract_and_archive`），按领域 `extraction_schema` 抽取原子事实，conflict-resolve 进 archival tier——**压缩掉的信息不丢失，变成可搜索的长期记忆**；
6. 同时 `discarded_messages` 经 `ArchivalDiscardedSink`（`manager.rs:528`）落库为原始转录快照（C2 兜底）。

整个过程**fail-safe**：抽取失败只 `tracing::warn!`，绝不传播错误（`compression.rs:327`）——坏抽取不会打断压缩路径。

> 接线点：`session.rs:701` 与 `session.rs:775`，用 `domain.memory_profile.extraction_schema` 作为抽取 schema，`memory_manager.fact_archive()` 作为 archive sink。即使没有 domain pack，也用默认 schema（`user_tooling_pref`/`decision`/`open_task`）接上抽取（`session.rs:764`），不再像旧版用 `NoopCompressor` 静默丢弃。

### 4.3 FactExtractor 的契约

`fact_extraction.rs:23`：LLM 被要求输出 JSON 数组 `[{fact_type, subject, predicate, content, importance?}]`。解析**容错**：strip ```json 围栏、取首个 `[...]` span；**fails-safe**：malformed 输出 → 0 条事实而非报错（`fact_extraction.rs:129`）。还会**过滤掉 schema 之外的事实类型**（`fact_extraction.rs:138`），防 LLM 漂移。每个类型有默认重要度：`decision`/`episodic` 0.85 > `critical_file` 0.75 > `open_task`/`user_tooling_pref` 0.65 > 其他 0.5（`fact_extraction.rs:171`）。

---

## 5. 召回机制：三因子混合评分

### 5.1 每轮召回路径

`AppSession.run` 在每轮推理前（`session.rs:626-636`）：

1. `set_session_id` + `load_persisted_facts`（把跨会话习惯 + 本会话 episodic 灌入 archival，幂等）；
2. 以**当前用户 task 文本**为 query，`recall_facts(task, top_k)`（`session.rs:629`），`top_k` 取自 `MemoryProfile.recall.top_k`（默认 5）；
3. `CoreMemorySource::set_recall(facts)`（`session.rs:636`）——召回结果进入抗压缩 core 块，而非一次性 system 消息。

### 5.2 三因子评分（Generative Agents 式）

`MemoryFactStore::search_hybrid`（`fact_store.rs:161`）对每条候选事实算：

```
score = 0.5 · relevance + 0.3 · recency + 0.2 · importance
```

- **relevance（相关度）**：query 与 fact 都有 embedding → 余弦相似度；否则 keyword 命中（content/subject/predicate 任一命中）给固定分 0.6。**relevance ≤ 0 的候选直接剔除**（`fact_store.rs:193`）——零相关的不会因近因/重要度混进来。
- **recency（近因）**：对 `updated_at` 做**指数衰减**，1 小时半衰期（`temporal_score_fact`，`fact_store.rs:212`，`0.5^(diff/3600)`）。可由 `RecallConfig.time_decay` 关闭。
- **importance（重要度）**：事实的 `importance` 字段。

> 注意：这三因子权重是**硬编码** `0.5/0.3/0.2`（`fact_store.rs:198`），且各因子**未做 min-max 归一化**（Generative Agents 原文要求归一化）。见 §8 缺口。

### 5.3 语义召回的退化与 query embedding

`recall_facts`（`manager.rs:347`）会在配了 `EmbeddingService` 时给 **query** 算 embedding（`svc.embed(query)`，`manager.rs:352`），再传给 `search_hybrid`。**但存储的事实 `embedding` 恒为 `None`**（`fact_extraction.rs:159`、`memory_tools.rs:35` 均写死 `embedding: None`），所以 `search_hybrid` 里 `f.embedding.as_ref()` 永远是 None → 退回 keyword 命中路径（0.6 分）。**语义召回当前实际退化为关键词召回**。详见 §8。

### 5.4 memory_search 兜底回溯

agent 的 `memory_search` 工具（`memory_tools.rs:66`）先走 archival 三因子搜索；若无结构化事实命中，**回退到本会话持久化原始转录快照**做 keyword 检索（`search_conversation_snapshot`，`memory_tools.rs:138`），每条摘录截 1000 字。这是「常态不召回原文，事实不够时按需回溯」的纠错/审计路径（R2）。

---

## 6. 自管理记忆工具（Letta 式「越用越好用」）

当 `MemoryProfile.enable_memory_tools` 为真，`AppBuilder` 注册三个工具（`builder.rs:1618-1629`），agent 自己策展记忆：

| 工具 | 作用 | risk | 文件 |
|---|---|---|---|
| `memory_search` | 从 archival 召回事实（三因子 + 原文兜底） | Low | `memory_tools.rs:66` |
| `core_memory_edit` | upsert 进常驻 core tier（冲突更新 + 预算驱逐 + 落 archival） | Medium | `memory_tools.rs:173` |
| `archival_memory_insert` | 显式归档一条事实（不必每轮都看） | Medium | `memory_tools.rs:241` |

`core_memory_edit` 的工具描述里有一段关键设计理念——**约束沉淀（constraint sedimentation）**（`memory_tools.rs:188`）：持久约束（用哪个包管理器、哪些模块绝不碰、token/步预算、编码规范）应写进 core，每轮保持显著，**不依赖从长历史召回**（长上下文会稀释对早期约束的注意力）。这正是业界「memory as editable context block」思路的体现。

工具名命空间都按 `MemoryManager` 当前 `user_id`/`session_id`（`memory_tools.rs:26 build_fact`），所以 habits 跨会话、episodic 留会话内。

---

## 7. 反思闭环：STM ↔ LTM 的 episodic 提炼

`MemoryReflection`（`reflection.rs:197`）在**会话末**触发（`AppSession.run` 末尾，`session.rs:839-858`，当 `auto_reflect` 为真）：

1. 取整个 `Conversation`（工作记忆单源）的 entry 视图；
2. LLM 反思，输出结构化 `REFLECTION / INSIGHTS / DECISIONS / OUTCOME`（`reflection.rs:266`），解析容错（无结构化字段则整段当 reflection，`reflection.rs:327`）；
3. 生成 `EpisodicMemory` → `to_fact()`（`reflection.rs:148`）落为 archival 事实，`fact_type="episodic"`，`subject="session.{id}"`，`predicate="reflection"`，`importance=0.8`（高显著，优先召回），并持久化。

这对应学术综述（Zhang et al. 2024）里的 **Memory Management (P)**：summarize → reflect。它是「提炼型 episodic 中间层」（M5）：比原子 fact 丰满，比原始转录紧凑。

> 触发条件：仅当配了 reflection 引擎（`with_compressor_and_reflection` 等，`manager.rs:131`）。否则返回 `Ok(None)`，不反思。

---

## 8. 持久化与会话恢复

`SqliteSessionStore`（`sqlite_store.rs`）实现 `MemoryPersistence`（`traits.rs:479`）：

- **`memories` 表**（`sqlite_store.rs:109`）：`id/user_id/session_id/fact_type/subject/predicate/content/embedding_json/metadata_json/created_at/updated_at/version/importance`，唯一索引 `(user_id, subject, predicate)`，索引 `user_id`、`session_id`。
- `store_fact`（`sqlite_store.rs:698`）：`INSERT ... ON CONFLICT DO UPDATE ... version+1`，镜像运行时冲突语义。
- `load_facts`（`sqlite_store.rs:730`）：`session_id=''` → 全量用户习惯；否则按会话范围。
- `save_conversation`/`load_conversation`（`sqlite_store.rs:539/582`）：原始对话快照（含 metadata，如 `task_anchor`）。
- 会话恢复：`AppSession.run` 每轮 `set_session_id` + `load_persisted_facts`（`session.rs:613/616`）；每轮结束 `save_session`（`session.rs:839`）。CLI：`oneai session list / resume <id> / delete / info`。

`AppBuilder::sqlite_persistence()` / `sqlite_persistence_at(path)`（`builder.rs:1319/1351`）一行开启；`embedding_service()`（`builder.rs:736`）接语义召回。`MemoryManager` 用 builder 方法按需组合：`new` / `with_embedding` / `with_compressor_and_reflection` / `with_persistence` / `with_all_features`（`manager.rs:99-247`）。

---

## 9. DomainPack 第 7 层 MemoryProfile：声明式记忆策略

`MemoryProfile`（`memory_profile.rs:36`）把记忆行为做成可声明、可合并、可校验的领域层，与 `CompressionTemplate`/`ContextSource`/`PermissionProfile` 同级：

| 字段 | 回答 | 驱动 |
|---|---|---|
| `extraction_schema: Vec<FactType>` | **记什么** | FactExtractor prompt schema |
| `recall: RecallConfig` | **怎么召回**（strategy/top_k/time_decay） | CoreMemorySource 每轮注入 |
| `core_budget_tokens` | **常驻多少** | CoreMemory 预算 |
| `enable_memory_tools` | **谁管** | AppBuilder 注册自管理工具 |
| `habit_fact_types` | **跨会话持久什么** | user 命名空间 habits |

`RecallConfig`（`types.rs:1282`）：`strategy ∈ {KeywordFirst, SemanticFirst, Hybrid}`、`top_k`、`time_decay`。内置 preset：`MemoryProfile::coding()`（`memory_profile.rs:110`，schema 4 类，Hybrid，top_k 5，core 2048，开工具，habit=`user_tooling_pref`）、`MemoryProfile::research()`（`memory_profile.rs:129`，schema 4 类，core 1536）。

**合并规则**（`memory_profile.rs:164 merge`，支持多领域 agent）：schema/habits 取并集去重；recall 取 primary；core_budget 取**最小**（最严）；enable_memory_tools 取 OR。

`AppBuilder` 接线（`builder.rs:1617-1630`）：读 `domain.memory_profile.enable_memory_tools` 决定是否注册工具；`session.rs:702` 读 `extraction_schema` 喂给 FactExtractor。**整个记忆行为一行 `AppBuilder::domain_pack(coding_pack(...))` 切换**——这是相对 Mem0/Letta「记忆策略写死在框架里」的核心优势。

---

## 10. 在长程任务中如何发挥作用

长程任务（multi-step、跨多轮、可能跨会话恢复）的记忆痛点是：上下文超长→压缩→信息丢失→约束遗忘→目标漂移。OneAI 的闭环逐点对应：

| 长程痛点 | OneAI 机制 | 位置 |
|---|---|---|
| 上下文超限 | token-budget 触发压缩（非固定 max_iter），留最近 6 轮 | `compression.rs:141`, `agent_loop.rs:680` |
| 压缩丢信息 | 压缩耦合 FactExtractor 抽取落档 + 原始转录快照兜底 | `compression.rs:306`, `manager.rs:381` |
| 原始 Goal 被摘要掉 | 首条 user 消息原文钉住（Q2/Q3 硬保证） | `compression.rs:159` |
| 早期约束被长上下文稀释 | core 层常驻 + `EveryIteration` 重注入 + 约束沉淀工具 | `core_memory_source.rs:89`, `memory_tools.rs:188` |
| 需要的历史事实召不回 | 三因子召回每轮注入 `[Recalled Context]`，top_k 由领域定 | `session.rs:629-636` |
| 事实随会话累积自相矛盾 | Mem0 式 `(user,subject,predicate)` 冲突更新，version+1 | `fact_store.rs:67` |
| 跨会话丢失偏好/习惯 | user 命名空间 + SQLite + resume 时 `load_persisted_facts` | `manager.rs:316` |
| 会话中断无法续 | `save_session` 每轮落库 + `oneai session resume` | `session.rs:839` |
| 任务结束时洞察未沉淀 | 会话末 reflection → episodic 事实（importance 0.8） | `session.rs:839-858` |
| 长输出（shell/file）淹没上下文 | 单条 older msg 截断 2000 字 + 指向 memory_search | `compression.rs:187` |

**一次典型的长程任务运行轨迹：**

1. `AppSession.run` → 设 session_id、`load_persisted_facts`（带上历史 habits）→ `recall_facts(task)` 注入相关历史决策 → CoreMemorySource 每轮重注入 core（含约束）。
2. AgentLoop 多轮推理/工具/委托；上下文超 threshold → 压缩：钉 Goal + 摘要旧段 + **丢弃段抽取成事实落档** + 原始转录落库；下一轮 core/recall 自动重注入（抗压缩）。
3. agent 发现关键约束 → 调 `core_memory_edit` 沉淀进常驻层（每轮显著）。
4. 事实冲突（如偏好变更）→ upsert 更新而非追加，version+1。
5. 会话末 → `reflection` 生成 episodic 事实（importance 0.8）落档 → `save_session` 落库。
6. 下次同 user 新会话 → `load_persisted_facts` 带回 habits + episodic → 「越用越好用」。

---

## 11. 与业界前沿对标

下表把 OneAI 的机制映射到 7 个主流系统 + 1 个学术综述。每行的「OneAI 现状」是对代码事实的判定，「缺口」指向 §12。

### 11.1 总览对标表

| 设计轴 | OneAI 现状 | 业界参照 | 评价 |
|---|---|---|---|
| **三层 tiering** | core / archival / recall（原始转录） | Letta core/archival/recall | ✅ 基本对齐；recall 层用快照而非独立消息存储 |
| **冲突更新** | 确定性结构键 `(user,subject,predicate)` update-vs-insert，version+1 | Mem0 LLM 判 ADD/UPDATE/DELETE/NONE；Zep 双时态边失效 | 🟡 结构化、确定、零幻觉；但缺 DELETE 与「相关但已变 vs 矛盾」区分 |
| **三因子召回** | `0.5·rel+0.3·rec+0.2·imp`，1h 半衰期，硬编码权重，**未归一化** | Generative Agents：`α=1` 三因子，min-max 归一化，0.995 衰减 | 🟡 思路一致；权重不可配、未归一化、half-life 硬编码 |
| **语义召回** | query 有 embedding，但**存储事实 embedding 恒 None** → 退化为 keyword | Mem0 向量+BM25+entity+temporal 融合；A-MEM 余弦 | 🔴 实际未生效，见 §12.1 |
| **反思/consolidation** | 会话末 LLM 反思 → episodic 事实（importance 0.8） | Generative Agents importance-sum 阈值 → 反思树；Cognee `improve` 后台 STM→LTM | 🟡 有反思但只在会话末一次性、不递归、不按阈值触发 |
| **自管理工具** | `memory_search` / `core_memory_edit` / `archival_memory_insert` | Letta `core_memory_append/replace` + `archival_memory_insert/search` | ✅ 几乎一一对应；约束沉淀理念领先 |
| **重要度评分** | 每类型默认标量（decision 0.85…）+ agent 可覆盖 | Generative Agents LLM 1–10 poignancy | ✅ 有显式标量，可被 agent 覆盖 |
| **时态/图结构** | 无图、无双时态；只有 `created/updated_at` | Zep 双时态 T/T' + 4 时间戳边失效；Mem0 native 实体共现图 | 🔴 缺失，见 §12.2 |
| **命名空间/多租户** | user_id + session_id 双命名空间 | Mem0 user/agent/run_id；Cognee dataset+session | ✅ 对齐 |
| **provenance 可追溯** | 原始转录快照可回溯（memory_search 兜底） | Zep episode→derived fact；Cognee relational provenance | 🟡 有快照兜底，但事实与来源未显式链接 |
| **声明式策略** | DomainPack 第 7 层 MemoryProfile，可合并可校验 | 各系统多为框架内写死 | ✅ **领先**：领域级一行切换记忆策略 |

### 11.2 各系统机制速记（便于深读对标）

- **Mem0**：外部记忆层。`add()` LLM 抽事实（SQL+向量+图三存储），`search()` 融合 semantic+BM25+entity-boost+temporal。冲突靠 LLM 对每 fact-id 判 `ADD/UPDATE/DELETE/NONE`（`mem0/configs/prompts.py`）。native 图是共现实体链接、schema-free、不打类型边。tiering：conversation/session/user/org。
- **Letta/MemGPT**：context window 当 RAM，OS 式分页。core（结构化 persona/human 块）/archival（向量库）/recall（消息历史）。**self-editing memory via tool calls** 是其原创。OneAI 的三个自管理工具直接源自这里。
- **Generative Agents**（Park et al. UIST'23）：memory stream + 三因子召回。recency 指数衰减 0.995/game-hour、importance LLM 1–10 poignancy、relevance 余弦，**min-max 归一化、α 全 1**。reflection：importance-sum 超 150 阈值 → 生成问题 → 检索 → 抽「带证据指针」的 insight → **递归反思树**。
- **Zep/Graphiti**：时态知识图。三子图（episode 原始/semantic entity/community）。**双时态**：T（事件序）+ T'（入库序），每条 fact 边 4 时间戳（`t_valid`/`t_invalid`/`t'_created`/`t'_expired`）。新事实来 → LLM 比对语义相关边 → 时态矛盾则旧边 `t_invalid` 设为新边 `t_valid`（**失效不删除，全史可回溯**）。
- **A-MEM**：Zettelkasten 式原子 note（content+keywords+tags+context+embedding+links）。插入新 note 时**反演演化旧 note**（LLM 重写最近邻的 context/keywords），存储本身 agentic。
- **Cognee**：三存储（relational/vector/graph），permanent vs session 两种模式，`improve()` 后台把 session 桥接进 permanent graph（显式 STM→LTM consolidation op）。
- **学术综述**（Zhang et al. 2024, arXiv:2404.13501）：Memory Writing (W) / Management (P) / Reading (R)；R = similarity + time-interval + importance。OneAI 的「抽取(W)→反思(P)→三因子召回(R)」正是这个形式模型。

---

## 12. 缺口与改进方向

> **状态（1.1.0）：本节 4 个缺口已全部修复并接入评测套件（见 §14）。** 下方原文保留作为问题陈述与修复方向记录，每条开头标注修复落地位置。

审阅中发现 4 个真实缺口（按影响排序）。

### 12.1 【高】语义召回形同虚设——事实从未被嵌入 ✅ 已修复（1.1.0）

**事实**：`FactExtractor::extract`（`fact_extraction.rs:159`）与 `memory_tools.rs::build_fact`（`memory_tools.rs:35`）都写死 `embedding: None`。`recall_facts` 只 embed 了 query（`manager.rs:352`），`search_hybrid` 里 `f.embedding.as_ref()` 恒为 None → relevance 走 keyword 命中（0.6 固定分）。**配了 EmbeddingService 也只是 keyword 召回**。

**影响**：`RecallStrategy::SemanticFirst`/`Hybrid` 的语义分支实际不工作；同义不同字的事实召不回（如查「包管理器」召不到 subject=`user.package_manager` 的事实，除非字面命中）。

**修复方向**：在 `archive_facts`（`manager.rs:300`）或 `upsert` 时，若配了 EmbeddingService 则对 `content`（或 `subject+predicate+content`）算 embedding 写入 `fact.embedding`；SQLite 同步存 `embedding_json`（列已存在，`sqlite_store.rs:700`）。`oneai-rag` 的 `AutoEmbeddingDocumentIndex`（`embedding.rs:813`）已是 RAG 文档的成熟自动嵌入范式，可对 MemoryFact 复用同一思路。

> **✅ 修复落地（1.1.0）**：`MemoryManager::embed_fact`（`manager.rs`）在 `archive_facts`、`reflect`、`memory_tools::build_fact` 三条写入路径统一嵌入 `"{subject} {predicate} {content}"`，fail-safe（嵌入失败仅 warn 不阻断）。SQLite `embedding_json` 列据此真正被填，resume 时 `load_persisted_facts` 带回嵌入向量。`oneai-eval` 评测锚点 `ie_synonym_cross_lang`（中文事实+英文查询）实测：keyword 召回 recall@5=0 → 语义召回 recall@5=1（见 §14）。

### 12.2 【中】冲突更新缺 DELETE 与语义区分 ✅ 已修复（1.1.0）

Mem0 用 LLM 区分「相关但已变（merge）」「矛盾（delete）」「重复（none）」。OneAI 只做 update-vs-insert：相同键一律覆盖。后果——agent 先说「用 JWT」后说「放弃 JWT 改用 session」时，旧决策被覆盖而非保留历史，无法回溯决策演变。

**修复方向**：引入可选的 LLM 冲突判定（可借鉴 Mem0 的 4-event prompt），或像 Zep 那样做**软失效**——旧值不删，标 `superseded`，召回时降权，保留可追溯。当前 `version` 字段已为演变留了位，差一个 `superseded` 标志或历史表。

> **✅ 修复落地（1.1.0）**：`MemoryFact` 加 `superseded`/`superseded_at` 字段；`MemoryFactStore::upsert` 冲突时把旧 revision 追加进 `metadata["_superseded_history"]`（决策演变可回溯，活行仍为新真值——Mem0 不变量不变）；新增 `invalidate`/`MemoryManager::invalidate_fact` 软删除路径（标 superseded，召回默认过滤，`search_hybrid_with_config(include_superseded=true)` 可审计回溯）。SQLite 加 `superseded`/`superseded_at` 列 + 迁移。评测锚点 `ku_auth_switch` 实测：旧值 JWT 被软失效后召回返回新值 session。

### 12.3 【中】反思非递归、只在会话末、非阈值触发 ✅ 已修复（1.1.0）

OneAI 的 `MemoryReflection` 在会话末一次性反思（`session.rs:839`）。Generative Agents 的反思是**按 importance-sum 阈值周期触发、递归生成反思树**，能在长会话中途沉淀中间抽象。Cognee 的 `improve` 是后台 consolidation op。

**修复方向**：在 AgentLoop 每 N 轮检查累积 importance，超阈值即触发一次反思；让反思可检索并引用已有 episodic 事实（递归）。这与 §12.1 的语义召回互补——反思生成的高层 insight 也应被嵌入，才能在后续召回。

> **✅ 修复落地（1.1.0）**：`MemoryReflectionConfig` 加 `reflectance_threshold`（默认 150，对齐 Generative Agents importance-sum 阈值）+ `trigger_interval_turns`（默认 10）；`MemoryReflection::should_reflect` 阈值+轮间隔双门控；`reflect_with_prior` 把 archival 中 top-3 既有 episodic 事实摘要喂进 prompt（递归反思雏形）；`MemoryManager::reflect_if_threshold` 暴露中途触发入口；`AppSession::run_agent` 在每轮收尾累计 importance 增量 + 迭代数，超阈值即中途反思并重置计数（不侵入 AgentLoop 内循环，保 1.0 边界）。

### 12.4 【低】三因子权重/归一化/衰减硬编码 ✅ 已修复（1.1.0）

`fact_store.rs:198` 的 `0.5/0.3/0.2`、未做 min-max 归一化、`temporal_score_fact` 的 1h 半衰期（`fact_store.rs:220`）均硬编码，不随 `RecallConfig` 调。Generative Agents 要求三因子归一化后加权，否则不同量纲（余弦 ∈[-1,1]、importance ∈[0,1]、recency ∈(0,1]）的加权和不可比。

**修复方向**：把权重与半衰期纳入 `RecallConfig`；召回前对候选集做 min-max 归一化再加权。

> **✅ 修复落地（1.1.0）**：`RecallConfig` 加 `relevance_weight`/`recency_weight`/`importance_weight`（默认 0.5/0.3/0.2）、`recency_half_life_secs`（默认 3600）、`normalize_factors`（默认 true）+ builder；`search_hybrid_with_config` 改为两遍：算原始三因子 → min-max 归一化（单候选/常量集退化为 1.0 不被抹零）→ 加权排序；`temporal_score_fact` 半衰期参数化；`recall_facts_with_config` 由 `AppSession` 每轮注入 domain `MemoryProfile.recall`；`MemorySearchTool` 注入 `Arc<RecallConfig>` 走同一 config。

---

## 13. 小结：OneAI 记忆系统的定位

OneAI 的记忆系统**在工程闭环上已达到一线开源框架水平**，甚至在两个维度上领先：

- **领先**：① 压缩耦合抽取（压缩即抽取落档）这一闭环在 Mem0/Letta 中并不原生；② 声明式 DomainPack `MemoryProfile` 让记忆策略可一行切换、可合并、可校验，比各系统「策略写死框架」更灵活；③ 抗压缩注入（`EveryIteration` + core 块 + 钉 Goal + 约束沉淀）对长程任务的「目标/约束漂移」有针对性防护。
- **持平**：三层 tiering、Mem0 式冲突更新、Generative-Agents 式三因子召回、Letta 式自管理工具、双命名空间 + 持久化 + 会话恢复。
- **落后**：语义召回因事实未嵌入而**实际未生效**（§12.1，最该先修）；无时态/图结构（§12.2）；反思非递归非阈值（§12.3）；召回权重/归一化硬编码（§12.4）。

一句话：**OneAI 把「记忆作为可声明、抗压缩、压缩无损的闭环」做对了，但「语义向量召回」这条腿还没落地**——补齐事实自动嵌入（§12.1）后，三因子召回与 `RecallStrategy` 的语义分支才能真正兑现设计意图。

---

### 附：关键文件索引

| 关注点 | 文件 |
|---|---|
| 统一入口 | `crates/oneai-memory/src/manager.rs:655` |
| 事实容器+冲突更新+三因子 | `crates/oneai-memory/src/fact_store.rs:361` |
| core 层（预算/pin） | `crates/oneai-memory/src/core_memory.rs:193` |
| 压缩+抽取闭环 | `crates/oneai-memory/src/compression.rs:492` |
| 事实抽取器 | `crates/oneai-memory/src/fact_extraction.rs:291` |
| 反思/episodic | `crates/oneai-memory/src/reflection.rs:562` |
| 抗压缩注入源 | `crates/oneai-memory/src/core_memory_source.rs:160` |
| 自管理工具 | `crates/oneai-memory/src/memory_tools.rs:354` |
| 声明式策略 | `crates/oneai-domain/src/memory_profile.rs:246` |
| 召回注入接线 | `crates/oneai-app/src/session.rs:626-636` |
| 压缩器接线 | `crates/oneai-app/src/session.rs:694-723` |
| 反思触发 | `crates/oneai-app/src/session.rs:839-858` |
| 工具注册 | `crates/oneai-app/src/builder.rs:1617-1630` |
| 持久化 memories 表 | `crates/oneai-persistence/src/sqlite_store.rs:109/698/730` |
| 共享 trait | `crates/oneai-core/src/traits.rs:479`(MemoryPersistence) `557`(DiscardedSink) `641`(EmbeddingService) |
| 共享类型 | `crates/oneai-core/src/types.rs:1200`(RecallStrategy) `1282`(RecallConfig) |

---

## 14. 记忆评测方案（1.1.0 新增）

`oneai-eval::memory` 是一套对齐业界权威基准的记忆子系统评测体系，形成「优化→评测→量化收益」闭环。方法论三来源：

- **LongMemEval**（arXiv:2410.10813，~427 引用）—— 5 长期记忆能力：Information Extraction (IE) / Multi-Session Reasoning (MR) / Temporal Reasoning (TR) / Knowledge Update (KU) / Abstention (ABS)。人工标注 evidence → 可算 Recall@k / NDCG@k，无需 LLM judge。
- **Mem0**（arXiv:2504.19413）—— F1 + BLEU-1 + LLM-as-Judge 三联评分（业界下游论文通用打分）。
- **MemBench**（arXiv:2506.21605）—— 直接评记忆本体（recall accuracy / capacity / temporal efficiency）。

### 14.1 评测锚点（对优化收益的量化）

| 锚点用例 | 验证缺口 | keyword 基线 | 语义召回 |
|---|---|---|---|
| `ie_synonym_cross_lang`（中文事实+英文查询） | §12.1 | recall@5=0.00 | recall@5=1.00 ✅ |
| `ie_pkg_manager`/`ie_test_runner`（自然语言问题→事实） | §12.1 | recall@5=0.00 | recall@5=1.00, F1=1.00 ✅ |
| `ku_auth_switch`（JWT→session 知识更新） | §12.2 | 召回旧值 | 召回新值 session, F1=1.00 ✅ |
| `abs_never_mentioned`/`abs_unrelated` | 不幻觉 | abstention=1.00 ✅ | abstention=1.00 ✅ |

跑法：`oneai eval memory --suite builtin` vs `--no-embedding` 对比 §12.1 增益。

### 14.2 架构

- **不依赖完整 AgentLoop**——evaluator 直驱 `MemoryManager`（replay 多会话 planted facts → `recall_facts_with_config` → 合成确定性 answer → 打分），消除 evaluator 自身不确定性污染记忆子系统分。
- **指标**：`recall_at_k`/`ndcg_at_k`（纯 Rust，比对 evidence_keys，CI 可跑）、`f1`/`bleu1`（LoCoMo/Mem0 口径，CJK 按字符切分）、`abstention`、可选 `llm_judge`（按 ability rubric）。
- **数据集**：内置合成套件（`builtin_suite()`，10 用例覆盖 5 能力 + 同义反例）+ `load_suite_jsonl` 加载器（兼容 LoCoMo/LongMemEval JSONL schema，附 `scripts/download_memory_bench.sh`）。
- **离线可跑**：`DeterministicEmbeddingService`（字节直方图向量）作为语义路径的离线占位，CI 无 API key 亦可演示 §12.1 收益；真实质量度量应替换为 OpenAI/Ollama embedding。

### 14.3 文件索引

| 关注点 | 文件 |
|---|---|
| 模块入口 | `crates/oneai-eval/src/memory.rs` |
| 用例/能力/会话类型 | `crates/oneai-eval/src/memory/case.rs` |
| 纯 Rust 指标 | `crates/oneai-eval/src/memory/metrics.rs` |
| 内置套件+JSONL 加载 | `crates/oneai-eval/src/memory/suite.rs` |
| Runner + 确定性 embedding | `crates/oneai-eval/src/memory/runner.rs` |
| CLI 子命令 | `examples/cli/src/cmd_eval.rs::cmd_eval_memory` |
