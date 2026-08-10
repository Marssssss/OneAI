//! ExecPolicy — config-driven token-prefix rule engine (#28 Stage 4).
//!
//! The first of #28's three approval paths: "command matches an exec-policy
//! rule → auto-approve / auto-deny / prompt". This is the **declarative** layer
//! that sits *above* [`RuleGuardian`](crate::guardian::RuleGuardian)'s hardcoded
//! heuristic allow/deny lists. A user/project declares rules ("`git commit` →
//! allow, `cp` → prompt, `git reset --hard` → deny"); a matching rule emits the
//! [`Verdict`] directly and the [`RuleGuardian`] heuristic is skipped. A command
//! no rule matches falls through to the heuristic (the pre-Stage-4 behaviour).
//!
//! The model is adapted from OpenAI Codex's `execpolicy` crate (source-verified):
//! ordered-token **prefix** equality with per-position string alternatives, plus
//! strictest-wins aggregation (`Deny` > `Prompt` > `Allow`) when several rules
//! match. No regex, no glob — token equality only. Codex's Starlark DSL is *not*
//! ported: OneAI DomainPacks are declarative, so rules are `serde`-deserialized
//! JSON structs (zero new dependencies).
//!
//! Codex's `bypass_sandbox` (an explicit `Allow` exempts a command from the
//! sandbox) is deliberately **not** ported — in OneAI the sandbox
//! (seatbelt/bwrap) is a separate isolation axis and `ExecPolicy` only decides
//! *approval* (`Run` still runs inside whatever sandbox is configured).
//!
//! The verdict a rule produces feeds straight into the existing
//! [`ApprovalPolicy::decide`](oneai_core::ApprovalPolicy::decide) matrix, so the
//! four-level policy (`Never`/`OnFailure`/`OnRequest`/`OnUntrustedDir`) governs
//! rule outcomes exactly as it governs Guardian verdicts — no new enum, no new
//! matrix. `Allow`→`Run` under every policy; `Deny`→`Deny` (→`Prompt` under
//! `OnRequest`, user may override); `Prompt`→`Escalate` (→`Prompt` under
//! `OnFailure`, →`Deny` under `Never`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oneai_core::Verdict;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
/// A rule's decision. Ordered `Allow < Prompt < Deny` so the strictest match
/// wins when several rules match one command (Codex `Decision` semantics).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ExecDecision {
    /// Auto-approve — no prompt. Maps to [`Verdict::Allow`] (→ `Run` under
    /// every `ApprovalPolicy`).
    Allow,
    /// Prompt the user. Maps to [`Verdict::Escalate`] (→ `Prompt` under
    /// `OnFailure`, → `Deny` under `Never`).
    Prompt,
    /// Forbid — hard deny. Maps to [`Verdict::Deny`] (→ `Deny`, or `Prompt`
    /// under `OnRequest` so the user may override a too-broad forbid).
    Deny,
}

impl ExecDecision {
    /// Lower this decision to a [`Verdict`] (the type the Guardian/approval
    /// matrix consumes).
    fn to_verdict(self, reason: String) -> Verdict {
        match self {
            ExecDecision::Allow => Verdict::Allow { reason },
            ExecDecision::Prompt => Verdict::Escalate { reason },
            ExecDecision::Deny => Verdict::Deny { reason },
        }
    }
}

/// One position in a rule pattern — either a fixed literal or a set of
/// alternatives (any of which matches that position). Mirrors Codex's
/// `PatternToken`.
///
/// `#[serde(untagged)]`: a JSON string deserializes to [`PatternToken::Single`],
/// a JSON array of strings to [`PatternToken::Alts`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum PatternToken {
    /// A fixed token at this position.
    Single(String),
    /// Any of these tokens at this position.
    Alts(Vec<String>),
}

impl PatternToken {
    /// Whether `arg` matches this position.
    fn matches(&self, arg: &str) -> bool {
        match self {
            PatternToken::Single(s) => s == arg,
            PatternToken::Alts(alts) => alts.iter().any(|a| a == arg),
        }
    }
}

/// A prefix rule: if a command's argv has this `pattern` as a prefix
/// (position-by-position equality / alternative-containment), the rule's
/// [`ExecDecision`] applies. Trailing argv beyond the pattern is ignored —
/// `["git", "commit"]` matches `git commit -m "x"`.
///
/// Fields with `#[serde(default)]` keep additions non-breaking for
/// deserialization; the struct is constructible externally (config / tests).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecRule {
    /// Pattern prefix. The first position seeds the rule index, so it should be
    /// a [`PatternToken::Single`] or [`PatternToken::Alts`] — both are indexed
    /// (an `Alts` first position indexes the rule under every alternative).
    pub pattern: Vec<PatternToken>,
    pub decision: ExecDecision,
    /// Free-text reason surfaced in the verdict (and so in prompts/denials).
    #[serde(default)]
    pub justification: Option<String>,
    /// Load-time positive examples (codex-style). Each is tokenized and checked
    /// to match the rule at construction; a mismatch drops the rule with a
    /// warning rather than panicking (a bad DomainPack rule must not crash CI).
    #[serde(default)]
    pub match_examples: Vec<String>,
    /// Load-time negative examples — must *not* match. Same warn-and-drop.
    #[serde(default)]
    pub not_match_examples: Vec<String>,
}

