//! SwebenchRunner — drives OneAI against checked-out SWE-bench instances.
//!
//! This is the SWE-bench adapter (Step 3). It owns the parts that don't fit the
//! generic `EvalRunner`'s text-output model:
//!
//! 1. **clone + checkout** each instance's repo at `base_commit` (via `git`,
//!    not `ShellTool` — no sandbox/working-dir plumbing needed);
//! 2. **drive the agent** on the `problem_statement`, pointing it at the clone;
//! 3. **collect `git diff`** as the patch (SWE-bench's "output");
//! 4. **judge** the patch via the external `SwebenchJudge` → the `resolved`
//!    verdict becomes the `swebench_resolved` metric score.
//!
//! Usage + trace (the usage / efficiency axes) flow through unchanged via the same
//! per-case collection pattern as `eval_runner.rs::run_agent_for_case`. The
//! agent's working directory isn't changed on the `App` — file tools target
//! the clone by absolute path, named in the prompt.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use oneai_core::error::Result;
use oneai_trace::TraceMetrics;

use crate::eval_result::{EvalReport, EvalResult};
use crate::eval_metric::EvalJudge;
use crate::swebench::instance::SwebenchInstance;
use crate::swebench::judge::SwebenchJudge;
use crate::swebench::leaderboard::SWEBENCH_RESOLVED_METRIC;

/// Configuration for the SWE-bench runner.
#[derive(Debug, Clone)]
pub struct SwebenchRunnerConfig {
    /// Root directory for cloned repos + prediction artifacts.
    pub workspace_dir: PathBuf,
    /// Python interpreter with `swebench` installed (default `~/.venvs/swebench/bin/python`).
    pub python_path: String,
    /// Run the judge harness via Modal (`--modal true`) — avoids local docker.
    pub use_modal: bool,
    /// Dataset name passed to the harness (e.g. `princeton-nlp/SWE-bench_Lite`).
    pub dataset_name: String,
    /// Cap on the number of instances to run (0 = no cap).
    pub max_instances: usize,
    /// Run id — the harness writes results under `evaluation_results/<run_id>/`.
    pub run_id: String,
}

impl Default for SwebenchRunnerConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        Self {
            workspace_dir: PathBuf::from("./swebench-workspace"),
            python_path: format!("{}/.venvs/swebench/bin/python", home),
            use_modal: true,
            dataset_name: "princeton-nlp/SWE-bench_Lite".to_string(),
            max_instances: 0,
            run_id: "oneai".to_string(),
        }
    }
}

/// The SWE-bench evaluation runner.
///
/// Holds a fully-configured `App` (provider + CodingPack + usage tracker +
/// tracing + auto-approval gate) and drives it instance-by-instance.
pub struct SwebenchRunner {
    app: Arc<oneai_app::App>,
    config: SwebenchRunnerConfig,
}

impl SwebenchRunner {
    /// Create a runner from a configured `App` + config.
    pub fn new(app: oneai_app::App, config: SwebenchRunnerConfig) -> Self {
        Self { app: Arc::new(app), config }
    }

    /// Run a batch of instances and produce an `EvalReport`.
    pub async fn run(&self, instances: &[SwebenchInstance]) -> Result<EvalReport> {
        let mut results = Vec::new();
        let limit = if self.config.max_instances == 0 {
            instances.len()
        } else {
            self.config.max_instances.min(instances.len())
        };

        for instance in instances.iter().take(limit) {
            let result = self.run_instance(instance).await;
            results.push(result);
        }

        Ok(EvalReport::new("swebench", results))
    }

