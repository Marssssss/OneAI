//! SandboxBackend — real process-level isolation for tool execution.
//!
//! This addresses the gap identified in the competitive analysis:
//! OneAI's SandboxMode::Enabled only uses a regex blacklist, which is a "nominal sandbox"
//! (the configuration says "sandbox" but there's no real isolation). All major coding
//! agents (Claude Code, Codex CLI, Devin, OpenHands) have real process-level isolation.
//!
//! The SandboxBackend trait provides platform-specific isolation:
//! - macOS: Seatbelt (sandbox-exec) — the same mechanism used by Claude Code
//! - Linux: Docker container — the same mechanism used by Codex CLI, OpenHands
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

    /// Whether network access is allowed.
    allow_network: bool,
}

impl SeatbeltBackend {
    /// Create a new SeatbeltBackend with the given allowed directories.
    pub fn new(allowed_write_dirs: Vec<PathBuf>, allow_network: bool) -> Self {
        Self { allowed_write_dirs, allow_network }
    }

    /// Create a basic SeatbeltBackend for coding tasks.
    /// Allows writes to the project directory and network access (for npm/pip/cargo).
    pub fn coding_defaults(project_dir: &Path) -> Self {
        Self {
            allowed_write_dirs: vec![project_dir.to_path_buf()],
            allow_network: true,
        }
    }

    /// Generate a Seatbelt profile string for the given configuration.
    ///
    /// The profile uses Apple's Seatbelt policy language (scheme version 1).
    /// Key restrictions:
    /// - Default: deny all file writes, deny network
    /// - Exceptions: allow file writes in allowed dirs, allow network if configured
    /// - Allow: file reads everywhere (needed for code understanding)
    /// - Allow: process execution (needed for running compilers/tests)
    fn generate_profile(&self) -> String {
        let mut rules = Vec::new();

        // Version header
        rules.push("(version 1)".to_string());

        // Default deny — all operations are denied unless explicitly allowed
        rules.push("(deny default)".to_string());

        // Allow file reads (essential for understanding code)
        rules.push("(allow file-read*)".to_string());

        // Allow file writes only in specified directories
        for dir in &self.allowed_write_dirs {
            let dir_str = dir.to_string_lossy();
            rules.push(format!(
                "(allow file-write* (subpath \"{}\"))",
                dir_str
            ));
        }

        // Allow file writes to temp directories (needed for compilers)
        rules.push("(allow file-write* (subpath \"/tmp\"))".to_string());
        rules.push("(allow file-write* (subpath \"/var/tmp\"))".to_string());

        // Network policy
        if self.allow_network {
            rules.push("(allow network*)".to_string());
        } else {
            rules.push("(deny network*)".to_string());
        }

        // Allow process execution (needed for compilers, tests, linters)
        // But restrict to specific paths if possible
        rules.push("(allow process-exec)".to_string());
        rules.push("(allow process-exec (literal \"/usr/bin/env\"))".to_string());
        rules.push("(allow process-exec (literal \"/bin/sh\"))".to_string());
        rules.push("(allow process-exec (literal \"/bin/bash\"))".to_string());

        // Allow signal delivery (needed for process management)
        rules.push("(allow signal (target self))".to_string());

        // Allow Mach port access (needed for basic system operations)
        rules.push("(allow mach-lookup)".to_string());

        rules.join("\n")
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
            allow_network: self.allow_network,
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

    /// Whether network access is allowed in the container.
    allow_network: bool,
}

impl DockerBackend {
    /// Create a new DockerBackend with the given image and mount directories.
    pub fn new(image: &str, mount_dirs: Vec<PathBuf>, allow_network: bool) -> Self {
        Self {
            image: image.to_string(),
            mount_dirs,
            allow_network,
        }
    }

