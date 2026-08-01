# OneAI Multi-Agent Mechanism (Whitepaper)

> Version: corresponds to the 1.1.0 line of the codebase. This document is written from a file-by-file review of the source of `crates/oneai-agent`, `oneai-workflow`, `oneai-domain`, `oneai-memory`, and `oneai-core`; mechanisms are annotated with `file:line` for verification. At the end, it is benchmarked against state-of-the-art multi-agent systems (LangGraph / AutoGen / CrewAI / OpenAI Swarm / MetaGPT / SWE-agent / Claude Code subagents / Google A2A / MCP).
>
> Note: At the time of writing this environment could not access the internet for retrieval; the state-of-the-art benchmarking section is based on training knowledge up to early 2025, and checkable paper/project names are marked where possible; exact version numbers are subject to each project's latest release.
>
> Historical note: OneAI's early implementation once provided three orchestration primitives — Team/Swarm/Handoff — which were removed wholesale in 2026-07. They duplicated the main Loop's `delegate` + `switch_paradigm` + deterministic StateGraph capabilities (Handoff was never wired into AgentLoop; Swarm's decomposition/quality assessment was hardcoded heuristics). Orchestration patterns such as aggregation/routing/debate are now expressed via `delegate` + StateGraph; the engine-level GroupChat primitive is retained. This document no longer describes those three.

---

## 0. One-Sentence Summary

OneAI's multi-agent system is an engine of **"Claude-Code-style dynamic Agentic Loop + model-driven delegate/switch_paradigm meta-tools + LangGraph-style loopable StateGraph closed loop + engine-level GroupChat primitive + compression-coupled memory"**: each iteration the model decides whether the next step is a direct answer, a tool call, a sub-agent delegation, or a paradigm switch — it is not a fixed pipeline. Delegation supports multiple delegations in one round + dependency-aware Kahn wave parallel scheduling; paradigm switching can inline-upgrade the system prompt and tool set, and can mount a DomainPack-predefined StateGraph graph flow; multi-agent collaboration happens inside the main Loop via layered sub-agent decomposition + the GroupChat primitive for scenario-based multi-role conversation, while aggregation/routing/debate patterns are expressed via `delegate` + deterministic StateGraph. The entire orchestration behavior is declared by DomainPack; one line, `AppBuilder::domain_pack(...)`, switches it.

---

## 1. Architecture Overview: Layering and Execution Model

```
                         ┌──────────────────────────────────────────────┐
                         │            AppBuilder (oneai-app)             │
                         │  All subsystems optional/pluggable → App →   │
                         │  AppSession                                   │
                         └──────────────────────┬───────────────────────┘
                                                │ assemble
                         ┌──────────────────────▼───────────────────────┐
                         │              AgentLoop (oneai-agent)         │
                         │  dynamic loop: infer → parse_decision →     │
                         │  dispatch                                     │
                         │  decisions: DirectAnswer / ToolCalls /      │
                         │        Delegate / SwitchParadigm              │
                         └──┬───────────┬───────────┬───────────┬───────┘
                            │           │           │           │
            ┌───────────────▼─┐ ┌───────▼────────┐ │  ┌────────▼──────────┐
            │ ContextAssembler│ │ ToolExecutor   │ │  │ StateGraphExecutor │
            │ + Pinned inject │ │ + domain perm/ │ │  │ (AgentLoopGraph    │
            │ + anti-compress │ │   approval     │ │  │  ActionExecutor)   │
            │   re-inject     │ │ + SmartRouter  │ │  │                    │
            └─────────────────┘ └────────────────┘ │  └────────────────────┘
                                ┌─────────────────▼─────────────┐
                                │  Delegation / GroupChat layer    │
                                │  · SubAgentWrapper(+worktree)   │
                                │  · spawn_sub_agents_batch       │
                                │    (Kahn wave DAG scheduling)   │
                                │  · GroupChatSession             │
                                │  · AsyncTaskRunner (background) │
                                └───────────────────────────────┘
                                                │
                                ┌───────────────▼───────────────┐
                                │  Long-horizon support           │
                                │  · MemoryManager (recall/       │
                                │    compression extraction)      │
                                │  · ContextCompressor+FactExt    │
                                │  · PlanState (live task list)   │
                                │  · ErrorRecovery / Retry        │
                                │  · ProviderPool / SmartRouter   │
                                └───────────────────────────────┘
```

**Key crate division of labor:**

| crate | role | key files |
|---|---|---|
| `oneai-core` | foundation types and traits: `ContentBlock`/`Conversation`, `TokenBudget`/`ContextBudgetManager`, `RecallStrategy` | `budget.rs`, `traits.rs`, `types.rs` |
| `oneai-agent` | multi-agent engine body: dynamic Loop, paradigms, sub-agents, parallelism, GroupChat | `agent_loop.rs:4741`, `sub_agent.rs:870`, `parallel_executor.rs`, `group_chat.rs:874`, `meta_tool.rs` |
| `oneai-workflow` | StateGraph engine: loopable graph, conditional edges, interrupt points, `GraphActionExecutor` bridge | `state_graph.rs:512`, `state_executor.rs:1093`, `dag.rs`, `executor.rs` |
| `oneai-domain` | DomainPack 7-layer declarative domain config (incl. paradigm strategies, StateGraph, MemoryProfile) | `domain_pack.rs:50`, `paradigm_strategy.rs`, `memory_profile.rs` |
| `oneai-memory` | long-term memory: three layers, compression-coupled extraction, three-factor recall | `manager.rs:655`, `compression.rs:492`, `fact_extraction.rs` |

---

## 2. Dynamic Agentic Loop: Core Execution Model

File: `crates/oneai-agent/src/agent_loop.rs`

### 2.1 What one iteration does

OneAI's `AgentLoop` is not a fixed `Plan → Parallel → ReAct → Reflect` pipeline, but a **model-driven dynamic loop** (inspired by Claude Code's Agentic Loop architecture, see the module header comment `agent_loop.rs:1-15`). Each iteration:

