# OneAI Memory Mechanism (Whitepaper)

> A declarative memory engine — Letta-style 3 tiers + Mem0-style conflict updates + Generative-Agents-style 3-factor recall + compression-coupled extraction: working memory is single-sourced on `Conversation`, long-term memory updates atomically per `(user_id, subject, predicate)`, and compressed-away turns are extracted into facts on file — closing the "compression-equals-loss" hole; behavior is declared by DomainPack layer 7 `MemoryProfile`.

> Version: corresponds to codebase `0.2.0` / 1.0.0 line. This document is written from a file-by-file review of the `crates/oneai-memory`, `oneai-rag`, `oneai-persistence`, `oneai-domain`, and `oneai-app` sources; every mechanism is annotated with `file:line` for verification. The end benchmarks against industry-frontier memory systems (Mem0 / Letta / Generative Agents / Zep-Graphiti / A-MEM / Cognee).

---

## 0. One-Sentence Summary

OneAI's memory system is a **"Letta-style three-tier + Mem0-style conflict update + Generative-Agents-style three-factor recall + compression-coupled extraction"** declarative memory engine: working memory is single-sourced on `Conversation`, long-term memory uses the atomic `MemoryFact` as its unit and conflict-updates by `(user_id, subject, predicate)`, each turn recalls and injects via "relevance + recency + importance", and turns discarded by compression are extracted by the LLM into facts and archived — "compression means loss" is closed off. The entire memory behavior is declared by DomainPack layer 7 `MemoryProfile`, switchable in one line via `AppBuilder::domain_pack(...)`.

---

## 1. Architecture Overview: Layering and Data Flow

```
                       ┌─────────────────────────────────────────────┐
                       │            AgentLoop (oneai-agent)           │
                       │  each iter: infer -> parse -> tool/delegate/paradigm  │
                       └───────────────┬─────────────────────────────┘
                                       │ re-assemble context each turn
                                       ▼
            ┌──────────────────────────────────────────────────────────┐
            │  ContextAssembler  (oneai-agent/context_assembler.rs)  │
            │  injection source: ContextSource trait, by epoch/refresh_policy  │
            └───────────────┬──────────────────────────────┬─────────┘
                            │                                │
            ┌───────────────▼──────────┐      ┌──────────────▼──────────┐
            │  CoreMemorySource         │      │  domain ContextSource  │
            │  (EveryIteration, compression-resistant)  │      │  (env diff / skill…)   │
            │  · [Core Memory] resident block │      └─────────────────────────┘
            │  · [Recalled Context]    │
            └───────────────┬──────────┘
                            │ set_recall(facts) written by AppSession each turn
                            │
       ┌────────────────────▼──────────────────────────────────────┐
       │                  MemoryManager (unified entry)           │
       │   oneai-memory/src/manager.rs                            │
       ├──────────────────────────────────────────────────────────┤
       │  core_memory  (CoreMemory, Letta core, token budget)     │
       │  fact_archive (MemoryFactStore, archival, full facts)    │
       │  reflection?  (MemoryReflection, end-of-session episodic extraction) │
       │  persistence? (MemoryPersistence -> SQLite)              │
       │  embedding?   (EmbeddingService -> semantic recall)      │
       └──────────────────────────────────────────────────────────┘
                            │                │                  │
              ┌─────────────▼─┐    ┌─────────▼────────┐  ┌──────▼─────────┐
              │ MemoryFact    │    │ ContextCompressor│  │ SqliteSession  │
              │ (oneai-core)  │    │ + FactExtractor  │  │ Store          │
              │ atomic fact   │    │ compression-coupled extraction │  │ memories table │
              └───────────────┘    └──────────────────┘  └────────────────┘
```

**Responsibility split across three crates:**

| crate | role | key files |
|---|---|---|
| `oneai-memory` | The memory engine itself: three tiers, extraction, recall, reflection, self-managed tools | `manager.rs:655`, `fact_store.rs`, `core_memory.rs`, `compression.rs`, `fact_extraction.rs`, `reflection.rs`, `core_memory_source.rs`, `memory_tools.rs` |
| `oneai-rag` | Embedding services (OpenAI/Anthropic/Ollama/FastEmbed) + auto-embedding document index | `embedding.rs:1258` |
| `oneai-persistence` | SQLite persistence: sessions/LTM/facts/usage + progressive checkpoints | `sqlite_store.rs:1302` |
| `oneai-domain` | `MemoryProfile` (DomainPack layer 7) declarative memory policy | `memory_profile.rs:246` |
| `oneai-core` | Shared types: `MemoryFact`/`FactType`/`RecallConfig`/`MemoryScope` + `MemoryPersistence`/`EmbeddingService`/`DiscardedSink` traits | `types.rs:1200`, `traits.rs:479` |

---

## 2. The Three-Tier Memory Structure (Letta-style)

OneAI explicitly divides memory into three tiers — a direct mapping of the Letta/MemGPT "core / archival / recall" three-tier model, but with one key correction: **single-sourcing of working memory (M1)**.

### 2.1 Working Memory — single-sourced on `Conversation`

> Historical lesson: in early implementations STM/LTM were two parallel `MemoryEntry` stores, which caused "compressed but STM not synced" drift. After the rework (M1), working memory's **only original log is `Conversation`**; the legacy STM/LTM `MemoryEntry` stores have been removed.

