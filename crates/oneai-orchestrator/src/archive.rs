//! Deep-hibernation volume archive (MVS3-C — 冷会话本地盘成本).
//!
//! Two-tier hibernation (design doc D6 + §6 MVS3「休眠卷归档」):
//!
//! ```text
//! Running ──idle_timeout──▶ Hibernating (docker stop, 卷保留本地, 秒级恢复)
//!                              │
//!                              └─deep_archive_timeout──▶ Hibernating+archived
//!                                   (卷导出 tar.gz → 归档存储, 容器+卷删除)
//! ```
//!
//! A deep-archived session is **not a new FSM state** — it is a `Hibernating`
//! entry carrying `PersistedEntry.archived = Some(manifest)` (adding a state
//! variant would touch every edge/predicate/route/test for zero semantic
//! gain: both tiers resume through the same `Resuming` transition).
//!
//! ## Data-loss red line
//! `runner.destroy(handle, remove_volumes=true)` may ONLY run after
//! [`VolumeArchiveStore::archive`] confirmed success. Any failure before
//! that leaves the session `Hibernating` with its local volumes intact and
//! retries on the next sweep — an archive that cannot be written must never
//! cost the session its data.
//!
//! ## MVS backend: [`LocalDirArchiveStore`]
//! tar.gz files under a configurable directory (NFS / cloud-disk mount
//! point in production). The volume export/import themselves run through
//! throwaway `alpine` containers (`docker run --rm -v <vol>:/data …tar`) —
//! no root-owned file handling on the host, no new Rust tar/gz deps. S3 and
//! friends slot in behind the trait in MVS4. Air-gapped hosts must pre-pull
//! [`ARCHIVE_HELPER_IMAGE`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{OrchestratorError, Result};

/// Throwaway helper image used to tar volumes in/out (≈8MB, universally
/// available; pre-pull on air-gapped hosts).
pub const ARCHIVE_HELPER_IMAGE: &str = "alpine:3.20";

/// Manifest of one session's successful archive. Persisted on
/// `PersistedEntry.archived` so resume knows exactly what to restore (and
/// the ops surface can show sizes/timestamps without touching the store).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveManifest {
    /// Session id the archive belongs to.
    pub session_id: String,
    /// One entry per archived docker volume.
    pub volumes: Vec<VolumeArchive>,
    /// RFC3339 completion timestamp.
    pub archived_at: String,
}

/// One volume's archive file inside the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeArchive {
    /// Docker named volume, e.g. `oneai-orch-abc-state`.
    pub volume_name: String,
    /// Archive file name relative to the store root, e.g.
    /// `abc/oneai-orch-abc-state.tar.gz`.
    pub archive_file: String,
    /// Compressed size at archive time (ops visibility; 0 if unknown).
    pub size_bytes: u64,
}

/// Object-storage seam for deep-archived session volumes. MVS ships
/// [`LocalDirArchiveStore`]; S3/GCS implementations slot in here (MVS4).
///
/// Contract:
/// - `archive` is all-or-nothing per call: on ANY failure it returns `Err`
///   and the source volumes are untouched (the caller keeps the session
///   hibernating locally — see the module data-loss red line). Re-archiving
///   the same session overwrites the previous files (idempotent layout).
/// - `restore` recreates the docker volumes named in the manifest and
///   unpacks the archives into them. Idempotent: restoring twice (or over
///   leftover volumes) is safe — tar overwrites in place.
/// - `remove` deletes the stored archive files for a session (post-resume
///   cleanup / explicit destroy). Missing files are not an error.
#[async_trait]
pub trait VolumeArchiveStore: Send + Sync {
    /// Export `volume_names` into the store; returns the manifest to persist.
    async fn archive(
        &self,
        session_id: &str,
        volume_names: &[String],
        docker_bin: &str,
    ) -> Result<ArchiveManifest>;

    /// Recreate + unpack the manifest's volumes.
    async fn restore(&self, manifest: &ArchiveManifest, docker_bin: &str) -> Result<()>;