1. **Refresh and compression decision** (`run_loop` @ `agent_loop.rs:1041-1077`): refresh the DomainPack's `ContextSource`, assemble "persistent log + temporary context sources + fixed blocks (TaskAnchor/PlanProgress/skill menu)". If the request would overflow the token budget, compress the **persistent log** (not the temporary assembly), then re-inject the fixed blocks on top of the compressed persistent log.
2. **Build inference request** (`agent_loop.rs:1092-1119`): filter tool definitions by the currently active paradigm (`build_tool_definitions_for_paradigm`), inject constrained-output configuration, thinking budget, prompt-cache policy.
3. **PreInfer gate** (`agent_loop.rs:1121-1181`): first run in-process hooks (audit/log only), then `InteractionGate::PreInfer` — the application layer can inject system messages, replace the request, require a feedback-based retry, or skip this round. This supersedes the old interactive `LifecycleHook` path.
4. **Inference** (`agent_loop.rs:1222-1234`): streaming or non-streaming; non-streaming wraps `tokio::select!` + `CancellationToken` so an interrupt immediately aborts an in-flight request.
5. **PostInfer gate + parse decision** (`parse_decision` @ `agent_loop.rs:2367-2468`): parse the model output into an `AgentDecision`.
6. **Dispatch decision** (from `agent_loop.rs:1434`): take different branches by decision type.

### 2.2 The four decision states: the `AgentDecision` enum

`agent_loop.rs:143-162`:

```rust
pub enum AgentDecision {
    DirectAnswer { text: String },        // model gives final answer → loop ends
    ToolCalls { calls: Vec<ToolCallRequest> },   // call one or more tools → execute and feed back
    Delegate { tasks: Vec<DelegateTask> },      // delegate a batch of subtasks to sub-agents
    SwitchParadigm { paradigm: ParadigmKind },  // switch paradigm (enter a fixed graph flow)
}
```

- **DirectAnswer**: when there are no tool calls and no delegations, the text is concatenated into the final answer, `mark_complete()` is called, and the loop ends.
- **ToolCalls**: `execute_tool_calls` (`agent_loop.rs:2470`) executes all calls in parallel (`futures::future::join_all`); each call first goes through `SmartToolRouter` (shell redirection, see `route_shell_to_specialized` @ `agent_loop.rs:2643`), then domain permission-profile resolution, then the approval gate, and finally feeds back the `tool_result`.
- **Delegate**: calls `spawn_sub_agents_batch` (see §4.2).
- **SwitchParadigm**: calls `apply_paradigm_switch_with_graph` (see §3.3).

### 2.3 Termination is governed by TokenBudget, not max_iterations

Loop condition (`agent_loop.rs:968`):

```rust
while !state.is_complete() && state.iterations < self.config.hard_max_iterations.unwrap_or(usize::MAX) {
```

The primary termination signal is `state.is_complete()` (the model produced a `DirectAnswer` or an external interrupt). `hard_max_iterations` is only a **safety backstop** (against runaway loops), defaulting to `usize::MAX`. The true runtime constraint is `TokenBudget` — each round `ContextBudgetManager::needs_compression` decides whether to compress, pinning long conversations inside the context window (see §6). This is fundamentally different from the industry practice of "truncating at a fixed `max_steps=50`": OneAI lets "whether the task is done" be judged by the model, and lets "whether the context overflows" be governed by the budget — the two are orthogonal.

### 2.4 Interrupt/Resume (human-in-the-loop)