/// An evaluated, indexed rule set. Construct via [`ExecPolicy::from_rules`];
/// query via [`ExecPolicy::evaluate`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecPolicy {
    /// The kept rules, in declaration order (stable for reason aggregation).
    rules: Vec<ExecRule>,
    /// First-token → indices into `rules` for O(1) lookup. An
    /// [`PatternToken::Alts`] first position seeds the rule under every alt.
    by_program: HashMap<String, Vec<usize>>,
}

impl ExecPolicy {
    /// No rules — matches nothing, always returns `None` (the heuristic
    /// fallback). This is the default posture: zero behaviour change unless a
    /// DomainPack configures rules.
    pub fn empty() -> Self {
        Self {
            rules: Vec::new(),
            by_program: HashMap::new(),
        }
    }

    /// Build a policy from rules, validating each rule's `match_examples` /
    /// `not_match_examples` against its own pattern. A rule whose example
    /// contract is broken is dropped with a `warn!` (not a panic — a
    /// misconfigured DomainPack must not crash the build). Rules with an empty
    /// pattern are dropped (no prefix = no match semantics).
    pub fn from_rules(rules: Vec<ExecRule>) -> Self {
        let mut kept: Vec<ExecRule> = Vec::with_capacity(rules.len());
        let mut by_program: HashMap<String, Vec<usize>> = HashMap::new();
        for rule in rules {
            if rule.pattern.is_empty() {
                tracing::warn!(
                    ?rule.justification,
                    "execpolicy: dropping rule with empty pattern (matches nothing)"
                );
                continue;
            }
            if !validate_examples(&rule) {
                // validate_examples already warned.
                continue;
            }
            let idx = kept.len();
            index_first_tokens(&rule.pattern, &mut by_program, idx);
            kept.push(rule);
        }
        Self {
            rules: kept,
            by_program,
        }
    }

    /// Whether the policy has any rules (an empty policy short-circuits to the
    /// heuristic in `GuardianContext::apply`).
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The kept rule count (introspection / tests).
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// The kept rules, in declaration order (for `PermissionProfile::merge` to
    /// union two policies' rules and rebuild a combined policy).
    pub fn rules(&self) -> &[ExecRule] {
        &self.rules
    }

    /// Evaluate a tokenized command against the policy.
    ///
    /// Returns `Some(Verdict)` if any rule's pattern is a prefix of `cmd`
    /// (strictest decision wins across all matches), else `None` (caller falls
    /// back to the heuristic reviewer). The command's first token is looked up
    /// in `by_program`; if it's an absolute path, the basename is also tried
    /// (codex `host_executable`-style fallback — without the path allowlist,
    /// which is deferred).
    pub fn evaluate(&self, cmd: &[String]) -> Option<Verdict> {
        if cmd.is_empty() {
            return None;
        }
        let candidates = self.candidates_for(&cmd[0]);
        if candidates.is_empty() {
            return None;
        }
        // Collect all matching rules; strictest (max Ord) decision wins.
        let mut matched: Vec<&ExecRule> = Vec::new();
        for &idx in candidates {
            let rule = &self.rules[idx];
            if pattern_is_prefix(&rule.pattern, cmd) {
                matched.push(rule);
            }
        }
        if matched.is_empty() {
            return None;
        }
        let decision = matched
            .iter()
            .map(|r| r.decision)
            .max()
            .expect("matched is non-empty");
        let reason = matched
            .iter()
            .filter_map(|r| r.justification.as_deref())
            .next()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("execpolicy rule matched (decision {:?})", decision));
        Some(decision.to_verdict(reason))
    }

    /// Rule indices whose first token could match `program` (direct lookup, plus
    /// basename lookup if `program` looks like an absolute path).
    fn candidates_for(&self, program: &str) -> &[usize] {
        if let Some(v) = self.by_program.get(program) {
            return v;
        }
        // Absolute-path argv[0] → basename fallback (e.g. `/usr/bin/git` →
        // `git`). Deferred: codex's host_executable path-allowlist (anti-hijack)
        // is not ported in v1.
        if program.contains(std::path::MAIN_SEPARATOR) || program.starts_with("./") {
            let base = Path::new(program)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(program);
            if let Some(v) = self.by_program.get(base) {
                return v;
            }
        }
        &[]
    }
}

// `ExecPolicy` serializes as its rule list (the `by_program` HashMap is a
// derived cache, rebuilt by `from_rules` on deserialize) so the resolved
// runtime struct round-trips through the same shape as `Vec<ExecRule>`.
impl Serialize for ExecPolicy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.rules.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExecPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let rules = Vec::<ExecRule>::deserialize(deserializer)?;
        Ok(Self::from_rules(rules))
    }
}

