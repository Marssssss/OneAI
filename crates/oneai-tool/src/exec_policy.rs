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
use std::path::Path;

use oneai_core::Verdict;
use serde::{Deserialize, Serialize};
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
}