- **External interrupt**: `request_interrupt` (`agent_loop.rs:2272`) sets an atomic flag; the next iteration boundary catches it, saves a checkpoint, and returns a partial result to pause (`agent_loop.rs:969-994`).
- **Resume**: `resume_from_interrupt` (`agent_loop.rs:2307`) continues based on human feedback. `CancellationToken` lets inference be aborted immediately even while in flight.
- **Rate limiting**: only after `MAX_CONSECUTIVE_RATE_LIMIT_ERRORS=10` consecutive rate-limit errors does it terminate; otherwise it waits 5s and retries (`agent_loop.rs:1242-1284`), so long-horizon tasks are not killed by transient throttling.

---

## 3. The Four Paradigms and Model-Driven Switching

### 3.1 The four paradigms `ParadigmKind` (`agent_loop.rs:166-173`)

`Plan / ReAct / Reflect / Explore` — each paradigm is a tuple `(system_prompt, tool_filter, decision_hint)` (`ParadigmConfig` @ `agent_loop.rs:195-305`), inspired by Aider's Architect/Editor dual-model mode; OneAI extends it to 4:

| Paradigm | Tool set | Responsibility |
|---|---|---|
| Plan | read/grep/glob/list/env (**no execution tools**) | only decompose the task into ordered steps |
| ReAct | full tool set (incl. edit/shell/web_fetch) | reason-act-observe-iterate (default execution mode) |
| Reflect | read-only tools | review the current state, find errors and improvements |
| Explore | read/grep/glob/list/web_fetch | breadth-first search, change nothing |

### 3.2 Semantic switch: `apply_paradigm_switch` (`agent_loop.rs:2980-3005`)

Switching paradigm is not just returning "switched" — it is a **real, observable behavior change** (resolving the "paradigm switch is semantically hollow" gap):
1. Remove the old system message and inject the paradigm-specific system prompt;
2. Inject a `decision_hint` telling the model what decisions to make under this paradigm;
3. Store the `ParadigmConfig` in `LoopState`; subsequent `build_tool_definitions_for_paradigm` filters tools accordingly.

### 3.3 Graph-flow switch: `apply_paradigm_switch_with_graph` (`agent_loop.rs:3017-3140`)

If the DomainPack has predefined a StateGraph for that paradigm (keys `react-loop` / `plan-workflow` / `reflect-workflow` / `explore-workflow`), it first does the semantic switch, then uses `StateGraphExecutor` to execute that graph flow, and injects the result back into the main Loop conversation. On failure it falls back to a pure semantic switch. This makes "paradigm = fixed graph flow" real: ReAct is not an implicit while loop, but an explicit loopable, interruptible, inspectable graph (see §5).

### 3.4 How the model triggers a switch: the `switch_paradigm` meta-tool

`meta_tool.rs:88-108`: the `ToolDefinition` of `switch_paradigm` is injected into the inference request; the model can call it in any round to switch paradigm. That call is intercepted at the `ContentBlock` layer of `parse_decision` into `AgentDecision::SwitchParadigm` (`agent_loop.rs:2415-2426`), **never entering the ToolExecutor** — `is_meta_tool` (`meta_tool.rs:33`) serves as a defensive backstop.

---

## 4. Sub-Agent Delegation and Parallel Scheduling

### 4.1 SubAgent: layered decomposition, only summary fed back

`sub_agent.rs:175-200`: `SubAgentWrapper` wraps an `AgentLoop` into a `SubAgent` — independent context window, scoped tool set, dedicated system prompt, token budget. Core principle (`sub_agent.rs:8-10`): **the sub-agent feeds back only a `SubAgentSummary` (summary + key_findings + token usage), not the full conversation**. This directly corresponds to Claude Code's subagent mode (the sub-agent's final text is the return value), keeping the main context window clean so that deep decomposition does not pollute the main context.

`SubAgentKind` (`sub_agent.rs:39-106`): `Plan / Explore / Code / Review / Custom`, each with a default system prompt and tool set. **The Code kind is isolated via a git worktree** (`worktree_config`, see §4.3); read-only kinds do not use it.

### 4.2 Multiple delegations per round + Kahn wave DAG scheduling

The `delegate` schema in `meta_tool.rs:46-87` supports `id` + `depends_on`; the model can fan out multiple `delegate` calls in the **same round** (see the `DelegateTask` comment at `agent_loop.rs:114-140`). `parse_decision` collects all `delegate` calls of the same round into an `AgentDecision::Delegate { tasks }` batch; any `depends_on` referencing an unknown id is dropped (`agent_loop.rs:2436-2462`).

`spawn_sub_agents_batch` (`agent_loop.rs:2852-2963`) implements **Kahn topological wave scheduling**:

```
while there are pending tasks:
    wave = all tasks whose depends_on are already completed
    if wave is empty → remaining tasks form a cycle → error (cycle detection)
    spawn this entire wave in parallel (JoinSet)
    wait for the whole wave to finish → write each task's summary into completed
    in the next wave, a task's depends_on this round gets the upstream summary
      automatically prepended to its task text
```

