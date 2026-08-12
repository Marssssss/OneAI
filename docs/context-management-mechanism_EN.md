# OneAI Context Management Mechanism (Whitepaper)

> A durable-log / ephemeral-assembly separation + anti-compression re-injection + token-budget-driven termination + 3-layer model-context resolution + four trimming strategies + compression-coupled extraction + cache-aware assembly context engine: `state.conversation` is the only durable log, the assembly is rebuilt ephemerally each turn on a clone, and compressed-away state is re-injected next turn rather than retained by the compressor.

> Version: corresponds to codebase `0.2.0` / 1.0.0 line. This document is written from a file-by-file review of the source code of `crates/oneai-core` (`budget.rs`/`context_manager.rs`/`context_accounting.rs`/`token_counter.rs`/`model_context.rs`), `crates/oneai-agent` (`agent_loop.rs`/`context_assembler.rs`/`sub_agent.rs`), `crates/oneai-memory` (`compression.rs`/`core_memory_source.rs`), `crates/oneai-domain` (`context_source.rs`/`compression_template.rs`), `crates/oneai-provider` (`anthropic.rs`); every mechanism is annotated with `file:line` for verification. The sister document *Memory Mechanism Whitepaper* (`docs/memory-mechanism_EN.md`) focuses on the three memory tiers and recall, while this document focuses on **the assembly, budgeting, resolution, trimming, compression, pinning, caching, and multi-agent isolation of the context window**.

---

## 0. One-sentence summary

OneAI's context management is an engine of **"durable-log/ephemeral-assembly separation + compression-resistant reinjection + token-budget-driven termination + three-layer model context resolution + four-strategy trimming + compression-coupled extraction + cache-aware assembly"**: `state.conversation` is the single durable log; each iteration **ephemerally rebuilds** an assembly on its clone (ContextSource blocks + pinned blocks); any state that gets compressed out survives via reinjection the next iteration rather than relying on the compressor to retain it; termination is governed by `TokenBudget` rather than a hardcoded `max_iterations`; the model context window is resolved through three layers — L1 user config > L2 provider API probe > L3 builtin library; when the budget is exceeded, trimming follows four strategies, and compression is coupled into the fact-extraction closed loop — "compression means loss" is closed off.

---

## 1. Core paradigm: durable/ephemeral separation

This is the **first principle** of the entire context-management design; understanding it is prerequisite to understanding everything else.

| Layer | Content | Written back? | Participates in compression? |
|---|---|---|---|
| **Durable log** `state.conversation` | system prompt, user task, assistant replies, tool results — appended, persisted, and compressed each iteration by `AgentLoop` | ✅ Written back | ✅ Object the compressor acts on |
| **Ephemeral assembly** `conv_for_inference` | Durable-log clone + ContextSource cached blocks + pinned blocks (TaskAnchor/PlanProgress/skill menu) | ❌ Never written back | ❌ Compressor only sees the durable log |

The doc comment in `context_assembler.rs:72-101` states this paradigm clearly:

> `state.conversation` is the durable log; `assemble()` produces a **fresh, ephemeral assembly each iteration** — the durable-log clone plus the cached content of each `ContextSource` — and the inference request uses it. Because the assembly is rebuilt each turn and never written back to the durable log, **pinned state (env sensing, core memory, task anchor) survives compression by reinjection**, rather than relying on the compressor to retain it. The compressor only sees the ephemeral assembly; whatever it summarizes away is restored the next turn.

### This yields three direct consequences

1. **Compression resistance does not rely on the compressor**: ContextSource blocks / TaskAnchor / PlanProgress that the compressor summarizes away are reinjected next turn by `assemble()` + `inject_pinned_blocks()`. The comment on `assemble()` at `context_assembler.rs:90-101` is explicit — "the epoch/baseline distinction no longer gates *injection*; only `refresh_sources` uses it to decide whether to re-invoke `load()`. This is what makes the block compression-resistant: it reappears every turn, regardless of what the compressor did to the previous assembly."
2. **RefreshPolicy governs only whether `load()` is re-invoked, not whether it is injected**: `refresh_sources()` at `context_assembler.rs:140-146` re-invokes `load()` for every source each turn to refresh the cache, whose content is then injected by `assemble()`. The old `OnceAtStart`/`OnChange` "skip injection" optimization only made sense under the old model where "injection accumulates into the durable log" — under the ephemeral model it would let a source disappear after the first turn, so it was abandoned (the test `every_source_reinjected_every_turn_regardless_of_policy` at `context_assembler.rs:86-89` locks this behavior down).
3. **Compression only compresses the durable log**: when the assembled request would overflow, what is compressed is the **durable log**, not the ephemeral assembly — so `discarded_messages` are real transcript, the durable log stays bounded, and pinned blocks are rebuilt on top of the post-compression durable log (`agent_loop.rs:1041-1077`).

> Historical lesson (`agent_loop.rs:1056-1058`): an early version dropped `assembled` on "non-compression turns" and sent the request straight from the bare durable log, causing ContextSource injection on normal turns to **never reach the model** — fixed; now every turn goes through the full assemble → inject → fit-check → (compress durable → re-assemble → re-inject).

---

## 2. Per-iteration assembly pipeline

The context-assembly steps of the `AgentLoop` main loop each turn (`agent_loop.rs:1041-1200`), in strict order:

