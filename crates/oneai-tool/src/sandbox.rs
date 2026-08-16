//! SandboxBackend — real process-level isolation for tool execution.
//!
//! This addresses the gap identified in the competitive analysis:
//! OneAI's SandboxMode::Enabled only uses a regex blacklist, which is a "nominal sandbox"
//! (the configuration says "sandbox" but there's no real isolation). All major coding
//! agents (Claude Code, Codex CLI, Devin, OpenHands) have real process-level isolation.
//!
//! The SandboxBackend trait provides platform-specific isolation:
//! - macOS: Seatbelt (sandbox-exec) — the same mechanism used by Claude Code
//! - Linux: Bubblewrap (bwrap) — the same mechanism used by Codex CLI
//! - Default: Enhanced regex + working directory restriction (improved baseline)
//!
//! The design follows the principle of "configuration most flexible, execution strongest":
//! OneAI has the most flexible sandbox configuration (DomainPack can specify per-tool
//! sandbox policy), but now also has real execution-level isolation.
//!
//! **Architecture**: The ShellTool calls `backend.wrap_command()` which transforms
//! the raw shell command into an isolated execution environment. The backend is
//! responsible for:
//! 1. Restricting the command to only operate within allowed directories
//! 2. Preventing network access (unless explicitly allowed)
//! 3. Blocking dangerous operations (the regex blacklist remains as a baseline check)
//! 4. Providing audit logging of sandbox operations

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use oneai_core::error::Result;

// ─── WrappedCommand ──────────────────────────────────────────────────────────

/// A command wrapped by a SandboxBackend for isolated execution.
///
/// The wrapper transforms the raw command into one that runs within
/// the sandbox's isolation boundary. The execution layer (ShellTool)
/// simply runs the wrapped command as-is — the sandbox does the rest.
pub struct WrappedCommand {
    /// The shell command to execute (after sandbox wrapping).
    pub shell_command: String,

    /// Environment variables to set for the sandboxed process.
    /// These override the inherited environment.
    pub env_vars: HashMap<String, String>,

    /// The working directory for the command (enforced by sandbox).
    pub working_dir: PathBuf,

    /// Whether network access is allowed for this command.
    pub allow_network: bool,
}

/// The network egress policy a sandbox backend enforces.
///
/// `LoopbackProxy` is the code-mode network gate (#28 Stage 1): the sandbox
/// allows loopback only (so the sandboxed process can reach the host's local
/// egress proxy on `127.0.0.1:<port>`) and denies direct internet egress. The
/// proxy then enforces a per-host allow-list + approval gate. Backends that
/// can't express "loopback-to-host-proxy" (bwrap's `--unshare-net` isolates a
/// *private* loopback unreachable from the host; docker/regex have no
/// per-host filter) degrade `LoopbackProxy` to `Denied` semantics — the host
/// proxy stays unreachable, so the sandboxed process simply has no network.
/// Only Seatbelt (mac) gives the true strong gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkPolicy {
    /// Deny all network (including loopback). The safe default.
    #[default]
    Denied,
    /// Allow loopback only — the sandboxed process may reach the host's local
    /// egress proxy but not the internet directly.
    LoopbackProxy,
    /// Blanket allow all network (the pre-#28 `allow_network = true` posture).
    Allowed,
}

impl NetworkPolicy {
    /// The legacy boolean: `true` → `Allowed`, `false` → `Denied`.
    pub fn from_allow_network(allow: bool) -> Self {
        if allow {
            Self::Allowed
        } else {
            Self::Denied
        }
    }
}

// ─── SandboxBackend Trait ────────────────────────────────────────────────────

/// Sandbox backend trait — platform-specific command isolation.
///
/// Each backend wraps a shell command in a way that restricts its
/// execution to the allowed boundaries. The trait is intentionally
/// simple — it only needs to transform the command string, not
/// manage the full execution lifecycle (that's ShellTool's job).
///
/// This separation allows DomainPack to select the appropriate
/// backend based on the domain's security requirements:
/// - Coding: Seatbelt on macOS (restrict file writes, allow network for npm/pip)
/// - IoT: Docker on Linux (full isolation, no network)
/// - Research: Regex backend (lightweight, allow web access)
pub trait SandboxBackend: Send + Sync {
    /// Wrap a command for isolated execution within the given working directory.
    ///
    /// Returns a WrappedCommand with the transformed shell command and
    /// any environment variable overrides needed for the sandbox.
    fn wrap_command(&self, command: &str, working_dir: &Path) -> Result<WrappedCommand>;

    /// Get the name of this sandbox backend (for logging and debugging).
    fn name(&self) -> &str;

    /// Check if this backend is available on the current platform.
    ///
    /// Returns false if the required tool (sandbox-exec, docker, etc.)
    /// is not installed or not accessible.
    fn is_available(&self) -> bool;
}

// ─── SeatbeltBackend (macOS) ─────────────────────────────────────────────────