- Working memory = `AppSession.conversation` (`crates/oneai-app/src/session.rs`); `AgentLoop` appends to / compresses it each turn.
- The `MemoryManager` doc comment states explicitly: `Working memory is single-sourced on the Conversation (M1); the legacy STM/LTM MemoryEntry stores have been removed.` (`manager.rs:5-7`)

### 2.2 Core Tier (resident, budgeted, agent self-managed)

`CoreMemory` (`core_memory.rs:193`) — wraps a `MemoryFactStore` + token budget:

- **Resident injection**: re-injected every turn by `CoreMemorySource` (`EveryIteration`) and **compression-resistant** (see §4.1).
- **Token budget**: `budget_tokens` (default 2048, declared via `MemoryProfile.core_budget_tokens`). When over budget it evicts the "least-recently-updated non-pinned fact" to the archival tier (`core_memory.rs:69 enforce_budget`), forming a **core ↔ archival paging closed loop**.
- **pinned**: the agent can pin key facts so they are never evicted by the budget (`core_memory.rs:46 pin`).
- **Self-managed**: the agent curates (add/modify) directly via the `core_memory_edit` tool — see §6.

### 2.3 Archival Tier (full facts, on-demand recall)

`MemoryFactStore` (`fact_store.rs:35`) — stores the full set of atomic `MemoryFact`s, recalled on demand via three factors. This is the **canonical container** for long-term memory: both the core tier and archival tier are instances of it (`manager.rs` holds one each as `core_memory` and `fact_archive`).

### 2.4 Recall Tier (original-log retrieval)

Recall is not a separate store but a **persisted conversation snapshot**: original `Message`s discarded by compression are landed in storage via `archive_discarded_snapshot` with id `"{session}::discarded::{uuid}"` (`manager.rs:381`), preserved as recoverable, auditable, on-demand `memory_search`-retrievable ground truth. This is the "compression is not loss" original-transcript backstop (C2).

---

## 3. The Atomic Fact Model `MemoryFact` and Mem0-style Conflict Update

### 3.1 Structure of a Fact

`MemoryFact` (`oneai-core/src/types.rs`, ~1233+) is the unit of long-term memory. Fields:

| field | meaning |
|---|---|
| `id` | unique fact id |
| `user_id` | **cross-session namespace** (habits scope) |
| `session_id` | **this-session namespace** (episodic scope) |
| `fact_type: FactType` | category label, constrained by the domain `extraction_schema` (coding: `user_tooling_pref`/`decision`/`open_task`/`critical_file`; research: `source`/`claim`/`open_question`/`user_interest`) |
| `subject` / `predicate` / `content` | triple: subject-predicate-value, e.g. `user.package_manager` / `prefers` / `pnpm` |
| `importance: f32` | importance [0,1], used for recall ranking |
| `embedding: Option<Vec<f32>>` | semantic vector (unified embedding by `archive_facts` at archive time; fixed in 1.1.0, see §12.1) |
| `created_at` / `updated_at` / `version` | timestamps and version number |

### 3.2 Conflict Update (Mem0 invariant)

Conflict key = `(user_id, subject, predicate)`. The logic of `MemoryFactStore::upsert` (`fact_store.rs:67`):

- A same-key fact already exists → **update in place** `content`/`embedding`/`metadata`/`fact_type`/`updated_at`, `version + 1`, return `Updated { previous_version }`;
- otherwise insert, `version` normalized to 1, return `Inserted`.

This means: when the agent learns "the user switched from npm to pnpm", **the old fact is updated, not appended**, so long-term memory does not drift into self-contradiction as sessions accumulate. The SQLite backend mirrors the same invariant with `ON CONFLICT(user_id, subject, predicate) DO UPDATE ... version = memories.version + 1` (`sqlite_store.rs:713`, together with `CREATE UNIQUE INDEX idx_memories_key ON memories(user_id, subject, predicate)`, `sqlite_store.rs:124`) — **runtime and persistence-layer conflict semantics are consistent**.

> For fine-grained differences vs Mem0 see §9.1: Mem0 uses an LLM to judge `ADD/UPDATE/DELETE/NONE` for each fact; OneAI uses a deterministic structural key for `update-vs-insert`, does not distinguish "related but changed (merge)" from "contradictory (delete)", and does not do DELETE.

### 3.3 Dual Namespace

- `user_id` (cross-session habits) + `session_id` (this-session episodic). On resume, `load_persisted_facts` (`manager.rs:316`) first pulls all user habits by empty session_id, then pulls this-session episodic facts, upsert-ing both into the archival tier.
- Persistence is in the unified `memories` table; the CLI `oneai memory search <kw> --user <id>` / `list --user <id>` namespaces cross-session memory.

---

## 4. Compression-Coupled Extraction: Closing Off "Compression Means Loss"

This is OneAI's **most distinctive** memory design, and the fundamental difference from naive RAG's "recall is everything".

### 4.1 Compression-Resistant Injection (CoreMemorySource)

`CoreMemorySource` (`core_memory_source.rs`) implements `ContextSource` with two key properties:

- `refresh_policy() = EveryIteration` (`core_memory_source.rs:89`) — **re-injected every turn**. `ContextCompressor` discards older turns when compressing (keeping only `keep_recent_turns`), but the next `assemble()` re-injects the core block, so **core memory is never summarized away**. Compare with the old design: a one-shot "Previous conversation context" system message buried in history that would be wiped by summarization.
- `priority() = 10` (`core_memory_source.rs:94`) — high priority, injected before domain env sources.