```
┌──────────────────────────────────────────────────────────────────────┐
│ 1. refresh_sources()            re-invoke all ContextSource.load() to refresh cache │ agent_loop.rs:1060-1062
│ 2. assemble(state)              durable-log clone + inject ContextSource cached blocks │ agent_loop.rs:1070
│ 3. inject_pinned_blocks()       inject pinned blocks (see §6)                │ agent_loop.rs:1071
│ 4. needs_compression(conv)?     check by token budget whether it overflows    │ agent_loop.rs:1073
│    ├─ no: use current conv_for_inference                                  │
│    └─ yes: compress(state.conversation) compress the durable log (see §5) │ agent_loop.rs:1074
│           → re-assemble + inject_pinned_blocks onto compressed durable log │ agent_loop.rs:1075-1076
│ 5. sync plan_state → metadata   live plan written back to durable-log metadata (compression-resistant + restorable) │ agent_loop.rs:1082-1090
│ 6. build InferenceRequest       + paradigm-aware tool defs                  │ agent_loop.rs:1100-1119
│ 7. PreInfer gate                may temporarily inject/rewrite request (see §7) │ agent_loop.rs:1144-1181
│ 8. ContextAccounting::account    per-category token breakdown of full assembly + tool defs (see §4) │ agent_loop.rs:1195-1200
│ 9. infer (streaming or not)                                                │ agent_loop.rs:1222-1234
└──────────────────────────────────────────────────────────────────────────────┘
```

### 2.1 Content order of the assembly (injected into `conv_for_inference`)

1. **Full durable log** (`state.conversation.clone()`, `context_assembler.rs:91`) — system prompt + historical turns.
2. **ContextSource cached blocks** (`inject_sources`, `context_assembler.rs:108-130`): injected in ascending `priority()` order, each non-empty cached content wrapped into a system message of the form `[Context: {key}] {content}`. The predicate is always `true` — every cached source is injected every turn.
3. **Pinned blocks** (`inject_pinned_blocks`, `agent_loop.rs:3625-3652`):
   - `[Task Anchor] (do not compress — original task)` — the original task verbatim + optional distilled intent (`context_assembler.rs:164` `task_anchor_block`);
   - `[Plan & Progress] (do not compress — live task list)` — when a live plan exists, render the ✅/🔄/⏳ checklist (`context_assembler.rs:179` `plan_progress_block`);
   - skill menu (Tier1 always present) + the full active skill `prompt_template` (when `inject_skills` is true).
4. **Runtime context block** (`runtime_context_block`, `context_assembler.rs:198-212`): the current date/time + guidance that time-sensitive questions must first use `web_search`/`web_fetch`, appended to the system prompt (at session start, `agent_loop.rs:908/936`). **Appended to the system prompt rather than a temporary system message**, because the system prompt is more compression-resistant (`context_assembler.rs:194-197`).

### 2.2 Ownership of env-diff detection

Environment sensing (git status, file tree, working directory, current date) **is wholly owned by the `ContextSource` implementations of `oneai-domain`** — `ContextAssembler` itself does not run any git/filesystem probes (`context_assembler.rs:17-21`). This makes env sensing pluggable, governed by RefreshPolicy, and composable across DomainPacks, rather than a hardcoded parallel path. For example, `GitStatusSource` is `OnChange`: when the git state changes, `load()` returns new content and the next turn's assembly injects the full git block.

---

## 3. Token-budget-driven termination (rather than max_iterations)

### 3.1 TokenBudget

`budget.rs:326` — the total token budget for a session/sub-agent:

| Field | Meaning |
|---|---|
| `total: u32` | Total budget (may be `from_context_window` = 0.8× window) |
| `consumed: u32` | Already consumed (prompt + completion + tool results) |

Key methods: `remaining()`(:355), `record_usage(prompt, completion)`(:360), `can_support_iteration(estimated_cost)`(:365), `estimated_remaining_iterations(per_iter_cost)`(:370).

### 3.2 Termination semantics

`AgentLoopConfig` has `hard_max_iterations: Some(200)` (`agent_loop.rs:621`) as a **safety guardrail**, but the primary termination condition is `TokenBudget` — when `can_support_iteration` is insufficient, it stops. The doc is explicit (`budget.rs:323-324`): "When `remaining()` drops below `min_iteration_cost`, the loop should terminate." This replaces a hardcoded `max_iterations`, letting long tasks be naturally bounded by budget while short tasks are not truncated by an artificial iteration cap.

### 3.3 BudgetAllocation (proportional allocation by source)

`budget.rs:383` — allocates the budget proportionally across different context sources:

| Source | Default share |
|---|---|
| system_prompt | 10% |
| recent_turns | 30% |
| tool_results | 25% (largest, because tool output can be extremely long) |
| skills | 10% |
| retrieved | 15% |
| overhead | 10% |

`CompressionPriority` (`budget.rs:461`) defines the trimming priority on budget overflow: `ToolResults`(1) → `OlderTurns`(2) → `Retrieved`(3) → `Skills`(4) → `RecentTurns`(5, touched only last).

### 3.4 ContextBudgetManager

`budget.rs:488` — the orchestrator of budget checks and automatic compression, injected into the `AgentLoop` assembly steps (`budget.rs:482-487` usage example):