/// macOS Seatbelt sandbox backend — uses `sandbox-exec` for process isolation.
///
/// This is the same approach used by Claude Code on macOS. Seatbelt is
/// Apple's built-in sandboxing mechanism that restricts:
/// - File system access (read-only outside allowed dirs)
/// - Network access (can be restricted)
/// - Process execution (can restrict which binaries can run)
/// - Mach port access, signal delivery, etc.
///
/// The Seatbelt profile is generated programmatically based on the
/// sandbox configuration (allowed dirs, network policy).
pub struct SeatbeltBackend {
    /// Directory paths that the sandboxed process can write to.
    allowed_write_dirs: Vec<PathBuf>,

    /// Network egress policy. See [`NetworkPolicy`].
    network: NetworkPolicy,
}

impl SeatbeltBackend {
    /// Create a new SeatbeltBackend with the given allowed directories.
    ///
    /// `allow_network` is the legacy boolean (mapped via
    /// [`NetworkPolicy::from_allow_network`]); use [`Self::with_network_policy`]
    /// for the `LoopbackProxy` gate.
    pub fn new(allowed_write_dirs: Vec<PathBuf>, allow_network: bool) -> Self {
        Self {
            allowed_write_dirs,
            network: NetworkPolicy::from_allow_network(allow_network),
        }
    }

    /// Override the network egress policy (e.g. set `LoopbackProxy` for the
    /// code-mode network gate).
    pub fn with_network_policy(mut self, policy: NetworkPolicy) -> Self {
        self.network = policy;
        self
    }

    /// Create a basic SeatbeltBackend for coding tasks.
    /// Allows writes to the project directory and network access (for npm/pip/cargo).
    pub fn coding_defaults(project_dir: &Path) -> Self {
        Self {
            allowed_write_dirs: vec![project_dir.to_path_buf()],
            network: NetworkPolicy::Allowed,
        }
    }