- **Independent tasks run in parallel**, **dependent tasks run serially**, and their task descriptions get the upstream `summary` + `key_findings` automatically prepended (`agent_loop.rs:2897-2918`) — the model need not restate upstream results.
- **Cycle detection**: if a wave cannot advance, the remaining tasks form a cycle and an error is raised (`agent_loop.rs:2878-2885`).
- **Failure semantics**: a single sub-agent failure propagates immediately rather than being silently dropped; downstream tasks depending on it will trigger the cycle guard in the next wave (`agent_loop.rs:2939-2946`).
- Results are fed back in input order (`agent_loop.rs:2957-2962`), guaranteeing determinism.

This is a key upgrade over "serial delegation one at a time": it hands DAG orchestration authority to the model, with the runtime auto-parallelizing in topological order.

### 4.3 Worktree isolation: parallel writes don't conflict

`worktree_isolation.rs:1-29`: multiple Code sub-agents modifying the same file in parallel would conflict. `WorktreeIsolation` uses `git worktree add -b <branch>` to give each sub-agent an isolated copy (sharing `.git`, lightweight; each on its own branch). When done, `merge_back` merges it back into the main branch; on conflict it keeps the worktree for manual resolution, and if there are no changes it cleans up immediately. When unavailable it falls back to directory-level isolation. This corresponds to Claude Code's agent isolation approach and resolves the P1#13 parallel-write conflict.

### 4.4 ParallelExecutor + ScopeState (MVI/Redux-style state isolation)

`parallel_executor.rs:1-11`: another parallel path (mostly used for non-coupled steps of Plan decomposition). Each sub-agent clones **read-only global memory** into an isolated `ScopeState`, makes local changes inside a private sandbox, and produces a `Reduction`; once all are done, a `StateReducer` merges them back into the `GlobalState` (`parallel_executor.rs:77-144`). This is the Redux/MVI unidirectional data-flow pattern, complementary to the "summary fed back" approach of §4.2: one keeps the context clean, the other keeps state consistent.

### 4.5 AsyncTaskRunner: background non-blocking delegation

`async_task_runner.rs:1-25`: the main agent can delegate tasks to a background worker, continue its own work, and query the result later. The state machine is `Pending → Running → Completed/Failed/Cancelled`, budget-aware, with progress pushed to the TUI via `AgentLoopObserver`. This corresponds to Claude Code's background subagents.

---

## 5. StateGraph ↔ AgentLoop Closed Loop (P2-2 Bridge)

Files `crates/oneai-workflow/src/state_graph.rs`, `state_executor.rs`.

### 5.1 Loopable graph

`StateGraph` (`state_graph.rs:1-21`) is inspired by LangGraph's core innovation: it **supports cyclic edges**, making the ReAct loop (Think→Act→Observe→Think) an explicit graph cycle rather than an implicit while — the state is visible, inspectable, and interruptible. This distinguishes it from `WorkflowDag` (a pure DAG for parallel-step orchestration).

`NodeAction` (`state_graph.rs:43-130`) has 6 node actions: `LlmInfer / ToolCall / Delegate / HumanApproval / ConditionCheck / SwitchParadigm`. `EdgeCondition` has 9 conditional routes (incl. `ParadigmEquals`, `IterationExceeds`).

### 5.2 GraphActionExecutor bridge: graph flow reuses the Loop's full infrastructure

`state_executor.rs:79-99`: the `GraphActionExecutor` trait lets StateGraph execution reuse the entire AgentLoop pipeline, rather than standing up a separate direct-to-provider path:
- **LlmInfer node**: gets paradigm-filtered tool definitions, domain decorators, PreInfer/PostInfer hooks, context assembly, and the OutputParser (`AgentLoopGraphActionExecutor::execute_llm_infer` at `agent_loop.rs:3821-3902`).
- **ToolCall node**: goes through the full permission/approval pipeline (domain PermissionProfile → approval gate) (`agent_loop.rs:3904-3968`).

This means: whether it is the top-level `run_with_state_graph` path of the main Loop or the graph flow triggered by an inline `apply_paradigm_switch_with_graph`, **both share the same `AgentLoopGraphActionExecutor`** (`agent_loop.rs:3769-3807`; the bridge-structure comment explicitly notes the two paths share it to eliminate consistency gaps).

### 5.3 Checkpoint and time travel

`StateGraphExecutor` supports `interrupt: true` nodes (HumanApproval) to pause, with `max_iterations` as a backstop against infinite loops. The Studio Web UI's "Checkpoint time travel" is built on this inspectability.

---

## 6. Long-Horizon Task Support