- `needs_compression(conv)`(:576): uses `TokenCounter` (if configured) or the compressor heuristic (~4 chars/token) to estimate tokens; exceeding `budget.total` means compression is needed;
- `compress(conv)`(:597): a **three-step pipeline** —
  1. `estimate_source_tokens`(:647) estimates per source;
  2. if tool_results exceed the allocation → `truncate_tool_results`(:688) **lossless truncation tier** — each `ToolResult` block is truncated head-wise to a character cap derived from the budget + a `[...output truncated — use memory_search for the full output]` pointer appended, telling the model to fetch the full output via `memory_search` (`budget.rs:705`);
  3. `compressor.compress()` summarizes the older segments;
  4. if a `DiscardedSink` is configured, the `discarded_messages` are persisted as a raw-transcript snapshot (`budget.rs:616-623`, C2 fallback) — "compression is not loss".

`with_token_counter(tc, model)`(:552) wires in model-aware token counting (see §4), replacing the compressor's ~4 chars/token heuristic for more accurate CJK-text estimation; `with_discarded_sink`(:565) wires in raw-transcript archival.

---

## 4. Three-layer model-context resolution + token counting

### 4.1 Three-layer resolution (opencode-style)

`model_context.rs` — the **single source of truth** for the model context window size, with strict priority (`model_context.rs:3-20`):

| Layer | Source | ContextSource label |
|---|---|---|
| **L1 User** | `ONEAI_CONTEXT_WINDOW` env var (global highest) / `ContextManagerConfig.profiles` per-model profile / `ModelConfig.extra["context_window"]` per-provider-model override | `UserEnv` / `UserProfile` / `UserProviderExtra` |
| **L2 Provider API** | `LlmProvider::probe_context_window()` — Ollama `/api/show`, Anthropic `/v1/models/{id}`, Gemini `models.get`, OpenAI-compat best-effort; results cached | `ProviderApi` |
| **L3 Builtin library** | `BUILTIN_MODEL_CONTEXT` static table (`model_context.rs:61-93`, covering Anthropic/OpenAI/Gemini/GLM/DeepSeek/Qwen/Llama, sorted specific→general); if still unknown, `infer_context_window_for_tokenizer` name-pattern heuristic | `BuiltinLibrary` / `NameHeuristic` |

**Two resolution paths** (`model_context.rs:16-20, 243-295`):

- `resolve_cached(model)` (sync, :248): L1 → probe cache → L3. **Never makes a network request** — safe to use inside the sync `TokenCounter::context_window_size`. Probe results are pre-cached by the async warm-up / agent-loop path.
- `resolve_with_provider(model, provider)` (async, :270): L1 → live L2 probe (writes cache) → L3. Used by the async trim path and CLI `token probe`.

This design **mirrors opencode's `BUILTIN_MODEL_CONTEXT` + three-layer resolution** while fitting OneAI's sync `TokenCounter` trait contract: probing is opt-in during warm-up, and the sync path only reads the cache.

### 4.2 HeuristicTokenCounter (per-provider, CJK-aware)

`token_counter.rs:475` — a reasonable estimate in the absence of a provider-specific tokenizer library:

- **Per provider family** (`ProviderTokenizerType`, :173): OpenAI tiktoken/BPE, Anthropic proprietary, Google SentencePiece, Ollama per-model, Generic fallback. Each family has different chars/token (:222-241):

  | Type | English CPT | CJK CPT |
  |---|---|---|
  | OpenAI | 4.0 | 2.0 |
  | Anthropic | 3.8 | 1.8 |
  | Google/Ollama/Generic | 4.0 | 2.0 |

- **CJK-aware** (`LanguageType::detect`, :280): uses Unicode ranges to classify CJK/Latin/Mixed; CJK proportion >30% is treated as CJK-dominant, and mixed text uses 50/50 weighting (`chars_per_token_for_text`, :448). GLM is also classified as Ollama-style (Chinese-oriented tokenization, :205).
- **Per-message overhead** (:354): role markers, delimiters, formatting — OpenAI 4 tok/msg, Anthropic 6 tok/msg, system-prompt overhead 8-10, tool-definition overhead 6-8. The naive ~4 chars/token heuristic ignores these.
- Estimation is typically within ±10% for English (:474 comment).

`count_conversation_tokens`(:572) counts per block Text/ToolCall/ToolResult/Image(170)/Thinking/File(50) + per-msg overhead + system overhead.

### 4.3 ContextFitResult (whether it fits)

`token_counter.rs:90` — the assembly-check result: `fits` (whether ≤ window×threshold), `total_tokens`, `context_window`, `remaining_tokens`, `overflow_tokens`, `utilization_pct`. The threshold defaults to 0.8 (leaving 20% for new tokens, :73-78). Used by SmartRouter context-aware routing and ContextManager trimming.

### 4.4 ContextAccounting (per-category token breakdown)

`context_accounting.rs:31` — breaks context-window occupancy **down by category**: system prompt / user / assistant / tool_call / tool_result / thinking / image / file, each with tokens + share + a visualization bar. Serves the TUI sidebar `📝~ctx N%` and the `/context` command, **both sourcing from the same** `HeuristicTokenCounter` for consistency (:9, :82-166). `agent_loop.rs:1195-1200` computes accounting each turn with the real model name (e.g. `glm-5.1` rather than the provider-type name) to feed the observer.