    /// Generate a Seatbelt profile string for the given configuration.
    ///
    /// The profile uses Apple's Seatbelt policy language (scheme version 1).
    ///
    /// **Policy: deny-default + explicit allow-list** (codex-style, replaces
    /// the Issue #16 allow-default posture).
    ///
    /// `(deny default)` makes unknown operations blocked-by-default rather
    /// than allowed-by-default — the stricter posture the threat model
    /// (prompt-injection) demands. The allow list re-permits what real
    /// toolchains need; the prior `(deny default)` profile broke only because
    /// its allow list was incomplete. The three Issue #16 failures are all
    /// covered here:
    /// - `process-fork` / `process-exec` — so the shell can fork for `||`,
    ///   `&&`, `;`, and pipes (the bare `deny default` profile made every
    ///   compound command exit 128 with "fork: Operation not permitted").
    /// - `/dev/null` + `/dev/dtracehelper` writes — `git` opens `/dev/null`
    ///   to redirect stderr; cargo/dyld touch dtracehelper.
    /// - mach / sysctl / iokit / ipc — dyld, library loading, runtime
    ///   introspection; denying these panics binaries at startup (the
    ///   "failed to allocate a guard page" class of failure).
    ///
    /// The isolation boundary remains targeted file-write denies (system
    /// files stay write-protected) **plus** a best-effort read-deny on a
    /// small secret set (`~/.ssh`, `~/.aws`, `~/.config/gh`, `~/.gnupg`) —
    /// the classic prompt-injection exfil target. Network is blanket
    /// allow/deny: LLM-API egress filtering needs a local proxy
    /// (codex-style `NetworkProxy`, future work, out of scope here).
    fn generate_profile(&self) -> String {
        // Unconditional literal rules (version + posture + the process/mach/
        // read allows that every toolchain needs) — `vec![]` macro per clippy
        // `vec_init_then_push`. Conditional pushes (secrets, write dirs, temp,
        // network) follow below.
        let mut rules = vec![
            // Version header + deny-default posture.
            "(version 1)".to_string(),
            "(deny default)".to_string(),
            // Process operations — without these, compound commands fail with
            // "fork: Operation not permitted" (exit 128), the Issue #16
            // regression. process-exec is left broad (not path-restricted) so
            // the shell can run the user's toolchain binaries without an
            // ever-growing allow list.
            "(allow process-fork)".to_string(),
            "(allow process-exec)".to_string(),
            "(allow process-info* (target self))".to_string(),
            "(allow signal (target self))".to_string(),
            // Mach / sysctl / iokit — needed for dyld, library loading, sysctl
            // queries, runtime introspection. Deny-default would block these
            // and break binary startup.
            "(allow mach-lookup)".to_string(),
            "(allow mach-task* (target self))".to_string(),
            "(allow sysctl*)".to_string(),
            "(allow iokit*)".to_string(),
            // Reads — broad allow (toolchains read system libs, the binary's
            // own text, project sources, package caches). Best-effort deny on a
            // small secret set: the classic prompt-injection exfil target is
            // the user's private key. SSH-over-git pushes aren't part of
            // sandboxed local coding ops (and network is denied by default
            // below when allow_network=false), so denying ~/.ssh reads is safe
            // and high-value.
            "(allow file-read*)".to_string(),
        ];

        let mut secrets: Vec<PathBuf> = Vec::new();
        if let Ok(home) = std::env::var("HOME") {
            for s in [".ssh", ".aws", ".config/gh", ".gnupg"] {
                secrets.push(PathBuf::from(&home).join(s));
            }
        }
        for secret in &secrets {
            if let Some(p) = profile_subpath(secret) {
                rules.push(format!("(deny file-read* (subpath \"{p}\"))",));
            }
        }

        // Writes — the isolation boundary. Deny all writes, then re-allow
        // only safe locations (everything outside these subpaths — system
        // dirs — stays write-protected).
        rules.push("(deny file-write*)".to_string());

        // Project / explicitly-allowed write dirs. Include the session's
        // active workspace (deepseek-harness parity: the user picked it, so
        // the agent may write there) alongside the baked allowlist. Reads are
        // already broad-allowed above, so `pwd`/`ls`/`cat` work regardless;
        // this only opens writes inside the workspace.
        let mut write_dirs = self.allowed_write_dirs.clone();
        if let Some(w) = oneai_core::active_cwd::active_cwd() {
            write_dirs.push(w);
        }
        for dir in &write_dirs {
            if let Some(p) = profile_subpath(dir) {
                rules.push(format!("(allow file-write* (subpath \"{p}\"))",));
            }
        }

        // Temp directories (compilers, package managers, test harnesses).
        // `/tmp` is a symlink to `/private/tmp` on macOS; include both forms
        // so the rule matches regardless of how the path is spelled.
        for t in [
            "/tmp",
            "/private/tmp",
            "/var/tmp",
            "/private/var/tmp",
            "/var/folders",
            "/private/var/folders",
        ] {
            rules.push(format!("(allow file-write* (subpath \"{t}\"))",));
        }
        // Per-user TMPDIR (macOS: /var/folders/xx…). Canonicalize so a symlink
        // path matches the resolved path sandboxd sees.
        if let Ok(td) = std::env::var("TMPDIR") {
            if let Some(p) = profile_subpath(std::path::Path::new(&td)) {
                rules.push(format!("(allow file-write* (subpath \"{p}\"))",));
            }
        }

        // User home — package-manager caches live here (~/.cargo, ~/.npm,
        // ~/Library/Caches, ~/.rustup). Denying HOME would break
        // `cargo build` / `npm install` etc. The regex blacklist already
        // guards `rm -rf ~` / `rm -rf $HOME`.
        if let Ok(home) = std::env::var("HOME") {
            if let Some(p) = profile_subpath(std::path::Path::new(&home)) {
                rules.push(format!("(allow file-write* (subpath \"{p}\"))",));
            }
        }

        // Device nodes tools redirect to.
        rules.push("(allow file-write* (literal \"/dev/null\"))".to_string());
        rules.push("(allow file-write* (literal \"/dev/dtracehelper\"))".to_string());

        // Network policy. LLM API IPs vary, so direct egress can't be
        // path-filtered without a local proxy; `LoopbackProxy` is exactly that
        // gate (#28 Stage 1) — allow loopback only (so the sandboxed process
        // can reach the host's local egress proxy on 127.0.0.1:<port>) and
        // let `(deny default)` cover everything else. `Denied` needs no rule
        // (deny-default); `Allowed` blanket-permits.
        match self.network {
            NetworkPolicy::Allowed => rules.push("(allow network*)".to_string()),
            NetworkPolicy::LoopbackProxy => {
                // Loopback only — the sandboxed process may reach the host's
                // local egress proxy on 127.0.0.1:<port>. `(local ip)` filters
                // to local (loopback) IP addresses; `(local)` alone is invalid
                // SBPL on macOS 14+/Darwin 24+ and crashes `sandbox-exec`'s
                // parser (`sbpl_parser.c:128 is_pair(p)` assertion → SIGABRT),
                // which surfaced as `Exit code: -1` (signal kill, no output)
                // for EVERY seatbelt-wrapped shell call. `(local ip)` is the
                // valid idiom and matches the same loopback-only intent.
                // Non-local stays denied by `(deny default)` (no blanket
                // `(deny network*)` — it would race the local allow on
                // precedence).
                rules.push("(allow network* (local ip))".to_string());
            }
            // `Denied` is fully covered by `(deny default)`, but emit an
            // explicit `(deny network*)` so the posture is self-describing.
            NetworkPolicy::Denied => {
                rules.push("(deny network*)".to_string());
            }
        }

        rules.join("\n")
    }
}

/// Canonicalize a path for use as a Seatbelt `(subpath "...")` argument.
///
/// `sandboxd` evaluates operations against the *resolved* path, so a
/// symlinked project dir (or a per-user TMPDIR under `/var/folders`) must be
/// canonicalized before it is placed in the profile, otherwise the subpath
/// rule silently fails to match and writes get denied. Returns `None` for a
/// path that can't be resolved (non-existent), so the caller can skip the rule
/// rather than emit a broken one.
fn profile_subpath(p: &std::path::Path) -> Option<String> {
    let resolved = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = resolved.to_string_lossy();
    if s.is_empty() {
        None
    } else {
        Some(s.into_owned())
    }
}

