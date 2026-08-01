# OneAI 工作流与状态图机制

> 声明式 DAG + 有环 StateGraph，与 AgentLoop 闭环：图节点能驱动范式切换 / 委托 / 工具调用。

## 职责

把「确定性的多步流程」从模型自由发挥里抽出来，变成可渲染、可校验、可执行的图。DAG 编排并行步骤；StateGraph 表达有环迭代流程（ReAct 循环 / 条件路由 / 中断点）。

## 两类图

- **WorkflowDag** — 声明式 DAG，并行步骤编排，可 `workflow show` 渲染。
- **StateGraph** — 有环有向图，迭代流程；**与 AgentLoop 闭环**：图节点可发 `GraphDecision::SwitchParadigm` / `Delegate` / `ToolCalls`，`AgentLoopGraphActionExecutor` 内联执行。frontier 支持并行多 walker（所有可满足出边并行，自然 join 合并）。

DomainPack 第 6 层内嵌领域预定义工作流与状态图（如 `react` / `plan` / `reflect` / `explore`）。

## 关键类型与文件

| 项 | 位置 |
|---|---|
| `WorkflowDag` | `crates/oneai-workflow/src/dag.rs` |
| `StateGraph` + frontier 并行 | `crates/oneai-workflow/src/state_graph.rs` |
| 编译器 / 校验器 | `crates/oneai-workflow/src/compiler.rs`、`validator.rs` |
| DAG 执行器 / StateGraph 执行器 | `crates/oneai-workflow/src/executor.rs`、`state_executor.rs` |
| ASCII 渲染 | `crates/oneai-workflow/src/render.rs` |

## 相关 CLI

[`workflow list / show / run`](cli-reference.md#工作流与状态图)、[`graph list / show / run`](cli-reference.md#工作流与状态图)、对话内 `/wf *` 斜杠命令。

## 深入阅读

- [CLAUDE.md — AgentLoop 与 StateGraph 闭环](../CLAUDE.md)
- StateGraph 如何驱动范式切换见 [多 Agent 机制](multi-agent-mechanism.md)