### 4.5 SmartRouter's token counting

`HeuristicTokenCounter`'s `context_window_size`(:629) delegates to the resolver when one is attached (L1→cache→L3, :634-636). SmartRouter uses it to decide "can this model hold the current conversation" for routing decisions — context-aware routing.

---

## 5. Four-strategy trimming + compression pipeline

OneAI has **two** trimming/compression implementations with different responsibilities:

| Implementation | crate | Role | When used |
|---|---|---|---|
| **ContextManager** (4 strategies) | `oneai-core` | **Immediate trimming** after SmartRouter routes to a specific model, ensuring the conversation fits that model's window | SmartRouter/CLI token path |
| **ContextCompressor** (LLM summary + extraction) | `oneai-memory` | **Summary compression** when the AgentLoop budget is exceeded + the compression-coupled fact-extraction closed loop | AgentLoop main loop |

### 5.1 ContextManager's four strategies (`context_manager.rs:46`)

`ContextTrimmingStrategy` — quality/cost/reliability tradeoffs:

| Strategy | Approach | Needs LLM | Default |
|---|---|---|---|
| **TruncateOldest**(:56) | Keep the most recent N turns (default 6 ≈ 3 interaction turns) + system + **first user message pinned**; old turns truncated to a 200-char stub; long tool_result truncated to 2000 chars | ❌ | ✅ Default |
| **ImportanceRanked**(:73) | Rank by importance: system always kept > recent turns full > tool_result truncated > old turns summarized; keep useful tool_results | ❌ | |
| **CompressMiddle**(:90) | Keep first N + last N, compress the middle into a single summary (long-conversation friendly) | ❌ | |
| **SmartSummary**(:112) | LLM generates a structured handoff (Goal/Progress/Key Decisions/Critical Files/Next Steps) + first user message pinned + most recent N turns | ✅ | requires summarizer |

**Key fix** (`context_manager.rs:535-552`): the old `SmartSummary` **always silently degraded** to TruncateOldest and never generated a handoff. Now `with_summarizer`(:439) wires in an LLM for real summarization; when no summarizer is present it degrades to "first-user-pinned TruncateOldest" **and logs it** (no longer silent).

**Q2 hard guarantee — first user message pinned** (`context_manager.rs:599, :1287` test): the original task is the context most worth preserving during compression; once identified it is treated the same as system/recent turns — even if it falls into the "old segment" it is not compressed into a 200-char stub. `TruncationCompressor` (`budget.rs:121`) and `ContextCompressor` (`memory/compression.rs:159`) both mirror this guarantee.

### 5.2 ContextWindowProfile (per-model window profile)

`context_manager.rs:193` — `model_name` + `context_window_tokens` + `max_output_tokens` + `recommended_utilization` (default 0.8) + `trimming_strategy`. `effective_limit`(:264) = window × utilization. `default_profiles`(:242) ships 12 built-in model profiles. `profile_for_model`(:467), when a resolver is attached, resolves the window size through three-layer resolution (:470-476) overriding the static profile.

### 5.3 ContextCompressor's compression pipeline (`memory/compression.rs:141`)

`compress()` steps (see sister doc §4.2 for detail):

1. Keep the most recent N turns (`keep_recent_turns`, default 6);
2. **Pin the first user message verbatim** (Q2/Q3 hard guarantee, :159) — placed between the summary and the recent tail;
3. **Losslessly truncate** each older message slated for summarization (`MAX_OLDER_MSG_CHARS=2000`, truncate head + pointer to `memory_search`, :187);
4. The LLM summarizes the older segments per the domain `CompressionTemplate` (`with_template`, :62);
5. **Compression-coupled extraction**: run `FactExtractor.extract` over `discarded_messages` (`extract_and_archive`, :306), extracting atomic facts per the domain `extraction_schema` and conflict-resolving them into the archival tier — **the compressed-out information is not lost, it becomes long-term memory**;
6. `discarded_messages` are persisted via `DiscardedSink` as a raw-transcript snapshot (C2 fallback).

The entire extraction is **fail-safe** (:327) — a bad extraction only `tracing::warn!`s and never interrupts compression.

> Wiring: `ContextCompressorTrait` (`budget.rs:31`) is a dependency-inversion trait — defined in `oneai-core`, implemented by `oneai-memory::ContextCompressor` (`compression.rs:341`); `ContextBudgetManager` accepts any implementation. This lets core not depend on memory.

---

## 6. Anti-compression pinning

The ephemeral reinjection model lets three classes of "must never be compressed out" state survive via **per-turn reinjection**:

| Pinned block | Content | Source | File |
|---|---|---|---|
| `[Task Anchor]` | Original task verbatim + optional distilled intent; metadata also mirrors `task_anchor` | `task_anchor_block` | `context_assembler.rs:164` |
| `[Plan & Progress]` | ✅/🔄/⏳ checklist of the live plan; synced into `metadata["plan_state"]` | `plan_progress_block` + `agent_loop.rs:1082-1090` | `context_assembler.rs:179` |
| `[Core Memory]` + `[Recalled Context]` | Resident curated facts + per-turn recall; `RefreshPolicy::EveryIteration` reinjected | `CoreMemorySource` | `core_memory_source.rs:80-91` |