It produces two blocks: `[Core Memory]` (resident curated facts) + `[Recalled Context]` (per-turn recall, written via `set_recall`, `core_memory_source.rs:48`).

### 4.2 Compression -> Extraction -> Archive Closed Loop

`ContextCompressor` (`compression.rs:26`) during `compress` (`compression.rs:141`):

1. Keeps the most recent N turns (`keep_recent_turns`, default 6);
2. **Pins the first user message verbatim** (Q2/Q3 hard guarantee, `compression.rs:159`) — the original Goal is not summarized away; after compression it is placed between the summary and the recent tail;
3. Performs **lossless truncation** on each older message about to be summarized (`MAX_OLDER_MSG_CHARS=2000`; over-long tool_result is head-truncated with a pointer to `memory_search`, `compression.rs:187`);
4. The LLM summarizes the old segment per the domain `CompressionTemplate`;
5. **Key step**: runs `FactExtractor.extract` over the discarded `discarded_messages` (`compression.rs:306 extract_and_archive`), extracting atomic facts per the domain `extraction_schema`, conflict-resolving them into the archival tier — **information dropped by compression is not lost; it becomes searchable long-term memory**;
6. Simultaneously the `discarded_messages` are landed as an original-transcript snapshot via `ArchivalDiscardedSink` (`manager.rs:528`) (the C2 backstop).

The whole process is **fail-safe**: extraction failure only `tracing::warn!`s and never propagates errors (`compression.rs:327`) — a bad extraction does not break the compression path.

> Wiring points: `session.rs:701` and `session.rs:775`, using `domain.memory_profile.extraction_schema` as the extraction schema and `memory_manager.fact_archive()` as the archive sink. Even without a domain pack, the default schema (`user_tooling_pref`/`decision`/`open_task`) is used to hook up extraction (`session.rs:764`) — no longer silently discarded via `NoopCompressor` as in the old version.

### 4.3 The FactExtractor Contract

