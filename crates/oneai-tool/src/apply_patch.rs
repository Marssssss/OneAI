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

use crate::tool_interfaces::PermissionAwareTool;
use async_trait::async_trait;
use oneai_core::error::Result;
use oneai_core::traits::Tool;
use oneai_core::{Artifact, PermissionLevel, RiskLevel, ToolOutput};

use crate::local_tools::infer_mime;

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
                return Err(oneai_core::error::OneAIError::Agent(format!(
                    "Invalid hunk header: {}",
                    line
                )));
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
            if let Some(content) = line.strip_prefix('+') {
                current_lines.push(DiffLine::Add(content.to_string()));
            } else if let Some(content) = line.strip_prefix('-') {
                current_lines.push(DiffLine::Remove(content.to_string()));
            } else if line.starts_with(' ') || line.is_empty() {
                // Context line (or empty line which is a context line with empty content)
                let content = if let Some(rest) = line.strip_prefix(' ') {
                    rest.to_string()
                } else {
                    String::new()
                };
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

/// Strip timestamps, quotes, `a/` or `b/` prefixes and `/dev/null` from
/// diff-header file paths.
fn clean_file_path(path: &str) -> String {
    let path = path.trim();
    // Tab-separated timestamp suffix ("file.rs\t2024-01-01 ...") first — the
    // timestamp sits OUTSIDE any quotes, so this must precede unquoting.
    let path = match path.find('\t') {
        Some(idx) => &path[..idx],
        None => path,
    };
    // Git wraps paths with special characters in C-style double quotes
    // (`core.quotePath`), and models also emit plain `"file name.md"`
    // headers. Unquote so the quotes don't become part of the file name
    // (2026-09 webUI verify-session: a literal `"剪映….md"` — quotes
    // included — was created on disk and cost 4 extra verification rounds).
    let path = unquote_diff_path(path);
    if path == "/dev/null" {
        return String::new();
    }
    // Strip common prefixes: a/ or b/
    let stripped = path.trim_start_matches("a/").trim_start_matches("b/");
    // Mixed form — prefix outside the quotes: `b/"file name.md"`.
    unquote_diff_path(stripped)
}

/// Unquote a diff-header path quoted in git's C-style (`core.quotePath`):
/// `"docs/\346\226\207\344\273\266.md"` → `docs/文件.md`. Supports the
/// standard escapes (`\"` `\\` `\t` `\n` `\r` `\a` `\b` `\f` `\v` and octal
/// `\NNN`, which git uses for non-ASCII bytes). Paths without a balanced
/// pair of enclosing quotes are returned unchanged.
fn unquote_diff_path(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return path.to_string();
    }
    let inner = &path[1..path.len() - 1];
    // Fast path: no escapes — just drop the enclosing quotes.
    if !inner.contains('\\') {
        return inner.to_string();
    }
    let b = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            match b[i + 1] {
                b'"' => out.push(b'"'),
                b'\\' => out.push(b'\\'),
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'r' => out.push(b'\r'),
                b'a' => out.push(0x07),
                b'b' => out.push(0x08),
                b'f' => out.push(0x0C),
                b'v' => out.push(0x0B),
                b'0'..=b'7' => {
                    // Octal byte escape — up to 3 digits (git's encoding of
                    // each UTF-8 byte under core.quotePath, e.g. \346).
                    let mut val: u32 = 0;
                    let mut j = i + 1;
                    let mut digits = 0;
                    while j < b.len() && digits < 3 && matches!(b[j], b'0'..=b'7') {
                        val = val * 8 + (b[j] - b'0') as u32;
                        j += 1;
                        digits += 1;
                    }
                    out.push((val & 0xFF) as u8);
                    i = j;
                    continue;
                }
                other => {
                    // Unknown escape — keep both characters verbatim.
                    out.push(b'\\');
                    out.push(other);
                }
            }
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
        without_prefix[idx + 1..].parse::<usize>().unwrap_or(1)
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
            let new_lines: Vec<String> = hunk
                .lines
                .iter()
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
        let start_idx = if hunk.old_start == 0 {
            0
        } else {
            hunk.old_start - 1
        };

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
            if let Some(idx) = search_start {
                // Re-apply with fuzzy match position
                let remove_count = hunk
                    .lines
                    .iter()
                    .filter(|l| matches!(l, DiffLine::Remove(_)))
                    .count();
                let add_lines: Vec<String> = hunk
                    .lines
                    .iter()
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
                    hunk.lines
                        .iter()
                        .filter_map(|l| match l {
                            DiffLine::Context(s) | DiffLine::Remove(s) => Some(s.clone()),
                            _ => None,
                        })
                        .next()
                        .unwrap_or_default(),
                    if start_idx < lines.len() {
                        &lines[start_idx]
                    } else {
                        "[EOF]"
                    }
                ),
            });
            continue;
        }

        // Apply the hunk: remove Remove lines, keep Context lines, add Add lines
        let remove_count = hunk
            .lines
            .iter()
            .filter(|l| matches!(l, DiffLine::Remove(_)))
            .count();
        let add_lines: Vec<String> = hunk
            .lines
            .iter()
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
    let anchor = hunk.lines.iter().find_map(|l| match l {
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

// ─── Backup / undo (gap P2 #12) ─────────────────────────────────────────────

/// In-memory snapshot of one patch target taken before the commit phase
/// (gap-analysis P2 #12 — a multi-file patch must never leave a
/// half-modified state). `original == None` means the file did not exist
/// before the patch — undo then deletes the created file.
#[derive(Debug, Clone)]
struct FileBackup {
    path: String,
    original: Option<String>,
}

/// The undo stack replay: restore every backup in reverse order. Returns
/// human-readable errors for any file that could not be restored (the
/// caller surfaces them; restore itself is best-effort and never panics).
async fn restore_backups(backups: &[FileBackup]) -> Vec<String> {
    let mut restore_errors = Vec::new();
    for backup in backups.iter().rev() {
        match &backup.original {
            Some(content) => {
                if let Err(e) = tokio::fs::write(&backup.path, content).await {
                    restore_errors.push(format!(
                        "CRITICAL: failed to restore {} during undo: {}",
                        backup.path, e
                    ));
                }
            }
            None => {
                // File did not exist before the patch — undo = delete it.
                match tokio::fs::remove_file(&backup.path).await {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => restore_errors.push(format!(
                        "CRITICAL: failed to remove created {} during undo: {}",
                        backup.path, e
                    )),
                }
            }
        }
    }
    restore_errors
}

// ─── ApplyPatchTool ──────────────────────────────────────────────────────────

/// Apply a unified diff patch across multiple files.
///
/// This tool enables batch editing of multiple files in a single operation,
/// which is critical for multi-file refactoring, applying code review
/// suggestions, and generating complete change sets.
///
/// The patch is applied **all-or-nothing** (gap-analysis P2 #12): first a
/// dry-run computes every file's new content in memory; if ANY file fails
/// validation (path traversal, read error, hunk context mismatch) the whole
/// patch aborts and no file is touched. The commit phase snapshots each
/// target in a backup stack before writing, so even a mid-commit IO failure
/// rolls back every file already modified — a patch can never leave a
/// half-modified state.
///
/// Inspired by Codex CLI's `apply_patch` and Aider's batch editing capability.
pub struct ApplyPatchTool;

impl ApplyPatchTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ApplyPatchTool {
    fn default() -> Self {
        Self::new()
    }
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
        new file creation, and file deletion. Application is all-or-nothing: \
        if any hunk fails (context mismatch) or any file can't be read, NO file \
        is modified and the error is reported. \
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
        let patch = args.get("patch").and_then(|v| v.as_str()).unwrap_or("");

        if patch.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No patch provided".to_string()),
                ..Default::default()
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
                     ..Default::default() });
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
                    file_hunks
                        .entry(target_file)
                        .or_default()
                        .push(hunk.clone());
                }

                // ─── Phase 1: dry-run (gap P2 #12) ──────────────────────────
                // Compute every file's new content IN MEMORY before touching
                // the filesystem. If any file fails validation (path
                // traversal, read error, hunk context mismatch), the whole
                // patch aborts and NOTHING is written — a multi-file patch
                // can no longer leave a half-modified state.
                struct PlannedChange {
                    path: String,
                    is_new_file: bool,
                    is_deletion: bool,
                    new_content: String,
                    hunk_count: usize,
                }

                let mut results = Vec::new();
                let mut errors = Vec::new();
                let mut files_changed = 0;
                let mut artifacts: Vec<Artifact> = Vec::new();
                let mut planned: Vec<PlannedChange> = Vec::new();

                for (file_path, file_hunk_list) in &file_hunks {
                    // Security: reject path traversal
                    if crate::tool_interfaces::path_has_traversal(file_path) {
                        errors.push(format!("Path traversal detected in: {}", file_path));
                        continue;
                    }

                    // Resolve relative paths against the active session
                    // workspace (active_cwd) — a background sub-agent inherits
                    // the parent turn's workspace but, without this, the raw
                    // tokio::fs call below would resolve `gomoku/main.js`
                    // against the process cwd (the binary's launch dir) and
                    // write outside the workspace the parent is operating in.
                    let file_path = oneai_core::active_cwd::resolve(file_path);

                    // Check if this is a new file creation or file deletion
                    let is_new_file = file_hunk_list.iter().any(|h| h.old_file.is_empty());
                    let is_deletion = file_hunk_list.iter().any(|h| h.new_file.is_empty());

                    if is_deletion {
                        // Deletion is validated by existence — a missing
                        // target is a patch error caught here, pre-commit.
                        match tokio::fs::read_to_string(&file_path).await {
                            Ok(_) => planned.push(PlannedChange {
                                path: file_path,
                                is_new_file: false,
                                is_deletion: true,
                                new_content: String::new(),
                                hunk_count: file_hunk_list.len(),
                            }),
                            Err(e) => errors
                                .push(format!("Failed to read {} for deletion: {}", file_path, e)),
                        }
                        continue;
                    }

                    // Read existing content (or empty for new files)
                    let content = if is_new_file {
                        String::new()
                    } else {
                        match tokio::fs::read_to_string(&file_path).await {
                            Ok(text) => text,
                            Err(e) => {
                                errors.push(format!("Failed to read {}: {}", file_path, e));
                                continue;
                            }
                        }
                    };

                    // Apply hunks to the in-memory copy only
                    let (new_content, hunk_results) =
                        apply_hunks_to_content(&content, file_hunk_list);

                    // Check if all hunks were applied successfully
                    let failed_hunks = hunk_results.iter().filter(|r| !r.applied).count();
                    if failed_hunks > 0 {
                        for result in &hunk_results {
                            if !result.applied {
                                errors.push(format!("{}: {}", file_path, result.message));
                            }
                        }
                        continue;
                    }

                    planned.push(PlannedChange {
                        path: file_path,
                        is_new_file,
                        is_deletion: false,
                        new_content,
                        hunk_count: file_hunk_list.len(),
                    });
                }

                // Any validation failure aborts the whole patch — zero
                // filesystem writes (all-or-nothing semantics).
                if !errors.is_empty() {
                    errors.push(
                        "Patch aborted before any file was modified (all-or-nothing)".to_string(),
                    );
                }

                // ─── Phase 2: commit with backup/undo (gap P2 #12) ──────────
                // Snapshot each target before writing; if any write/delete
                // fails mid-commit, replay the backup stack to roll back
                // every file already touched.
                if errors.is_empty() {
                    let mut backups: Vec<FileBackup> = Vec::new();
                    let mut commit_failed = false;

                    for change in &planned {
                        // Backup first (None = file did not exist).
                        let original = if change.is_new_file {
                            None
                        } else {
                            match tokio::fs::read_to_string(&change.path).await {
                                Ok(text) => Some(text),
                                Err(e) => {
                                    errors
                                        .push(format!("Failed to back up {}: {}", change.path, e));
                                    commit_failed = true;
                                    break;
                                }
                            }
                        };
                        backups.push(FileBackup {
                            path: change.path.clone(),
                            original,
                        });

                        // Then commit.
                        let outcome = if change.is_deletion {
                            tokio::fs::remove_file(&change.path).await
                        } else {
                            if change.is_new_file {
                                // Create parent directories if needed
                                if let Some(parent) = std::path::Path::new(&change.path).parent() {
                                    if !parent.as_os_str().is_empty() {
                                        let _ = tokio::fs::create_dir_all(parent).await;
                                    }
                                }
                            }
                            tokio::fs::write(&change.path, &change.new_content).await
                        };

                        if let Err(e) = outcome {
                            errors.push(format!(
                                "Failed to {} {}: {} — rolling back",
                                if change.is_deletion {
                                    "delete"
                                } else {
                                    "write"
                                },
                                change.path,
                                e
                            ));
                            commit_failed = true;
                            break;
                        }
                    }

                    if commit_failed {
                        // Undo: restore every file touched so far.
                        let mut restore_errors = restore_backups(&backups).await;
                        errors.append(&mut restore_errors);
                    } else {
                        // All committed — report per-file results + artifacts.
                        for change in &planned {
                            if change.is_deletion {
                                results.push(format!("Deleted: {}", change.path));
                            } else {
                                results.push(format!(
                                    "Applied {} hunk(s) to {}",
                                    change.hunk_count, change.path
                                ));
                            }
                            files_changed += 1;
                            // Deliverable surface — the patched file's path so a
                            // frontend can list turn-end outputs (deletions
                            // have no artifact).
                            if !change.is_deletion {
                                let size = tokio::fs::metadata(&change.path)
                                    .await
                                    .map(|m| m.len())
                                    .ok();
                                artifacts.push(Artifact {
                                    path: change.path.clone(),
                                    mime_type: infer_mime(&change.path).to_string(),
                                    description: format!(
                                        "{} {} hunk(s)",
                                        if change.is_new_file {
                                            "created"
                                        } else {
                                            "patched"
                                        },
                                        change.hunk_count
                                    ),
                                    size_bytes: size,
                                });
                            }
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
                    error: if errors.is_empty() {
                        None
                    } else {
                        Some(format!("{} errors during patch application", errors.len()))
                    },
                    artifacts,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Failed to parse patch: {}", e)),
                ..Default::default()
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
        // Quoted header paths (git core.quotePath / model-emitted) — the
        // quotes must NOT become part of the file name (2026-09 incident).
        assert_eq!(clean_file_path("\"剪映CapCut.md\""), "剪映CapCut.md");
        assert_eq!(clean_file_path("a/\"file name.md\""), "file name.md");
        assert_eq!(clean_file_path("b/\"file.md\"\t2024-01-01"), "file.md");
        // A quoted /dev/null is still the deletion marker.
        assert_eq!(clean_file_path("\"/dev/null\""), "");
    }

    #[test]
    fn test_unquote_diff_path_c_style_escapes() {
        // Plain quotes — fast path.
        assert_eq!(unquote_diff_path("\"file name.md\""), "file name.md");
        // Git octal-escaped UTF-8: \346\226\207\346\241\243 = 文档.
        assert_eq!(
            unquote_diff_path(r#""docs/\346\226\207\346\241\243.md""#),
            "docs/文档.md"
        );
        // Escaped quote + backslash + tab.
        assert_eq!(unquote_diff_path(r#""a\"b\\c\td""#), "a\"b\\c\td");
        // No quotes / unbalanced quotes → unchanged.
        assert_eq!(unquote_diff_path("plain.md"), "plain.md");
        assert_eq!(unquote_diff_path("\"unbalanced"), "\"unbalanced");
        assert_eq!(unquote_diff_path("\""), "\"");
        assert_eq!(unquote_diff_path(""), "");
    }

    #[test]
    fn test_parse_diff_with_quoted_paths() {
        // The incident shape: git-style quoted headers must parse to the
        // bare file name, not a quoted one.
        let diff =
            "--- \"a/剪映CapCut.md\"\n+++ \"b/剪映CapCut.md\"\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_file, "剪映CapCut.md");
        assert_eq!(hunks[0].new_file, "剪映CapCut.md");
    }

    #[tokio::test]
    async fn patch_with_quoted_header_targets_unquoted_file() {
        // End-to-end regression (2026-09 verify-session): a patch whose
        // header quotes the path must modify the real file, not create a
        // `"...quoted..."` sibling.
        let dir = temp_dir("quoted-header");
        let target = dir.join("剪映CapCut.md");
        std::fs::write(&target, "line1\nold\n").unwrap();

        let patch = format!(
            "--- \"{f}\"\n+++ \"{f}\"\n@@ -1,2 +1,2 @@\n line1\n-old\n+new\n",
            f = target.display()
        );

        let tool = ApplyPatchTool::new();
        let out = tool
            .execute(serde_json::json!({"patch": patch}))
            .await
            .unwrap();

        assert!(out.success, "output: {}", out.content);
        assert!(std::fs::read_to_string(&target).unwrap().contains("new"));
        // No quoted sibling may have appeared.
        let quoted_sibling = dir.join(format!("\"{}\"", "剪映CapCut.md"));
        assert!(
            !quoted_sibling.exists(),
            "a file with literal quotes must not be created"
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    // ─── All-or-nothing + backup/undo (gap P2 #12) ────────────────────────

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oneai-apply-patch-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn multi_file_patch_with_one_bad_hunk_modifies_nothing() {
        // Gap P2 #12 — file1's hunk is valid, file2's hunk mismatches.
        // Pre-fix behavior left file1 modified (half-state); now the whole
        // patch aborts and BOTH files stay untouched.
        let dir = temp_dir("atomic-abort");
        let f1 = dir.join("file1.txt");
        let f2 = dir.join("file2.txt");
        std::fs::write(&f1, "line1\nline2\n").unwrap();
        std::fs::write(&f2, "alpha\nbeta\n").unwrap();

        let patch = format!(
            "--- a/{f1}\n+++ b/{f1}\n@@ -1,2 +1,2 @@\n line1\n-line2\n+line2_patched\n\
             --- a/{f2}\n+++ b/{f2}\n@@ -1,2 +1,2 @@\n alpha\n-THIS_LINE_DOES_NOT_EXIST\n+gamma\n",
            f1 = f1.display(),
            f2 = f2.display()
        );

        let tool = ApplyPatchTool::new();
        let out = tool
            .execute(serde_json::json!({"patch": patch}))
            .await
            .unwrap();

        assert!(!out.success);
        assert!(out.content.contains("aborted before any file was modified"));
        // Neither file may have changed.
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "line1\nline2\n");
        assert_eq!(std::fs::read_to_string(&f2).unwrap(), "alpha\nbeta\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn successful_multi_file_patch_applies_all() {
        let dir = temp_dir("success");
        let f1 = dir.join("file1.txt");
        let f2 = dir.join("file2.txt");
        std::fs::write(&f1, "line1\nline2\n").unwrap();
        std::fs::write(&f2, "alpha\nbeta\n").unwrap();

        let patch = format!(
            "--- a/{f1}\n+++ b/{f1}\n@@ -1,2 +1,2 @@\n line1\n-line2\n+line2_patched\n\
             --- a/{f2}\n+++ b/{f2}\n@@ -1,2 +1,2 @@\n alpha\n-beta\n+beta_patched\n",
            f1 = f1.display(),
            f2 = f2.display()
        );

        let tool = ApplyPatchTool::new();
        let out = tool
            .execute(serde_json::json!({"patch": patch}))
            .await
            .unwrap();

        assert!(out.success, "output: {}", out.content);
        assert!(std::fs::read_to_string(&f1)
            .unwrap()
            .contains("line2_patched"));
        assert!(std::fs::read_to_string(&f2)
            .unwrap()
            .contains("beta_patched"));
        assert_eq!(out.artifacts.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_backups_rolls_back_modifications_and_creations() {
        // Direct undo-stack test: modified files are restored to their
        // snapshot; files created by the patch (backup None) are deleted.
        let dir = temp_dir("undo");
        let existing = dir.join("existing.txt");
        let created = dir.join("created.txt");
        std::fs::write(&existing, "original content\n").unwrap();
        std::fs::write(&created, "should vanish\n").unwrap();

        let backups = vec![
            FileBackup {
                path: existing.to_string_lossy().into_owned(),
                original: Some("original content\n".to_string()),
            },
            FileBackup {
                path: created.to_string_lossy().into_owned(),
                original: None, // did not exist pre-patch → undo deletes
            },
        ];

        // Simulate a mid-commit state: both files carry wrong content.
        std::fs::write(&existing, "WRONG\n").unwrap();

        let errors = restore_backups(&backups).await;
        assert!(errors.is_empty(), "restore errors: {:?}", errors);
        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            "original content\n"
        );
        assert!(!created.exists(), "created file must be deleted by undo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deletion_patch_validates_target_exists() {
        // Deleting a missing file is caught in the dry-run phase.
        let dir = temp_dir("delete-missing");
        let missing = dir.join("nope.txt");
        let patch = format!(
            "--- a/{f}\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-gone\n",
            f = missing.display()
        );
        let tool = ApplyPatchTool::new();
        let out = tool
            .execute(serde_json::json!({"patch": patch}))
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.content.contains("aborted before any file was modified"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