impl SandboxBackend for SeatbeltBackend {
    fn wrap_command(&self, command: &str, working_dir: &Path) -> Result<WrappedCommand> {
        let profile = self.generate_profile();

        // sandbox-exec -p <profile> <command>
        // The profile is passed as a command-line argument
        // Note: sandbox-exec reads the profile from stdin if -f is used,
        // but for simplicity we use -p with the profile inline.
        // For profiles longer than ~4KB, we should write to a temp file and use -f.

        let escaped_command = command.replace("'", "'\\''"); // Basic shell escaping
        let wrapped = format!(
            "sandbox-exec -p '{}' sh -c '{}'",
            profile.replace("'", "'\\''"),
            escaped_command
        );

        Ok(WrappedCommand {
            shell_command: wrapped,
            env_vars: HashMap::new(),
            working_dir: working_dir.to_path_buf(),
            allow_network: self.network == NetworkPolicy::Allowed,
        })
    }

    fn name(&self) -> &str {
        "seatbelt"
    }

    fn is_available(&self) -> bool {
        // Check if sandbox-exec is available (only on macOS)
        if !cfg!(target_os = "macos") {
            return false;
        }
        std::path::Path::new("/usr/bin/sandbox-exec").exists()
    }
}

// ─── DockerBackend (Linux) ──────────────────────────────────────────────────

/// Docker sandbox backend — uses Docker containers for full process isolation.
///
/// This is the approach used by Codex CLI and OpenHands. Docker provides:
/// - Complete filesystem isolation (the container has its own filesystem)
/// - Network isolation (by default, no network access)
/// - Process isolation (the container has its own PID namespace)
/// - Resource limits (CPU, memory constraints can be applied)
///
/// **Requirements**: Docker must be installed and running on the host.
/// Falls back to RegexBackend if Docker is not available.
pub struct DockerBackend {
    /// Docker image to use for the sandbox.
    /// Default: "oneai-sandbox" (a lightweight image with common dev tools).
    image: String,

    /// Directory paths to mount into the container.
    mount_dirs: Vec<PathBuf>,

    /// Network egress policy. See [`NetworkPolicy`].
    network: NetworkPolicy,
}

impl DockerBackend {
    /// Create a new DockerBackend with the given image and mount directories.
    pub fn new(image: &str, mount_dirs: Vec<PathBuf>, allow_network: bool) -> Self {
        Self {
            image: image.to_string(),
            mount_dirs,
            network: NetworkPolicy::from_allow_network(allow_network),
        }
    }

    /// Override the network egress policy. Docker has no loopback-to-proxy
    /// mode, so `LoopbackProxy` degrades to `Denied` (`--network none`).
    pub fn with_network_policy(mut self, policy: NetworkPolicy) -> Self {
        self.network = policy;
        self
    }

    /// Create a basic DockerBackend for coding tasks.
    /// Mounts the project directory, allows network (for npm/pip/cargo).
    pub fn coding_defaults(project_dir: &Path) -> Self {
        Self {
            image: "oneai-sandbox:latest".to_string(),
            mount_dirs: vec![project_dir.to_path_buf()],
            network: NetworkPolicy::Allowed,
        }
    }
}

impl SandboxBackend for DockerBackend {
    fn wrap_command(&self, command: &str, working_dir: &Path) -> Result<WrappedCommand> {
        let mut docker_args = Vec::new();

        // docker run --rm (auto-remove container after exit)
        docker_args.push("docker run --rm".to_string());

        // Mount directories
        for dir in &self.mount_dirs {
            let dir_str = dir.to_string_lossy();
            docker_args.push(format!("-v {}:{}", dir_str, dir_str));
        }

        // Mount working directory specifically
        let wd_str = working_dir.to_string_lossy();
        if !self.mount_dirs.iter().any(|d| d == working_dir) {
            docker_args.push(format!("-v {}:{}", wd_str, wd_str));
        }

        // Set working directory in container
        docker_args.push(format!("-w {}", wd_str));

        // Network policy
        if self.network != NetworkPolicy::Allowed {
            docker_args.push("--network none".to_string());
        }

        // Resource limits (optional, prevents runaway processes)
        docker_args.push("--memory 512m".to_string());
        docker_args.push("--cpus 1".to_string());

        // Image
        docker_args.push(self.image.clone());

        // Command
        let escaped_command = command.replace("'", "'\\''");
        docker_args.push(format!("sh -c '{}'", escaped_command));

        Ok(WrappedCommand {
            shell_command: docker_args.join(" "),
            env_vars: HashMap::new(),
            working_dir: working_dir.to_path_buf(),
            allow_network: self.network == NetworkPolicy::Allowed,
        })
    }

    fn name(&self) -> &str {
        "docker"
    }

    fn is_available(&self) -> bool {
        // Check if Docker is installed and running
        // Simple check: try to find the docker binary
        std::path::Path::new("/usr/bin/docker").exists()
            || std::path::Path::new("/usr/local/bin/docker").exists()
    }
}

// ─── BubblewrapBackend (Linux) ───────────────────────────────────────────────

