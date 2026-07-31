//! `FileOperations` — abstract the file-IO surface so the file tools
//! (`FileReadTool`/`FileEditTool`/`FileWriteTool`/`FileListTool`) no longer
//! call `tokio::fs` directly, but hold an `Arc<dyn FileOperations>` (Phase
//! 4.2, evolution-plan §4.2 — "Gondolin tool-override + Remote Operations").
//!
//! This mirrors the [`crate::terminal::TerminalBackend`] seam (Phase 3.3):
//! `ShellTool` already holds an `Arc<dyn TerminalBackend>` — that *is* the
//! bash-operations interface, so `BashOperations` is not re-invented here.
//! The new surface is the *file*-operations analogue, so a
//! `ContainerizedCodingPack` can route read/edit/write/list through a
//! micro-VM backend (the "VM is the boundary" synthesis — auth stays on the
//! host, tool side-effects go into the VM, permissions are NOT cut).
//!
//! ## Safety boundary (load-bearing)
//!
//! Path-level pre-flight safety (`path_has_traversal`, empty-path check,
//! per-tool `max_size` enforcement) stays in each tool's `execute()` and
//! runs **before** delegating to `FileOperations` — so it applies uniformly
//! to every backend (a traversal attempt must never reach a VM). The IO
//! mechanics (read/write/list/stat) live inside each impl.
//!
//! ## Backends
//!
//! - [`LocalFileOps`] — current behavior, verbatim. Moves the existing
//!   `tokio::fs::read_to_string`/`read`/`write`/`read_dir` logic so the local
//!   path is byte-identical to the pre-refactor tools (discipline #7).
//! - [`RemoteFileOps`] — routes IO through a [`TerminalBackend`] via
//!   `cat`/`base64`/`find -printf`/`stat`. The trusted internal write path
//!   uses `printf '<b64>' | base64 -d > path` (base64 is shell-safe inside
//!   single quotes and carries no quoting/delimiter collision regardless of
//!   content) — this *bypasses* `ShellTool::detect_shell_file_write` by
//!   design: that guard only blocks the *model* authoring `cat >`/heredocs;
//!   `RemoteFileOps` IS the write_file tool's own backend.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};

use crate::terminal::{ExecOptions, ExecResult, TerminalBackend};

// ─── Supporting structs ──────────────────────────────────────────────────────

/// Result of [`FileOperations::read`]. `text` is `Some` when the file decodes
/// as UTF-8; `bytes` is `Some` for binary files (the tool base64-encodes).
/// `total_lines`/`size` let the tool do offset/limit + max-size checks
/// without a second round-trip for the local case.
///
/// `#[non_exhaustive]` per the v0.2.0 stability commitment — downstream must
/// use [`FileReadResult::new`].
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct FileReadResult {
    /// UTF-8 text of the file, when decodable.
    pub text: Option<String>,
    /// Raw bytes, when the file is not valid UTF-8 (binary).
    pub bytes: Option<Vec<u8>>,
    /// Number of lines in `text` (0 for binary).
    pub total_lines: usize,
    /// File size in bytes.
    pub size: u64,
}

impl FileReadResult {
    pub fn new(
        text: Option<String>,
        bytes: Option<Vec<u8>>,
        total_lines: usize,
        size: u64,
    ) -> Self {
        Self {
            text,
            bytes,
            total_lines,
            size,
        }
    }
}

/// One directory entry, returned by [`FileOperations::list_dir`].
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

impl DirEntry {
    pub fn new(name: impl Into<String>, is_dir: bool, size: u64) -> Self {
        Self {
            name: name.into(),
            is_dir,
            size,
        }
    }
}

// ─── FileOperations trait ─────────────────────────────────────────────────────

/// Abstract file-operations backend.
///
/// The file tools hold an `Arc<dyn FileOperations>` and delegate IO (after
/// their own path-safety pre-flight). `LocalFileOps` is the zero-change
/// default; `RemoteFileOps` routes through a [`TerminalBackend`] so a
/// `ContainerizedCodingPack` can keep all file side-effects inside a VM.
#[async_trait]
pub trait FileOperations: Send + Sync {
    /// Read the whole file. The tool does offset/limit slicing + max-size
    /// enforcement on the result (mirroring the pre-refactor behavior).
    async fn read(&self, path: &str) -> Result<FileReadResult>;

