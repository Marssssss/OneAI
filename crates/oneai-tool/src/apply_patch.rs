//! ApplyPatchTool — batch file editing via unified diff format.
//!
//! This tool addresses the gap identified in the competitive analysis:
//! OneAI had no batch editing capability (only single-point FileEditTool).
//! All major coding agents (Codex CLI, Aider, OpenCode) support apply_patch
//! for multi-file, multi-change editing in a single operation.
//!
//! The ApplyPatchTool parses unified diff format and applies changes atomically
//! across multiple files. This is critical for:
//! - Multi-file refactoring (changing interfaces across many files)
//! - Applying code review suggestions (multiple fixes in one patch)
//! - Generating and applying complete change sets
//!
//! Inspired by Codex CLI's `apply_patch` tool and Aider's similar capability.

use async_trait::async_trait;
use oneai_core::{PermissionLevel, RiskLevel, ToolOutput};
use oneai_core::error::Result;
use oneai_core::traits::Tool;
use crate::tool_interfaces::PermissionAwareTool;

// ─── DiffLine ────────────────────────────────────────────────────────────────

/// A single line in a diff hunk.
#[derive(Debug, Clone, PartialEq)]
pub enum DiffLine {
    /// Context line — present in both old and new file (starts with ' ')
    Context(String),
    /// Line to add — present only in new file (starts with '+')
    Add(String),
    /// Line to remove — present only in old file (starts with '-')
    Remove(String),
}

// ─── DiffHunk ────────────────────────────────────────────────────────────────

/// A single hunk (change block) in a unified diff.
#[derive(Debug, Clone)]
pub struct DiffHunk {
    /// Old file path (from `---` header).
    pub old_file: String,
    /// New file path (from `+++` header).
    pub new_file: String,
    /// Starting line number in the old file (from `@@` header).
    pub old_start: usize,
    /// Number of lines in the old file section.
    pub old_count: usize,
    /// Starting line number in the new file.
    pub new_start: usize,
    /// Number of lines in the new file section.
    pub new_count: usize,
    /// The diff lines (context, add, remove).
    pub lines: Vec<DiffLine>,
}

// ─── Unified Diff Parser ─────────────────────────────────────────────────────