    /// Drive a single instance: clone → checkout → agent → diff → judge.
    async fn run_instance(&self, instance: &SwebenchInstance) -> EvalResult {
        let start = Instant::now();
        let mut result = EvalResult::new(&instance.instance_id, &instance.problem_statement, "");
        result.set_metadata("base_commit", &instance.base_commit);
        result.set_metadata("repo", &instance.repo);
        result.set_metadata("version", &instance.version);

        if !self.app.has_provider() {
            result.error = Some("No LLM provider configured".to_string());
            result.duration_ms = start.elapsed().as_millis() as u64;
            return result;
        }
        if !instance.is_runnable() {
            result.error = Some(format!(
                "instance not runnable (missing repo/base_commit): {}", instance.instance_id
            ));
            result.duration_ms = start.elapsed().as_millis() as u64;
            return result;
        }

        // 1. clone + checkout
        let t_clone = Instant::now();
        let clone_dir = self.clone_instance(instance);
        let clone_dir = match clone_dir {
            Ok(p) => p,
            Err(e) => {
                result.error = Some(format!("clone/checkout failed: {}", e));
                result.duration_ms = start.elapsed().as_millis() as u64;
                return result;
            }
        };
        result.set_metadata("dur_clone_ms", &t_clone.elapsed().as_millis().to_string());

        // 2 + 3. drive the agent + collect usage/trace (mirrors EvalRunner.run_agent_for_case)
        self.drive_agent(instance, &clone_dir, &mut result).await;

        // 4. collect the patch via `git diff`
        let t_diff = Instant::now();
        let patch = match collect_git_diff(&clone_dir) {
            Ok(p) => p,
            Err(e) => {
                result.error = Some(format!("git diff failed: {}", e));
                result.duration_ms = start.elapsed().as_millis() as u64;
                return result;
            }
        };
        result.set_metadata("dur_diff_ms", &t_diff.elapsed().as_millis().to_string());
        result.actual_output = patch.clone();
        result.set_metadata("patch", &patch);

        // 5. judge — the 能力 axis. An empty patch short-circuits unresolved
        //    inside the judge without spawning the harness.
        let judge = SwebenchJudge::new(
            &instance.instance_id,
            &self.config.python_path,
            self.config.use_modal,
            &self.config.run_id,
            &self.config.dataset_name,
            self.config.workspace_dir.clone(),
        );
        let t_judge = Instant::now();
        let score = judge.judge(&instance.problem_statement, &patch).await;
        result.set_metadata("dur_judge_ms", &t_judge.elapsed().as_millis().to_string());
        result.set_metadata(
            "resolved",
            if score.passed { "true" } else { "false" },
        );
        result.add_score(SWEBENCH_RESOLVED_METRIC, score);
        // tests_status summary is embedded in the score reason; mirror it for
        // any consumer that wants the raw breakdown.
        if let Some(reason) = result.scores.last().map(|ms| ms.score.reason.clone()) {
            result.set_metadata("verdict_reason", &reason);
        }

        result.duration_ms = start.elapsed().as_millis() as u64;
        result
    }