    /// Write `content` to `path`. `append=true` appends; `append=false`
    /// overwrites (creating parent dirs as needed — the write_file contract).
    async fn write(&self, path: &str, content: &str, append: bool) -> Result<()>;

    /// List directory entries (name/type/size).
    async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>>;

    /// Whether `path` exists.
    async fn exists(&self, path: &str) -> bool;

    /// File size in bytes, or `None` if unknown/unstat-able.
    async fn metadata_size(&self, path: &str) -> Option<u64>;

    /// Backend name (`"local"` for [`LocalFileOps`], `"remote"` for
    /// [`RemoteFileOps`]).
    fn name(&self) -> &str;
}

// ─── Shell quoting (shared pure helper) ──────────────────────────────────────

/// POSIX single-quote escaping: wraps `s` in `'…'` with every embedded `'`
/// rewritten as `'\''`. Guarantees the result is a single shell word that
/// can never be broken out of — defence against path/argument injection
/// when `RemoteFileOps` interpolates a path into a remote command.
pub(crate) fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod quote_tests {
    use super::shell_quote;

    #[test]
    fn plain_path_is_single_quoted() {
        assert_eq!(shell_quote("/tmp/foo.txt"), "'/tmp/foo.txt'");
    }

    #[test]
    fn embedded_quote_is_escaped() {
        // A path containing `'` must not break out of the quoting.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_metachars_are_inert_inside_quotes() {
        let q = shell_quote("$(rm -rf /); `echo x`");
        // Single-quoted → the metachars are literal, not executed. The whole
        // value is one quoted token: starts and ends with `'`, and the count
        // of `'` is even (no unescaped breakout).
        assert!(q.starts_with('\''));
        assert!(q.ends_with('\''));
        assert_eq!(q.matches('\'').count() % 2, 0);
    }
}

// ─── LocalFileOps ────────────────────────────────────────────────────────────

/// Local filesystem backend — the current file-tool behavior, verbatim.
///
/// Moves the existing `tokio::fs` read/write/list logic so the default
/// (`FileReadTool::new()` etc.) is byte-identical to the pre-refactor tools
/// (discipline #7 — don't let a fix destroy what it protects).
pub struct LocalFileOps;

impl Default for LocalFileOps {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalFileOps {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl FileOperations for LocalFileOps {
    fn name(&self) -> &str {
        "local"
    }

    async fn read(&self, path: &str) -> Result<FileReadResult> {
        let size = tokio::fs::metadata(path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        // Try UTF-8 first (mirrors FileReadTool::execute). On failure, read
        // raw bytes for the tool to base64-encode.
        match tokio::fs::read_to_string(path).await {
            Ok(text) => {
                let total_lines = text.lines().count();
                Ok(FileReadResult::new(Some(text), None, total_lines, size))
            }
            Err(_) => {
                let bytes = tokio::fs::read(path)
                    .await
                    .map_err(|e| OneAIError::Other(format!("failed to read file '{path}': {e}")))?;
                Ok(FileReadResult::new(None, Some(bytes), 0, size))
            }
        }
    }

    async fn write(&self, path: &str, content: &str, append: bool) -> Result<()> {
        if append {
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await
                .map_err(|e| OneAIError::Other(format!("failed to open '{path}': {e}")))?;
            use tokio::io::AsyncWriteExt;
            let mut writer = tokio::io::BufWriter::new(file);
            writer
                .write_all(content.as_bytes())
                .await
                .map_err(|e| OneAIError::Other(format!("failed to append to '{path}': {e}")))?;
            writer
                .flush()
                .await
                .map_err(|e| OneAIError::Other(format!("flush failed on '{path}': {e}")))?;
            Ok(())
        } else {
            // Create parent directories if missing (write_file contract).
            if let Some(parent) = Path::new(path).parent() {
                if !parent.as_os_str().is_empty()
                    && !tokio::fs::try_exists(parent).await.unwrap_or(false)
                {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        OneAIError::Other(format!("failed to create parent dirs for '{path}': {e}"))
                    })?;
                }
            }
            tokio::fs::write(path, content)
                .await
                .map_err(|e| OneAIError::Other(format!("failed to write '{path}': {e}")))?;
            Ok(())
        }
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let mut read_dir = tokio::fs::read_dir(path)
            .await
            .map_err(|e| OneAIError::Other(format!("failed to read dir '{path}': {e}")))?;
        let mut entries = Vec::new();
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry
                .file_type()
                .await
                .map(|ft| ft.is_dir())
                .unwrap_or(false);
            let size = if !is_dir {
                entry.metadata().await.map(|m| m.len()).unwrap_or(0)
            } else {
                0
            };
            entries.push(DirEntry::new(name, is_dir, size));
        }
        // Stable order (mirrors FileListTool::execute, which sorts the
        // formatted lines — sorting here keeps the raw entry order stable
        // across runs, a discipline-#6 invariant not a frozen value).
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn exists(&self, path: &str) -> bool {
        tokio::fs::try_exists(path).await.unwrap_or(false)
    }