**Task Anchor double safeguard** (`context_assembler.rs:155-163`): it is both injected as a pinned block ephemerally each turn and mirrored into `Conversation::metadata["task_anchor"]` — every compressor copies metadata verbatim (`budget.rs:200`, `compression.rs:248`), so even if the first user message itself is summarized away, the task_anchor in metadata remains.

**Plan State double safeguard** (`agent_loop.rs:1082-1090`): the live plan is synced into `metadata["plan_state"]`, restored by `from_conversation` — both compression-resistant and reload-resistant.

**CoreMemorySource compression resistance** (`core_memory_source.rs:86-96`): `refresh_policy() = EveryIteration` is the key to compression resistance — the compressor drops old turns (keeping only `keep_recent_turns`), but the next turn's `assemble()` reinjects the core block. Contrast with the old design: a one-shot "Previous conversation context" system message buried in history would be erased by summarization. `priority() = 10` is high priority for earlier injection.

---

## 7. Cache-aware assembly + interaction-gate context control

### 7.1 Prompt caching (Anthropic `cache_control: ephemeral`)

`anthropic.rs:179-262` — static context gets `cache_control: ephemeral` breakpoints to avoid resending every turn:

- a breakpoint on the system-prompt block (:192);
- a breakpoint on the last tool definition (:240-262) — this creates a cache boundary so that the tool defs + the stable system prefix hit the cache.
- `InferenceRequest.metadata["prompt_cache_policy"]` (passed through at `agent_loop.rs:1116-1117`) controls this: `Off` strips all breakpoints (baseline measurement); default `Auto` is on (:183).

This **synergizes** with the ephemeral-assembly paradigm: reinjecting pinned blocks every turn looks like "resending", but the stable prefix (system + tool definitions) hits the cache and only the changed parts are actually billed — the "throttle only suppresses publish, not dispatch" lesson recorded in `stream-macOS-mainqueue-flooding` and related memories also resonates: the token cost of reinjection is largely amortized by the prompt cache.

### 7.2 Interaction-gate control of context

Of `InteractionGate`'s 7 decision points, two directly rewrite context (`PreInfer`/`PostInfer`, `agent_loop.rs:1144-1181`):

- **PreInfer**: the application layer may `ProceedWith{InjectSystemMessage}` **temporarily inject** (not written to the durable log, to prevent accumulation, :1156-1160), `ReplaceRequest` rewrite, `Revise{feedback}` making the feedback **both a durable user turn and part of this turn's request** (:1166-1172), or `Abort`.
- **PostInfer**: may validate/filter/replace the response, or request a feedback-grounded retry.

`Revise`'s dual-write design is a context-management detail: the feedback is both a durable turn (the next turn's assembly includes it) and in the current turn's request (the model sees it this turn).

---

## 8. Multi-agent context isolation (sub-agent / delegate)

### 8.1 Sub-agent returns only a summary (context isolation)

`sub_agent.rs:8` principle: "a sub-agent only returns a **summary** to the main agent; the full conversation is not brought back". The `summary` field (:127) of `SubAgentSummary` (:113) is "a distilled summary, not the full output". This makes deep task decomposition feasible — the sub-agent does substantial work in an isolated context while the main agent's context only inflates by one summary.

### 8.2 Parallel multi-delegation + Kahn-wave scheduling

`parallel_executor.rs` + `scope_state.rs` implement single-turn multi-`delegate` batching + Kahn topological-sort wave scheduling (memory `parallel-multi-delegation.md`):

- **Independent delegates execute in parallel**;
- **Dependent delegates execute serially**, with upstream summaries auto-injected into downstream sub-agent contexts (dependency-aware);
- Cycle detection.

Summary injection is the extension of context management to the multi-agent layer: the `SubAgentSummary` of an upstream sub-agent is injected into the downstream initial context, letting information flow along the dependency chain without exploding the main context.

### 8.3 Paradigm switching rewrites context

The model-driven `switch_paradigm` / `delegate` in `meta_tool.rs`: `apply_paradigm_switch` + `AgentLoopGraphActionExecutor` inline-upgrade the paradigm (system prompt + tool filter, see `paradigm-delegate-metatool.md`). A paradigm switch is a context reorganization — Plan/Reflect/Explore each have a different system prompt and tool subset, so context changes with the paradigm.

---

## 9. The closed loop in long-horizon tasks (pain point × mechanism)

| Long-horizon context pain point | OneAI mechanism | Location |
|---|---|---|
| Context overflow | token-budget triggers compression (not max_iter), keeps the most recent 6 turns | `agent_loop.rs:1073`, `budget.rs:576` |
| Inaccurate window size (new/unknown models) | three-layer resolution L1>L2>L3, sync makes no network call | `model_context.rs:248` |
| Token-estimation drift (CJK) | per-provider + CJK-aware HeuristicTokenCounter | `token_counter.rs:475` |
| Compression loses information | compression-coupled FactExtractor extraction + archival + raw-transcript snapshot fallback | `compression.rs:306`, `budget.rs:616` |
| Original Goal summarized away | first user message pinned (Q2/Q3) + metadata mirror | `context_manager.rs:599`, `compression.rs:159` |
| Plan/progress compressed out | PlanProgress reinjected each turn + metadata sync | `agent_loop.rs:1082-1090` |
| Early constraints diluted by long context | CoreMemorySource EveryIteration reinjection + constraint sedimentation | `core_memory_source.rs:89` |
| Long output floods context | lossless truncation tier (truncate head + memory_search pointer) | `budget.rs:688`, `compression.rs:187` |
| Token cost of reinjection | prompt-cache ephemeral breakpoints amortize the stable prefix | `anthropic.rs:179-262` |
| Multi-agent context explosion | sub-agent returns only summary + parallel dependency-aware summary injection | `sub_agent.rs:8` |
| Assembly turn dropped (historical bug) | full assemble→inject→fit→compress every turn | `agent_loop.rs:1056-1058` |