    /// Clone the instance's repo into the workspace and check out `base_commit`.
    /// Returns the clone directory. Reuses an existing clone (fetches + checks out).
    fn clone_instance(&self, instance: &SwebenchInstance) -> std::result::Result<PathBuf, String> {
        std::fs::create_dir_all(&self.config.workspace_dir)
            .map_err(|e| format!("cannot create workspace: {}", e))?;
        let clone_dir = self.config.workspace_dir.join(&instance.instance_id);

        let repo_url = if instance.repo.starts_with("http") || instance.repo.starts_with("/") {
            instance.repo.clone()
        } else {
            format!("https://github.com/{}.git", instance.repo)
        };

        if !clone_dir.exists() {
            // Fresh clone.
            let out = Command::new("git")
                .arg("clone")
                .arg(&repo_url)
                .arg(&clone_dir)
                .output()
                .map_err(|e| format!("git clone spawn failed: {}", e))?;
            if !out.status.success() {
                return Err(format!(
                    "git clone failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }

        // Check out the base commit (works for fresh clones and re-used ones).
        let out = Command::new("git")
            .arg("-C")
            .arg(&clone_dir)
            .arg("checkout")
            .arg(&instance.base_commit)
            .output()
            .map_err(|e| format!("git checkout spawn failed: {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "git checkout {} failed: {}",
                instance.base_commit,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        // Reset any leftover uncommitted edits from a prior run on a reused clone.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&clone_dir)
            .args(["checkout", "--", "."])
            .output();
        let _ = Command::new("git")
            .arg("-C")
            .arg(&clone_dir)
            .args(["clean", "-fd"])
            .output();

        Ok(clone_dir)
    }

    /// Run the agent on the instance's problem statement, writing the final
    /// answer, trace metrics, and usage into `result`.
    async fn drive_agent(
        &self,
        instance: &SwebenchInstance,
        clone_dir: &Path,
        result: &mut EvalResult,
    ) {
        let prompt = format!(
            "You are working on the repository checked out at:\n  {}\n\n\
             Here is a bug report for this codebase (checked out at the relevant \
             commit). Reproduce it, locate the root cause, and fix it by editing \
             the source. Do not commit — just leave the changes in the working tree.\n\n\
             {}\n",
            clone_dir.display(),
            instance.problem_statement,
        );

        let mut session = self.app.create_session();
        let session_id = session.session_id().to_string();

        // Isolate this case's usage accounting.
        if let Some(ct) = &self.app.usage_tracker {
            let _ = ct.clear_session(&session_id).await;
        }

        let t_agent = Instant::now();
        let agent_result = session.run_agent_silent(&prompt).await;
        let dur_agent_ms = t_agent.elapsed().as_millis();
        result.set_metadata("dur_agent_ms", &dur_agent_ms.to_string());
        match agent_result {
            Ok(loop_result) => {
                // The agent's textual answer isn't the SWE-bench output (the
                // patch is) — but keep it on the result for debugging until the
                // patch overwrites `actual_output`.
                let _ = loop_result.final_answer;

                if let Some(ctx) = session.trace_context() {
                    let tree = ctx.build_tree();
                    result.trace_metrics = TraceMetrics::compute_from_tree(&tree.root_span);
                    // 效率 axis detail: split the agent's wall-clock into
                    // inference vs tool vs overhead, straight from the span tree.
                    // (Only available now that trace_context is wired into the
                    // loop — previously the tree held only the SESSION span.)
                    let tb = trace_timing_breakdown(&tree.root_span, dur_agent_ms);
                    if let Ok(json) = serde_json::to_string(&tb) {
                        result.set_metadata("timing", &json);
                    }
                }
            }
            Err(e) => {
                result.set_metadata("agent_error", &e.to_string());
            }
        }

        // Collect the usage axis: api_calls + token breakdown.
        if let Some(ct) = &self.app.usage_tracker {
            if let Ok(summary) = ct.session_usage(&session_id).await {
                result.api_calls = summary.call_count;
                result.estimated_calls = summary.estimated_call_count;
                result.prompt_tokens = summary.prompt_tokens;
                result.completion_tokens = summary.completion_tokens;
            }
        }
    }
}

/// Per-instance timing breakdown — wall-clock split of the run + a
/// trace-derived decomposition of the agent's time.
///
/// `dur_agent_ms` is the measured wall-clock of the whole agent drive.
/// `inference_ms` / `tool_ms` are summed from the trace span tree (LLM and
/// TOOL spans respectively); `overhead_ms` is the agent time not attributable
/// to either (context assembly, parsing, compression, etc.). Stamped into
/// `EvalResult.metadata["timing"]` as JSON for the report + leaderboard.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TimingBreakdown {
    inference_ms: u64,
    inference_calls: usize,
    tool_ms: u64,
    tool_calls: usize,
    overhead_ms: u64,
    dur_agent_ms: u64,
}

/// Sum LLM/TOOL span durations from the trace tree to decompose agent time.
fn trace_timing_breakdown(root: &oneai_trace::Span, dur_agent_ms: u128) -> TimingBreakdown {
    use oneai_trace::SpanKind;
    let sum_kind = |kind: SpanKind| -> (u64, usize) {
        let spans = root.spans_by_kind(kind);
        let total: u64 = spans.iter().filter_map(|s| s.duration_ms).sum();
        (total, spans.len())
    };
    let (inference_ms, inference_calls) = sum_kind(SpanKind::LLM);
    let (tool_ms, tool_calls) = sum_kind(SpanKind::TOOL);
    let attributed = inference_ms + tool_ms;
    let overhead_ms = dur_agent_ms.saturating_sub(attributed as u128) as u64;
    TimingBreakdown {
        inference_ms,
        inference_calls,
        tool_ms,
        tool_calls,
        overhead_ms,
        dur_agent_ms: dur_agent_ms as u64,
    }
}

/// Run `git diff` (unstaged) in `clone_dir` and return the patch text.
fn collect_git_diff(clone_dir: &Path) -> std::result::Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(clone_dir)
        .arg("diff")
        .output()
        .map_err(|e| format!("git diff spawn failed: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "git diff failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Create a throwaway local git fixture repo with one committed file and
    /// return (repo_path, base_commit_sha). Used to exercise clone+checkout+diff
    /// without touching the network.
    fn make_fixture_repo() -> (std::path::PathBuf, String) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        // Combine a process-global counter with the nanos timestamp: the counter
        // guarantees uniqueness across concurrent test threads even when the
        // system clock's nanos resolution is too coarse to distinguish them
        // (observed on macOS — caused an intermittent "does not appear to be a
        // git repository" clone failure when two fixtures collided on a path).
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "oneai_swebench_fixture_{}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            id,
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // git init + commit a file so there's a real base_commit to checkout.
        let run = |args: &[&str]| {
            Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);

        let file_path = dir.join("lib.py");
        let mut f = std::fs::File::create(&file_path).unwrap();
        writeln!(f, "def f(x):").unwrap();
        writeln!(f, "    return x + 1").unwrap();
        drop(f);

        run(&["add", "lib.py"]);
        run(&["commit", "-q", "-m", "init"]);

        // base_commit = HEAD
        let sha_out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
        (dir, sha)
    }

    fn skip_if_no_git() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    #[tokio::test]
    async fn test_runner_empty_patch_unresolved() {
        if !skip_if_no_git() {
            eprintln!("skipping: git not installed");
            return;
        }
        let (repo_path, base_commit) = make_fixture_repo();

        // Mock provider answers with plain text and never edits files → empty
        // patch → judge short-circuits to unresolved without spawning python.
        let provider: std::sync::Arc<dyn oneai_core::traits::LlmProvider> =
            std::sync::Arc::new(oneai_agent::mock_provider::MockProvider::always_answers(
                "I would fix this but I'm a mock.",
            ));

        let workspace = std::env::temp_dir().join(format!(
            "oneai_swebench_ws_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));

        let app = oneai_app::AppBuilder::new()
            .provider(provider)
            .auto_approval_gate()
            .default_parser()
            .default_usage_tracker()
            .trace_in_memory()
            .build()
            .await
            .expect("app build");

        let config = SwebenchRunnerConfig {
            workspace_dir: workspace.clone(),
            python_path: "/nonexistent/python".into(),
            use_modal: true,
            dataset_name: "princeton-nlp/SWE-bench_Lite".into(),
            max_instances: 0,
            run_id: "oneai-test".into(),
        };
        let runner = SwebenchRunner::new(app, config);

        let instance = SwebenchInstance {
            instance_id: "fixture__test-1".into(),
            repo: repo_path.to_string_lossy().into_owned(), // local path
            base_commit,
            problem_statement: "bug: f returns wrong value".into(),
            test_patch: String::new(),
            fail_to_pass: String::new(),
            pass_to_pass: String::new(),
            version: "1.0".into(),
        };

        let report = runner.run(&[instance]).await.expect("run ok");
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&repo_path);

        assert_eq!(report.results.len(), 1);
        let r = &report.results[0];
        assert!(r.error.is_none(), "unexpected error: {:?}", r.error);
        // No edits → empty patch → unresolved.
        assert_eq!(r.metadata.get("patch").map(|s| s.as_str()), Some(""));
        assert!(!r.metric_passed(SWEBENCH_RESOLVED_METRIC));
        assert_eq!(r.metadata.get("resolved"), Some(&"false".to_string()));

        // usage axis: the loop records usage into the app's usage tracker (the
        // mock direct_answer reports 100 prompt / 50 completion tokens and runs
        // one inference). Regression guard for the AppSession→AgentLoopConfig
        // propagation: previously usage_tracker was None and these stayed at 0.
        assert!(r.api_calls > 0, "api_calls should be recorded, got {}", r.api_calls);
        assert!(r.prompt_tokens > 0, "prompt_tokens should be recorded, got {}", r.prompt_tokens);
        assert!(r.completion_tokens > 0, "completion_tokens should be recorded, got {}", r.completion_tokens);

        // 效率 axis: phase wall-clock keys are stamped for every phase...
        for key in ["dur_clone_ms", "dur_agent_ms", "dur_diff_ms", "dur_judge_ms"] {
            assert!(r.metadata.contains_key(key), "missing timing key {}", key);
        }
        // ...and the trace-derived decomposition is populated now that
        // trace_context is wired into the loop. The mock runs one direct_answer
        // inference → at least one LLM span recorded.
        let timing = r.metadata.get("timing").expect("timing metadata present");
        let tb: TimingBreakdown =
            serde_json::from_str(timing).expect("timing JSON parses");
        assert!(tb.inference_calls >= 1, "expected ≥1 inference span, got {}", tb.inference_calls);
        assert!(tb.dur_agent_ms > 0, "agent wall-clock should be > 0");
        // The trace tree now holds LLM spans (inference_calls >= 1 above) — i.e.
        // trace_context is wired into the loop. We do NOT assert on
        // avg_inference_latency_ms here because the mock provider returns
        // instantly (<1ms), so span durations are 0ms; real runs have seconds.
    }

    #[tokio::test]
    async fn test_runner_domain_pack_wires_usage_and_trace() {
        // Regression guard for the domain-pack branch of session.rs: the
        // swebench CLI builds the App with a CodingPack (domain_pack set), which
        // takes the `if let Some(domain)` config branch. That branch previously
        // omitted usage_tracker AND trace_context from AgentLoopConfig (the
        // no-domain else branch had them) → the real swebench run reported
        // tokens 0+0 AND infer=0ms×0 even though api_calls counted.
        // This test mirrors the CLI's app construction (domain_pack + usage +
        // trace) so the domain branch is actually exercised.
        if !skip_if_no_git() {
            eprintln!("skipping: git not installed");
            return;
        }
        let (repo_path, base_commit) = make_fixture_repo();

        let provider: std::sync::Arc<dyn oneai_core::traits::LlmProvider> =
            std::sync::Arc::new(oneai_agent::mock_provider::MockProvider::always_answers(
                "I would fix this but I'm a mock.",
            ));

        let project_dir = std::env::temp_dir()
            .to_string_lossy()
            .into_owned();
        let coding_pack = oneai_domain::coding_pack(&project_dir);

        let workspace = std::env::temp_dir().join(format!(
            "oneai_swebench_dom_{}",
            std::sync::atomic::AtomicU64::new(0)
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));

        let app = oneai_app::AppBuilder::new()
            .provider(provider)
            .auto_approval_gate()
            .default_parser()
            .default_usage_tracker()
            .trace_in_memory()
            .domain_pack(coding_pack)
            .build()
            .await
            .expect("app build");

        let config = SwebenchRunnerConfig {
            workspace_dir: workspace.clone(),
            python_path: "/nonexistent/python".into(),
            use_modal: true,
            dataset_name: "princeton-nlp/SWE-bench_Lite".into(),
            max_instances: 0,
            run_id: "oneai-test".into(),
        };
        let runner = SwebenchRunner::new(app, config);

        let instance = SwebenchInstance {
            instance_id: "fixture__dom-1".into(),
            repo: repo_path.to_string_lossy().into_owned(),
            base_commit,
            problem_statement: "bug: f returns wrong value".into(),
            test_patch: String::new(),
            fail_to_pass: String::new(),
            pass_to_pass: String::new(),
            version: "1.0".into(),
        };

        let report = runner.run(&[instance]).await.expect("run ok");
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&repo_path);

        let r = &report.results[0];
        assert!(r.error.is_none(), "unexpected error: {:?}", r.error);

        // 成本 axis wired through the domain branch.
        assert!(r.api_calls > 0, "domain branch should record api_calls");
        assert!(r.prompt_tokens > 0, "domain branch should record tokens");

        // 效率 axis: the domain branch must hand trace_context to the loop so
        // LLM spans land in the tree (previously infer=0ms×0 in real runs).
        let timing = r.metadata.get("timing").expect("timing metadata present");
        let tb: TimingBreakdown =
            serde_json::from_str(timing).expect("timing JSON parses");
        assert!(tb.inference_calls >= 1,
            "domain branch should produce ≥1 LLM span, got {}", tb.inference_calls);
    }

    #[tokio::test]
    async fn test_runner_estimates_tokens_when_provider_omits_usage() {
        // The GLM-style case: provider streaming returns NO usage (all-zero),
        // so naive accounting records tokens=0+0. With a TokenCounter wired in,
        // the loop must fall back to counting locally and mark the record
        // estimated. Mirrors cmd_eval_swebench's app construction (domain_pack
        // + usage + token_counter + trace).
        if !skip_if_no_git() {
            eprintln!("skipping: git not installed");
            return;
        }
        let (repo_path, base_commit) = make_fixture_repo();

        // Direct answer with ZERO usage — simulates a streaming provider that
        // never sends a usage chunk.
        let zero_usage = oneai_core::TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        };
        let provider: std::sync::Arc<dyn oneai_core::traits::LlmProvider> =
            std::sync::Arc::new(oneai_agent::mock_provider::MockProvider::from_script(
                vec![oneai_agent::mock_provider::ScriptedResponse::custom(
                    vec![oneai_core::ContentBlock::Text {
                        text: "I would fix this but I'm a mock.".to_string(),
                    }],
                    zero_usage,
                )],
            ));

        let project_dir = std::env::temp_dir().to_string_lossy().into_owned();
        let coding_pack = oneai_domain::coding_pack(&project_dir);

        static EST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let workspace = std::env::temp_dir().join(format!(
            "oneai_swebench_est_{}",
            EST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));

        let app = oneai_app::AppBuilder::new()
            .provider(provider)
            .auto_approval_gate()
            .default_parser()
            .default_usage_tracker()
            .default_token_counter()
            .trace_in_memory()
            .domain_pack(coding_pack)
            .build()
            .await
            .expect("app build");

        let config = SwebenchRunnerConfig {
            workspace_dir: workspace.clone(),
            python_path: "/nonexistent/python".into(),
            use_modal: true,
            dataset_name: "princeton-nlp/SWE-bench_Lite".into(),
            max_instances: 0,
            run_id: "oneai-test".into(),
        };
        let runner = SwebenchRunner::new(app, config);

        let instance = SwebenchInstance {
            instance_id: "fixture__est-1".into(),
            repo: repo_path.to_string_lossy().into_owned(),
            base_commit,
            problem_statement: "bug: f returns wrong value".into(),
            test_patch: String::new(),
            fail_to_pass: String::new(),
            pass_to_pass: String::new(),
            version: "1.0".into(),
        };

        let report = runner.run(&[instance]).await.expect("run ok");
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&repo_path);

        let r = &report.results[0];
        assert!(r.error.is_none(), "unexpected error: {:?}", r.error);
        // Provider gave no usage → loop counted locally → non-zero tokens,
        // and the call is flagged estimated.
        assert!(r.api_calls > 0, "api_calls should be recorded");
        assert_eq!(r.estimated_calls, r.api_calls,
            "all calls should be estimated when provider omits usage, got {}/{}",
            r.estimated_calls, r.api_calls);
        assert!(r.prompt_tokens > 0, "prompt_tokens should be estimated > 0, got {}", r.prompt_tokens);
        assert!(r.completion_tokens > 0, "completion_tokens should be estimated > 0, got {}", r.completion_tokens);
    }

    #[tokio::test]
    async fn test_runner_missing_provider_errors() {
        // Build an App with NO provider → instance marked as error, not panic.
        let app = oneai_app::AppBuilder::new()
            .auto_approval_gate()
            .default_parser()
            .build()
            .await
            .expect("app build");

        let config = SwebenchRunnerConfig::default();
        let runner = SwebenchRunner::new(app, config);

        let instance = SwebenchInstance {
            instance_id: "x__y-1".into(),
            repo: "owner/repo".into(),
            base_commit: "abc".into(),
            problem_statement: "p".into(),
            test_patch: String::new(),
            fail_to_pass: String::new(),
            pass_to_pass: String::new(),
            version: "1.0".into(),
        };

        let report = runner.run(&[instance]).await.expect("run ok");
        assert_eq!(report.results.len(), 1);
        assert!(report.results[0].error.is_some());
        assert!(report.results[0].error.as_ref().unwrap().contains("provider"));
    }

    #[test]
    fn test_collect_git_diff_on_clean_repo() {
        if !skip_if_no_git() {
            eprintln!("skipping: git not installed");
            return;
        }
        let (repo_path, _sha) = make_fixture_repo();
        let patch = collect_git_diff(&repo_path).unwrap();
        let _ = std::fs::remove_dir_all(&repo_path);
        // No uncommitted changes → empty diff.
        assert!(patch.is_empty());
    }
}