    async fn metadata_size(&self, path: &str) -> Option<u64> {
        tokio::fs::metadata(path).await.ok().map(|m| m.len())
    }
}

// ─── RemoteFileOps ───────────────────────────────────────────────────────────

/// Remote filesystem backend — routes file IO through a [`TerminalBackend`]
/// (a Docker container, a Modal/Daytona serverless terminal). This is the
/// "VM is the boundary" half of the Gondolin synthesis: the file tools'
/// side-effects land inside the VM rather than on the host.
///
/// Reads use `cat` (text) or `base64` (binary, detected via a NUL byte in the
/// `cat` output). Writes use `printf '<b64>' | base64 -d > path` — base64 is
/// shell-safe inside single quotes, so content of any shape (quotes,
/// heredocs, `%`-specifiers, newlines) round-trips without delimiter
/// collision or quoting risk. Listing uses GNU `find -printf` (one round-trip
/// for name/type/size, including hidden entries, excluding `.`/`..`).
///
/// `find -printf` / `stat -c` / `base64` are GNU coreutils — every target
/// backend (Docker/Modal/Daytona) runs Linux. The local fallback is
/// [`LocalFileOps`]; non-Linux remote backends are out of scope for v1.
pub struct RemoteFileOps {
    backend: Arc<dyn TerminalBackend>,
    /// Output cap passed to the backend for each IO command (the file tools
    /// enforce their own per-tool max_size via `metadata_size` before
    /// reading, so this only bounds command stdout, not file size).
    max_output_bytes: usize,
}

impl RemoteFileOps {
    /// Wrap a [`TerminalBackend`] for file IO. `max_output_bytes` defaults to
    /// a generous 8 MiB (a `cat` of a 1 MiB file base64-encodes to ~1.4 MiB).
    pub fn new(backend: Arc<dyn TerminalBackend>) -> Self {
        Self {
            backend,
            max_output_bytes: 8 * 1024 * 1024,
        }
    }

    /// Configure the per-command output cap.
    pub fn with_max_output_bytes(mut self, n: usize) -> Self {
        self.max_output_bytes = n;
        self
    }

    fn opts(&self) -> ExecOptions {
        ExecOptions::new(30, None, self.max_output_bytes)
    }

    async fn run(&self, command: &str) -> Result<ExecResult> {
        self.backend.execute(command, &self.opts()).await
    }

    /// Require the command to succeed and return its stdout content.
    fn require_ok(res: ExecResult, what: &str) -> Result<String> {
        if !res.success {
            return Err(OneAIError::Other(format!(
                "{what}: {}",
                res.error
                    .filter(|e| !e.trim().is_empty())
                    .unwrap_or_else(|| res.content.clone())
            )));
        }
        Ok(res.content)
    }
}

#[async_trait]
impl FileOperations for RemoteFileOps {
    fn name(&self) -> &str {
        "remote"
    }