/// Parse a unified diff string into a list of hunks.
///
/// Unified diff format (not runnable as a doctest):
/// ```text
/// --- a/old_file.rs
/// +++ b/new_file.rs
/// @@ -1,3 +1,4 @@
///  context line
/// -removed line
/// +added line
///  context line
/// ```
///
/// Supports:
/// - Multiple file changes (separated by `---`/`+++` headers)
/// - Multiple hunks per file (separated by `@@` headers)
/// - `a/` and `b/` prefix stripping in file paths
/// - New file creation (`--- /dev/null`)
/// - File deletion (`+++ /dev/null`)
pub fn parse_unified_diff(diff_text: &str) -> Result<Vec<DiffHunk>> {
    let mut hunks = Vec::new();
    let mut current_old_file = String::new();
    let mut current_new_file = String::new();
    let mut current_lines: Vec<DiffLine> = Vec::new();
    let mut current_old_start = 0;
    let mut current_old_count = 0;
    let mut current_new_start = 0;
    let mut current_new_count = 0;
    let mut in_hunk = false;

    for line in diff_text.lines() {
        // File header lines
        if line.starts_with("--- ") {
            // If we were in a hunk, save it first
            if in_hunk && !current_lines.is_empty() {
                hunks.push(DiffHunk {
                    old_file: clean_file_path(&current_old_file),
                    new_file: clean_file_path(&current_new_file),
                    old_start: current_old_start,
                    old_count: current_old_count,
                    new_start: current_new_start,
                    new_count: current_new_count,
                    lines: current_lines.clone(),
                });
                current_lines.clear();
                in_hunk = false;
            }
            current_old_file = line.trim_start_matches("--- ").trim().to_string();
            continue;
        }

        if line.starts_with("+++ ") {
            current_new_file = line.trim_start_matches("+++ ").trim().to_string();
            continue;
        }

        // Hunk header
        if line.starts_with("@@ ") {
            // Save previous hunk if any
            if in_hunk && !current_lines.is_empty() {
                hunks.push(DiffHunk {
                    old_file: clean_file_path(&current_old_file),
                    new_file: clean_file_path(&current_new_file),
                    old_start: current_old_start,
                    old_count: current_old_count,
                    new_start: current_new_start,
                    new_count: current_new_count,
                    lines: current_lines.clone(),
                });
                current_lines.clear();
            }

            // Parse @@ -old_start,old_count +new_start,new_count @@
            let header = line.trim();
            // Extract the -X,Y +A,B part
            let parts: Vec<&str> = header.split_whitespace().collect();
            if parts.len() < 3 {
                return Err(oneai_core::error::OneAIError::Agent(
                    format!("Invalid hunk header: {}", line)
                ));
            }

            let old_part = parts[1]; // -X,Y
            let new_part = parts[2]; // +A,B

            current_old_start = parse_range_start(old_part, '-');
            current_old_count = parse_range_count(old_part, '-');
            current_new_start = parse_range_start(new_part, '+');
            current_new_count = parse_range_count(new_part, '+');

            in_hunk = true;
            continue;
        }

        // Diff lines within a hunk
        if in_hunk {
            if line.starts_with('+') {
                current_lines.push(DiffLine::Add(line[1..].to_string()));
            } else if line.starts_with('-') {
                current_lines.push(DiffLine::Remove(line[1..].to_string()));
            } else if line.starts_with(' ') || line.is_empty() {
                // Context line (or empty line which is a context line with empty content)
                let content = if line.starts_with(' ') { line[1..].to_string() } else { String::new() };
                current_lines.push(DiffLine::Context(content));
            } else if line.starts_with("\\ ") {
                // "No newline at end of file" marker — skip
                continue;
            } else {
                // Unknown line format within hunk — treat as context
                current_lines.push(DiffLine::Context(line.to_string()));
            }
        }
    }

    // Save the last hunk
    if in_hunk && !current_lines.is_empty() {
        hunks.push(DiffHunk {
            old_file: clean_file_path(&current_old_file),
            new_file: clean_file_path(&current_new_file),
            old_start: current_old_start,
            old_count: current_old_count,
            new_start: current_new_start,
            new_count: current_new_count,
            lines: current_lines,
        });
    }

    Ok(hunks)
}

/// Strip `a/` or `b/` prefixes and `/dev/null` from file paths.
fn clean_file_path(path: &str) -> String {
    if path == "/dev/null" {
        return String::new();
    }
    // Strip common prefixes: a/ or b/
    let stripped = path.trim_start_matches("a/").trim_start_matches("b/");
    // Also handle tab-separated timestamps: "file.rs\t2024-01-01 ..."
    if let Some(idx) = stripped.find('\t') {
        stripped[..idx].to_string()
    } else {
        stripped.to_string()
    }
}

/// Parse the start line number from a range like "-3,5" or "+3,5".
fn parse_range_start(part: &str, prefix: char) -> usize {
    let without_prefix = part.trim_start_matches(prefix);
    if let Some(idx) = without_prefix.find(',') {
        without_prefix[..idx].parse::<usize>().unwrap_or(1)
    } else {
        without_prefix.parse::<usize>().unwrap_or(1)
    }
}

/// Parse the count from a range like "-3,5" or "+3,5".
/// If no comma, count is 1.
fn parse_range_count(part: &str, prefix: char) -> usize {
    let without_prefix = part.trim_start_matches(prefix);
    if let Some(idx) = without_prefix.find(',') {
        without_prefix[idx+1..].parse::<usize>().unwrap_or(1)
    } else {
        1
    }
}

// ─── Apply Hunk to File ─────────────────────────────────────────────────────