---

## 10. Benchmarking against the state of the art

### 10.1 Overview benchmark table

| Design axis | OneAI status | Industry reference | Assessment |
|---|---|---|---|
| **Durable/ephemeral separation** | `state.conversation` durable + `conv_for_inference` ephemerally rebuilt each turn, never written back | Claude Code "context edit", MemGPT OS-style paging | ✅ Leading: reinjection paradigm makes compression resistance not depend on the compressor |
| **Anti-compression pinning** | TaskAnchor/PlanProgress/CoreMemory reinjected each turn + metadata double safeguard | Aider repo-map reinjected each turn, Cursor context pinning | ✅ Aligned, and the metadata double safeguard is more robust |
| **Termination semantics** | TokenBudget-driven, hard_max_iterations only a guardrail | Most frameworks use max_iterations or a token cap | ✅ Natural budget constraint |
| **Model-context resolution** | three-layer L1>L2>L3, sync resolve_cached makes no network call | opencode `BUILTIN_MODEL_CONTEXT` three layers | ✅ Directly aligned with opencode |
| **Token counting** | per-provider + CJK-aware + per-msg overhead heuristic | tiktoken/real tokenizers; LangChain len-token counter | 🟡 Heuristic ±10%, CJK-friendly; lacks a real tokenizer |
| **Trimming strategies** | 4 strategies (Truncate/Importance/CompressMiddle/SmartSummary) | LangChain `trim_messages` (token/message/selector); LlamaIndex postprocessor | ✅ Rich strategies, SmartSummary really generates a handoff |
| **Compression pipeline** | LLM summary + lossless truncation tier + first-user-pinned + compression-coupled extraction | Claude Code auto-compact; MemGPT summary compression | ✅ Compression-coupled extraction is an OneAI distinctive feature |
| **Cache awareness** | Anthropic ephemeral breakpoints + policy switch | Anthropic prompt caching official guide; cache_control across frameworks | ✅ Aligned with official, policy can be turned off |
| **Multi-agent isolation** | sub-agent returns only summary + parallel dependency-aware injection | Claude Code subagents isolate context; LangGraph state channels | ✅ Aligned, Kahn-wave dependency injection is leading |
| **Interaction-layer context control** | PreInfer temporary injection/Revise dual-write | Most frameworks rely on callbacks | ✅ Clear ephemeral/durable separation |
| **Context accounting** | ContextAccounting per-category breakdown, sidebar and /context same source | Claude Code `/context`, Aider /tokens | ✅ Aligned |

### 10.2 State-of-the-art research/product notes (for deeper benchmarking)

- **Anthropic "context engineering" (2025-06)**: proposes moving from "prompt engineering" to "context engineering" — an agent's success depends on what it puts into and discards from context each turn. Core practices: ① **just-in-time context** (fetch on demand, not stuff everything in); ② **microagents/subagents** (isolate context for subtasks, return only conclusions); ③ **auto-compaction** (summarize old context past a threshold); ④ **context window as budget**. OneAI's ephemeral reinjection + sub-agent-returns-only-summary + token budget + compression pipeline **correspond item-by-item** to this methodology.
- **Claude Code**: `/compact` manual/auto compression, subagent isolated context, `context-edit` dynamic rewriting, `/context` shows occupancy. OneAI's `ContextAccounting` ↔ `/context`, `ContextCompressor` ↔ auto-compact, `sub_agent` summary ↔ subagent isolation. OneAI additionally **couples compression into fact extraction**, preserving more information than Claude Code's pure summarization.
- **MemGPT/Letta**: treats the context window as RAM, OS-style core↔archival paging. OneAI's `CoreMemory` has a token budget + core↔archival paging (enforce_budget evicts the least-recently-updated non-pinned fact to archival), directly corresponding. But OneAI adds "ephemeral reinjection" — the core block is reinjected at the assembly layer each turn, rather than paged in/out.
- **LangChain/LangGraph**: `trim_messages` (by token/message, first/last selector) postprocessor, `ContextModule` cross-step context trimming, long-term-memory store. OneAI's 4 strategies ↔ `trim_messages`'s selector; `ContextBudgetManager`'s proportional per-source allocation ↔ ContextModule's budget-allocation idea. OneAI lacks LangChain's "cross-step state channel" fine-grained trimming.
- **Aider**: repo-map reinjected each turn (sorted by file importance), an engineering exemplar of "durable/ephemeral separation". OneAI's `ContextSource` (including domain env sources like GitStatusSource) is reinjected each turn, the same idea at its root; OneAI generalizes it into a declarative, RefreshPolicy-governed, cross-DomainPack composable trait.
- **"Lost in the middle"** (Liu et al. 2024): information in the middle of a long context is easily overlooked. OneAI's `CompressMiddle` strategy (keep head and tail, compress the middle into a summary) and `ImportanceRanked` (keep useful tool_results rather than purely by recency) are direct engineering countermeasures to this.
- **"Just-in-time context"** (industry consensus): not all possibly-relevant information needs to be stuffed into context; recall on demand. OneAI's `[Recalled Context]` recalls top-k per query each turn (rather than bulk-loading everything) + the `memory_search` tool retrieves the raw transcript on demand, exactly this paradigm.