The core difficulties of long-horizon tasks are: the context overflows, the goal is forgotten, subtasks are lost, and errors accumulate. OneAI addresses these systematically with the following mechanisms.

### 6.1 Persistent/temporary separation + fixed-block anti-compression re-injection

The **temporary re-injection model** of `context_assembler.rs:72-101`:

- `state.conversation` is the **persistent log** (system prompt, user task, assistant replies, tool results) — appended in a loop, persisted, and compressible.
- The ContextAssembler produces a **fresh temporary assembly** each round (persistent-log clone + all ContextSource caches + fixed blocks); the inference request uses it, and it is **never written back to the persistent log**.
- Therefore fixed state (env awareness, core memory, TaskAnchor, PlanProgress) survives compression by **re-injection** rather than "relying on the compressor to retain it" — the compressor only sees the temporary assembly, and whatever it summarizes away is automatically restored next round (`context_assembler.rs:77-90`).

The three fixed blocks (`context_assembler.rs:155-185`):
- **`[Task Anchor]`**: the original task + distilled intent, mirrored to `metadata["task_anchor"]` (verbatim-preserved by every compressor).
- **`[Plan & Progress]`**: the ✅/🔄/⏳ rendering of the live task list, mirrored to `metadata["plan_state"]`.
- **Runtime block**: today's date + guidance that time-sensitive questions should prefer `web_search`/`web_fetch`, appended to the end of the system prompt (`runtime_context_block` @ `context_assembler.rs:198-212`), because the system prompt survives compression better than temporary system messages.

Fixed historical bug (comment at `agent_loop.rs:1056-1058`): in non-compression rounds `assembled` was once discarded and the request used the bare persistent log, so ContextSource injection never reached the model — now a real request size is assembled every round before judging overflow.

### 6.2 Compression-coupled fact extraction (the "compression = loss" closed loop)

`with_fact_extraction` at `compression.rs:80-90`: on each compression, the `discarded_messages` that got summarized away are passed through a `FactExtractor` (schema-based) to extract facts, which are conflict-updated into `archive` in Mem0 fashion. The rounds discarded by compression are no longer "lost once dropped" but are archived as recallable long-term facts.

### 6.3 Three-factor recall injected every round

`manager.rs:9-13, 347-580`: `MemoryManager::recall_facts(query, top_k)` uses **relevance + recency + importance** (Generative-Agents-style) three factors to recall from archival, injected every round via `CoreMemorySource`. With an `EmbeddingService` it goes through semantic relevance; otherwise it degrades to keyword matching. At the end of the session, `reflect()` distills the entire conversation into episodic facts. See `docs/memory-mechanism_EN.md` for details.

> Note: the defect where stored facts' embedding was always None and semantic recall degraded to keyword matching has been fixed in 1.1.0 (archive_facts uniformly embeds the embedding). See `docs/memory-mechanism_EN.md`.

### 6.4 PlanState: live task list prevents forgetting

`plan_state.rs:1-9`: unlike the one-shot `PlanAgent` that produces a plan once, `PlanState` is a live list that the model continuously mutates during execution via the `task_create/task_update/task_list` control tools, stored in `LoopState` (agent-side) and mirrored to `metadata["plan_state"]` for anti-compression + anti-reload. Each round the `[Plan & Progress]` block re-injects ✅/🔄/⏳, so the model knows the progress without re-reading the compressed-away rounds.

### 6.5 Error recovery + retry + fault tolerance

- `error_recovery.rs`: `RecoveryManager` selects a recovery strategy based on the failed tool result, `select_recovery_strategy` (`agent_loop.rs:3664`).
- Provider-level 429 retry (`ProviderRetryConfig` + `send_with_retry`), with AgentLoop-level `MAX_CONSECUTIVE_RATE_LIMIT_ERRORS` backstop.
- `ProviderPool` failover chain + `SmartRouter` multi-factor routing + circuit breaker (`circuit_breaker` @ `agent_loop.rs:1018-1029`) — on provider failure it does not kill the long-horizon task but degrades/failovers.

### 6.6 The long-horizon closed loop in one sentence

One long-horizon iteration = refresh ContextSource → assemble (persistent log + fixed blocks + recalled facts) → if overflow, compress the persistent log (the compressed-away rounds are extracted into facts and archived) → inject PlanProgress/TaskAnchor (the model forgets neither goal nor progress) → PreInfer gate → inference → parse decision → tool/delegate/switch → PostInfer → feed back → next round. Any link that fails has retry/degradation/circuit-breaker backstops, and the goal and progress survive compression doubly via fixed blocks and metadata. This lets OneAI run arbitrarily long tasks within a fixed context window.

---

## 7. GroupChat primitive (scenario-based multi-role conversation)

Besides sub-agent delegation within the main Loop, OneAI provides an engine-level orchestration primitive for scenario-based multi-role conversation; fan-out/routing/debate topologies are expressed via `delegate` + deterministic StateGraph (the historical Team/Swarm/Handoff three primitives were removed, see the historical note at the top).