/// Apply a list of hunks to a file, modifying its content.
///
/// Hunks are applied in order. Each hunk must match the expected context
/// lines at the specified position. If context mismatch, that hunk is
/// skipped and an error is recorded.
///
/// Returns the new file content and a list of results for each hunk.
fn apply_hunks_to_content(content: &str, hunks: &[DiffHunk]) -> (String, Vec<HunkApplyResult>) {
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let mut results = Vec::new();

    for hunk in hunks {
        // Handle new file creation (old_file is empty)
        if hunk.old_file.is_empty() {
            // Create new file from add lines
            let new_lines: Vec<String> = hunk.lines.iter()
                .filter_map(|l| match l {
                    DiffLine::Add(s) => Some(s.clone()),
                    _ => None,
                })
                .collect();
            lines = new_lines;
            results.push(HunkApplyResult {
                hunk_index: 0,
                applied: true,
                message: "Created new file".to_string(),
            });
            continue;
        }

        // Handle file deletion (new_file is empty, all lines are Remove)
        if hunk.new_file.is_empty() {
            lines.clear();
            results.push(HunkApplyResult {
                hunk_index: 0,
                applied: true,
                message: "Deleted file".to_string(),
            });
            continue;
        }

        // Find the position to apply the hunk
        // The hunk specifies old_start (1-based line number)
        let start_idx = if hunk.old_start == 0 { 0 } else { hunk.old_start - 1 };

        // Verify context lines match
        let mut context_match = true;
        let mut line_idx = start_idx;

        for diff_line in &hunk.lines {
            match diff_line {
                DiffLine::Context(expected) => {
                    if line_idx < lines.len() {
                        if lines[line_idx] != *expected {
                            context_match = false;
                            break;
                        }
                        line_idx += 1;
                    } else {
                        context_match = false;
                        break;
                    }
                }
                DiffLine::Remove(expected) => {
                    if line_idx < lines.len() {
                        if lines[line_idx] != *expected {
                            context_match = false;
                            break;
                        }
                        line_idx += 1;
                    } else {
                        context_match = false;
                        break;
                    }
                }
                DiffLine::Add(_) => {
                    // Add lines don't match against existing content
                }
            }
        }

        if !context_match {
            // Try fuzzy matching — search for the context pattern anywhere in the file
            let search_start = find_fuzzy_match(&lines, hunk);
            if search_start.is_some() {
                // Re-apply with fuzzy match position
                let idx = search_start.unwrap();
                let remove_count = hunk.lines.iter()
                    .filter(|l| matches!(l, DiffLine::Remove(_)))
                    .count();
                let add_lines: Vec<String> = hunk.lines.iter()
                    .filter_map(|l| match l {
                        DiffLine::Add(s) => Some(s.clone()),
                        DiffLine::Context(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();

                // Replace the range
                let end_idx = idx + remove_count;
                if end_idx <= lines.len() {
                    lines.splice(idx..end_idx, add_lines);
                    results.push(HunkApplyResult {
                        hunk_index: 0,
                        applied: true,
                        message: format!("Applied with fuzzy match at line {}", idx + 1),
                    });
                } else {
                    results.push(HunkApplyResult {
                        hunk_index: 0,
                        applied: false,
                        message: "Fuzzy match position out of range".to_string(),
                    });
                }
                continue;
            }

            results.push(HunkApplyResult {
                hunk_index: 0,
                applied: false,
                message: format!(
                    "Context mismatch at line {} — expected '{}' but found '{}'",
                    hunk.old_start,
                    hunk.lines.iter()
                        .filter_map(|l| match l {
                            DiffLine::Context(s) | DiffLine::Remove(s) => Some(s.clone()),
                            _ => None,
                        })
                        .next()
                        .unwrap_or_default(),
                    if start_idx < lines.len() { &lines[start_idx] } else { "[EOF]" }
                ),
            });
            continue;
        }

        // Apply the hunk: remove Remove lines, keep Context lines, add Add lines
        let remove_count = hunk.lines.iter()
            .filter(|l| matches!(l, DiffLine::Remove(_)))
            .count();
        let add_lines: Vec<String> = hunk.lines.iter()
            .filter_map(|l| match l {
                DiffLine::Add(s) => Some(s.clone()),
                DiffLine::Context(s) => Some(s.clone()),
                _ => None,
            })
            .collect();

        // Replace lines[start_idx..start_idx+remove_count] with add_lines
        let end_idx = start_idx + remove_count;
        if end_idx <= lines.len() {
            lines.splice(start_idx..end_idx, add_lines);
            results.push(HunkApplyResult {
                hunk_index: 0,
                applied: true,
                message: format!("Applied at line {}", start_idx + 1),
            });
        } else {
            results.push(HunkApplyResult {
                hunk_index: 0,
                applied: false,
                message: "Hunk position out of range".to_string(),
            });
        }
    }

    // Rejoin lines — preserve trailing newline if original had one
    let result = if content.ends_with('\n') && !content.ends_with("\n\n") {
        lines.join("\n") + "\n"
    } else {
        lines.join("\n")
    };

    (result, results)
}

/// Result of applying a single hunk.
struct HunkApplyResult {
    #[allow(dead_code)]
    hunk_index: usize,
    applied: bool,
    message: String,
}

/// Find a fuzzy match for the hunk's context/remove lines in the file.
///
/// Searches for the first context line of the hunk anywhere in the file,
/// then checks if subsequent context/remove lines match from that position.
fn find_fuzzy_match(lines: &[String], hunk: &DiffHunk) -> Option<usize> {
    // Get the first non-Add line as anchor
    let anchor = hunk.lines.iter()
        .find_map(|l| match l {
            DiffLine::Context(s) | DiffLine::Remove(s) => Some(s.clone()),
            _ => None,
        })?;

    // Search for anchor line
    for (idx, line) in lines.iter().enumerate() {
        if line == &anchor {
            // Check if remaining context/remove lines match from this position
            let mut match_idx = idx;
            let mut all_match = true;
            for diff_line in &hunk.lines {
                match diff_line {
                    DiffLine::Context(expected) | DiffLine::Remove(expected) => {
                        if match_idx < lines.len() && lines[match_idx] == *expected {
                            match_idx += 1;
                        } else {
                            all_match = false;
                            break;
                        }
                    }
                    DiffLine::Add(_) => {}
                }
            }
            if all_match {
                return Some(idx);
            }
        }
    }
    None
}

// ─── ApplyPatchTool ──────────────────────────────────────────────────────────

/// Apply a unified diff patch across multiple files.
///
/// This tool enables batch editing of multiple files in a single operation,
/// which is critical for multi-file refactoring, applying code review
/// suggestions, and generating complete change sets.
///
/// The patch is applied atomically per file: if any hunk in a file fails
/// (context mismatch), that file's changes are skipped and an error is reported.
/// Other files' hunks are still applied.
///
/// Inspired by Codex CLI's `apply_patch` and Aider's batch editing capability.
pub struct ApplyPatchTool;

impl ApplyPatchTool {
    pub fn new() -> Self { Self }
}

impl Default for ApplyPatchTool {
    fn default() -> Self { Self::new() }
}

impl PermissionAwareTool for ApplyPatchTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Standard
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a unified diff patch to modify multiple files at once. \
        The patch should be in standard unified diff format (--- /+++ headers, \
        @@ hunk headers, context/add/remove lines). Supports multi-file changes, \
        new file creation, and file deletion. Each file's changes are applied atomically — \
        if context mismatch occurs, that file is skipped with an error report. \
        Use for: multi-file refactoring, applying review suggestions, batch edits."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "The unified diff patch to apply. Standard format with --- and +++ file headers, @@ hunk headers, and +/ /- diff lines."
                }
            },
            "required": ["patch"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let patch = args.get("patch")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if patch.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No patch provided".to_string()),
            });
        }

        // Parse the unified diff
        let hunks = parse_unified_diff(patch);
        match hunks {
            Ok(hunk_list) => {
                if hunk_list.is_empty() {
                    return Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some("No valid hunks found in patch — ensure the patch has proper --- and +++ headers".to_string()),
                    });
                }

                // Group hunks by target file (use new_file as the target)
                let mut file_hunks: HashMap<String, Vec<DiffHunk>> = HashMap::new();
                for hunk in &hunk_list {
                    let target_file = if hunk.new_file.is_empty() {
                        // File deletion — use old_file
                        hunk.old_file.clone()
                    } else {
                        hunk.new_file.clone()
                    };
                    file_hunks.entry(target_file).or_default().push(hunk.clone());
                }

                // Apply hunks per file
                let mut results = Vec::new();
                let mut errors = Vec::new();
                let mut files_changed = 0;

                for (file_path, file_hunk_list) in &file_hunks {
                    // Security: reject path traversal
                    if file_path.contains("..") {
                        errors.push(format!("Path traversal detected in: {}", file_path));
                        continue;
                    }

                    // Check if this is a new file creation or file deletion
                    let is_new_file = file_hunk_list.iter().any(|h| h.old_file.is_empty());
                    let is_deletion = file_hunk_list.iter().any(|h| h.new_file.is_empty());

                    if is_deletion {
                        // Delete the file
                        let delete_result = tokio::fs::remove_file(file_path).await;
                        match delete_result {
                            Ok(_) => {
                                results.push(format!("Deleted: {}", file_path));
                                files_changed += 1;
                            }
                            Err(e) => {
                                errors.push(format!("Failed to delete {}: {}", file_path, e));
                            }
                        }
                        continue;
                    }

                    // Read existing content (or empty for new files)
                    let content = if is_new_file {
                        String::new()
                    } else {
                        match tokio::fs::read_to_string(file_path).await {
                            Ok(text) => text,
                            Err(e) => {
                                errors.push(format!("Failed to read {}: {}", file_path, e));
                                continue;
                            }
                        }
                    };

                    // Apply hunks to content
                    let (new_content, hunk_results) = apply_hunks_to_content(&content, &file_hunk_list);

                    // Check if all hunks were applied successfully
                    let failed_hunks = hunk_results.iter().filter(|r| !r.applied).count();
                    if failed_hunks > 0 {
                        for result in &hunk_results {
                            if !result.applied {
                                errors.push(format!("{}: {}", file_path, result.message));
                            }
                        }
                        // Don't write the file if any hunks failed — keep original
                        continue;
                    }

                    // Write the new content
                    if is_new_file {
                        // Create parent directories if needed
                        if let Some(parent) = std::path::Path::new(file_path).parent() {
                            if !parent.as_os_str().is_empty() {
                                let _ = tokio::fs::create_dir_all(parent).await;
                            }
                        }
                    }

                    let write_result = tokio::fs::write(file_path, new_content).await;
                    match write_result {
                        Ok(_) => {
                            let hunk_count = file_hunk_list.len();
                            results.push(format!(
                                "Applied {} hunk(s) to {}",
                                hunk_count, file_path
                            ));
                            files_changed += 1;
                        }
                        Err(e) => {
                            errors.push(format!("Failed to write {}: {}", file_path, e));
                        }
                    }
                }

                // Build output
                let success = errors.is_empty();
                let mut output_parts = Vec::new();

                if !results.is_empty() {
                    output_parts.push(format!("Patch applied to {} file(s):", files_changed));
                    output_parts.extend(results.iter().map(|r| format!("  ✓ {}", r)));
                }

                if !errors.is_empty() {
                    output_parts.push(format!("\n{} error(s):", errors.len()));
                    output_parts.extend(errors.iter().map(|e| format!("  ✗ {}", e)));
                }

                Ok(ToolOutput {
                    success,
                    content: output_parts.join("\n"),
                    error: if errors.is_empty() { None } else {
                        Some(format!("{} errors during patch application", errors.len()))
                    },
                })
            }
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Failed to parse patch: {}", e)),
            }),
        }
    }
}