    /// Create a basic DockerBackend for coding tasks.
    /// Mounts the project directory, allows network (for npm/pip/cargo).
    pub fn coding_defaults(project_dir: &Path) -> Self {
        Self {
            image: "oneai-sandbox:latest".to_string(),
            mount_dirs: vec![project_dir.to_path_buf()],
            allow_network: true,
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
            docker_args.push(format!(
                "-v {}:{}", dir_str, dir_str
            ));
        }

        // Mount working directory specifically
        let wd_str = working_dir.to_string_lossy();
        if !self.mount_dirs.iter().any(|d| d == working_dir) {
            docker_args.push(format!(
                "-v {}:{}", wd_str, wd_str
            ));
        }

        // Set working directory in container
        docker_args.push(format!("-w {}", wd_str));

        // Network policy
        if !self.allow_network {
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
            allow_network: self.allow_network,
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
    allowed_dirs: Vec<PathBuf>,

    /// Whether network access is allowed.
    allow_network: bool,
}

impl RegexBackend {
    /// Create a new RegexBackend with the given allowed directories.
    pub fn new(allowed_dirs: Vec<PathBuf>, allow_network: bool) -> Self {
        Self { allowed_dirs, allow_network }
    }

    /// Create a basic RegexBackend for coding tasks.
    pub fn coding_defaults(project_dir: &Path) -> Self {
        Self {
            allowed_dirs: vec![project_dir.to_path_buf()],
            allow_network: true,
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
            allow_network: self.allow_network,
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
/// 2. Linux → DockerBackend (if Docker available)
/// 3. Fallback → RegexBackend (always available)
///
/// This is used by ShellTool to automatically select the appropriate
/// sandbox backend based on the platform.
pub fn default_sandbox_backend(project_dir: &Path, allow_network: bool) -> Arc<dyn SandboxBackend> {
    if cfg!(target_os = "macos") {
        let seatbelt = SeatbeltBackend::coding_defaults(project_dir);
        if seatbelt.is_available() {
            tracing::info!("Using Seatbelt sandbox backend on macOS");
            return Arc::new(seatbelt);
        }
    }

    if cfg!(target_os = "linux") {
        let docker = DockerBackend::coding_defaults(project_dir);
        if docker.is_available() {
            tracing::info!("Using Docker sandbox backend on Linux");
            return Arc::new(docker);
        }
    }

    tracing::info!("Using regex-based sandbox backend (platform-specific isolation not available)");
    Arc::new(RegexBackend::coding_defaults(project_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regex_backend_wrapping() {
        let backend = RegexBackend::coding_defaults(Path::new("/project"));
        let result = backend.wrap_command("cargo build", Path::new("/project")).unwrap();
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
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow file-read*)"));
        assert!(profile.contains("/myproject"));
        assert!(profile.contains("(allow network*)")); // allow_network=true
    }

    #[test]
    fn test_seatbelt_no_network() {
        let backend = SeatbeltBackend::new(vec![PathBuf::from("/project")], false);
        let profile = backend.generate_profile();
        assert!(profile.contains("(deny network*)"));
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn test_seatbelt_wrapping() {
        let backend = SeatbeltBackend::coding_defaults(Path::new("/project"));
        let result = backend.wrap_command("cargo test", Path::new("/project")).unwrap();
        assert!(result.shell_command.starts_with("sandbox-exec"));
        assert!(result.shell_command.contains("cargo test"));
    }

    #[test]
    fn test_docker_backend_wrapping() {
        let backend = DockerBackend::coding_defaults(Path::new("/project"));
        let result = backend.wrap_command("cargo test", Path::new("/project")).unwrap();
        assert!(result.shell_command.starts_with("docker run --rm"));
        assert!(result.shell_command.contains("-v /project:/project"));
        assert!(result.shell_command.contains("-w /project"));
        assert!(result.shell_command.contains("cargo test"));
    }

    #[test]
    fn test_docker_no_network() {
        let backend = DockerBackend::new("oneai-sandbox:latest", vec![PathBuf::from("/project")], false);
        let result = backend.wrap_command("npm install", Path::new("/project")).unwrap();
        assert!(result.shell_command.contains("--network none"));
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