    /// Whether an archive exists for this session.
    async fn has_archive(&self, session_id: &str) -> bool;

    /// Delete the session's archive files (tolerant of absence).
    async fn remove(&self, session_id: &str) -> Result<()>;
}

/// Sweep-side bundle: what [`crate::idle`] needs to run the deep-archive
/// pass (constructed once in `server::run` from the config).
pub struct DeepArchive {
    /// The store volumes are archived into.
    pub store: Arc<dyn VolumeArchiveStore>,
    /// A Hibernating entry older than this gets deep-archived.
    pub timeout: Duration,
    /// Docker CLI used for the throwaway export/import containers.
    pub docker_bin: String,
}

// ─── Pure argv builders (no IO, golden-tested like docker.rs) ───────────────

/// `docker run --rm -v <vol>:/data:ro -v <root>:/archive <helper>
///  sh -c "mkdir -p /archive/<parent> && tar czf /archive/<rel_file> -C /data ."`
///
/// `:ro` on the data mount — an export must never mutate the source volume.
/// `<root>` is the store root bind-mounted as `/archive`. The session
/// subdirectory is (re)created INSIDE the helper container: when the docker
/// daemon runs in a VM (colima/Docker Desktop), a host-side `create_dir_all`
/// is not necessarily visible through the bind mount, and busybox tar refuses
/// to create leading directories of the output path (MVS3-C acceptance caught
/// this: `tar: can't open '/archive/<sid>/…'`).
pub fn build_volume_export_argv(
    docker_bin: &str,
    volume_name: &str,
    store_root: &Path,
    rel_file: &str,
) -> Vec<String> {
    let parent = rel_file.rsplit_once('/').map(|(dir, _)| dir).unwrap_or(".");
    vec![
        docker_bin.into(),
        "run".into(),
        "--rm".into(),
        "-v".into(),
        format!("{volume_name}:/data:ro"),
        "-v".into(),
        format!("{}:/archive", store_root.display()),
        ARCHIVE_HELPER_IMAGE.into(),
        "sh".into(),
        "-c".into(),
        format!("mkdir -p /archive/{parent} && tar czf /archive/{rel_file} -C /data ."),
    ]
}

/// `docker volume create <vol>` (idempotent — docker returns the name when
/// the volume already exists). Mirrors `docker::build_volume_create_args`;
/// duplicated here so the archive module stays self-contained in argv terms.
pub fn build_volume_create_argv(docker_bin: &str, volume_name: &str) -> Vec<String> {
    vec![
        docker_bin.into(),
        "volume".into(),
        "create".into(),
        volume_name.into(),
    ]
}

/// `docker run --rm -v <vol>:/data -v <root>:/archive <helper>
///  tar xzf /archive/<rel_file> -C /data`
///
/// The volume mount auto-creates an absent volume, but the caller runs
/// [`build_volume_create_argv`] first anyway (explicit beats implicit, and
/// a failed create is a clearer error than a failed run).
pub fn build_volume_import_argv(
    docker_bin: &str,
    volume_name: &str,
    store_root: &Path,
    rel_file: &str,
) -> Vec<String> {
    vec![
        docker_bin.into(),
        "run".into(),
        "--rm".into(),
        "-v".into(),
        format!("{volume_name}:/data"),
        "-v".into(),
        format!("{}:/archive", store_root.display()),
        ARCHIVE_HELPER_IMAGE.into(),
        "tar".into(),
        "xzf".into(),
        format!("/archive/{rel_file}"),
        "-C".into(),
        "/data".into(),
    ]
}

// ─── LocalDirArchiveStore ────────────────────────────────────────────────────