### 10.3 OneAI's lead/par/lag relative to the state of the art

- **Leading**: ① **Durable/ephemeral separation + reinjection anti-compression** — compression resistance does not rely on the compressor to retain, does not accumulate, does not drift, more robust than the naive "summarize then stuff back into history"; ② **Compression-coupled extraction** — compressed-out information is extracted into searchable long-term memory (Mem0/Letta do not natively have this); ③ **Three-layer model-context resolution**, sync makes no network call, fits the sync trait contract; ④ **Declarative DomainPack** — ContextSource/CompressionTemplate/MemoryProfile switch context and compression strategy in one line, more flexible than each framework hardcoding them.
- **Par**: token-budget-driven termination, 4 trimming strategies, prompt-cache ephemeral, sub-agent context isolation, context accounting.
- **Lagging**: ① token counting is **heuristic** (±10%), not a real tokenizer (tiktoken etc.); CJK is already addressed but precision is limited; ② three-factor recall weights/normalization are hardcoded (see sister doc §12.4); ③ no "cross-step state channel" fine-grained trimming (LangGraph style); ④ lacks automatic "context warm-up" — the L2 provider probe needs an explicit warm-up trigger and is not auto-ready before the first inference.

---

## 11. Gaps and improvement directions

Four context-management-level gaps were found during review (ordered by impact; for memory-layer gaps see sister doc §12).

### 11.1 【Medium】Token counting is heuristic, not a real tokenizer

**Fact**: `HeuristicTokenCounter` (`token_counter.rs:475`) estimates via chars/token ratios + per-msg overhead, ±10% for English. `infer_context_window_for_tokenizer` (`token_counter.rs:687`) relies on name patterns for unknown models (`glm-5`→203K etc.).

**Impact**: budget checks and trim trigger points have ±10% drift, larger for CJK/mixed text; may cause "thought it fit but actually overflows" or "premature compression".

**Fix direction**: integrate `tiktoken-rs` (OpenAI) / per-provider token-count APIs (Anthropic `/v1/messages/count_tokens`, OpenAI token endpoint), selecting a real tokenizer per provider; keep `HeuristicTokenCounter` as an offline/no-network fallback. `ONEAI_CONTEXT_WINDOW` is already the user-override channel; an `ONEAI_TOKENIZER=real|heuristic` switch can be added.

### 11.2 【Medium】L2 provider probe is not auto-warmed

**Fact**: `resolve_cached` (:248) only reads the probe cache and does not initiate a probe; the L2 live probe is triggered explicitly and asynchronously by `AppSession::warm_model_context` / CLI `token probe` (`model_context.rs:150-154`).

**Impact**: if warm-up is not run or fails, `context_window_size` falls back to the L3 static value before the first inference, which may be inaccurate for new models not in the builtin library (e.g. just-released models), affecting routing and trim decisions.

**Fix direction**: automatically call `resolve_with_provider` (provider already available) before the first turn in `AppSession.run`, failing open to L3 to avoid depending on an external explicit warm-up.

### 11.3 【Low】Lacks "cross-step state channel" fine-grained trimming

LangGraph's `ContextModule`/state channels can keep/trim specific state keys per step; OneAI's trimming granularity is "message-level" (system/recent turns/tool_result/old turns) and cannot trim "some tool's accumulated state" independently.

**Fix direction**: introduce "budgeted cumulative sources" at the `ContextSource` layer — the source manages its own token budget and trimming rather than being reinjected in full. The current `CoreMemory` already has a token budget + eviction; this can be generalized to other long-lived sources.

### 11.4 【Low】CompressMiddle / ImportanceRanked not wired into the ContextBudgetManager main path