### 7.1 GroupChatSession (shared-transcript conversation, engine primitive)

`group_chat.rs:1-29`: unlike delegation's fan-out-merge (aggregating multiple results), GroupChat is a **conversation**: N persona agents take turns speaking within **one shared Conversation**, with a human in the loop. This corresponds to AutoGen GroupChat / Coze multi-agent conversation patterns, sunk to the engine layer so every native port (macOS/Windows/Android/iOS) gets it for free, without re-implementing orchestration at the UI layer.

- Each member is a slimmed `AgentLoop` (persona system prompt, shared provider/tools/parser).
- One shared `Conversation` holds the conversation; each member runs on a **derived transcript** (shared minus system messages), and its own persona system prompt is freshly injected by the loop; only that member's final answer is fed back, marked with `metadata["speaker"]=<id>`.
- **Turn policy** (`TurnPolicy` @ `group_chat.rs:100-116`): `Scripted` (fixed order, e.g. an interview `[coach, interviewer]`) / `RoundRobin` (member order) / `Moderator` (a chair-member picks the next speaker, and may hand back to `"user"`).
- **ReviewLoopConfig** (`group_chat.rs:126-134`): a writing-workshop-style review-revise loop — writer drafts → editor reviews → writer revises → … until the editor emits an `approve_marker` or the `max_rounds` limit is reached.

### 7.2 Delegation vs GroupChat comparison

| Primitive | Topology | Control | Main context | Typical scenario |
|---|---|---|---|---|
| SubAgent delegation | layered (parent→child) | model-driven meta-tool | parent stays clean (only receives summary) | task decomposition, isolated execution |
| GroupChat | shared-conversation turns | turn policy / moderator | shared transcript | multi-role conversation, human-in-loop |

---

## 8. Declarative Configuration of Orchestration Behavior: DomainPack

`domain_pack.rs:50-89`: DomainPack is the central unit of declarative domain-knowledge configuration, 7 layers:

1. **Tools + ToolDecorators**: domain tool set + base tool description/permission overrides
2. **ContextSources**: env awareness with refresh policy (git status, file tree, …)
3. **PermissionProfile**: domain permission tiers
4. **ParadigmStrategies**: task-pattern → paradigm-sequence / sub-agent-config mapping
5. **CompressionTemplate**: compression retention priority
6. **Workflows + StateGraphs**: domain-predefined workflows and loopable graphs
7. **MemoryProfile**: memory policy (RecallStrategy, core memory budget, fact schema)

`CodingPack` is the built-in reference implementation; `ResearchPack` is for the research domain. Multiple DomainPacks can be merged (`merge.rs`): permissions take the strictest, ContextSources merge by priority. One line, `AppBuilder::domain_pack(...)`, switches the entire orchestration behavior. `#[non_exhaustive]` guards public enums to honor the v0.2.0 stability commitment.

**Key significance**: orchestration paradigms (when to switch to Plan, when to delegate, which graph flow to use, what compression keeps, how recall is computed) are not hardcoded in agent_loop but declared in DomainPack — one engine, swap the pack to swap the "domain persona".

---

## 9. Benchmarking Against the State of the Art

> The benchmarking below is based on training knowledge (up to early 2025); its purpose is to locate OneAI's design coordinates, not a precise per-version per-feature comparison.

### 9.1 Overview comparison table