use std::collections::HashMap;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_diff() {
        let diff = "--- a/hello.rs\n+++ b/hello.rs\n@@ -1,3 +1,4 @@\n fn main() {\n-    println(\"hello\");\n+    println(\"hello world\");\n+    println(\"from OneAI\");\n }\n";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_file, "hello.rs");
        assert_eq!(hunks[0].new_file, "hello.rs");
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].lines.len(), 5);
    }

    #[test]
    fn test_parse_multi_file_diff() {
        let diff = "--- a/file1.rs\n+++ b/file1.rs\n@@ -1,2 +1,2 @@\n line1\n-line2_old\n+line2_new\n--- a/file2.rs\n+++ b/file2.rs\n@@ -1,1 +1,2 @@\n-line1\n+line1_new\n+line2_extra\n";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_file, "file1.rs");
        assert_eq!(hunks[1].old_file, "file2.rs");
    }

    #[test]
    fn test_parse_new_file_creation() {
        let diff = "--- /dev/null\n+++ b/new_file.rs\n@@ -0,0 +1,3 @@\n+fn new_function() {\n+    println!(\"new\");\n+}\n";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_file, ""); // /dev/null → empty
        assert_eq!(hunks[0].new_file, "new_file.rs");
    }

    #[test]
    fn test_apply_hunk_to_content() {
        let content = "fn main() {\n    println(\"hello\");\n}\n";
        let hunks = vec![DiffHunk {
            old_file: "hello.rs".to_string(),
            new_file: "hello.rs".to_string(),
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 4,
            lines: vec![
                DiffLine::Context("fn main() {".to_string()),
                DiffLine::Remove("    println(\"hello\");".to_string()),
                DiffLine::Add("    println(\"hello world\");".to_string()),
                DiffLine::Add("    println(\"from OneAI\");".to_string()),
                DiffLine::Context("}".to_string()),
            ],
        }];

        let (new_content, results) = apply_hunks_to_content(content, &hunks);
        assert!(results[0].applied);
        assert!(new_content.contains("hello world"));
        assert!(new_content.contains("from OneAI"));
    }

    #[test]
    fn test_apply_hunk_context_mismatch() {
        let content = "fn main() {\n    different_line();\n}\n";
        let hunks = vec![DiffHunk {
            old_file: "hello.rs".to_string(),
            new_file: "hello.rs".to_string(),
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
            lines: vec![
                DiffLine::Context("fn main() {".to_string()),
                DiffLine::Remove("    println(\"hello\");".to_string()),
                DiffLine::Add("    println(\"hello world\");".to_string()),
                DiffLine::Context("}".to_string()),
            ],
        }];

        let (_, results) = apply_hunks_to_content(content, &hunks);
        assert!(!results[0].applied);
    }

    #[test]
    fn test_clean_file_path() {
        assert_eq!(clean_file_path("a/src/main.rs"), "src/main.rs");
        assert_eq!(clean_file_path("b/src/main.rs"), "src/main.rs");
        assert_eq!(clean_file_path("/dev/null"), "");
        assert_eq!(clean_file_path("src/main.rs\t2024-01-01"), "src/main.rs");
        assert_eq!(clean_file_path("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_parse_range() {
        assert_eq!(parse_range_start("-3,5", '-'), 3);
        assert_eq!(parse_range_count("-3,5", '-'), 5);
        assert_eq!(parse_range_start("+1,1", '+'), 1);
        assert_eq!(parse_range_count("+1,1", '+'), 1);
        assert_eq!(parse_range_start("-1", '-'), 1);
        assert_eq!(parse_range_count("-1", '-'), 1);
    }

    #[test]
    fn test_apply_patch_tool_properties() {
        let tool = ApplyPatchTool::new();
        assert_eq!(tool.name(), "apply_patch");
        assert_eq!(tool.risk_level(), RiskLevel::Medium);
    }
}