/// A live, swappable, persistable policy — the **runtime-amendment** layer
/// (#28 Stage 5). Holds the DomainPack's *static* base rules ∪ *amendments*
/// the user approved at runtime (appended to `rules_file`), behind a
/// `tokio::sync::RwLock` so an approval can hot-swap the live `ExecPolicy`
/// without rebuilding the `GuardianContext`.
///
/// Model: codex's `blocking_append_allow_prefix_rule` + ArcSwap hot-swap
/// (`exec_policy.rs`), minus the `arc-swap` dependency — OneAI's executor is
/// already async and already uses `tokio::sync::RwLock`, so an `RwLock` over
/// the immutable `ExecPolicy` snapshot is the equivalent. The read path
/// (`evaluate`) is a cheap read-lock; the write path (`add_amendment_rule`)
/// rebuilds the policy from `base_rules ∪ amendments` and swaps it in.
///
/// `ExecPolicy` itself stays immutable + cheap to clone; the `Store` owns the
/// one mutable cell. `GuardianContext::apply` consults `evaluate` exactly as
/// it consulted the pre-Stage-5 `ExecPolicy::evaluate` — the swap is
/// transparent to callers.
pub struct ExecPolicyStore {
    /// The live, swappable policy = `base_rules ∪ amendments`, rebuilt on
    /// every `add_amendment_rule`.
    live: RwLock<ExecPolicy>,
    /// The DomainPack-static seed rules (never persisted — they come from
    /// config, not user approval). Kept so an amendment can rebuild the full
    /// set without re-reading config.
    base_rules: Vec<ExecRule>,
    /// Where amendments are persisted as JSONL (one `ExecRule` per line).
    /// `None` = in-memory only (tests, or amendment disabled).
    rules_file: Option<PathBuf>,
}

impl std::fmt::Debug for ExecPolicyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.live.try_read().map(|p| p.rule_count()).unwrap_or(0);
        f.debug_struct("ExecPolicyStore")
            .field("live_rules", &n)
            .field("base_rules", &self.base_rules.len())
            .field("rules_file", &self.rules_file)
            .finish_non_exhaustive()
    }
}

impl ExecPolicyStore {
    /// Build a store from DomainPack-static base rules, optionally persisting
    /// / loading amendments from `rules_file` (JSONL, one `ExecRule` per line).
    /// If the file exists, its rules are merged on top of `base` and the
    /// combined policy is built once (re-running `from_rules`' match/not_match
    /// validation — a bad persisted line is warned and skipped, never panics).
    /// The file's parent directory is created lazily on the first
    /// `add_amendment_rule`, not here.
    pub fn from_base(base: Vec<ExecRule>, rules_file: Option<PathBuf>) -> Self {
        let mut all = base.clone();
        if let Some(ref path) = rules_file {
            if let Ok(text) = std::fs::read_to_string(path) {
                for (i, line) in text.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<ExecRule>(line) {
                        Ok(r) => {
                            // Idempotent reload: skip a line that duplicates an
                            // already-collected rule (same pattern + decision).
                            // Persisted files may carry dupes from races or
                            // hand-edits; `from_rules` would keep both, but the
                            // canonical set is the dedup'd one.
                            let dup = all
                                .iter()
                                .any(|e| e.pattern == r.pattern && e.decision == r.decision);
                            if !dup {
                                all.push(r);
                            }
                        }
                        Err(e) => tracing::warn!(
                            path = %path.display(),
                            line = i,
                            error = %e,
                            "execpolicy: skipping malformed amendment rule line"
                        ),
                    }
                }
            }
        }
        Self {
            live: RwLock::new(ExecPolicy::from_rules(all)),
            base_rules: base,
            rules_file,
        }
    }

    /// No base rules, no persistence (tests, or the amendment-disabled posture).
    pub fn empty_in_memory() -> Self {
        Self::from_base(Vec::new(), None)
    }

    /// Evaluate a tokenized command against the live policy. Read-locks the
    /// hot-swap cell; returns `Some(Verdict)` on a match (strictest-wins),
    /// `None` when no rule matches (caller falls back to the reviewer).
    pub async fn evaluate(&self, cmd: &[String]) -> Option<Verdict> {
        self.live.read().await.evaluate(cmd)
    }

    /// Whether the live policy has any rules (an empty policy short-circuits
    /// to the heuristic in `GuardianContext::apply`).
    pub async fn is_empty(&self) -> bool {
        self.live.read().await.is_empty()
    }

    /// The live rule count (introspection / tests).
    pub async fn rule_count(&self) -> usize {
        self.live.read().await.rule_count()
    }

    /// The static base (DomainPack) rule count — amendments are
    /// `rule_count() - base_rule_count()`.
    pub fn base_rule_count(&self) -> usize {
        self.base_rules.len()
    }

    /// The path amendments are persisted to, if any.
    pub fn rules_file(&self) -> Option<&Path> {
        self.rules_file.as_deref()
    }

    /// Append a user-approved amendment rule and hot-swap the live policy.
    /// Dedup: if a rule with the same `pattern` + `decision` already lives in
    /// the policy, this is a no-op (returns `false`, no file write). Otherwise
    /// rebuilds from `base_rules ∪ existing amendments ∪ new rule` (re-running
    /// `from_rules` validation), swaps the live cell, and — when `rules_file`
    /// is set — appends one JSONL line atomically. Returns whether a new rule
    /// was actually added.
    pub async fn add_amendment_rule(&self, rule: ExecRule) -> bool {
        let mut live = self.live.write().await;
        // Dedup against the live set (base ∪ amendments): same pattern +
        // decision means the user already approved this (or the DomainPack
        // already declared it) — don't pile on duplicates.
        let already = live
            .rules()
            .iter()
            .any(|r| r.pattern == rule.pattern && r.decision == rule.decision);
        if already {
            return false;
        }
        let mut combined = live.rules().to_vec();
        combined.push(rule.clone());
        let next = ExecPolicy::from_rules(combined);
        // from_rules may drop the new rule (bad match/not_match contract). If
        // it did, there's nothing to persist.
        let added = next.rule_count() > live.rule_count();
        if added {
            if let Some(ref path) = self.rules_file {
                if let Some(parent) = path.parent() {
                    if !parent.as_os_str().is_empty() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                }
                match serde_json::to_string(&rule) {
                    Ok(line) => {
                        use std::io::Write;
                        let mut line = line;
                        line.push('\n');
                        if let Ok(mut f) = std::fs::OpenOptions::new()
                            .append(true)
                            .create(true)
                            .open(path)
                        {
                            let _ = f.write_all(line.as_bytes());
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "execpolicy: could not serialize amendment rule")
                    }
                }
            }
            *live = next;
        }
        added
    }
}

