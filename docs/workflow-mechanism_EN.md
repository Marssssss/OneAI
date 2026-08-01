# OneAI Workflow & StateGraph Mechanism

> A declarative DAG + a cyclic StateGraph, closed-looped with AgentLoop: graph nodes can drive paradigm switch / delegate / tool calls.

## Responsibility

Pull "deterministic multi-step flows" out of free-form model behavior into renderable, validatable, executable graphs. DAGs orchestrate parallel steps; StateGraphs express cyclic iterative flows (ReAct loops / conditional routing / interrupt points).

## Two graph kinds

- **WorkflowDag** — declarative DAG for parallel-step orchestration, renderable via `workflow show`.
- **StateGraph** — a cyclic directed graph for iterative flows; **closed-looped with AgentLoop**: graph nodes can emit `GraphDecision::SwitchParadigm` / `Delegate` / `ToolCalls`, executed inline by `AgentLoopGraphActionExecutor`. The frontier supports parallel multi-walkers (all satisfiable out-edges run in parallel, joined naturally).

DomainPack layer 6 embeds domain-predefined workflows and state graphs (e.g. `react` / `plan` / `reflect` / `explore`).

## Key types & files

| Item | Location |
|---|---|
| `WorkflowDag` | `crates/oneai-workflow/src/dag.rs` |
| `StateGraph` + parallel frontier | `crates/oneai-workflow/src/state_graph.rs` |
| compiler / validator | `crates/oneai-workflow/src/compiler.rs`, `validator.rs` |
| DAG executor / StateGraph executor | `crates/oneai-workflow/src/executor.rs`, `state_executor.rs` |
| ASCII rendering | `crates/oneai-workflow/src/render.rs` |

## Related CLI

[`workflow list / show / run`](cli-reference_EN.md#workflow-and-state-graph), [`graph list / show / run`](cli-reference_EN.md#workflow-and-state-graph), in-conversation `/wf *` slash commands.

## Further reading

- [CLAUDE.md — AgentLoop & StateGraph closed loop](../CLAUDE.md)
- How StateGraph drives paradigm switch — see [Multi-agent mechanism](multi-agent-mechanism_EN.md)