    async fn read(&self, path: &str) -> Result<FileReadResult> {
        let q = shell_quote(path);
        // Size first (the tool enforces max_size before reading).
        let size = self.metadata_size(path).await.unwrap_or(0);

        // `cat` the file. NUL byte in the output ⇒ binary (NUL is valid UTF-8
        // U+0000, so it survives from_utf8_lossy as '\0' — a reliable probe).
        let cat_cmd = format!("cat {q}");
        let res = self.run(&cat_cmd).await?;
        let content = Self::require_ok(res, "read(cat)")?;

        if content.contains('\0') {
            // Binary — fetch via base64 and decode to raw bytes.
            let b64_cmd = format!("base64 {q}");
            let res = self.run(&b64_cmd).await?;
            let b64 = Self::require_ok(res, "read(base64)")?;
            let bytes = base64_decode(&b64)?;
            Ok(FileReadResult::new(None, Some(bytes), 0, size))
        } else {
            let total_lines = content.lines().count();
            Ok(FileReadResult::new(Some(content), None, total_lines, size))
        }
    }

    async fn write(&self, path: &str, content: &str, append: bool) -> Result<()> {
        let q = shell_quote(path);
        let redirect = if append { ">>" } else { ">" };

        // Create parent dirs (write_file contract). Only for overwrite mode;
        // append mode assumes the file (and its dir) already exists.
        if !append {
            if let Some(parent) = Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    let pq = shell_quote(&parent.to_string_lossy());
                    // mkdir -p is idempotent; ignore failure (may already exist).
                    let _ = self.run(&format!("mkdir -p {pq}")).await;
                }
            }
        }

