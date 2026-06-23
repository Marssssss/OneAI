//! SWE-bench instance loading.
//!
//! A `SwebenchInstance` mirrors one row of the SWE-bench Lite/Verified JSONL
//! distribution. The runner consumes instances to know which repo to clone,
//! which commit to check out, the issue text to feed the agent, and the test
//! spec the external harness judges against.
//!
//! Instances are loaded from a local JSONL file (the standard SWE-bench
//! distribution format). HF dataset fetching stays in the phase-1 Python
//! `fetch_instance.py` script — keeping Rust free of dataset-server networking.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Deserialize a field that the dataset may store either as a JSON string
/// (e.g. `"[\"test_a\"]"`, the canonical SWE-bench format) or as an actual JSON
/// array of strings (`["test_a"]`, as some `datasets` exports return it).
/// Either form normalizes to the string form so the rest of the runner always
/// sees a `String`. We never interpret these ourselves — the external harness
/// reads `FAIL_TO_PASS`/`PASS_TO_PASS` from its own dataset load — so this is
/// purely load-robustness.
fn string_or_array<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            serde_json::to_string(&value).map_err(D::Error::custom)
        }
        serde_json::Value::Null => Ok(String::new()),
        other => serde_json::to_string(&other).map_err(D::Error::custom),
    }
}

/// One SWE-bench instance — a single bug-fix task.
///
/// Field names match the SWE-bench dataset columns verbatim so a downloaded
/// Lite/Verified JSONL can be fed in directly. Fields not needed for driving
/// the agent or judging are still `#[serde(default)]`-tolerant for forward
/// compatibility with future dataset columns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwebenchInstance {
    /// `owner__repo-issue_number` (double underscore) — the unique task id.
    pub instance_id: String,

    /// GitHub repo as `owner/repo`.
    #[serde(default)]
    pub repo: String,

    /// The commit the agent must base its fix on (the patch must apply here).
    #[serde(default)]
    pub base_commit: String,

    /// The issue text / problem statement fed to the agent as the task.
    #[serde(default)]
    pub problem_statement: String,

    /// The test patch the harness applies on top of the agent's fix.
    /// Not needed to drive the agent (the harness applies it), but kept for
    /// completeness / reproducibility exports.
    #[serde(default)]
    pub test_patch: String,

    /// Tests that must transition from failing → passing (`FAIL_TO_PASS`).
    /// Stored as the string form as it appears in the dataset; the harness
    /// parses it. Accepts either string or array form on load (see
    /// `string_or_array`).
    #[serde(default, rename = "FAIL_TO_PASS", deserialize_with = "string_or_array")]
    pub fail_to_pass: String,

    /// Tests that must remain passing (`PASS_TO_PASS`).
    #[serde(default, rename = "PASS_TO_PASS", deserialize_with = "string_or_array")]
    pub pass_to_pass: String,

    /// Repo version tag (e.g. `"3.4"`) — used by the harness to pick the image.
    #[serde(default)]
    pub version: String,
}

impl SwebenchInstance {
    /// Whether this instance has the minimum fields to be runnable
    /// (a repo to clone + a commit to check out).
    pub fn is_runnable(&self) -> bool {
        !self.instance_id.is_empty() && !self.repo.is_empty() && !self.base_commit.is_empty()
    }
}

/// Load all instances from a SWE-bench JSONL file (one JSON object per line).
///
/// Blank lines and lines that fail to parse are skipped with a `tracing`
/// warning rather than aborting the whole load — a partially-bad dataset row
/// shouldn't kill a batch run.
pub fn load_instances(path: &Path) -> Vec<SwebenchInstance> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("swebench: cannot read instances file {}: {}", path.display(), e);
            return Vec::new();
        }
    };

    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<SwebenchInstance>(line) {
            Ok(inst) => Some(inst),
            Err(e) => {
                tracing::warn!("swebench: skipping malformed instance line: {}", e);
                None
            }
        })
        .collect()
}