/// Linux bubblewrap (bwrap) sandbox backend — unprivileged user-namespace
/// isolation, the same mechanism Codex CLI uses on Linux (the DockerBackend
/// doc-comment claiming Codex uses Docker was wrong).
///
/// vs Docker (the previous Linux default): bwrap has no root daemon, starts in
/// milliseconds, and — critically — maps the calling uid into the namespace so
/// files written through bind mounts land with the **real owner**, not root
/// (Docker's `root`-in-container + `-v` bind is the classic "host files become
/// un-deletable" footgun). bwrap also has a far smaller escape surface (no
/// runc/containerd CVE history). It is the right default for a coding agent
/// that runs the host toolchain per command.
///
/// Posture: the host filesystem is mounted **read-only** at `/`, then `--dev`
/// / `--proc` provide device/proc filesystems, then each allowed dir is
/// overlaid as a read-write `--bind` (project, system temp, HOME caches). All
/// namespaces are unshared; network is shared with the host only when
/// `allow_network` (else the sandbox netns is loopback-only).
pub struct BubblewrapBackend {
    /// Directory paths the sandboxed process may write to (read-write bind).
    allowed_write_dirs: Vec<PathBuf>,

    /// Network egress policy. See [`NetworkPolicy`].
    network: NetworkPolicy,
}

impl BubblewrapBackend {
    /// Create a new BubblewrapBackend with the given allowed directories.
    pub fn new(allowed_write_dirs: Vec<PathBuf>, allow_network: bool) -> Self {
        Self {
            allowed_write_dirs,
            network: NetworkPolicy::from_allow_network(allow_network),
        }
    }

    /// Override the network egress policy. Note: bwrap cannot express
    /// `LoopbackProxy` (an isolated netns's loopback is unreachable from the
    /// host), so `LoopbackProxy` degrades to `Denied` here.
    pub fn with_network_policy(mut self, policy: NetworkPolicy) -> Self {
        self.network = policy;
        self
    }

    /// Create a basic BubblewrapBackend for coding tasks. Project dir is the
    /// writable root; network defaults off (mirrors the CodingPack seatbelt
    /// default). Extra writable roots (temp, HOME caches) are added by
    /// [`Self::writable_roots`] so cargo/npm/rustup caches keep working.
    pub fn coding_defaults(project_dir: &Path) -> Self {
        Self::new(vec![project_dir.to_path_buf()], false)
    }

    /// The full writable set: the caller's allowed dirs plus system temp and
    /// HOME (package-manager / compiler caches live there). Mirrors the
    /// SeatbeltBackend write-allow set so the two backends grant the same
    /// writable surface.
    fn writable_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.allowed_write_dirs.clone();
        roots.push(PathBuf::from("/tmp"));
        roots.push(PathBuf::from("/var/tmp"));
        if let Ok(td) = std::env::var("TMPDIR") {
            roots.push(PathBuf::from(td));
        }
        if let Ok(home) = std::env::var("HOME") {
            roots.push(PathBuf::from(home));
        }
        roots
    }
}

impl SandboxBackend for BubblewrapBackend {
    fn wrap_command(&self, command: &str, working_dir: &Path) -> Result<WrappedCommand> {
        let mut args: Vec<String> = vec!["bwrap".into(), "--unshare-all".into()];

        // --unshare-all includes --unshare-net; re-share only when blanket
        // allowed. `LoopbackProxy` keeps the netns unshared (an isolated
        // loopback can't reach the host proxy) — the host proxy is unreachable
        // here, so it degrades to no-network; only Seatbelt (mac) gives the
        // true loopback-to-proxy gate.
        if self.network == NetworkPolicy::Allowed {
            args.push("--share-net".into());
        }

        // Host fs read-only at /, then dev/proc filesystems (these override
        // the ro-bind at those paths so device nodes + proc are usable).
        args.extend(["--ro-bind".into(), "/".into(), "/".into()]);
        args.extend(["--dev".into(), "/dev".into()]);
        args.extend(["--proc".into(), "/proc".into()]);

        // Overlay read-write binds for each existing allowed root. Canonicalize
        // so a symlinked project/TMPDIR matches the resolved path bwrap sees
        // (bwrap compares the host source path literally).
        let roots = self.writable_roots();
        for dir in &roots {
            let resolved = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
            if resolved.exists() {
                let s = resolved.to_string_lossy().into_owned();
                // same path as host source and in-sandbox target (no remap)
                args.push("--bind".into());
                args.push(s.clone());
                args.push(s);
            }
        }

        // Ensure the requested working dir is writable inside the sandbox.
        let wd = std::fs::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
        let wd_known = roots.iter().any(|r| {
            let r = std::fs::canonicalize(r).unwrap_or_else(|_| r.clone());
            wd == r || wd.starts_with(&r)
        });
        if wd.exists() && !wd_known {
            let s = wd.to_string_lossy().into_owned();
            args.extend(["--bind".into(), s.clone(), s]);
        }

        args.extend(["sh".into(), "-c".into()]);
        let escaped = command.replace("'", "'\\''");
        args.push(format!("'{escaped}'"));

        Ok(WrappedCommand {
            shell_command: args.join(" "),
            env_vars: HashMap::new(),
            working_dir: working_dir.to_path_buf(),
            allow_network: self.network == NetworkPolicy::Allowed,
        })
    }

    fn name(&self) -> &str {
        "bwrap"
    }

    fn is_available(&self) -> bool {
        if !cfg!(target_os = "linux") {
            return false;
        }
        std::path::Path::new("/usr/bin/bwrap").exists()
            || std::path::Path::new("/usr/local/bin/bwrap").exists()
    }
}

// ─── RegexBackend (Default) ──────────────────────────────────────────────────

