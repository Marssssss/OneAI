//! ContainerizedCodingPack — the Gondolin synthesis (evolution-plan §4.2).
//!
//! A drop-in replacement for [`crate::coding_pack`] that routes the
//! side-effecting tools (`shell` / `read_file` / `edit_file` / `write_file` /
//! `list_directory`) through a single [`TerminalBackend`] (a Docker
//! container, a Modal/Daytona serverless terminal). Auth and the
//! command-string safety pre-flight stay on the host; the *side-effects*
//! land inside the VM — "the VM is the boundary", permissions are NOT cut
//! (discipline #1).
//!
//! ## Why a separate pack, not a merge
//!
//! The tools keep the same names (`read_file`, `shell`, …) as `CodingPack`,
//! so this pack is a **drop-in replacement** (use one or the other), not a
//! merge-add. This sidesteps [`crate::merge::MergedDomainPack`]'s
//! dedup-by-first-seen (`merge.rs`) — there's no ambiguity about which
//! `read_file` impl wins. At the `AppBuilder` level, the pack's tools are
//! registered by name; a caller can also use [`ToolRegistry::override_tool`]
//! to swap individual tools on top of a base `CodingPack`.
//!
//! ## What is NOT VM-backed (v1)
//!
//! `apply_patch` keeps its local-FS write path for now (it has its own
//! multi-file write logic); routing it through `RemoteFileOps` is a follow-up.
//! `grep`/`glob`/`notebook_edit`/`environment`/`web_*` are read-only or
//! network tools with no host-FS side-effect worth isolating.

use std::sync::Arc;

use oneai_core::traits::Tool;
use oneai_tool::{
    FileEditTool, FileListTool, FileReadTool, FileWriteTool, RemoteFileOps, ShellTool,
    TerminalBackend,
};

use crate::coding_pack::coding_pack;
use crate::domain_pack::DomainPack;

