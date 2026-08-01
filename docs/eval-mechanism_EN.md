# OneAI Eval Mechanism

> `EvalCase` / `EvalMetric` / `EvalRunner` + 6 built-in metrics + 3 suites; SWE-bench three-axis (capability × usage × efficiency) for coding-agent eval.

## Responsibility

Make agent quality measurable. The eval framework runs cases, collects metrics, and emits reports; SWE-bench integration uses real repos + an external harness, collecting along three axes to avoid a single "resolved-only" view.

## Framework

- `EvalCase` / `ExpectedOutput` / `EvalMetric` / `EvalRunner` + `EvalReport`
- 6 built-in metrics + 3 suites (`coding_basics` etc.)
- Supports trace recording + ghost replay for determinism (`--record` / `eval replay`)

## SWE-bench three axes

Integrates [SWE-bench Lite](https://www.swebench.com/) (300 instances, or Verified 500):

| Axis | Source |
|---|---|
| **Capability** | SWE-bench external harness `resolved` verdict (`SwebenchJudge` calls a Python subprocess) |
| **Usage** | `UsageTracker.session_usage()` (api_calls + prompt/completion/total tokens, token-only, no USD) |
| **Efficiency** | `TraceMetrics` (total_tokens / tool_call_count / avg_iterations) + per-stage wall-clock breakdown |

Each instance: `git clone` → `checkout <base_commit>` → drive the agent with `problem_statement` (CodingPack provides read/edit/grep/glob/shell) → `git diff` to collect the patch → external harness judges `resolved`; all three axes go into `EvalResult`. Artifacts: `predictions.jsonl` + `leaderboard.json` (swebench.com submission schema; USD fields removed, comparable axis is `api_calls`).

## Key types & files

| Item | Location |
|---|---|
| `EvalCase` / `EvalMetric` / `EvalRunner` / `EvalSuite` / `EvalResult` | `crates/oneai-eval/src/{eval_case,eval_metric,eval_runner,eval_suite,eval_result}.rs` |
| 6 built-in metrics + 3 suites | `crates/oneai-eval/src/{builtin_metrics,builtin_suites}.rs` |
| efficiency axis | `crates/oneai-eval/src/efficiency.rs` |
| SWE-bench scripts | `scripts/swebench/` (`export_dataset.py` etc.) |

## Related CLI

[`eval list / run / score / replay / swebench`](cli-reference_EN.md#eval-framework).

## Further reading

- README "Eval" section has the smoke command
- Usage tracking — see [CLAUDE.md — UsageTracker](../CLAUDE.md)