/// Build an amendment rule from a shell command the user just approved, or
/// `None` when the command must not be auto-recorded. The rule's pattern is
/// the command's **full argv** (every token), `decision=Allow` — so only a
/// token-for-token identical command auto-runs next time (`rm scratch.tmp`
/// allows exactly `rm scratch.tmp`, not `rm other.tmp`). Trailing argv beyond
/// the pattern is still ignored by the matcher, but since the pattern *is* the
/// full command, "trailing" is empty in practice.
///
/// `BANNED_PREFIX_SUGGESTIONS` (codex spirit): wrappers whose argv-prefix
/// model is meaningless are **refused** — privilege escalation (`sudo`/`doas`),
/// shell `-c`/`-lc` wrappers (`bash`/`sh`/`zsh`/`fish`/`dash`), and interpreter
/// inline-code forms (`python -c`, `node -e`, `ruby -e`). The full-argv pattern
/// would otherwise embed a varying script body as a "literal" token. `rm`/
/// `git`/`npm` are *not* banned — the full-argv model makes their amendment
/// narrow and safe (codex's broad `rm` ban targets its own wide-prefix model).
pub fn amendment_rule_for(cmd: &str) -> Option<ExecRule> {
    let tokens = shell_tokens(cmd);
    if tokens.is_empty() {
        return None;
    }
    if is_banned_amendment_wrapper(&tokens) {
        return None;
    }
    let pattern = tokens.into_iter().map(PatternToken::Single).collect();
    Some(ExecRule {
        pattern,
        decision: ExecDecision::Allow,
        justification: Some("user-approved at runtime".into()),
        match_examples: Vec::new(),
        not_match_examples: Vec::new(),
    })
}

/// Whether `tokens` is a wrapper whose argv-prefix model is meaningless and
/// so must never be auto-recorded as an amendment.
fn is_banned_amendment_wrapper(tokens: &[String]) -> bool {
    let Some(first) = tokens.first().map(String::as_str) else {
        return true;
    };
    let base = Path::new(first)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first);
    // Privilege escalation — never auto-allow.
    if matches!(base, "sudo" | "doas") {
        return true;
    }
    // Shell wrappers — `-c`/`-lc` carry a script body, not argv the prefix
    // model can meaningfully capture.
    if matches!(base, "bash" | "sh" | "zsh" | "fish" | "dash")
        && tokens
            .iter()
            .skip(1)
            .take(2)
            .any(|t| t == "-c" || t == "-lc" || t == "-ic")
    {
        return true;
    }
    // Interpreter inline-code — `python -c "..."`, `node -e "..."`,
    // `ruby -e "..."`.
    let has_inline = tokens
        .iter()
        .skip(1)
        .take(2)
        .any(|t| t == "-c" || t == "-e");
    if matches!(base, "python" | "python3" | "node" | "ruby" | "perl") && has_inline {
        return true;
    }
    false
}

/// Whether `pattern` is a prefix of `cmd` (position-by-position). A pattern
/// longer than `cmd` never matches; trailing `cmd` is ignored. For a path-like
/// `argv[0]` (e.g. `/usr/bin/git`), the first position also matches the rule's
/// token against the path's basename — codex `host_executable`-style fallback
/// (without the path allowlist, which is deferred).
fn pattern_is_prefix(pattern: &[PatternToken], cmd: &[String]) -> bool {
    if pattern.len() > cmd.len() {
        return false;
    }
    pattern
        .iter()
        .zip(cmd.iter())
        .enumerate()
        .all(|(i, (tok, arg))| (i == 0 && token_matches_program(tok, arg)) || tok.matches(arg))
}

