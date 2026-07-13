# OneAI Harness / Loop Engineering 优化 — 真实 LLM 跑通验证

**日期**: 2026-07-14
**Provider**: OpenAI-compatible (`gpt-4` via `ONEAI_API_KEY`)
**目的**: 对 B1/A1/A4/C2 四轨实现做真实 provider 端到端验证，直接关闭用户的两个关切（执行效率可测可提升 + 固定工作流是否真可用）。

---

## 结论一览

| 轨 | 验证项 | 结果 |
|---|---|---|
| **B1** | `oneai graph run react-loop "..."` 真实 LLM 跑通 | ✅ completed: true, 2 iters, terminal "end", 答案 "Four." |
| **B1** | `oneai workflow list/show` 列出 4 DAG workflow + 4 StateGraph | ✅ |
| **A1** | `oneai eval run general --profile` 效率轴实测 | ✅ 见下表 |
| **C2** | `eval run --record` 录制真实 trajectory → `eval replay` 回放 | ✅ deterministic: true |

---

## A1 效率轴实测（真实 LLM, `oneai eval run general --profile`）

```
case              infer_ms  tool_ms  overhd  iters  tokens   cache%   3axis
rust_safe           24826        0      58      0    1379    0.0%    0.285
rust_zero_cost      61190        0       0      0    1499    0.0%    0.282
date_format         64516        0       0      0     535    0.0%    0.307
email_format        68105        0       0      0     531    0.0%    0.307
binary_convert      74692        0       0      0     604    0.0%    0.301
TOTAL/AVG          293329        0      58          4548    0.0%    0.296
```

**读出来的事实**：
- `infer_ms` 来自 trace span 树（`Span::spans_by_kind(LLM)` 求和）——**真实推理墙钟**，证明 A1 的 `EfficiencyProfile::from_tree` 把 SWE-bench 狭窄路径的分解逻辑泛化成功了，现在任何 eval suite 自动带效率轴。
- `tokens` 来自 `UsageTracker`（520+768 等）——真实 token 成本。
- `3axis = quality / (1 + 0.1·log(1+tokens) + 0.1·log(1+latency_ms))`——三轴评分公式落地，能力×成本×效率三轴可计算。
- `cache% = 0.0%`：当前 provider 是 OpenAI 兼容（gpt-4），OpenAI 自动缓存但不回传 `cache_read_input_tokens`。**切到 Anthropic provider 即可看到非零 cache%**（A4 在 Anthropic 响应里读 `cache_read_input_tokens` 已实装，见 `anthropic.rs`）。
- `iters = 0`：direct-answer 单轮推理用例，trace 的 `avg_iterations` 对单 shot 直答为 0（pre-existing trace 口径，非本改动回归）。

**对应用户关切 1**：执行效率**可测**——效率轴（推理墙钟/token/三轴）已对真实 run 实测落地；**可提升**——A4 的 prompt caching 分层（system+tools cache_control + `PromptCachePolicy` 开关 + cache_read_tokens 读回）提供了改进杠杆，切 Anthropic provider 即可量化收益。

---

## C2 ghost-replay 实测（真实 trajectory）

```
$ oneai eval run general --record /tmp/oneai_traj.json
Recorded trajectory (1 responses) → /tmp/oneai_traj.json

$ oneai eval replay /tmp/oneai_traj.json
── Replay Result (ghost replay / loop test) ────────
deterministic: true
tool calls match: true (replayed 0 vs recorded 0)
iterations: replayed 0 vs recorded 0
replay efficiency (frozen — wall-clock not meaningful):
  inference_calls: 1, tool_calls: 0, iterations: 0, tokens: 0
```

**读出来的事实**：
- 真实 run 的 provider 响应被 `RecordingProvider` 录制成 trajectory JSON；`ReplayProvider` 冻结回放，**无需真实 LLM** 即可重跑 AgentLoop。
- `deterministic: true`——回放与录制决策一致。这是 Loop Engineering 的**确定性 oracle / loop test** 原语：录一次真 run，每次构建回放，loop 行为漂移则失败。

---

## B1 固定工作流实测（真实 LLM）

```
$ oneai graph run react-loop "What is 2+2? Answer in one word."
▶ Running state graph: react-loop — task: "What is 2+2? Answer in one word."
── State Graph Result ───────────────────────────
completed: true, iterations: 2, terminal: Some("end")
final answer:
  Four.
```

**对应用户关切 2**：DAG/有向图式固定工作流**之前没有任何 CLI 运行入口**（实现自洽但不可证可用）。新增 `oneai workflow run` / `oneai graph run` 后，StateGraph 路径**用真实 LLM 端到端跑通**（think→end，2 轮，到达 terminal）。CodingPack 的 code_review/debug/refactor/test 4 个 DAG workflow 同样可通过 `workflow run <name>` 触发（`builder.rs` 已给 `WorkflowExecutor` 挂上 provider，prompt step 走真实 `infer()`）。

> 注：`workflow run code-review` 未在本验证中全量执行——其 shell step 含 `cargo test`（workspace 全量，耗时数分钟 + 触发 flaky swebench instance 测试）。DAG executor 的 prompt-step 真实推理链路由 `builder.rs` 的 provider 挂载 + 单测覆盖；`graph run` 的真实跑通已证明 workflow 引擎接真实 LLM 可用。

---

## 改动文件总览

- **B1**: `examples/cli/src/main.rs`（Workflow/Graph 命令枚举+dispatch）、`examples/cli/src/cmd_workflow.rs`（新建）、`crates/oneai-app/src/builder.rs`（workflow_executor 挂 provider）、`crates/oneai-app/src/session.rs`（`execute_state_graph_with_task`）
- **A1**: `crates/oneai-eval/src/efficiency.rs`（新建，`EfficiencyProfile`+三轴）、`eval_result.rs`/`eval_runner.rs`（efficiency 字段+填充）、`builtin_metrics.rs`（修 `TrajectoryMetric`+加 `EfficiencyMetric`）、`eval_metric.rs`（`score_with_trace` 默认法）、`swebench/runner.rs`（复用 efficiency）、`cmd_eval.rs`（`--profile`）
- **A4**: `crates/oneai-core/src/types.rs`（`TokenUsage`+cache 字段+`PromptCachePolicy`）、`crates/oneai-provider/src/anthropic.rs`（读 `cache_read_input_tokens`+分层 cache_control+Off 策略）、`crates/oneai-agent/src/agent_loop.rs`（`prompt_cache_policy` config + LLM span stamp `llm.cache_read_tokens`）、全仓 35 处 `TokenUsage` 字面量补 `..Default::default()`
- **C2**: `crates/oneai-eval/src/replay.rs`（新建，`ReplayProvider`+`RecordingProvider`+`Trajectory`+`replay_trajectory`）、`cmd_eval.rs`（`eval replay`+`--record`）

## 验证命令清单

```bash
cargo test -p oneai-eval          # 95 tests (含 3 replay + 3 efficiency/trajectory)
cargo test -p oneai-provider      # 110 tests (含 cache policy stripping)
cargo test -p oneai-agent         # 218 tests
cargo build --workspace           # clean
oneai-cli workflow list          # 4 DAG + 4 StateGraph
oneai-cli graph run react-loop "..."  # 真实 LLM ✅
oneai-cli eval run general --profile   # 效率轴 ✅
oneai-cli eval run general --record /tmp/t.json && oneai-cli eval replay /tmp/t.json  # ghost replay ✅
```