| Dimension | OneAI | State-of-the-art reference | Assessment |
|---|---|---|---|
| **Execution model** | dynamic Agentic Loop, model decides 4 states each round | Claude Code Agentic Loop, OpenAI "Building Effective Agents" | same origin of thought; OneAI makes the decision an explicit, observable enum |
| **Loop structure** | loopable StateGraph (explicit graph cycle) | LangGraph cyclic graphs | OneAI is inspired by LangGraph (`state_graph.rs:7-9`), and forms a two-way closed loop with the Loop |
| **Delegation/sub-agent** | meta-tool `delegate`, only feeds back summary | Claude Code subagents, Devin subtasks | consistent with the Claude Code pattern, emphasizing a clean main context |
| **Parallel delegation** | multiple delegations per round + Kahn wave DAG scheduling + cycle detection | LLMCompiler (parallel function calling), LangGraph parallel branches | OneAI hands DAG orchestration to the model; topological auto-parallelization |
| **Isolation** | git worktree + ScopeState (MVI/Redux) | Claude Code worktree isolation | same use of git worktree, with additional state isolation |
| **Multi-agent orchestration** | `delegate` Kahn-wave parallel + GroupChat primitive | AutoGen GroupChat, CrewAI roles, OpenAI Swarm handoff, MetaGPT SOP | OneAI converges aggregation/routing/debate into `delegate` + StateGraph, sinks the conversation topology to an engine GroupChat primitive shared by native ports |
| **Paradigm switching** | 4 paradigms + inline upgrade of prompt/tool set + graph-flow mounting | Aider Architect/Editor, Reflexion, Plan-and-Solve | OneAI extends to 4 paradigms and links them with StateGraph |
| **Long-horizon context** | persistent/temporary separation + fixed-block re-injection against compression | LangGraph state channels, Letta memory blocks | OneAI's "re-inject rather than rely on the compressor" approach is distinctive |
| **Memory** | Letta three layers + Mem0 conflict update + three-factor recall + compression-coupled extraction | Letta, Mem0, Generative Agents, Zep-Graphiti | fuses several; the "compression = loss" closed loop is a highlight (see the memory whitepaper) |
| **Protocol interop** | A2A SDK (P2-5) + MCP server ecosystem (P3-6) | Google A2A Protocol, Anthropic MCP | OneAI implements both A2A client/server and consumes MCP |
| **Human-in-the-loop** | InteractionGate 5 decision points + interrupt/resume + Checkpoint time travel | LangGraph interrupt, AutoGen human-in-loop | OneAI converges on a unified 5-decision-point gate; Studio provides time travel |
| **Declarative domain** | DomainPack 7 layers, mergeable | CrewAI role/goal, AgentScope config | OneAI goes further: orchestration, memory, compression, and graph flow are all declarative |

### 9.2 vs AutoGen / LangGraph

**vs LangGraph**: LangGraph's core innovation is "loopable stateful graph + channel-style state". OneAI's `StateGraph` directly absorbs this idea (`state_graph.rs:7-9` explicitly notes inspiration from LangGraph), and goes further — the `GraphActionExecutor` bridge lets graph-flow execution reuse the entire AgentLoop pipeline (hooks/permissions/tool assembly/parser), so the graph and the loop are not two systems but two sides of the same coin: the main loop can `switch_paradigm` into a graph flow, and a graph-flow `Delegate` node returns to the sub-agent factory via `DelegateFactory`. LangGraph's channel state corresponds to OneAI's `GraphState` + `metadata` anti-compression persistence.

**vs AutoGen**: AutoGen v0.4's actor-based multi-agent conversation and GroupChat are its signature. OneAI's `GroupChatSession` (`group_chat.rs:1-29`) explicitly benchmarks against AutoGen GroupChat / Coze multi-agent conversation, but sinks it to an engine primitive — shared transcript + speaker marking + three turn policies + a review loop — so that native ports (rather than Python scripts) can directly drive multi-role conversation. OneAI additionally has the "model-driven parallel delegation DAG" layer that AutoGen (which leans toward sequential conversation) lacks.

### 9.3 vs Claude Code / Devin

OneAI explicitly benchmarks against Claude Code in its comments multiple times: the dynamic Agentic Loop (`agent_loop.rs:1-15`), the sub-agent only feeds back a summary (`sub_agent.rs:8-10, 116-118`), worktree isolation (`worktree_isolation.rs:24`), the background AsyncTaskRunner (`async_task_runner.rs:1-25`). Difference: OneAI is implemented in Rust and declarativizes orchestration behavior into DomainPack, so the same engine can be deployed across domains (coding/research/IoT) and across ends (desktop/mobile). Compared to Devin's subtask decomposition, OneAI's "multiple delegations per round + dependency-aware parallelism" is a parallelization upgrade over serial decomposition.

### 9.4 vs MetaGPT / SWE-agent / CrewAI

- **MetaGPT**: encodes multi-agent collaboration via SOPs (standard operating procedures). OneAI's DomainPack layer 6 Workflows+StateGraph is the equivalent — declaring the domain SOP as a loopable graph. CodingPack is "the SOP of the coding domain".
- **SWE-agent**: its Agent-Computer Interface (constraining raw shell to dedicated commands) corresponds to OneAI's `SmartToolRouter` (`route_shell_to_specialized` @ `agent_loop.rs:2643-2642`), redirecting `shell cat` to `read_file` and `ls` to `list_directory` — even if the model (GLM/Qwen) ignores tool-preference rules, the runtime still routes correctly. OneAI also has the SWE-bench three-axis (capability × cost × efficiency) evaluation framework (see the memory `swe-bench-eval-three-axis.md`).
- **CrewAI**: role-based multi-agent. OneAI's GroupChat (conversational multi-role + role roster + turn policy + background-field visibility) covers its role-orchestration scenarios; pipeline/routing topologies are expressed via `delegate` + StateGraph.

### 9.5 Protocol layer: A2A and MCP