        // base64-encode the content → fully shell-safe inside single quotes.
        // `printf '%s' '<b64>' | base64 -d > path` round-trips any content.
        let b64 = base64_encode(content.as_bytes());
        let cmd = format!("printf '%s' '{b64}' | base64 -d {redirect} {q}");
        let res = self.run(&cmd).await?;
        Self::require_ok(res, "write")?;
        Ok(())
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let q = shell_quote(path);
        // GNU find -printf: name\ttype\tsize, one round-trip, hidden included.
        let cmd = format!("find {q} -maxdepth 1 -mindepth 1 -printf '%f\\t%y\\t%s\\n' 2>/dev/null");
        let res = self.run(&cmd).await?;
        let content = Self::require_ok(res, "list_dir")?;
        let mut entries = Vec::new();
        for line in content.lines() {
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split('\t');
            let Some(name) = parts.next() else {
                continue;
            };
            let kind = parts.next().unwrap_or("");
            let size: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            entries.push(DirEntry::new(name, kind == "d", size));
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn exists(&self, path: &str) -> bool {
        let q = shell_quote(path);
        let res = match self.run(&format!("test -e {q} && echo y || echo n")).await {
            Ok(r) => r,
            Err(_) => return false,
        };
        res.success && res.content.trim() == "y"
    }

    async fn metadata_size(&self, path: &str) -> Option<u64> {
        let q = shell_quote(path);
        let res = self
            .run(&format!("stat -c %s {q} 2>/dev/null"))
            .await
            .ok()?;
        if !res.success {
            return None;
        }
        res.content.trim().parse().ok()
    }
}

// ─── base64 helpers (no new dependency — workspace already pulls `base64`) ───

fn base64_encode(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD.encode(bytes)
}

fn base64_decode(b64: &str) -> Result<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD
        .decode(b64.trim())
        .map_err(|e| OneAIError::Other(format!("base64 decode failed: {e}")))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::ExecResult;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory `TerminalBackend` for `RemoteFileOps` tests — records every
    /// command string it receives and returns a canned `ExecResult` keyed by
    /// a prefix (or the verbatim command). No real process is spawned.
    struct FakeBackend {
        calls: Mutex<Vec<String>>,
        replies: HashMap<String, ExecResult>,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                replies: HashMap::new(),
            }
        }

        /// Insert a canned reply matched by command prefix.
        fn on_prefix(&mut self, prefix: &str, content: &str, success: bool) {
            self.replies.insert(
                prefix.to_string(),
                ExecResult::new(success, content.to_string(), None),
            );
        }
    }

    #[async_trait]
    impl TerminalBackend for FakeBackend {
        fn name(&self) -> &str {
            "fake"
        }
        async fn execute(&self, command: &str, _opts: &ExecOptions) -> Result<ExecResult> {
            self.calls.lock().unwrap().push(command.to_string());
            // Longest matching prefix wins (deterministic-ish: first hit).
            let matched = self
                .replies
                .iter()
                .find(|(k, _)| command.starts_with(k.as_str()));
            Ok(match matched {
                Some((_, v)) => v.clone(),
                None => ExecResult::new(true, String::new(), None),
            })
        }
    }

    #[tokio::test]
    async fn local_read_write_roundtrip_matches_tokio_fs() {
        let tmp = std::env::temp_dir().join(format!(
            "oneai_fileops_local_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("hello.txt");
        let ops = LocalFileOps::new();

        ops.write(path.to_str().unwrap(), "line1\nline2\n", false)
            .await
            .unwrap();
        let r = ops.read(path.to_str().unwrap()).await.unwrap();
        assert_eq!(r.text.as_deref(), Some("line1\nline2\n"));
        assert_eq!(r.total_lines, 2);
        assert!(r.bytes.is_none());

        // Invariant: LocalFileOps content == direct tokio::fs read.
        let direct = std::fs::read_to_string(&path).unwrap();
        assert_eq!(r.text.unwrap(), direct);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn local_append_does_not_overwrite() {
        let tmp = std::env::temp_dir().join(format!(
            "oneai_fileops_append_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("a.txt");
        let ops = LocalFileOps::new();
        ops.write(path.to_str().unwrap(), "first", false)
            .await
            .unwrap();
        ops.write(path.to_str().unwrap(), "second", true)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "firstsecond");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn local_list_dir_returns_entries() {
        let tmp = std::env::temp_dir().join(format!(
            "oneai_fileops_list_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("f1.txt"), "x").unwrap();
        std::fs::create_dir(tmp.join("d1")).unwrap();
        let ops = LocalFileOps::new();
        let entries = ops.list_dir(tmp.to_str().unwrap()).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"f1.txt"));
        assert!(names.contains(&"d1"));
        let dir_entry = entries.iter().find(|e| e.name == "d1").unwrap();
        assert!(dir_entry.is_dir);
        let file_entry = entries.iter().find(|e| e.name == "f1.txt").unwrap();
        assert!(!file_entry.is_dir);
        assert_eq!(file_entry.size, 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn remote_read_text_issues_cat() {
        // `cat` of a text file (no NUL) ⇒ text path.
        let mut fake = FakeBackend::new();
        fake.on_prefix("cat ", "hello world\n", true);
        fake.on_prefix("stat -c %s ", "12\n", true);
        let ops = RemoteFileOps::new(Arc::new(fake));
        let r = ops.read("/tmp/x.txt").await.unwrap();
        assert_eq!(r.text.as_deref(), Some("hello world\n"));
        assert!(r.bytes.is_none());
        assert_eq!(r.size, 12);
        assert_eq!(r.total_lines, 1);
    }

    #[tokio::test]
    async fn remote_read_binary_falls_back_to_base64() {
        // A NUL in the cat output triggers the binary path.
        let mut fake = FakeBackend::new();
        fake.on_prefix("cat ", "ab\0cd", true); // contains NUL
        fake.on_prefix("base64 ", "AAAB\n", true); // 3 bytes 0x00 0x00 0x01
        fake.on_prefix("stat -c %s ", "4\n", true);
        let ops = RemoteFileOps::new(Arc::new(fake));
        let r = ops.read("/tmp/bin").await.unwrap();
        let bytes = r.bytes.expect("binary bytes");
        assert_eq!(bytes, vec![0u8, 0, 1]);
        assert!(r.text.is_none());
    }

    #[tokio::test]
    async fn remote_write_uses_base64_redirect() {
        // Write of arbitrary content must use `printf '<b64>' | base64 -d > path`
        // — the content never appears raw in the command (no quoting risk).
        struct RecordingBackend {
            calls: Mutex<Vec<String>>,
        }
        #[async_trait]
        impl TerminalBackend for RecordingBackend {
            fn name(&self) -> &str {
                "rec"
            }
            async fn execute(&self, command: &str, _opts: &ExecOptions) -> Result<ExecResult> {
                self.calls.lock().unwrap().push(command.to_string());
                Ok(ExecResult::new(true, String::new(), None))
            }
        }
        let rec = Arc::new(RecordingBackend {
            calls: Mutex::new(Vec::new()),
        });
        let ops = RemoteFileOps::new(rec.clone());
        ops.write("/app/out.txt", "he\"llo\n$rm", false)
            .await
            .unwrap();
        let calls = rec.calls.lock().unwrap().clone();
        // The printf command carries base64, not the raw content.
        let printf = calls.iter().find(|c| c.starts_with("printf")).unwrap();
        assert!(printf.contains("base64 -d"));
        assert!(!printf.contains("he\"llo"));
        assert!(!printf.contains("$rm"));
        // Parent dir creation happened.
        assert!(calls.iter().any(|c| c.starts_with("mkdir -p ")));
        // base64 round-trips back to the content.
        let b64: String = printf
            .trim_start_matches("printf '%s' '")
            .trim_end_matches("' | base64 -d > '/app/out.txt'")
            .to_string();
        let decoded = base64_decode(&b64).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "he\"llo\n$rm");
    }

    #[tokio::test]
    async fn remote_list_dir_parses_find_printf() {
        let mut fake = FakeBackend::new();
        fake.on_prefix("find ", "f1.txt\tf\t3\nsub\td\t0\n.hidden\tf\t1\n", true);
        let ops = RemoteFileOps::new(Arc::new(fake));
        let entries = ops.list_dir("/dir").await.unwrap();
        assert_eq!(entries.len(), 3);
        let f = entries.iter().find(|e| e.name == "f1.txt").unwrap();
        assert!(!f.is_dir);
        assert_eq!(f.size, 3);
        let d = entries.iter().find(|e| e.name == "sub").unwrap();
        assert!(d.is_dir);
        // Sorted by name — .hidden first (dotfiles sort before letters).
        assert_eq!(entries[0].name, ".hidden");
    }

    #[tokio::test]
    async fn remote_exists_and_metadata_size() {
        let mut fake = FakeBackend::new();
        fake.on_prefix("test -e ", "y\n", true);
        fake.on_prefix("stat -c %s ", "42\n", true);
        let ops = RemoteFileOps::new(Arc::new(fake));
        assert!(ops.exists("/tmp/exists").await);
        assert_eq!(ops.metadata_size("/tmp/exists").await, Some(42));
    }

    #[tokio::test]
    async fn remote_path_is_shell_quoted() {
        // A path with a shell metachar must arrive single-quoted in the command.
        struct Rec {
            calls: Mutex<Vec<String>>,
        }
        #[async_trait]
        impl TerminalBackend for Rec {
            fn name(&self) -> &str {
                "rec"
            }
            async fn execute(&self, command: &str, _opts: &ExecOptions) -> Result<ExecResult> {
                self.calls.lock().unwrap().push(command.to_string());
                Ok(ExecResult::new(true, String::new(), None))
            }
        }
        let rec = Arc::new(Rec {
            calls: Mutex::new(Vec::new()),
        });
        let ops = RemoteFileOps::new(rec.clone());
        let _ = ops.metadata_size("/tmp/a b; rm -rf /").await;
        let cmd = rec.calls.lock().unwrap().join("|");
        // The dangerous tail must be inert inside single quotes — no bare `;`.
        assert!(cmd.contains("'/tmp/a b; rm -rf /'"));
        assert!(!cmd.contains("'; '"));
    }
}