/// Load instances from a JSONL file, keeping only those whose `instance_id`
/// appears in `ids`. If `ids` is empty, all instances are returned.
pub fn load_instances_filtered(path: &Path, ids: &[String]) -> Vec<SwebenchInstance> {
    let instances = load_instances(path);
    if ids.is_empty() {
        return instances;
    }
    let want: std::collections::HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
    instances.into_iter().filter(|i| want.contains(i.instance_id.as_str())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp_jsonl(content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "oneai_swebench_test_{}.jsonl",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_load_instances_parses_jsonl() {
        let jsonl = "\
{\"instance_id\":\"astropy__astropy-12907\",\"repo\":\"astropy/astropy\",\"base_commit\":\"abc123\",\"problem_statement\":\"bug X\",\"version\":\"3.4\",\"FAIL_TO_PASS\":\"[\\\"test_a\\\"]\",\"PASS_TO_PASS\":\"[]\"}\n\
{\"instance_id\":\"django__django-11099\",\"repo\":\"django/django\",\"base_commit\":\"def456\",\"problem_statement\":\"bug Y\",\"version\":\"3.0\"}\n";
        let path = write_temp_jsonl(jsonl);
        let instances = load_instances(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].instance_id, "astropy__astropy-12907");
        assert_eq!(instances[0].repo, "astropy/astropy");
        assert_eq!(instances[0].base_commit, "abc123");
        assert_eq!(instances[0].version, "3.4");
        assert_eq!(instances[0].fail_to_pass, "[\"test_a\"]");
        assert!(instances[0].is_runnable());
        assert_eq!(instances[1].instance_id, "django__django-11099");
        // pass_to_pass absent → default empty
        assert_eq!(instances[1].pass_to_pass, "");
    }

    #[test]
    fn test_load_instances_skips_bad_lines() {
        let jsonl = "\
{\"instance_id\":\"ok\",\"repo\":\"a/b\",\"base_commit\":\"c\",\"problem_statement\":\"\"}\n\
not json at all\n\
\n\
{\"instance_id\":\"ok2\",\"repo\":\"a/b\",\"base_commit\":\"c\",\"problem_statement\":\"\"}\n";
        let path = write_temp_jsonl(jsonl);
        let instances = load_instances(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].instance_id, "ok");
        assert_eq!(instances[1].instance_id, "ok2");
    }

    #[test]
    fn test_load_instances_filtered() {
        let jsonl = "\
{\"instance_id\":\"a\",\"repo\":\"a/b\",\"base_commit\":\"c\",\"problem_statement\":\"\"}\n\
{\"instance_id\":\"b\",\"repo\":\"a/b\",\"base_commit\":\"c\",\"problem_statement\":\"\"}\n\
{\"instance_id\":\"c\",\"repo\":\"a/b\",\"base_commit\":\"c\",\"problem_statement\":\"\"}\n";
        let path = write_temp_jsonl(jsonl);
        let ids = vec!["a".to_string(), "c".to_string()];
        let instances = load_instances_filtered(&path, &ids);
        let _ = std::fs::remove_file(&path);

        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].instance_id, "a");
        assert_eq!(instances[1].instance_id, "c");
    }

    #[test]
    fn test_load_instances_filtered_empty_ids_returns_all() {
        let jsonl = "\
{\"instance_id\":\"a\",\"repo\":\"a/b\",\"base_commit\":\"c\",\"problem_statement\":\"\"}\n\
{\"instance_id\":\"b\",\"repo\":\"a/b\",\"base_commit\":\"c\",\"problem_statement\":\"\"}\n";
        let path = write_temp_jsonl(jsonl);
        let instances = load_instances_filtered(&path, &[]);
        let _ = std::fs::remove_file(&path);
        assert_eq!(instances.len(), 2);
    }

    #[test]
    fn test_fail_to_pass_accepts_string_or_array() {
        // Some `datasets` exports emit FAIL_TO_PASS/PASS_TO_PASS as real JSON
        // arrays instead of strings. Both must load without error.
        let jsonl = "\
{\"instance_id\":\"a\",\"repo\":\"a/b\",\"base_commit\":\"c\",\"problem_statement\":\"\",\"FAIL_TO_PASS\":\"[\\\"t1\\\"]\",\"PASS_TO_PASS\":\"[]\"}\n\
{\"instance_id\":\"b\",\"repo\":\"a/b\",\"base_commit\":\"c\",\"problem_statement\":\"\",\"FAIL_TO_PASS\":[\"t1\",\"t2\"],\"PASS_TO_PASS\":[]}\n";
        let path = write_temp_jsonl(jsonl);
        let instances = load_instances(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(instances.len(), 2);
        // string form → kept as-is
        assert_eq!(instances[0].fail_to_pass, "[\"t1\"]");
        // array form → normalized back to the string form
        assert_eq!(instances[1].fail_to_pass, "[\"t1\",\"t2\"]");
        assert_eq!(instances[1].pass_to_pass, "[]");
    }
}