OneAI implements both the **Google A2A protocol** (`oneai-a2a`: P2-5 client SDK + P4-1 server host, `A2AClient`/`A2AServerHost`/`A2ARouter`/`TaskStore`, DomainPack→AgentCard auto-generation) and connects to **Anthropic MCP** (`oneai-mcp`: McpServerHost + McpPluginRegistry + AppBuilder integration + CLI). This lets a OneAI agent both be called as an A2A service by other agents and consume MCP tools/data sources — cross-framework interop without an adapter layer.

### 9.6 OneAI's relative differentiation

Synthesizing the benchmarking, OneAI has independent designs versus the state of the art in the following:

1. **Explicit decision enum**: converges the model's per-round output into 4 `AgentDecision` states that are observable (observer 14 callbacks + OTEL trace spans), rather than a black-box while loop.
2. **Inline delegation DAG**: the model can express multiple delegations + dependencies in a single round, with Kahn-wave auto-parallelization at runtime — sinking "parallel orchestration" from framework code to model capability.
3. **Two-way closed loop of graph flow and Loop**: the `GraphActionExecutor` bridge makes StateGraph not a side system; a paradigm switch enters a graph flow, and graph-flow nodes call back into the loop infrastructure.
4. **Fixed-block anti-compression re-injection**: persistent/temporary separation + TaskAnchor/PlanProgress survive compression by re-injection rather than relying on the compressor — long-horizon goals are not lost.
5. **Declarativized orchestration**: orchestration paradigms, memory, compression, and graph flow are all declared in DomainPack's 7 layers; one line switches domains and multiple domains can be merged.
6. **Orchestration convergence**: aggregation/routing/debate converge into `delegate` + StateGraph, and scenario-based conversation sinks to an engine GroupChat primitive shared natively across ends, not Python-script glue.

---

## 10. Known Limitations and TODOs

- **(Fixed, 1.1.0) semantic recall**: the defect where stored facts' embedding was always None and three-factor recall degraded to keyword matching has been fixed in 1.1.0 — archive_facts uniformly embeds the embedding, and the semantic path is connected. See `docs/memory-mechanism_EN.md`.
- **Graph-flow bridge not fully wired**: the `parser`/`hook_registry`/`recovery_manager` fields of `AgentLoopGraphActionExecutor` have been cloned but are not yet read inside the `GraphActionExecutor` impl (comment at `agent_loop.rs:3790-3796` marks this as follow-up), i.e. the graph-flow path's OutputParser decision parsing, PreInfer/PostInfer triggering, and tool error recovery are not yet fully consistent with the main loop.
- **Only 4 paradigms**: `ParadigmKind` is `#[non_exhaustive]`, extensible, but currently ships only Plan/ReAct/Reflect/Explore.

---

## Appendix: Key File Index

| Mechanism | File:line |
|---|---|
| Dynamic Loop main loop | `crates/oneai-agent/src/agent_loop.rs:945` (`run_loop`) |
| Decision parsing | `agent_loop.rs:2367` (`parse_decision`) |
| Paradigm config/defaults | `agent_loop.rs:195-305` (`ParadigmConfig`) |
| Semantic paradigm switch | `agent_loop.rs:2980` (`apply_paradigm_switch`) |
| Graph-flow paradigm switch | `agent_loop.rs:3017` (`apply_paradigm_switch_with_graph`) |
| Parallel delegation scheduling | `agent_loop.rs:2852` (`spawn_sub_agents_batch`) |
| Tool execution + domain permissions | `agent_loop.rs:2470` (`execute_tool_calls`) |
| SmartToolRouter | `agent_loop.rs:2643` (`route_shell_to_specialized`) |
| Graph-flow bridge | `agent_loop.rs:3769-3810` (`AgentLoopGraphActionExecutor`) |
| Meta-tool definitions | `crates/oneai-agent/src/meta_tool.rs:45` |
| Sub-agent wrapper | `crates/oneai-agent/src/sub_agent.rs:175` |
| Worktree isolation | `crates/oneai-agent/src/worktree_isolation.rs:1` |
| Parallel executor | `crates/oneai-agent/src/parallel_executor.rs:77` |
| Background tasks | `crates/oneai-agent/src/async_task_runner.rs` |
| GroupChat | `crates/oneai-agent/src/group_chat.rs:1` |
| StateGraph | `crates/oneai-workflow/src/state_graph.rs:1` |
| StateGraph executor | `crates/oneai-workflow/src/state_executor.rs:1` |
| Context assembly | `crates/oneai-agent/src/context_assembler.rs:46` |
| Memory manager | `crates/oneai-memory/src/manager.rs:58` |
| Compression-coupled extraction | `crates/oneai-memory/src/compression.rs:80` |
| PlanState | `crates/oneai-agent/src/plan_state.rs:17` |
| DomainPack | `crates/oneai-domain/src/domain_pack.rs:50` |

*This document is kept in sync with the `0.2.0`/1.0.0 code line. When mechanisms change, update the file:line index accordingly.*