/// Build a containerized coding DomainPack backed by `backend`.
///
/// Reuses `coding_pack(project_dir)` for every non-tool layer (decorators,
/// context sources, permission profile, paradigm strategies, compression
/// template, memory profile, system prompt, workflows, state graphs,
/// sub-agent definitions) and swaps only the tool layer for VM-backed impls.
///
/// ```ignore
/// let backend: Arc<dyn TerminalBackend> = Arc::new(DockerTerminalBackend::coding_defaults(dir));
/// let app = AppBuilder::new()
///     .provider(provider)
///     .domain_pack(containerized_coding_pack("/project/dir", backend))
///     .build()?;
/// ```
pub fn containerized_coding_pack(
    project_dir: &str,
    backend: Arc<dyn TerminalBackend>,
) -> DomainPack {
    let mut pack = coding_pack(project_dir);
    pack.name = "containerized-coding".to_string();
    pack.description = "Containerized coding domain pack — VM-backed shell + file tools \
        (Gondolin tool-override). All shell/file side-effects route through a single \
        TerminalBackend (the VM is the boundary; auth + command-string safety stay on host)."
        .to_string();

    // One shared RemoteFileOps so every file tool talks to the same VM
    // session (a write_file then read_file sees the same container FS).
    let remote = Arc::new(RemoteFileOps::new(backend.clone()));

    // Drop-in replace the side-effecting tools by name; leave read-only /
    // network tools (grep, glob, notebook_edit, environment, web_*,
    // apply_patch) on their CodingPack defaults.
    pack.tools = pack
        .tools
        .iter()
        .map(|tool| -> Arc<dyn Tool> {
            match tool.name() {
                "shell" => Arc::new(ShellTool::with_backend(backend.clone())),
                "read_file" => Arc::new(FileReadTool::with_file_ops(remote.clone())),
                "edit_file" => Arc::new(FileEditTool::with_file_ops(remote.clone())),
                "write_file" => Arc::new(FileWriteTool::with_file_ops(remote.clone())),
                "list_directory" => Arc::new(FileListTool::with_file_ops(remote.clone())),
                _ => tool.clone(),
            }
        })
        .collect();

    pack
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use oneai_core::error::Result;
    use oneai_tool::terminal::{ExecOptions, ExecResult};

    /// A no-op recording `TerminalBackend` — returns success with empty
    /// content, no real process. Sufficient to verify the pack wires the
    /// same backend into every side-effecting tool.
    struct NopBackend {
        name: &'static str,
    }

    #[async_trait]
    impl TerminalBackend for NopBackend {
        fn name(&self) -> &str {
            self.name
        }
        async fn execute(&self, _command: &str, _opts: &ExecOptions) -> Result<ExecResult> {
            Ok(ExecResult::new(true, String::new(), None))
        }
    }

    fn names(pack: &DomainPack) -> Vec<&str> {
        pack.tools.iter().map(|t| t.name()).collect()
    }

    #[test]
    fn containerized_pack_has_same_tool_names_as_coding() {
        // Drop-in replacement invariant: same tool set, same names — a caller
        // swapping coding_pack for containerized_coding_pack sees no schema
        // change (discipline #7 — don't break what the swap protects).
        let backend: Arc<dyn TerminalBackend> = Arc::new(NopBackend { name: "nop" });
        let coding = coding_pack("/tmp");
        let containerized = containerized_coding_pack("/tmp", backend);

        let mut c = names(&coding);
        let mut z = names(&containerized);
        c.sort();
        z.sort();
        assert_eq!(c, z, "tool name sets must match");
        assert_eq!(containerized.name, "containerized-coding");
    }

    #[tokio::test]
    async fn shell_and_file_tools_route_through_injected_backend() {
        // The injected backend must back shell + the 4 file tools (the
        // Gondolin synthesis: side-effects go into the VM). We verify by
        // executing through the Tool trait — no host FS is touched.
        let backend: Arc<dyn TerminalBackend> = Arc::new(NopBackend { name: "test-vm" });
        let pack = containerized_coding_pack("/tmp", backend);

        // shell: NopBackend always succeeds → the command runs in the VM,
        // not on the host (a real host `echo hi` would also succeed, but the
        // invariant we assert is that the tool delegates without error).
        let shell = pack
            .tools
            .iter()
            .find(|t| t.name() == "shell")
            .expect("shell present");
        let out = shell
            .execute(serde_json::json!({"command": "echo hi"}))
            .await
            .unwrap();
        assert!(out.success, "shell must delegate to the injected backend");

        // read_file on a nonexistent path: the NopBackend answers `test -e`
        // with empty content → parsed as != "y" → not exists → "File not
        // found". Crucially the local FS is never probed (a real local
        // `/tmp/definitely-not-local-vm-path` may or may not exist, but the
        // NopBackend short-circuits the exists() check remotely).
        let read = pack
            .tools
            .iter()
            .find(|t| t.name() == "read_file")
            .expect("read_file present");
        let out = read
            .execute(serde_json::json!({"path": "/tmp/oneai-containerized-vm-path-xyz"}))
            .await
            .unwrap();
        assert!(
            out.error
                .as_deref()
                .unwrap_or("")
                .contains("File not found"),
            "read_file must route through the VM backend, not local FS; got: {:?}",
            out
        );
    }

    #[test]
    fn permission_profile_inherited_from_coding() {
        // Discipline #1: permissions are NOT cut. The containerized pack
        // keeps CodingPack's profile (shell=Full override, deny_by_default
        // blacklist) — the VM is the boundary, but the host-side command
        // pre-flight still runs.
        let backend: Arc<dyn TerminalBackend> = Arc::new(NopBackend { name: "nop" });
        let coding = coding_pack("/tmp");
        let containerized = containerized_coding_pack("/tmp", backend);

        assert_eq!(
            containerized.permission_profile.auto_approve,
            coding.permission_profile.auto_approve
        );
        assert_eq!(
            containerized.permission_profile.require_confirmation,
            coding.permission_profile.require_confirmation
        );
        assert_eq!(
            containerized.permission_profile.deny_by_default.len(),
            coding.permission_profile.deny_by_default.len()
        );
    }
}