/// Whether the first-position `tok` matches `program`, accounting for an
/// absolute-path / `./`-relative `argv[0]` by also comparing against its
/// basename.
fn token_matches_program(tok: &PatternToken, program: &str) -> bool {
    if !(program.contains(std::path::MAIN_SEPARATOR) || program.starts_with("./")) {
        return false;
    }
    let Some(base) = Path::new(program).file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    base != program && tok.matches(base)
}

/// Seed `by_program` with the first-position tokens of `pattern` → `idx`.
/// `Single` seeds one key; `Alts` seeds every alternative.
fn index_first_tokens(
    pattern: &[PatternToken],
    by_program: &mut HashMap<String, Vec<usize>>,
    idx: usize,
) {
    let Some(first) = pattern.first() else {
        return;
    };
    let keys: Vec<String> = match first {
        PatternToken::Single(s) => vec![s.clone()],
        PatternToken::Alts(alts) => alts.clone(),
    };
    for k in keys {
        by_program.entry(k).or_default().push(idx);
    }
}

/// Validate a rule's example contract: every `match_example` must tokenize to a
/// command the rule's pattern is a prefix of; every `not_match_example` must
/// not. Returns `false` (after warning) if the contract is broken.
fn validate_examples(rule: &ExecRule) -> bool {
    let mut ok = true;
    for ex in &rule.match_examples {
        let toks = shell_tokens(ex);
        if !pattern_is_prefix(&rule.pattern, &toks) {
            tracing::warn!(
                pattern = ?rule.pattern,
                example = %ex,
                "execpolicy: rule match_example does NOT match its pattern — dropping rule"
            );
            ok = false;
        }
    }
    for ex in &rule.not_match_examples {
        let toks = shell_tokens(ex);
        if pattern_is_prefix(&rule.pattern, &toks) {
            tracing::warn!(
                pattern = ?rule.pattern,
                example = %ex,
                "execpolicy: rule not_match_example DOES match its pattern — dropping rule"
            );
            ok = false;
        }
    }
    ok
}