/// Regex-based sandbox backend — enhanced baseline for when platform-specific
/// backends are not available.
///
/// This is the improved version of OneAI's current "regex blacklist" sandbox.
/// It adds:
/// - Working directory restriction (commands can only write to allowed dirs)
/// - Network restriction flag (marks network-accessing commands)
/// - The existing regex blacklist patterns remain as a baseline check
///
/// This backend is NOT true process-level isolation, but it's better than
/// the current SandboxMode::Enabled which only does regex blocking.
/// When a real backend (Seatbelt, Docker) is available, it should be preferred.
pub struct RegexBackend {
    /// Directory paths that commands are allowed to operate in.
    #[allow(dead_code)]
    allowed_dirs: Vec<PathBuf>,

    /// Network egress policy. See [`NetworkPolicy`].
    #[allow(dead_code)]
    network: NetworkPolicy,
}

impl RegexBackend {
    /// Create a new RegexBackend with the given allowed directories.
    pub fn new(allowed_dirs: Vec<PathBuf>, allow_network: bool) -> Self {
        Self {
            allowed_dirs,
            network: NetworkPolicy::from_allow_network(allow_network),
        }
    }

    /// Override the network egress policy. RegexBackend has no real isolation
    /// (it only passes the command through); `LoopbackProxy` is informational.
    pub fn with_network_policy(mut self, policy: NetworkPolicy) -> Self {
        self.network = policy;
        self
    }

    /// Create a basic RegexBackend for coding tasks.
    pub fn coding_defaults(project_dir: &Path) -> Self {
        Self {
            allowed_dirs: vec![project_dir.to_path_buf()],
            network: NetworkPolicy::Allowed,
        }
    }
}

impl SandboxBackend for RegexBackend {
    fn wrap_command(&self, command: &str, working_dir: &Path) -> Result<WrappedCommand> {
        // RegexBackend doesn't actually transform the command —
        // the isolation is enforced at the ShellTool level via:
        // 1. Regex blacklist check (before execution)
        // 2. Working directory enforcement (the ShellTool runs in allowed dirs)
        // 3. Network restriction (future: could use LD_PRELOAD or similar)
        //
        // We simply pass the command through unchanged, but set the
        // working_dir and allow_network flags for ShellTool to enforce.

        Ok(WrappedCommand {
            shell_command: command.to_string(),
            env_vars: HashMap::new(),
            working_dir: working_dir.to_path_buf(),
            allow_network: self.network == NetworkPolicy::Allowed,
        })
    }

    fn name(&self) -> &str {
        "regex"
    }

    fn is_available(&self) -> bool {
        // Always available — no external dependencies
        true
    }
}

// ─── Default Sandbox Selector ────────────────────────────────────────────────

/// Select the best available sandbox backend for the current platform.
///
/// Priority order:
/// 1. macOS → SeatbeltBackend (if sandbox-exec available)
/// 2. Linux → BubblewrapBackend (if bwrap available) — the Codex-CLI mechanism
/// 3. Linux → DockerBackend (if Docker available) — bwrap-unavailable fallback
/// 4. Fallback → RegexBackend (always available)
///
/// This is used by ShellTool to automatically select the appropriate
/// sandbox backend based on the platform.
pub fn default_sandbox_backend(
    project_dir: &Path,
    _allow_network: bool,
) -> Arc<dyn SandboxBackend> {
    // The bool arg is a legacy vestige (always ignored — `coding_defaults`
    // hard-codes the per-backend network posture). Selectors that need a real
    // network policy (e.g. code-interpreter's `LoopbackProxy` gate) use
    // [`default_sandbox_backend_with_policy`].
    if cfg!(target_os = "macos") {
        let seatbelt = SeatbeltBackend::coding_defaults(project_dir);
        if seatbelt.is_available() {
            tracing::info!("Using Seatbelt sandbox backend on macOS");
            return Arc::new(seatbelt);
        }
    }

    if cfg!(target_os = "linux") {
        let bwrap = BubblewrapBackend::coding_defaults(project_dir);
        if bwrap.is_available() {
            tracing::info!("Using Bubblewrap sandbox backend on Linux");
            return Arc::new(bwrap);
        }
        // bwrap unavailable (rare on modern distros) — fall back to Docker.
        let docker = DockerBackend::coding_defaults(project_dir);
        if docker.is_available() {
            tracing::info!("Using Docker sandbox backend on Linux (bwrap unavailable)");
            return Arc::new(docker);
        }
    }

    tracing::info!("Using regex-based sandbox backend (platform-specific isolation not available)");
    Arc::new(RegexBackend::coding_defaults(project_dir))
}