/// Filesystem-backed [`VolumeArchiveStore`]: one directory per session under
/// `root` holding `<volume>.tar.gz` files plus a `manifest.json`. Point
/// `root` at an NFS / cloud-disk mount for real cold storage; the semantics
/// are plain-POSIX.
///
/// Ownership note: the tar runs as root inside the helper container, so on a
/// Linux host a NON-root orchestrator may not be able to chmod/remove the
/// produced files (best-effort chmod 0644 after each export). Run the
/// orchestrator as root, or point `archive_dir` at a share that maps uids
/// (colima/macOS and NFS squashed mounts do), when deep-archive is enabled.
pub struct LocalDirArchiveStore {
    root: PathBuf,
}

impl LocalDirArchiveStore {
    /// New store rooted at `root` (created on first archive).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Store root (ops/tests).
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    /// Relative (to root) archive path for one volume of one session. The
    /// volume name is `[A-Za-z0-9_.-]` by docker's own rules, so it is safe
    /// as a path segment; session ids are validated `[a-zA-Z0-9_-]+`.
    fn rel_archive_file(session_id: &str, volume_name: &str) -> String {
        format!("{session_id}/{volume_name}.tar.gz")
    }

    fn manifest_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("manifest.json")
    }

    async fn run_docker(argv: &[String]) -> Result<String> {
        let out = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| OrchestratorError::Runner(format!("{}: {e}", argv[0])))?;
        if !out.status.success() {
            return Err(OrchestratorError::Runner(format!(
                "archive helper `{} {}` failed ({}): {}",
                argv[0],
                argv.get(1).map(|s| s.as_str()).unwrap_or("?"),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

#[async_trait]
impl VolumeArchiveStore for LocalDirArchiveStore {
    async fn archive(
        &self,
        session_id: &str,
        volume_names: &[String],
        docker_bin: &str,
    ) -> Result<ArchiveManifest> {
        if volume_names.is_empty() {
            return Err(OrchestratorError::Runner(
                "archive called with no volumes".into(),
            ));
        }
        tokio::fs::create_dir_all(self.session_dir(session_id)).await?;
        let mut volumes = Vec::with_capacity(volume_names.len());
        for vol in volume_names {
            let rel = Self::rel_archive_file(session_id, vol);
            Self::run_docker(&build_volume_export_argv(docker_bin, vol, &self.root, &rel)).await?;
            // The tar ran as root inside the helper container — make sure a
            // non-root orchestrator can still read/remove it later.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = tokio::fs::set_permissions(
                    self.root.join(&rel),
                    std::fs::Permissions::from_mode(0o644),
                )
                .await;
            }
            let size_bytes = tokio::fs::metadata(self.root.join(&rel))
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            volumes.push(VolumeArchive {
                volume_name: vol.clone(),
                archive_file: rel,
                size_bytes,
            });
        }
        let manifest = ArchiveManifest {
            session_id: session_id.to_string(),
            volumes,
            archived_at: chrono::Utc::now().to_rfc3339(),
        };
        // Manifest written LAST: a crash mid-archive leaves no manifest, so
        // `has_archive`/`restore` never see a half-written archive set (and
        // the next sweep simply re-archives over the partial files).
        let json = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| OrchestratorError::Persist(e.to_string()))?;
        tokio::fs::write(self.manifest_path(session_id), &json).await?;
        Ok(manifest)
    }

    async fn restore(&self, manifest: &ArchiveManifest, docker_bin: &str) -> Result<()> {
        for va in &manifest.volumes {
            let abs = self.root.join(&va.archive_file);
            if !abs.exists() {
                return Err(OrchestratorError::Runner(format!(
                    "archive file missing for volume {}: {} (store root: {})",
                    va.volume_name,
                    abs.display(),
                    self.root.display()
                )));
            }
            Self::run_docker(&build_volume_create_argv(docker_bin, &va.volume_name)).await?;
            Self::run_docker(&build_volume_import_argv(
                docker_bin,
                &va.volume_name,
                &self.root,
                &va.archive_file,
            ))
            .await?;
        }
        Ok(())
    }

    async fn has_archive(&self, session_id: &str) -> bool {
        self.manifest_path(session_id).exists()
    }

    async fn remove(&self, session_id: &str) -> Result<()> {
        // remove_dir_all tolerates absence via NotFound mapping below.
        match tokio::fs::remove_dir_all(self.session_dir(session_id)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_argv_golden() {
        assert_eq!(
            build_volume_export_argv(
                "docker",
                "oneai-orch-s1-state",
                Path::new("/srv/oneai-archive"),
                "s1/oneai-orch-s1-state.tar.gz"
            ),
            vec![
                "docker",
                "run",
                "--rm",
                "-v",
                "oneai-orch-s1-state:/data:ro",
                "-v",
                "/srv/oneai-archive:/archive",
                "alpine:3.20",
                "sh",
                "-c",
                "mkdir -p /archive/s1 && tar czf /archive/s1/oneai-orch-s1-state.tar.gz -C /data .",
            ]
        );
    }

    #[test]
    fn import_argv_golden() {
        assert_eq!(
            build_volume_import_argv(
                "docker",
                "oneai-orch-s1-ws",
                Path::new("/srv/oneai-archive"),
                "s1/oneai-orch-s1-ws.tar.gz"
            ),
            vec![
                "docker",
                "run",
                "--rm",
                "-v",
                "oneai-orch-s1-ws:/data",
                "-v",
                "/srv/oneai-archive:/archive",
                "alpine:3.20",
                "tar",
                "xzf",
                "/archive/s1/oneai-orch-s1-ws.tar.gz",
                "-C",
                "/data",
            ]
        );
    }

    #[test]
    fn volume_create_argv_golden() {
        assert_eq!(
            build_volume_create_argv("docker", "v1"),
            vec!["docker", "volume", "create", "v1"]
        );
    }

    #[tokio::test]
    async fn local_store_manifest_lifecycle_without_docker() {
        // Manifest write/read/remove are pure filesystem — exercised without
        // the docker-dependent archive path (that one is covered by the
        // real-docker acceptance run).
        let dir = tempfile::tempdir().unwrap();
        let store = LocalDirArchiveStore::new(dir.path());
        assert!(!store.has_archive("s1").await);
        let manifest = ArchiveManifest {
            session_id: "s1".into(),
            volumes: vec![VolumeArchive {
                volume_name: "oneai-orch-s1-state".into(),
                archive_file: LocalDirArchiveStore::rel_archive_file("s1", "oneai-orch-s1-state"),
                size_bytes: 42,
            }],
            archived_at: "2026-09-13T00:00:00+00:00".into(),
        };
        tokio::fs::create_dir_all(store.session_dir("s1"))
            .await
            .unwrap();
        tokio::fs::write(
            store.root.join(&manifest.volumes[0].archive_file),
            b"not really a tar",
        )
        .await
        .unwrap();
        tokio::fs::write(
            store.manifest_path("s1"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .await
        .unwrap();
        assert!(store.has_archive("s1").await);
        // Restore with an intact manifest + file gets as far as the docker
        // call; with a MISSING file it must fail before any docker spawn.
        tokio::fs::remove_file(store.root.join(&manifest.volumes[0].archive_file))
            .await
            .unwrap();
        let err = store.restore(&manifest, "docker").await.unwrap_err();
        assert!(err.to_string().contains("archive file missing"), "{err}");
        // remove() clears the session dir; second remove is a tolerant no-op.
        store.remove("s1").await.unwrap();
        assert!(!store.has_archive("s1").await);
        store.remove("s1").await.unwrap();
    }

    #[test]
    fn manifest_serde_roundtrip() {
        let m = ArchiveManifest {
            session_id: "s1".into(),
            volumes: vec![VolumeArchive {
                volume_name: "v".into(),
                archive_file: "s1/v.tar.gz".into(),
                size_bytes: 7,
            }],
            archived_at: "2026-09-13T00:00:00+00:00".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<ArchiveManifest>(&json).unwrap(), m);
    }
}