/// Tokenize a shell command string into argv tokens — whitespace split with
/// single/double-quote awareness (quotes group, are stripped). No escape
/// processing, no `&&`/`|`/`;` operator splitting (those are shell syntax, not
/// argv; the RuleGuardian deny heuristic still catches a destructive composite
/// like `ls; rm -rf /`). Sufficient for v1 prefix-rule matching; a real shell
/// parser (shlex) is deferred to avoid a new dependency.
pub fn shell_tokens(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut any = false; // a token is accumulating
    for ch in cmd.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                any = true; // a quote starts/continues a token even if empty
            }
            '"' if !in_single => {
                in_double = !in_double;
                any = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(pattern: Vec<PatternToken>, decision: ExecDecision) -> ExecRule {
        ExecRule {
            pattern,
            decision,
            justification: Some("test".into()),
            match_examples: Vec::new(),
            not_match_examples: Vec::new(),
        }
    }

    // ── shell_tokens ────────────────────────────────────────────────────────

    #[test]
    fn shell_tokens_plain_whitespace() {
        assert_eq!(
            shell_tokens("git commit -m x"),
            ["git", "commit", "-m", "x"]
        );
    }

    #[test]
    fn shell_tokens_double_quote_groups() {
        assert_eq!(
            shell_tokens(r#"git commit -m "hello world""#),
            ["git", "commit", "-m", "hello world"]
        );
    }

    #[test]
    fn shell_tokens_single_quote_groups() {
        assert_eq!(shell_tokens("echo 'a b c'"), ["echo", "a b c"]);
    }

    #[test]
    fn shell_tokens_leading_and_extra_whitespace() {
        assert_eq!(shell_tokens("   ls    -la   "), ["ls", "-la"]);
    }

    #[test]
    fn shell_tokens_empty_string() {
        let toks = shell_tokens("");
        assert!(toks.is_empty());
    }

    // ── evaluate: basic matching ────────────────────────────────────────────

    #[test]
    fn literal_prefix_matches_and_ignores_trailing() {
        let p = ExecPolicy::from_rules(vec![rule(
            vec![
                PatternToken::Single("git".into()),
                PatternToken::Single("commit".into()),
            ],
            ExecDecision::Allow,
        )]);
        assert!(matches!(
            p.evaluate(&shell_tokens("git commit -m x")),
            Some(Verdict::Allow { .. })
        ));
    }

    #[test]
    fn no_match_returns_none() {
        let p = ExecPolicy::from_rules(vec![rule(
            vec![
                PatternToken::Single("git".into()),
                PatternToken::Single("commit".into()),
            ],
            ExecDecision::Allow,
        )]);
        assert!(p.evaluate(&shell_tokens("git push")).is_none());
        assert!(p.evaluate(&shell_tokens("cargo build")).is_none());
    }

    #[test]
    fn alts_first_token_indexed_under_every_alt() {
        let p = ExecPolicy::from_rules(vec![rule(
            vec![
                PatternToken::Alts(vec!["cat".into(), "bat".into()]),
                PatternToken::Single("foo".into()),
            ],
            ExecDecision::Allow,
        )]);
        assert!(matches!(
            p.evaluate(&["cat".to_string(), "foo".to_string()]),
            Some(Verdict::Allow { .. })
        ));
        assert!(matches!(
            p.evaluate(&["bat".to_string(), "foo".to_string()]),
            Some(Verdict::Allow { .. })
        ));
        assert!(p
            .evaluate(&["cat".to_string(), "bar".to_string()])
            .is_none());
    }

    #[test]
    fn alts_in_non_first_position_matches_any() {
        let p = ExecPolicy::from_rules(vec![rule(
            vec![
                PatternToken::Single("ls".into()),
                PatternToken::Alts(vec!["-l".into(), "--long".into()]),
            ],
            ExecDecision::Allow,
        )]);
        assert!(matches!(
            p.evaluate(&["ls".to_string(), "-l".to_string()]),
            Some(Verdict::Allow { .. })
        ));
        assert!(matches!(
            p.evaluate(&["ls".to_string(), "--long".to_string()]),
            Some(Verdict::Allow { .. })
        ));
        assert!(p.evaluate(&["ls".to_string(), "-a".to_string()]).is_none());
    }

    // ── evaluate: strictest-wins + verdict mapping ──────────────────────────

    #[test]
    fn strictest_wins_deny_over_allow() {
        // Two rules match `cp src dst`: one Allow, one Deny → Deny wins.
        let p = ExecPolicy::from_rules(vec![
            rule(vec![PatternToken::Single("cp".into())], ExecDecision::Allow),
            rule(vec![PatternToken::Single("cp".into())], ExecDecision::Deny),
        ]);
        assert!(matches!(
            p.evaluate(&["cp".to_string(), "a".to_string(), "b".to_string()]),
            Some(Verdict::Deny { .. })
        ));
    }

    #[test]
    fn prompt_beats_allow_but_loses_to_deny() {
        let cp_rule = |d| rule(vec![PatternToken::Single("x".into())], d);
        let p = ExecPolicy::from_rules(vec![
            cp_rule(ExecDecision::Allow),
            cp_rule(ExecDecision::Prompt),
        ]);
        assert!(matches!(
            p.evaluate(&["x".to_string()]),
            Some(Verdict::Escalate { .. })
        ));

        let p = ExecPolicy::from_rules(vec![
            cp_rule(ExecDecision::Prompt),
            cp_rule(ExecDecision::Deny),
        ]);
        assert!(matches!(
            p.evaluate(&["x".to_string()]),
            Some(Verdict::Deny { .. })
        ));
    }

    #[test]
    fn decision_maps_to_verdict_correctly() {
        for dec in [
            ExecDecision::Allow,
            ExecDecision::Prompt,
            ExecDecision::Deny,
        ] {
            let p = ExecPolicy::from_rules(vec![rule(vec![PatternToken::Single("k".into())], dec)]);
            let v = p.evaluate(&["k".to_string()]).expect("matched");
            match (dec, v) {
                (ExecDecision::Allow, Verdict::Allow { .. }) => {}
                (ExecDecision::Prompt, Verdict::Escalate { .. }) => {}
                (ExecDecision::Deny, Verdict::Deny { .. }) => {}
                other => panic!("decision {dec:?} → verdict {other:?} mismatch"),
            }
        }
    }

    // ── absolute-path argv[0] basename fallback ────────────────────────────

    #[test]
    fn absolute_path_argv0_basename_fallback() {
        let p = ExecPolicy::from_rules(vec![rule(
            vec![
                PatternToken::Single("git".into()),
                PatternToken::Single("status".into()),
            ],
            ExecDecision::Allow,
        )]);
        assert!(matches!(
            p.evaluate(&["/usr/bin/git".to_string(), "status".to_string()]),
            Some(Verdict::Allow { .. })
        ));
    }

    // ── from_rules: validation ─────────────────────────────────────────────

    #[test]
    fn empty_pattern_rule_dropped() {
        let p = ExecPolicy::from_rules(vec![rule(vec![], ExecDecision::Allow)]);
        assert!(p.is_empty());
        assert!(p.evaluate(&["anything".to_string()]).is_none());
    }

    #[test]
    fn broken_match_example_drops_rule() {
        // Pattern is `git commit` but the match_example `git push` doesn't match
        // → rule dropped, evaluate returns None.
        let mut r = rule(
            vec![
                PatternToken::Single("git".into()),
                PatternToken::Single("commit".into()),
            ],
            ExecDecision::Allow,
        );
        r.match_examples = vec!["git push".into()];
        let p = ExecPolicy::from_rules(vec![r]);
        assert!(p.is_empty());
        assert!(p.evaluate(&shell_tokens("git commit")).is_none());
    }

    #[test]
    fn broken_not_match_example_drops_rule() {
        let mut r = rule(
            vec![
                PatternToken::Single("git".into()),
                PatternToken::Single("commit".into()),
            ],
            ExecDecision::Allow,
        );
        r.not_match_examples = vec!["git commit -m x".into()]; // does match → contract broken
        let p = ExecPolicy::from_rules(vec![r]);
        assert!(p.is_empty());
    }

    #[test]
    fn valid_examples_keep_rule() {
        let mut r = rule(
            vec![
                PatternToken::Single("git".into()),
                PatternToken::Single("commit".into()),
            ],
            ExecDecision::Allow,
        );
        r.match_examples = vec!["git commit -m x".into(), "git commit".into()];
        r.not_match_examples = vec!["git push".into(), "cargo build".into()];
        let p = ExecPolicy::from_rules(vec![r]);
        assert_eq!(p.rule_count(), 1);
        assert!(matches!(
            p.evaluate(&shell_tokens("git commit -m x")),
            Some(Verdict::Allow { .. })
        ));
    }

    // ── serde round-trip ───────────────────────────────────────────────────

    #[test]
    fn serde_rule_round_trip() {
        let json = json!({
            "pattern": ["git", "reset", ["--hard", "--keep"]],
            "decision": "deny",
            "justification": "destructive",
            "match_examples": ["git reset --hard"],
            "not_match_examples": ["git status"]
        });
        let r: ExecRule = serde_json::from_value(json).unwrap();
        assert_eq!(r.pattern.len(), 3);
        assert!(matches!(r.pattern[0], PatternToken::Single(_)));
        assert!(matches!(r.pattern[2], PatternToken::Alts(_)));
        assert_eq!(r.decision, ExecDecision::Deny);
        let p = ExecPolicy::from_rules(vec![r]);
        assert_eq!(p.rule_count(), 1);
        assert!(matches!(
            p.evaluate(&shell_tokens("git reset --hard")),
            Some(Verdict::Deny { .. })
        ));
        assert!(matches!(
            p.evaluate(&shell_tokens("git reset --keep")),
            Some(Verdict::Deny { .. })
        ));
        assert!(p.evaluate(&shell_tokens("git status")).is_none());
    }

    // ── amendment_rule_for ─────────────────────────────────────────────────

    #[test]
    fn amendment_rule_full_argv_allow() {
        let r = amendment_rule_for("git commit -m x").expect("git commit recordable");
        assert_eq!(r.decision, ExecDecision::Allow);
        assert_eq!(r.pattern.len(), 4);
        assert!(r
            .pattern
            .iter()
            .all(|t| matches!(t, PatternToken::Single(_))));
        assert_eq!(r.justification.as_deref(), Some("user-approved at runtime"));
    }

    #[test]
    fn amendment_rule_strips_quotes() {
        // `git commit -m "hello world"` → 4 tokens, the quoted body is one
        // literal token (so the recorded pattern matches the re-tokenized
        // command exactly).
        let r = amendment_rule_for(r#"git commit -m "hello world""#).expect("recordable");
        assert_eq!(r.pattern.len(), 4);
    }

    #[test]
    fn amendment_rule_refuses_privilege_escalation() {
        assert!(amendment_rule_for("sudo ls").is_none());
        assert!(amendment_rule_for("doas ls").is_none());
    }

    #[test]
    fn amendment_rule_refuses_shell_wrappers() {
        assert!(amendment_rule_for(r#"bash -c "rm -rf /""#).is_none());
        assert!(amendment_rule_for(r#"sh -c "echo hi""#).is_none());
        assert!(amendment_rule_for(r#"zsh -lc "ls""#).is_none());
        // `bash` without -c is fine (e.g. `bash script.sh`) — not a wrapper.
        assert!(amendment_rule_for("bash deploy.sh").is_some());
    }

    #[test]
    fn amendment_rule_refuses_interpreter_inline() {
        assert!(amendment_rule_for(r#"python -c "print(1)""#).is_none());
        assert!(amendment_rule_for(r#"python3 -c "print(1)""#).is_none());
        assert!(amendment_rule_for(r#"node -e "console.log(1)""#).is_none());
        // `python script.py` (no -c) is fine — a script path, not inline code.
        assert!(amendment_rule_for("python script.py").is_some());
    }

    #[test]
    fn amendment_rule_rm_is_recordable_full_argv() {
        // Full-argv model: `rm scratch.tmp` allows exactly that, not `rm x`.
        let r = amendment_rule_for("rm scratch.tmp").expect("rm full-argv recordable");
        assert_eq!(r.pattern.len(), 2);
    }

    #[test]
    fn amendment_rule_empty_command_is_none() {
        assert!(amendment_rule_for("").is_none());
        assert!(amendment_rule_for("   ").is_none());
    }

    // ── ExecPolicyStore ────────────────────────────────────────────────────

    #[tokio::test]
    async fn store_from_base_evaluates() {
        let store = ExecPolicyStore::from_base(
            vec![ExecRule {
                pattern: vec![
                    PatternToken::Single("git".into()),
                    PatternToken::Single("commit".into()),
                ],
                decision: ExecDecision::Allow,
                justification: Some("project rule".into()),
                match_examples: Vec::new(),
                not_match_examples: Vec::new(),
            }],
            None,
        );
        assert!(matches!(
            store.evaluate(&shell_tokens("git commit -m x")).await,
            Some(Verdict::Allow { .. })
        ));
        assert!(store.evaluate(&shell_tokens("git push")).await.is_none());
        assert_eq!(store.base_rule_count(), 1);
    }

    #[tokio::test]
    async fn store_add_amendment_then_evaluate_hits() {
        // No base rule for `npm install` → Escalate path normally. After
        // recording an amendment, evaluate returns Allow directly.
        let store = ExecPolicyStore::empty_in_memory();
        assert!(store.evaluate(&shell_tokens("npm install")).await.is_none());
        let rule = amendment_rule_for("npm install").expect("recordable");
        let added = store.add_amendment_rule(rule).await;
        assert!(added, "first add should record");
        assert!(matches!(
            store.evaluate(&shell_tokens("npm install")).await,
            Some(Verdict::Allow { .. })
        ));
    }

    #[tokio::test]
    async fn store_add_amendment_dedups() {
        let store = ExecPolicyStore::empty_in_memory();
        let rule = amendment_rule_for("cargo build").expect("recordable");
        assert!(store.add_amendment_rule(rule.clone()).await);
        // Same pattern + decision → no-op, returns false.
        assert!(!store.add_amendment_rule(rule).await);
        assert_eq!(store.rule_count().await, 1);
    }

    #[tokio::test]
    async fn store_add_amendment_strictest_wins_with_prompt_base() {
        // Base has a Prompt rule for `cp`; user approves `cp a b` → an Allow
        // amendment for the full argv is added. strictest-wins means a future
        // `cp a b` still sees both rules → Prompt beats Allow (the explicit
        // Prompt is authoritative; amendment does not override declared
        // policy). This is the documented limitation: amendment narrows future
        // *unmatched* commands, not ones a rule already classifies.
        let store = ExecPolicyStore::from_base(
            vec![ExecRule {
                pattern: vec![PatternToken::Single("cp".into())],
                decision: ExecDecision::Prompt,
                justification: Some("declared prompt".into()),
                match_examples: Vec::new(),
                not_match_examples: Vec::new(),
            }],
            None,
        );
        let rule = amendment_rule_for("cp a b").expect("recordable");
        store.add_amendment_rule(rule).await;
        assert!(matches!(
            store.evaluate(&shell_tokens("cp a b")).await,
            Some(Verdict::Escalate { .. })
        ));
    }

    #[tokio::test]
    async fn store_persists_and_reloads_amendments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("default.rules");
        // First store: approve `git commit -m x`, persist.
        {
            let store = ExecPolicyStore::from_base(Vec::new(), Some(path.clone()));
            let rule = amendment_rule_for("git commit -m x").expect("recordable");
            assert!(store.add_amendment_rule(rule).await);
            assert!(path.exists(), "rules file created on add");
        }
        // Second store: same path, base empty — the persisted amendment is
        // loaded at construction.
        let store = ExecPolicyStore::from_base(Vec::new(), Some(path.clone()));
        assert!(matches!(
            store.evaluate(&shell_tokens("git commit -m x")).await,
            Some(Verdict::Allow { .. })
        ));
        // Dedup survives reload: approving the same command again is a no-op.
        let rule = amendment_rule_for("git commit -m x").expect("recordable");
        assert!(!store.add_amendment_rule(rule).await);
    }

    #[tokio::test]
    async fn store_skips_malformed_persisted_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("default.rules");
        // One good rule + one garbage line + one empty line.
        let good = serde_json::to_string(&amendment_rule_for("ls").unwrap()).unwrap();
        std::fs::write(&path, format!("{good}\n{{not valid json\n\n{good}\n")).unwrap();
        let store = ExecPolicyStore::from_base(Vec::new(), Some(path));
        // Both good `ls` rules dedup to one live rule; the garbage line is
        // warned-and-skipped, never panics.
        assert_eq!(store.rule_count().await, 1);
        assert!(matches!(
            store.evaluate(&shell_tokens("ls")).await,
            Some(Verdict::Allow { .. })
        ));
    }

    #[tokio::test]
    async fn store_rebuild_after_amendment_still_validates_examples() {
        // A base rule whose match_example is broken is dropped at from_base
        // (Stage 4 semantics). An amendment rule with no examples always
        // survives. Confirm the rebuild path preserves the drop.
        let store = ExecPolicyStore::from_base(
            vec![ExecRule {
                pattern: vec![PatternToken::Single("git".into())],
                decision: ExecDecision::Allow,
                justification: None,
                match_examples: vec!["cargo build".into()], // does NOT match → dropped
                not_match_examples: Vec::new(),
            }],
            None,
        );
        assert_eq!(store.rule_count().await, 0);
        assert_eq!(store.base_rule_count(), 1); // base vec still holds it, just not kept live
        let rule = amendment_rule_for("make all").expect("recordable");
        assert!(store.add_amendment_rule(rule).await);
        assert_eq!(store.rule_count().await, 1);
    }
}