`fact_extraction.rs:23`: the LLM is asked to output a JSON array `[{fact_type, subject, predicate, content, importance?}]`. Parsing is **fault-tolerant**: strips ```json fences, takes the first `[...]` span; **fails safe**: malformed output -> 0 facts rather than an error (`fact_extraction.rs:129`). It also **filters out fact types outside the schema** (`fact_extraction.rs:138`) to prevent LLM drift. Each type has a default importance: `decision`/`episodic` 0.85 > `critical_file` 0.75 > `open_task`/`user_tooling_pref` 0.65 > others 0.5 (`fact_extraction.rs:171`).

---

## 5. Recall Mechanism: Three-Factor Hybrid Scoring

### 5.1 Per-Turn Recall Path

`AppSession.run`, before each turn of inference (`session.rs:626-636`):

1. `set_session_id` + `load_persisted_facts` (load cross-session habits + this-session episodic into archival, idempotently);
2. using the **current user task text** as the query, `recall_facts(task, top_k)` (`session.rs:629`), where `top_k` comes from `MemoryProfile.recall.top_k` (default 5);
3. `CoreMemorySource::set_recall(facts)` (`session.rs:636`) — recall results go into the compression-resistant core block, not a one-shot system message.

### 5.2 Three-Factor Scoring (Generative-Agents-style)

`MemoryFactStore::search_hybrid` (`fact_store.rs:161`) computes for each candidate fact:

```
score = 0.5 · relevance + 0.3 · recency + 0.2 · importance
```

- **relevance**: both query and fact have embeddings -> cosine similarity; otherwise keyword hit (any of content/subject/predicate) gives a fixed score of 0.6. **Candidates with relevance <= 0 are dropped outright** (`fact_store.rs:193`) — zero-relevance facts do not sneak in via recency/importance.
- **recency**: **exponential decay** on `updated_at`, 1-hour half-life (`temporal_score_fact`, `fact_store.rs:212`, `0.5^(diff/3600)`). Can be disabled via `RecallConfig.time_decay`.
- **importance**: the fact's `importance` field.

> Note: the three-factor weights and recency half-life are now tunable via `RecallConfig`; the candidate set is min-max normalized before weighting (fixed in 1.1.0, see §12.4).

### 5.3 Semantic Recall and Query Embedding

`recall_facts` (`manager.rs:347`) embeds the **query** (`svc.embed(query)`, `manager.rs:352`) when an `EmbeddingService` is configured, then passes it to `search_hybrid` for dense + keyword hybrid retrieval. On the fact side: `FactExtractor::extract` (`fact_extraction.rs:159`) and `memory_tools::build_fact` (`memory_tools.rs:42`) produce facts with `embedding` set to `None`, but `MemoryManager::archive_facts` unifies embeddings on `"{subject} {predicate} {content}"` at archive time (`manager.rs:550`; embedding failure only warns and does not block), so stored facts carry vectors and the dense branch of `search_hybrid` takes effect.

> Before 1.1.0: archiving did not embed and facts' `embedding` was always `None`, so semantic recall degenerated to keyword recall (the query was embedded but had no fact vectors to compare against) — fixed; see §12.1.

### 5.4 memory_search Backstop Retrieval

The agent's `memory_search` tool (`memory_tools.rs:66`) first runs the archival three-factor search; if no structured facts match, it **falls back to this-session persisted original-transcript snapshots** for keyword retrieval (`search_conversation_snapshot`, `memory_tools.rs:138`), truncating each excerpt to 1000 chars. This is the "normally don't recall raw text; retrieve on demand when facts are insufficient" error-correction/audit path (R2).

---

## 6. Self-Managed Memory Tools (Letta-style "Gets Better With Use")

When `MemoryProfile.enable_memory_tools` is true, `AppBuilder` registers three tools (`builder.rs:1618-1629`) so the agent curates its own memory:

| tool | purpose | risk | file |
|---|---|---|---|
| `memory_search` | recall facts from archival (three-factor + raw-text backstop) | Low | `memory_tools.rs:66` |
| `core_memory_edit` | upsert into the resident core tier (conflict update + budget eviction + archival landing) | Medium | `memory_tools.rs:173` |
| `archival_memory_insert` | explicitly archive one fact (not necessarily visible every turn) | Medium | `memory_tools.rs:241` |

`core_memory_edit`'s tool description carries a key design rationale — **constraint sedimentation** (`memory_tools.rs:188`): persistent constraints (which package manager, which modules never to touch, token/step budgets, coding conventions) should be written into core, kept salient every turn, and **not depend on recall from long history** (long context dilutes attention to early constraints). This is exactly the industry's "memory as editable context block" idea.

The tools are namespaced by the `MemoryManager`'s current `user_id`/`session_id` (`memory_tools.rs:26 build_fact`), so habits are cross-session and episodic stays within the session.

---

## 7. Reflection Closed Loop: STM ↔ LTM Episodic Distillation

`MemoryReflection` (`reflection.rs:197`) triggers **at end of session** (at the end of `AppSession.run`, `session.rs:839-858`, when `auto_reflect` is true):

1. Takes the entry view of the entire `Conversation` (working-memory single source);
2. the LLM reflects, outputting structured `REFLECTION / INSIGHTS / DECISIONS / OUTCOME` (`reflection.rs:266`), with fault-tolerant parsing (no structured fields -> the whole segment is treated as the reflection, `reflection.rs:327`);
3. generates an `EpisodicMemory` -> `to_fact()` (`reflection.rs:148`) landed as an archival fact with `fact_type="episodic"`, `subject="session.{id}"`, `predicate="reflection"`, `importance=0.8` (high salience, prioritized for recall), and persisted.

This corresponds to **Memory Management (P)** in the academic survey (Zhang et al. 2024): summarize -> reflect. It is the "distillation-style episodic middle tier" (M5): fuller than an atomic fact, more compact than the raw transcript.

> Trigger condition: only when a reflection engine is configured (`with_compressor_and_reflection` etc., `manager.rs:131`). Otherwise it returns `Ok(None)` and does not reflect.

---

## 8. Persistence and Session Recovery

`SqliteSessionStore` (`sqlite_store.rs`) implements `MemoryPersistence` (`traits.rs:479`):

- **`memories` table** (`sqlite_store.rs:109`): `id/user_id/session_id/fact_type/subject/predicate/content/embedding_json/metadata_json/created_at/updated_at/version/importance`, unique index `(user_id, subject, predicate)`, indexes `user_id`, `session_id`.
- `store_fact` (`sqlite_store.rs:698`): `INSERT ... ON CONFLICT DO UPDATE ... version+1`, mirroring runtime conflict semantics.
- `load_facts` (`sqlite_store.rs:730`): `session_id=''` -> all user habits; otherwise by session scope.
- `save_conversation`/`load_conversation` (`sqlite_store.rs:539/582`): original conversation snapshot (incl. metadata, e.g. `task_anchor`).
- Session recovery: `AppSession.run` does `set_session_id` + `load_persisted_facts` each turn (`session.rs:613/616`); `save_session` at the end of each turn (`session.rs:839`). CLI: `oneai session list / resume <id> / delete / info`.

`AppBuilder::sqlite_persistence()` / `sqlite_persistence_at(path)` (`builder.rs:1319/1351`) enables it in one line; `embedding_service()` (`builder.rs:736`) wires semantic recall. `MemoryManager` combines features on demand via builder methods: `new` / `with_embedding` / `with_compressor_and_reflection` / `with_persistence` / `with_all_features` (`manager.rs:99-247`).

---

## 9. DomainPack Layer 7 MemoryProfile: Declarative Memory Policy

`MemoryProfile` (`memory_profile.rs:36`) makes memory behavior a declarable, mergeable, validatable domain layer, on the same level as `CompressionTemplate`/`ContextSource`/`PermissionProfile`:

| field | answers | drives |
|---|---|---|
| `extraction_schema: Vec<FactType>` | **what to remember** | FactExtractor prompt schema |
| `recall: RecallConfig` | **how to recall** (strategy/top_k/time_decay) | CoreMemorySource per-turn injection |
| `core_budget_tokens` | **how much is resident** | CoreMemory budget |
| `enable_memory_tools` | **who manages it** | AppBuilder registers self-managed tools |
| `habit_fact_types` | **what persists cross-session** | user namespace habits |

`RecallConfig` (`types.rs:1282`): `strategy ∈ {KeywordFirst, SemanticFirst, Hybrid}`, `top_k`, `time_decay`. Built-in presets: `MemoryProfile::coding()` (`memory_profile.rs:110`, schema 4 types, Hybrid, top_k 5, core 2048, tools on, habit=`user_tooling_pref`), `MemoryProfile::research()` (`memory_profile.rs:129`, schema 4 types, core 1536).

**Merge rule** (`memory_profile.rs:164 merge`, supporting multi-domain agents): schema/habits take union-dedup; recall takes primary; core_budget takes the **minimum** (strictest); enable_memory_tools takes OR.

`AppBuilder` wiring (`builder.rs:1617-1630`): reads `domain.memory_profile.enable_memory_tools` to decide whether to register tools; `session.rs:702` reads `extraction_schema` to feed the FactExtractor. **The entire memory behavior is switched in one line via `AppBuilder::domain_pack(coding_pack(...))`** — this is the core advantage over Mem0/Letta's "memory policy hardcoded in the framework".

---

## 10. How It Plays Out in Long-Horizon Tasks

The memory pain points of long-horizon tasks (multi-step, multi-turn, possibly cross-session resume) are: over-long context -> compression -> information loss -> constraint forgetting -> goal drift. OneAI's closed loop addresses each point:

| long-horizon pain | OneAI mechanism | location |
|---|---|---|
| context overflow | token-budget triggers compression (not a fixed max_iter), keeps most recent 6 turns | `compression.rs:141`, `agent_loop.rs:680` |
| compression loses info | compression-coupled FactExtractor extraction-to-archive + original-transcript snapshot backstop | `compression.rs:306`, `manager.rs:381` |
| original Goal summarized away | first user message verbatim pinned (Q2/Q3 hard guarantee) | `compression.rs:159` |
| early constraints diluted by long context | core tier resident + `EveryIteration` re-injection + constraint-sedimentation tool | `core_memory_source.rs:89`, `memory_tools.rs:188` |
| needed historical facts not recalled | three-factor recall injected each turn into `[Recalled Context]`, top_k domain-defined | `session.rs:629-636` |
| facts accumulate into self-contradiction across sessions | Mem0-style `(user,subject,predicate)` conflict update, version+1 | `fact_store.rs:67` |
| cross-session loss of preferences/habits | user namespace + SQLite + `load_persisted_facts` on resume | `manager.rs:316` |
| session interruption can't continue | `save_session` lands per turn + `oneai session resume` | `session.rs:839` |
| end-of-task insights not sedimented | end-of-session reflection -> episodic fact (importance 0.8) | `session.rs:839-858` |
| long output (shell/file) floods context | single older msg truncated to 2000 chars + pointer to memory_search | `compression.rs:187` |

**A typical long-horizon task run trajectory:**

1. `AppSession.run` -> set session_id, `load_persisted_facts` (carry in historical habits) -> `recall_facts(task)` injects relevant historical decisions -> CoreMemorySource re-injects core (incl. constraints) every turn.
2. AgentLoop does multi-turn inference/tool/delegation; context exceeds threshold -> compress: pin Goal + summarize old segment + **extract discarded segment into facts and archive** + land original transcript; next turn core/recall auto re-injected (compression-resistant).
3. Agent discovers a key constraint -> calls `core_memory_edit` to sediment it into the resident tier (salient every turn).
4. Fact conflict (e.g. preference change) -> upsert updates rather than appends, version+1.
5. End of session -> `reflection` generates an episodic fact (importance 0.8) and archives it -> `save_session` lands it.
6. Next new session for the same user -> `load_persisted_facts` brings back habits + episodic -> "gets better with use".

---

## 11. Benchmarking Against the Industry Frontier

The table below maps OneAI's mechanisms to 7 mainstream systems + 1 academic survey. Each row's "OneAI status" is a judgment of code facts; "gaps" point to §12.

### 11.1 Overview Benchmark Table

| design axis | OneAI status | industry reference | assessment |
|---|---|---|---|
| **three-tier tiering** | core / archival / recall (original transcript) | Letta core/archival/recall | ✅ basically aligned; recall tier uses snapshots rather than a separate message store |
| **conflict update** | deterministic structural key `(user,subject,predicate)` update-vs-insert, version+1 | Mem0 LLM judges ADD/UPDATE/DELETE/NONE; Zep bi-temporal edge invalidation | 🟡 structured, deterministic, zero hallucination; but lacks DELETE and "related-but-changed vs contradictory" distinction |
| **three-factor recall** | `0.5·rel+0.3·rec+0.2·imp`, 1h half-life, hardcoded weights, **not normalized** | Generative Agents: `α=1` three factors, min-max normalization, 0.995 decay | 🟡 idea aligned; weights non-configurable, not normalized, half-life hardcoded |
| **semantic recall** | query has embedding, but **stored fact embedding always None** -> degenerates to keyword | Mem0 vector+BM25+entity+temporal fusion; A-MEM cosine | 🔴 effectively non-functional, see §12.1 |
| **reflection/consolidation** | end-of-session LLM reflection -> episodic fact (importance 0.8) | Generative Agents importance-sum threshold -> reflection tree; Cognee `improve` background STM->LTM | 🟡 has reflection but only one-shot at end-of-session, non-recursive, not threshold-triggered |
| **self-managed tools** | `memory_search` / `core_memory_edit` / `archival_memory_insert` | Letta `core_memory_append/replace` + `archival_memory_insert/search` | ✅ nearly one-to-one; constraint-sedimentation idea is ahead |
| **importance scoring** | default scalar per type (decision 0.85…) + agent-overridable | Generative Agents LLM 1–10 poignancy | ✅ has explicit scalars, agent-overridable |
| **temporal/graph structure** | no graph, no bi-temporality; only `created/updated_at` | Zep bi-temporal T/T' + 4-timestamp edge invalidation; Mem0 native entity co-occurrence graph | 🔴 missing, see §12.2 |
| **namespace/multi-tenancy** | user_id + session_id dual namespace | Mem0 user/agent/run_id; Cognee dataset+session | ✅ aligned |
| **provenance traceability** | original-transcript snapshot retrievable (memory_search backstop) | Zep episode->derived fact; Cognee relational provenance | 🟡 has snapshot backstop, but facts and sources not explicitly linked |
| **declarative policy** | DomainPack layer 7 MemoryProfile, mergeable, validatable | most systems hardcode within the framework | ✅ **leading**: domain-level one-line memory-policy switch |

### 11.2 Quick Notes on Each System's Mechanism (for deeper reading)

- **Mem0**: external memory layer. `add()` LLM-extracts facts (SQL + vector + graph triple store), `search()` fuses semantic + BM25 + entity-boost + temporal. Conflict resolution relies on an LLM judging `ADD/UPDATE/DELETE/NONE` per fact-id (`mem0/configs/prompts.py`). The native graph is co-occurring entity links, schema-free, without typed edges. Tiering: conversation/session/user/org.
- **Letta/MemGPT**: context window as RAM, OS-style paging. core (structured persona/human blocks) / archival (vector store) / recall (message history). **Self-editing memory via tool calls** is its original contribution. OneAI's three self-managed tools come directly from here.
- **Generative Agents** (Park et al. UIST'23): memory stream + three-factor recall. recency exponential decay 0.995/game-hour, importance LLM 1–10 poignancy, relevance cosine, **min-max normalized, α all 1**. reflection: importance-sum exceeding 150 threshold -> generate question -> retrieve -> extract "insight with evidence pointer" -> **recursive reflection tree**.
- **Zep/Graphiti**: temporal knowledge graph. Three subgraphs (episode raw / semantic entity / community). **Bi-temporal**: T (event order) + T' (insertion order); each fact edge has 4 timestamps (`t_valid`/`t_invalid`/`t'_created`/`t'_expired`). New fact arrives -> LLM compares semantically related edges -> on temporal contradiction the old edge's `t_invalid` is set to the new edge's `t_valid` (**invalidation, not deletion; full history retrievable**).
- **A-MEM**: Zettelkasten-style atomic note (content+keywords+tags+context+embedding+links). On inserting a new note it **inversely evolves old notes** (LLM rewrites nearest neighbors' context/keywords); the store itself is agentic.
- **Cognee**: three stores (relational/vector/graph), permanent vs session modes, `improve()` backgrounds bridging session into the permanent graph (explicit STM->LTM consolidation op).
- **Academic survey** (Zhang et al. 2024, arXiv:2404.13501): Memory Writing (W) / Management (P) / Reading (R); R = similarity + time-interval + importance. OneAI's "extraction (W) -> reflection (P) -> three-factor recall (R)" is exactly this formal model.

---

## 12. Gaps and Improvement Directions

> **Status (1.1.0): all four gaps in this section have been fixed and wired into the eval suite (see §14).** The original text below is retained as a problem statement and record of fix directions; each item is prefixed with where the fix landed.

Four real gaps were found during review (ordered by impact).

### 12.1 [High] Semantic Recall Is Effectively Dead — Facts Were Never Embedded ✅ Fixed (1.1.0)

**Facts**: `FactExtractor::extract` (`fact_extraction.rs:159`) and `memory_tools.rs::build_fact` (`memory_tools.rs:35`) both hardcode `embedding: None`. `recall_facts` only embeds the query (`manager.rs:352`); inside `search_hybrid`, `f.embedding.as_ref()` is always None -> relevance goes through keyword hit (fixed 0.6 score). **Even with an EmbeddingService configured, it is only keyword recall.**

**Impact**: the semantic branch of `RecallStrategy::SemanticFirst`/`Hybrid` does not actually work; facts synonymous-but-different-in-wording cannot be recalled (e.g. querying "package manager" cannot recall a fact with subject=`user.package_manager` unless it matches literally).

**Fix direction**: in `archive_facts` (`manager.rs:300`) or `upsert`, if an EmbeddingService is configured, compute an embedding for `content` (or `subject+predicate+content`) and write it into `fact.embedding`; SQLite should also store `embedding_json` (the column already exists, `sqlite_store.rs:700`). `oneai-rag`'s `AutoEmbeddingDocumentIndex` (`embedding.rs:813`) is already a mature auto-embedding pattern for RAG documents and the same idea can be reused for MemoryFact.

> **✅ Fix landed (1.1.0)**: `MemoryManager::embed_fact` (`manager.rs`) unifies embedding of `"{subject} {predicate} {content}"` across the three write paths `archive_facts`, `reflect`, and `memory_tools::build_fact`, fail-safe (embedding failure only warns, does not block). The SQLite `embedding_json` column is now actually populated, and on resume `load_persisted_facts` carries the embedding vectors back. The `oneai-eval` anchor `ie_synonym_cross_lang` (Chinese fact + English query) measured: keyword recall recall@5=0 -> semantic recall recall@5=1 (see §14).

### 12.2 [Medium] Conflict Update Lacks DELETE and Semantic Distinction ✅ Fixed (1.1.0)

Mem0 uses an LLM to distinguish "related but changed (merge)", "contradictory (delete)", "duplicate (none)". OneAI only does update-vs-insert: the same key is always overwritten. Consequence — when the agent first says "use JWT" and then "abandon JWT, use session instead", the old decision is overwritten rather than history retained, and the decision evolution cannot be traced.

**Fix direction**: introduce optional LLM conflict judgment (could borrow Mem0's 4-event prompt), or like Zep do **soft invalidation** — don't delete the old value, mark it `superseded`, down-weight it on recall, and keep traceability. The current `version` field already reserves a slot for evolution; what's missing is a `superseded` flag or history table.

> **✅ Fix landed (1.1.0)**: `MemoryFact` gains `superseded`/`superseded_at` fields; `MemoryFactStore::upsert` appends the old revision into `metadata["_superseded_history"]` on conflict (decision evolution is traceable; the live row is still the new truth — the Mem0 invariant is unchanged); a new `invalidate`/`MemoryManager::invalidate_fact` soft-delete path is added (marks superseded, filtered out of recall by default; `search_hybrid_with_config(include_superseded=true)` allows audit retrieval). SQLite gains `superseded`/`superseded_at` columns + migration. Eval anchor `ku_auth_switch` measured: after the old value JWT is soft-invalidated, recall returns the new value session.

### 12.3 [Medium] Reflection Is Non-Recursive, Only at End-of-Session, Not Threshold-Triggered ✅ Fixed (1.1.0)

OneAI's `MemoryReflection` reflects once at end-of-session (`session.rs:839`). Generative Agents' reflection is **periodically triggered by an importance-sum threshold and recursively generates a reflection tree**, sedimenting intermediate abstractions mid-long-session. Cognee's `improve` is a background consolidation op.

**Fix direction**: every N turns in AgentLoop, check accumulated importance and trigger a reflection when it exceeds the threshold; let reflection retrieve and reference existing episodic facts (recursive). This complements §12.1's semantic recall — high-level insights generated by reflection should also be embedded to be recallable later.

> **✅ Fix landed (1.1.0)**: `MemoryReflectionConfig` gains `reflectance_threshold` (default 150, aligned with Generative Agents importance-sum threshold) + `trigger_interval_turns` (default 10); `MemoryReflection::should_reflect` is dual-gated by threshold + turn interval; `reflect_with_prior` summarizes the top-3 existing episodic facts from archival into the prompt (a recursive-reflection prototype); `MemoryManager::reflect_if_threshold` exposes a mid-session trigger entry point; `AppSession::run_agent` accumulates importance increments + iteration count at each turn's close and triggers a mid-session reflection when the threshold is exceeded, resetting the counter (without intruding into the AgentLoop inner loop, preserving the 1.0 boundary).

### 12.4 [Low] Three-Factor Weights/Normalization/Decay Hardcoded ✅ Fixed (1.1.0)

The `0.5/0.3/0.2` in `fact_store.rs:198`, the lack of min-max normalization, and the 1h half-life in `temporal_score_fact` (`fact_store.rs:220`) are all hardcoded and not adjustable via `RecallConfig`. Generative Agents requires the three factors to be normalized before weighting; otherwise weighted sums of different dimensions (cosine ∈[-1,1], importance ∈[0,1], recency ∈(0,1]) are not comparable.

**Fix direction**: fold the weights and half-life into `RecallConfig`; min-max normalize the candidate set before weighting.

> **✅ Fix landed (1.1.0)**: `RecallConfig` gains `relevance_weight`/`recency_weight`/`importance_weight` (defaults 0.5/0.3/0.2), `recency_half_life_secs` (default 3600), `normalize_factors` (default true) + builder; `search_hybrid_with_config` is now two passes: compute raw three factors -> min-max normalize (single-candidate/constant set degenerates to 1.0 so it is not zeroed out) -> weighted sort; `temporal_score_fact` half-life is parameterized; `recall_facts_with_config` is injected by `AppSession` each turn from the domain `MemoryProfile.recall`; `MemorySearchTool` injects `Arc<RecallConfig>` to use the same config.

---

## 13. Summary: The Positioning of OneAI's Memory System

OneAI's memory system **has reached first-tier open-source-framework levels on engineering closure**, and even leads in two dimensions:

- **Leading**: ① the compression-coupled extraction (compression-means-extraction-to-archive) closed loop is not native in Mem0/Letta; ② the declarative DomainPack `MemoryProfile` makes memory policy one-line switchable, mergeable, and validatable, more flexible than each system's "policy hardcoded in the framework"; ③ compression-resistant injection (`EveryIteration` + core block + pin Goal + constraint sedimentation) specifically guards long-horizon tasks' "goal/constraint drift".
- **On par**: three-tier tiering, Mem0-style conflict update, Generative-Agents-style three-factor recall, Letta-style self-managed tools, dual namespace + persistence + session recovery.
- **Behind**: semantic recall **effectively non-functional because facts were not embedded** (§12.1, should be fixed first); no temporal/graph structure (§12.2); reflection non-recursive and non-threshold (§12.3); recall weights/normalization hardcoded (§12.4).

In one sentence: **OneAI gets "memory as a declarative, compression-resistant, lossless closed loop" right, but the "semantic vector recall" leg has not landed** — only after auto-embedding facts (§12.1) can the three-factor recall and the semantic branch of `RecallStrategy` truly fulfill the design intent.

---

### Appendix: Key File Index

| concern | file |
|---|---|
| unified entry | `crates/oneai-memory/src/manager.rs:655` |
| fact container + conflict update + three-factor | `crates/oneai-memory/src/fact_store.rs:361` |
| core tier (budget/pin) | `crates/oneai-memory/src/core_memory.rs:193` |
| compression + extraction closed loop | `crates/oneai-memory/src/compression.rs:492` |
| fact extractor | `crates/oneai-memory/src/fact_extraction.rs:291` |
| reflection/episodic | `crates/oneai-memory/src/reflection.rs:562` |
| compression-resistant injection source | `crates/oneai-memory/src/core_memory_source.rs:160` |
| self-managed tools | `crates/oneai-memory/src/memory_tools.rs:354` |
| declarative policy | `crates/oneai-domain/src/memory_profile.rs:246` |
| recall injection wiring | `crates/oneai-app/src/session.rs:626-636` |
| compressor wiring | `crates/oneai-app/src/session.rs:694-723` |
| reflection trigger | `crates/oneai-app/src/session.rs:839-858` |
| tool registration | `crates/oneai-app/src/builder.rs:1617-1630` |
| persistence memories table | `crates/oneai-persistence/src/sqlite_store.rs:109/698/730` |
| shared traits | `crates/oneai-core/src/traits.rs:479`(MemoryPersistence) `557`(DiscardedSink) `641`(EmbeddingService) |
| shared types | `crates/oneai-core/src/types.rs:1200`(RecallStrategy) `1282`(RecallConfig) |

---

## 14. Memory Evaluation Plan (new in 1.1.0)

`oneai-eval::memory` is a memory-subsystem evaluation system aligned with authoritative industry benchmarks, forming an "optimize -> evaluate -> quantify gains" closed loop. The methodology has three sources:

- **LongMemEval** (arXiv:2410.10813, ~427 citations) — 5 long-term-memory abilities: Information Extraction (IE) / Multi-Session Reasoning (MR) / Temporal Reasoning (TR) / Knowledge Update (KU) / Abstention (ABS). Human-annotated evidence -> Recall@k / NDCG@k computable without an LLM judge.
- **Mem0** (arXiv:2504.19413) — F1 + BLEU-1 + LLM-as-Judge triple scoring (the scoring commonly used by downstream industry papers).
- **MemBench** (arXiv:2506.21605) — directly evaluates the memory body itself (recall accuracy / capacity / temporal efficiency).

### 14.1 Evaluation Anchors (quantifying optimization gains)

| anchor case | gap verified | keyword baseline | semantic recall |
|---|---|---|---|
| `ie_synonym_cross_lang` (Chinese fact + English query) | §12.1 | recall@5=0.00 | recall@5=1.00 ✅ |
| `ie_pkg_manager`/`ie_test_runner` (natural-language question -> fact) | §12.1 | recall@5=0.00 | recall@5=1.00, F1=1.00 ✅ |
| `ku_auth_switch` (JWT->session knowledge update) | §12.2 | recalls old value | recalls new value session, F1=1.00 ✅ |
| `abs_never_mentioned`/`abs_unrelated` | no hallucination | abstention=1.00 ✅ | abstention=1.00 ✅ |

How to run: `oneai eval memory --suite builtin` vs `--no-embedding` to compare the §12.1 gain.

### 14.2 Architecture

- **Does not depend on the full AgentLoop** — the evaluator drives `MemoryManager` directly (replay multi-session planted facts -> `recall_facts_with_config` -> synthesize a deterministic answer -> score), eliminating the evaluator's own uncertainty from contaminating the memory-subsystem score.
- **Metrics**: `recall_at_k`/`ndcg_at_k` (pure Rust, comparing evidence_keys, CI-runnable), `f1`/`bleu1` (LoCoMo/Mem0 definition, CJK split by character), `abstention`, optional `llm_judge` (per ability rubric).
- **Datasets**: built-in synthetic suite (`builtin_suite()`, 10 cases covering 5 abilities + synonym negatives) + `load_suite_jsonl` loader (compatible with LoCoMo/LongMemEval JSONL schema, with `scripts/download_memory_bench.sh`).
- **Offline-runnable**: `DeterministicEmbeddingService` (byte-histogram vectors) serves as an offline placeholder for the semantic path, so CI can demonstrate the §12.1 gain without an API key; real quality measurement should substitute OpenAI/Ollama embeddings.

### 14.3 File Index

| concern | file |
|---|---|
| module entry | `crates/oneai-eval/src/memory.rs` |
| case/ability/session types | `crates/oneai-eval/src/memory/case.rs` |
| pure-Rust metrics | `crates/oneai-eval/src/memory/metrics.rs` |
| built-in suite + JSONL loading | `crates/oneai-eval/src/memory/suite.rs` |
| Runner + deterministic embedding | `crates/oneai-eval/src/memory/runner.rs` |
| CLI subcommand | `examples/cli/src/cmd_eval.rs::cmd_eval_memory` |

---

## Further reading

- [context-management-mechanism](context-management-mechanism_EN.md) — the assembly/compression loop coupled to memory extraction
- [working-state-mechanism](working-state-mechanism_EN.md) — task-level working-state persistence (a separate path from memory)
- [domain-pack-mechanism](domain-pack-mechanism_EN.md) — layer 7 `MemoryProfile` declarative memory policy
- [rag-mechanism](rag-mechanism_EN.md) — `EmbeddingService` and the semantic-recall backend
- [persistence-mechanism](persistence-mechanism_EN.md) — SQLite persistence (memories table / facts / usage)
- [eval-mechanism](eval-mechanism_EN.md) — the `oneai-eval::memory` eval suite (LongMemEval 5 abilities)
- Source: `crates/oneai-memory/src/` (13 files / ~7.7K LOC) + `crates/oneai-rag` + `crates/oneai-persistence`
- [CLAUDE.md — Architecture: Memory/Persistence](../CLAUDE.md)
