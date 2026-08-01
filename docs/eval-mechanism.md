# OneAI 评测机制

> `EvalCase` / `EvalMetric` / `EvalRunner` + 6 内置指标 + 3 套件；SWE-bench 三轴（能力 × 用量 × 效率）做 coding agent 评测。

## 职责

让 Agent 的好坏可量化。评测框架跑用例、采指标、出报告；SWE-bench 接入用真实仓库 + 外部 harness 判定，按三轴采集，避免「只看 resolved」的单一视角。

## 框架组成

- `EvalCase` / `ExpectedOutput` / `EvalMetric` / `EvalRunner` + `EvalReport`
- 6 内置指标 + 3 套件（`coding_basics` 等）
- 支持录制轨迹 + 幽灵重放校验确定性（`--record` / `eval replay`）

## SWE-bench 三轴

接入 [SWE-bench Lite](https://www.swebench.com/)（300 实例，或 Verified 500）：

| 轴 | 来源 |
|---|---|
| **能力** | SWE-bench 外部 harness 判定 `resolved`（`SwebenchJudge` 调 Python subprocess） |
| **用量** | `UsageTracker.session_usage()`（api_calls + prompt/completion/total token，纯 token，无 USD） |
| **效率** | `TraceMetrics`（total_tokens / tool_call_count / avg_iterations）+ 各阶段 wall-clock 拆解 |

每条实例：`git clone` → `checkout <base_commit>` → 用 `problem_statement` 驱动 agent（CodingPack 提供 read/edit/grep/glob/shell）→ `git diff` 收 patch → 外部 harness 判 `resolved`，三轴写入 `EvalResult`。产物 `predictions.jsonl` + `leaderboard.json`（swebench.com 提交 schema，USD 字段已移除，可比口径是 `api_calls`）。

## 关键类型与文件

| 项 | 位置 |
|---|---|
| `EvalCase` / `EvalMetric` / `EvalRunner` / `EvalSuite` / `EvalResult` | `crates/oneai-eval/src/{eval_case,eval_metric,eval_runner,eval_suite,eval_result}.rs` |
| 6 内置指标 + 3 套件 | `crates/oneai-eval/src/{builtin_metrics,builtin_suites}.rs` |
| 效率轴 | `crates/oneai-eval/src/efficiency.rs` |
| SWE-bench 脚本 | `scripts/swebench/`（`export_dataset.py` 等） |

## 相关 CLI

[`eval list / run / score / replay / swebench`](cli-reference.md#评测框架)。

## 深入阅读

- README「评测」段有冒烟命令
- 用量统计见 [CLAUDE.md — UsageTracker](../CLAUDE.md)