`ContextBudgetManager.compress` (`budget.rs:597`) goes through `ContextCompressorTrait` (memory's LLM summary), while core's 4-strategy `ContextManager` mainly serves SmartRouter routing. The two strategy spaces are not unified — e.g. the AgentLoop main loop cannot declaratively choose `CompressMiddle` instead of LLM summary.

**Fix direction**: let `ContextBudgetManager` accept a `ContextTrimmingStrategy`, declaring the compression strategy (LLM summary / CompressMiddle / ImportanceRanked) per the domain `MemoryProfile`/`CompressionTemplate`, unifying trimming strategies at the DomainPack layer.

---

## 12. Summary: the positioning of OneAI's context management

OneAI's context management **reaches the level of first-tier agent frameworks on engineering closure**, and leads in two dimensions:

- **Leading**: ① Durable/ephemeral separation + reinjection anti-compression — compression resistance does not rely on the compressor, does not accumulate, does not drift, a thorough implementation of the "context engineering" paradigm; ② Compression-coupled extraction — compressed-out information becomes searchable long-term memory, closing off "compression means loss"; ③ Three-layer model-context resolution sync makes no network call + declarative DomainPack lets context/compression strategy switch in one line.
- **Par**: token-budget-driven termination, 4 trimming strategies, prompt-cache ephemeral, sub-agent context isolation + parallel dependency-aware summary injection, per-category context accounting, first-user/PlanState double-safeguard pinning.
- **Lagging**: token counting is heuristic not a real tokenizer (§11.1, should be fixed first); L2 probe not auto-warmed (§11.2); no cross-step state-channel fine-grained trimming (§11.3); compression and trimming strategy spaces not unified (§11.4).

In one sentence: **OneAI gets "context as compression-resistant, budget-driven, losslessly-compressed, declaratively-switchable ephemeral assembly" right, corresponding item-by-item to Anthropic's 2025 "context engineering" methodology** — once the real tokenizer (§11.1) and L2 auto-warm-up (§11.2) are filled in, the four elements of the context window — "fits, trims accurately, compresses without loss, recalls back" — fully realize the design intent.

---

### Appendix: Key file index

| Concern | File |
|---|---|
| Ephemeral assembler | `crates/oneai-agent/src/context_assembler.rs:46` |
| Per-turn assembly/compression pipeline | `crates/oneai-agent/src/agent_loop.rs:1041-1077` |
| Pinned-block injection | `crates/oneai-agent/src/agent_loop.rs:3625` |
| Context accounting | `crates/oneai-agent/src/agent_loop.rs:1195-1200` |
| Budget manager | `crates/oneai-core/src/budget.rs:488` |
| Lossless truncation tier | `crates/oneai-core/src/budget.rs:688` |
| Compression trait (dependency inversion) | `crates/oneai-core/src/budget.rs:31` |
| 4 trimming strategies + ContextManager | `crates/oneai-core/src/context_manager.rs:46` |
| Three-layer model-context resolution | `crates/oneai-core/src/model_context.rs:159` |
| Builtin model library | `crates/oneai-core/src/model_context.rs:61` |
| Heuristic token counting | `crates/oneai-core/src/token_counter.rs:475` |
| ContextFitResult | `crates/oneai-core/src/token_counter.rs:90` |
| Context-accounting types | `crates/oneai-core/src/context_accounting.rs:31` |
| LLM compressor + extraction closed loop | `crates/oneai-memory/src/compression.rs:26` |
| Anti-compression injection source | `crates/oneai-memory/src/core_memory_source.rs:31` |
| RefreshPolicy + ContextSource | `crates/oneai-domain/src/context_source.rs:30` |
| Compression template | `crates/oneai-domain/src/compression_template.rs` |
| Prompt-cache breakpoints | `crates/oneai-provider/src/anthropic.rs:179-262` |
| Sub-agent summary isolation | `crates/oneai-agent/src/sub_agent.rs:8` |
| Parallel dependency-aware scheduling | `crates/oneai-agent/src/parallel_executor.rs` |
| Paradigm/delegate meta-tool | `crates/oneai-agent/src/meta_tool.rs` |
| Sister document | `docs/memory-mechanism_EN.md` |

---

## Dependencies

| Direction | Who | What |
|---|---|---|
| Upstream | `oneai-core` | `ContextBudgetManager`/`ContextManager`/`ContextAccounting`/`ModelContextResolver`/`TokenCounter`/`ContextCompressorTrait` (dependency-inversion trait) |
| Upstream | `oneai-memory` | `ContextCompressor` + `FactExtractor` (compression-coupled extraction) + `CoreMemorySource` (anti-compression injection source) |
| Upstream | `oneai-domain` | `ContextSource` trait + `RefreshPolicy`, `CompressionTemplate` |
| Upstream | `oneai-provider` | Anthropic prompt-cache breakpoints (`anthropic.rs:179-262`) |
| Downstream | `oneai-agent` | `AgentLoop` per-turn assembly pipeline (`context_assembler.rs`), sub-agent context isolation, parallel dependency-aware summary injection |
| Downstream | `oneai-app` | `AppBuilder` wires the default `ModelContextResolver` + 3-layer resolution |
| Cross-cutting | DomainPack layer 5 | `CompressionTemplate` declares compression policy; layer 2 `ContextSource` injects by `RefreshPolicy` |

---

## Further reading

- [memory-mechanism](memory-mechanism_EN.md) — the downstream of compression-coupled extraction: discarded turns are refined into `MemoryFact`s
- [multi-agent-mechanism](multi-agent-mechanism_EN.md) — sub-agent context isolation + parallel-delegation summary injection
- [domain-pack-mechanism](domain-pack-mechanism_EN.md) — layer 2 ContextSource / layer 5 CompressionTemplate
- [provider-mechanism](provider-mechanism_EN.md) — 3-layer model-context resolution (L2 provider API probe) + prompt cache
- [working-state-mechanism](working-state-mechanism_EN.md) — task-level state persistence (a separate path from the context window)
- Source: `crates/oneai-core/src/{budget,context_manager,context_accounting,token_counter,model_context}.rs` + `crates/oneai-agent/src/context_assembler.rs` + `crates/oneai-memory/src/compression.rs`
- [CLAUDE.md — Architecture: Context management](../CLAUDE.md)