/// Like [`default_sandbox_backend`] but applies an explicit network egress
/// [`NetworkPolicy`] to the selected backend. This is the code-interpreter seam:
/// `LoopbackProxy` wires the code-mode network gate (the sandboxed process can
/// reach only the host's local egress proxy), `Denied` fully air-gaps it.
///
/// Only Seatbelt (mac) gives the true `LoopbackProxy` strong gate; bwrap/docker/
/// regex degrade `LoopbackProxy` to no-network (their isolation can't reach the
/// host's loopback). See [`NetworkPolicy`] for the platform asymmetry.
pub fn default_sandbox_backend_with_policy(
    project_dir: &Path,
    policy: NetworkPolicy,
) -> Arc<dyn SandboxBackend> {
    if cfg!(target_os = "macos") {
        let seatbelt = SeatbeltBackend::coding_defaults(project_dir).with_network_policy(policy);
        if seatbelt.is_available() {
            tracing::info!(
                "Using Seatbelt sandbox backend on macOS (network policy: {:?})",
                policy
            );
            return Arc::new(seatbelt);
        }
    }

    if cfg!(target_os = "linux") {
        let bwrap = BubblewrapBackend::coding_defaults(project_dir).with_network_policy(policy);
        if bwrap.is_available() {
            tracing::info!(
                "Using Bubblewrap sandbox backend on Linux (network policy: {:?})",
                policy
            );
            return Arc::new(bwrap);
        }
        let docker = DockerBackend::coding_defaults(project_dir).with_network_policy(policy);
        if docker.is_available() {
            tracing::info!(
                "Using Docker sandbox backend on Linux (network policy: {:?})",
                policy
            );
            return Arc::new(docker);
        }
    }

    tracing::info!(
        "Using regex-based sandbox backend (network policy: {:?}; platform-specific isolation not available)",
        policy
    );
    Arc::new(RegexBackend::coding_defaults(project_dir).with_network_policy(policy))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regex_backend_wrapping() {
        let backend = RegexBackend::coding_defaults(Path::new("/project"));
        let result = backend
            .wrap_command("cargo build", Path::new("/project"))
            .unwrap();
        assert_eq!(result.shell_command, "cargo build");
        assert_eq!(result.working_dir, Path::new("/project"));
        assert!(result.allow_network);
    }

    #[test]
    fn test_regex_backend_always_available() {
        let backend = RegexBackend::coding_defaults(Path::new("/project"));
        assert!(backend.is_available());
        assert_eq!(backend.name(), "regex");
    }

    #[test]
    fn test_seatbelt_profile_generation() {
        let backend = SeatbeltBackend::coding_defaults(Path::new("/myproject"));
        let profile = backend.generate_profile();
        assert!(profile.contains("(version 1)"));
        // deny-default posture (codex-style) — replaces the Issue #16
        // allow-default posture. Unknown operations are blocked, not allowed.
        assert!(profile.contains("(deny default)"));
        assert!(!profile.contains("(allow default)"));
        // The three Issue #16 must-haves: process-fork/exec (compound
        // commands), /dev/null (git), and the file-write isolation boundary.
        assert!(profile.contains("(allow process-fork)"));
        assert!(profile.contains("(allow process-exec)"));
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("/myproject")); // allowed write dir
        assert!(profile.contains("/dev/null")); // git redirects via /dev/null
        assert!(profile.contains("(allow network*)")); // allow_network=true
                                                       // Best-effort secret read-deny (~/.ssh et al.) — expand HOME.
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert!(profile.contains(&format!("{home}/.ssh")));
        }
        // Temp dirs (both symlink and resolved forms) are writable.
        assert!(profile.contains("/private/tmp"));
    }

    #[test]
    fn test_seatbelt_no_network() {
        let backend = SeatbeltBackend::new(vec![PathBuf::from("/project")], false);
        let profile = backend.generate_profile();
        assert!(profile.contains("(deny network*)"));
        assert!(!profile.contains("(allow network*)"));
    }

    /// #28 Stage 1 — the LoopbackProxy profile admits only loopback (so the
    /// sandboxed process can reach the host's local egress proxy) and denies
    /// everything else. The `with_network_policy` seam is how the code-mode
    /// network gate wires in.
    #[test]
    fn test_seatbelt_loopback_proxy_profile() {
        let backend = SeatbeltBackend::coding_defaults(Path::new("/project"))
            .with_network_policy(NetworkPolicy::LoopbackProxy);
        let profile = backend.generate_profile();
        assert!(
            profile.contains("(allow network* (local ip))"),
            "loopback must be allowed for the host proxy"
        );
        // No blanket allow (direct internet stays denied by deny-default).
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn test_seatbelt_wrapping() {
        let backend = SeatbeltBackend::coding_defaults(Path::new("/project"));
        let result = backend
            .wrap_command("cargo test", Path::new("/project"))
            .unwrap();
        assert!(result.shell_command.starts_with("sandbox-exec"));
        assert!(result.shell_command.contains("cargo test"));
    }

    /// Regression guard for the deny-default profile: `sandbox-exec` must
    /// **parse** the generated profile without error (an invalid operation
    /// name or syntax makes sandbox-exec reject the whole profile and every
    /// command fails). Runs `/bin/true` under the profile — if the profile
    /// is malformed, sandbox-exit exits non-zero with a parse error on stderr.
    /// macOS-only; skipped elsewhere.
    #[cfg(target_os = "macos")]
    #[test]
    fn test_seatbelt_profile_parses() {
        if !std::path::Path::new("/usr/bin/sandbox-exec").exists() {
            eprintln!("sandbox-exec not present; skipping");
            return;
        }
        let backend = SeatbeltBackend::coding_defaults(&std::env::current_dir().unwrap());
        let profile = backend.generate_profile();
        let out = std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg("/usr/bin/true")
            .output()
            .expect("spawn sandbox-exec");
        assert!(
            out.status.success(),
            "sandbox-exec rejected the profile (parse error): stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Regression guard for Issue #16. The strict `(deny default)` profile
    /// made every compound command fail with "fork: Operation not permitted"
    /// (exit 128) because the sandboxed shell could not fork to evaluate
    /// `||`/`&&`/pipes. With the deny-default + allow-list posture
    /// (process-fork re-permitted), a compound command runs to completion
    /// (exit 0). Only runs on macOS where `sandbox-exec` exists; skipped
    /// elsewhere so CI on other OSes is unaffected.
    #[cfg(target_os = "macos")]
    #[test]
    fn test_seatbelt_compound_command_runs() {
        if !std::path::Path::new("/usr/bin/sandbox-exec").exists() {
            eprintln!("sandbox-exec not present; skipping");
            return;
        }
        let backend = SeatbeltBackend::coding_defaults(&std::env::current_dir().unwrap());
        let cwd = std::env::current_dir().unwrap();
        let wrapped = backend.wrap_command("true || echo FAIL", &cwd).unwrap();
        // LocalBackend runs `sh -c <wrapped.shell_command>` — reproduce it.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&wrapped.shell_command)
            .output()
            .expect("spawn sh");
        assert!(
            out.status.success(),
            "compound command failed under seatbelt: exit {:?}, stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn test_docker_backend_wrapping() {
        let backend = DockerBackend::coding_defaults(Path::new("/project"));
        let result = backend
            .wrap_command("cargo test", Path::new("/project"))
            .unwrap();
        assert!(result.shell_command.starts_with("docker run --rm"));
        assert!(result.shell_command.contains("-v /project:/project"));
        assert!(result.shell_command.contains("-w /project"));
        assert!(result.shell_command.contains("cargo test"));
    }

    #[test]
    fn test_docker_no_network() {
        let backend = DockerBackend::new(
            "oneai-sandbox:latest",
            vec![PathBuf::from("/project")],
            false,
        );
        let result = backend
            .wrap_command("npm install", Path::new("/project"))
            .unwrap();
        assert!(result.shell_command.contains("--network none"));
    }

    #[test]
    fn test_bwrap_backend_wrapping() {
        // Use a real existing dir so binds are actually emitted (bwrap skips
        // non-existent sources). A temp dir is fine: it's under TMPDIR, so it
        // is covered by the TMPDIR --bind rather than an explicit one — the
        // structural assertions below don't depend on the project being its
        // own bind.
        let tmp = std::env::temp_dir().join(format!(
            "oneai_bwrap_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let backend = BubblewrapBackend::coding_defaults(&tmp);
        let result = backend.wrap_command("cargo test", &tmp).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(result.shell_command.starts_with("bwrap --unshare-all"));
        // Host fs is read-only at /.
        assert!(result.shell_command.contains("--ro-bind / /"));
        // dev/proc filesystems are provisioned.
        assert!(result.shell_command.contains("--dev /dev"));
        assert!(result.shell_command.contains("--proc /proc"));
        // Command runs via sh -c.
        assert!(result.shell_command.contains("sh -c"));
        assert!(result.shell_command.contains("cargo test"));
    }

    #[test]
    fn test_bwrap_no_network_by_default() {
        // coding_defaults → allow_network=false → no --share-net.
        let backend = BubblewrapBackend::coding_defaults(Path::new("/project"));
        let result = backend
            .wrap_command("curl evil.com", Path::new("/project"))
            .unwrap();
        assert!(!result.shell_command.contains("--share-net"));
        assert!(result.shell_command.contains("--unshare-all"));
    }

    #[test]
    fn test_bwrap_share_net_when_allowed() {
        let backend = BubblewrapBackend::new(vec![PathBuf::from("/project")], true);
        let result = backend
            .wrap_command("cargo fetch", Path::new("/project"))
            .unwrap();
        assert!(result.shell_command.contains("--share-net"));
    }

    /// Regression guard mirroring `test_seatbelt_compound_command_runs`: a
    /// compound command (`||`) must run to completion under bwrap, not be
    /// killed by namespace restrictions. Only runs on Linux where `bwrap`
    /// exists; skipped elsewhere so CI on other OSes is unaffected.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_bwrap_compound_command_runs() {
        if !std::path::Path::new("/usr/bin/bwrap").exists()
            && !std::path::Path::new("/usr/local/bin/bwrap").exists()
        {
            eprintln!("bwrap not present; skipping");
            return;
        }
        let backend = BubblewrapBackend::coding_defaults(&std::env::current_dir().unwrap());
        let cwd = std::env::current_dir().unwrap();
        let wrapped = backend.wrap_command("true || echo FAIL", &cwd).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&wrapped.shell_command)
            .output()
            .expect("spawn sh");
        assert!(
            out.status.success(),
            "compound command failed under bwrap: exit {:?}, stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn test_default_sandbox_selector() {
        let backend = default_sandbox_backend(Path::new("/project"), true);
        // On any platform, this should return a valid backend
        assert!(backend.is_available());
        // The name depends on the platform:
        // macOS with sandbox-exec → "seatbelt"
        // Linux with Docker → "docker"
        // Otherwise → "regex"
    }
}
