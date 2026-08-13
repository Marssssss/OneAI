//! Scenario library — the shared, front-end-agnostic store for multi-agent
//! scenarios, surfaced over the `scenario/*` JSON-RPC methods.
//!
//! Every non-Rust frontend (macOS / VS Code / browser) edits and persists
//! scenarios through this one store rather than each keeping a private local
//! file (the macOS app's `~/Library/Application Support/oneai_scenarios.json`
//! was Swift-only). The store holds the rich [`BusScenario`] (cast + turn
//! policy + topic-intake fields + debrief + review loop + locale) — the unit
//! a scenario *editor* manipulates. At launch the frontend compiles it to a
//! [`BusGroupScenario`] (engine payload) via
//! [`BusScenario::to_group_scenario`] and submits `group/start`; the engine
//! never sees the editor-only fields.
//!
//! [`BusScenario::validate`] is the single authoritative validator (the macOS
//! `ScenarioEditor.validate` client-side mirror and any future VS Code /
//! browser editor both call `scenario/validate`), killing the per-frontend
//! drift the old comment called out.
//!
//! `FileScenarioStore` persists to `~/.oneai/scenarios.json` with an atomic
//! temp-file + rename (mirrors `FileJobStore` / `FileWorkingStateStore`) so a
//! crash mid-write can't corrupt the library.

use std::path::PathBuf;

use async_trait::async_trait;
use tokio::sync::Mutex;

use oneai_bus::{BusDebriefConfig, BusScenario, BusScenarioMember, BusTopicField};

/// A scenario library backing the `scenario/*` methods. Object-safe via
/// `#[async_trait]` so the app-server holds `Arc<dyn ScenarioStore + Send +
/// Sync>` and tests can swap an in-memory impl.
#[async_trait]
pub trait ScenarioStore: Send + Sync {
    /// All scenarios, in store order (the sidebar's display order).
    async fn list(&self) -> std::io::Result<Vec<BusScenario>>;
    /// One scenario by id, or `None` if unknown.
    async fn get(&self, id: &str) -> std::io::Result<Option<BusScenario>>;
    /// Insert or replace by `scenario.id`. The caller is expected to have
    /// validated first (see `scenario/validate`); the store does not re-check.
    async fn upsert(&self, scenario: BusScenario) -> std::io::Result<()>;
    /// Remove by id; no-op if absent.
    async fn delete(&self, id: &str) -> std::io::Result<()>;
}

/// Default file location — `~/.oneai/scenarios.json` (sits next to the
/// supervisor's `instances.json` / the working-state store).
pub fn default_scenarios_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".oneai")
        .join("scenarios.json")
}

/// File-backed [`ScenarioStore`]. The whole library is one JSON array under a
/// `Mutex` (scenarios are small and edits are infrequent — a single lock
/// serializing read-modify-write is simpler than a concurrent map and
/// guarantees no torn reads during the atomic rename).
pub struct FileScenarioStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileScenarioStore {
    /// Open (or create) the store at `path`. If the file does not yet exist,
    /// seed it with [`builtin_presets`] so a fresh install (e.g. a VS Code
    /// extension with no macOS app ever run) isn't an empty library. The
    /// macOS app's richer 5×2 locale preset set is upserted on its first run,
    /// layering over these defaults.
    pub async fn new(path: PathBuf) -> std::io::Result<Self> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.ok();
            }
            let store = Self {
                path: path.clone(),
                lock: Mutex::new(()),
            };
            store.write_all_unlocked(&builtin_presets()).await?;
            return Ok(store);
        }
        Ok(Self {
            path,
            lock: Mutex::new(()),
        })
    }

    // Write without acquiring the lock — used only from `new` before anyone
    // else can hold the lock (the store was just constructed). The locked
    // read/write helpers live in the impl block below.
    async fn write_all_unlocked(&self, scenarios: &[BusScenario]) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(scenarios)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }
}

#[async_trait]
impl ScenarioStore for FileScenarioStore {
    async fn list(&self) -> std::io::Result<Vec<BusScenario>> {
        let _guard = self.lock.lock().await;
        self.read_all_locked().await
    }

    async fn get(&self, id: &str) -> std::io::Result<Option<BusScenario>> {
        let _guard = self.lock.lock().await;
        Ok(self
            .read_all_locked()
            .await?
            .into_iter()
            .find(|s| s.id == id))
    }

    async fn upsert(&self, scenario: BusScenario) -> std::io::Result<()> {
        let _guard = self.lock.lock().await;
        let mut all = self.read_all_locked().await?;
        if let Some(existing) = all.iter_mut().find(|s| s.id == scenario.id) {
            *existing = scenario;
        } else {
            all.push(scenario);
        }
        self.write_all_locked(&all).await
    }

    async fn delete(&self, id: &str) -> std::io::Result<()> {
        let _guard = self.lock.lock().await;
        let mut all = self.read_all_locked().await?;
        let before = all.len();
        all.retain(|s| s.id != id);
        if all.len() != before {
            self.write_all_locked(&all).await?;
        }
        Ok(())
    }
}

impl FileScenarioStore {
    /// Read holding the lock already acquired (helpers called under `_guard`).
    async fn read_all_locked(&self) -> std::io::Result<Vec<BusScenario>> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => {
                if bytes.is_empty() {
                    return Ok(Vec::new());
                }
                serde_json::from_slice(&bytes).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Write holding the lock already acquired.
    async fn write_all_locked(&self, scenarios: &[BusScenario]) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(scenarios)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }
}

/// The default scenario set seeded into a fresh store. Two presets exercising
/// the turn-policy variety: a moderator-driven interview (moderator picks the
/// next speaker) and a scripted writing workshop with a review-revise loop.
/// This is a minimal starter library for non-macOS frontends; the macOS app
/// upserts its richer 5×2-locale preset set on first run, layering over these.
pub fn builtin_presets() -> Vec<BusScenario> {
    vec![
        BusScenario {
            id: "preset-interview".into(),
            name: "Interview Practice".into(),
            icon: Some("person.2.crop.square.stack".into()),
            members: vec![
                BusScenarioMember {
                    id: "interviewer".into(),
                    name: "Interviewer".into(),
                    role: Some("Interviewer".into()),
                    system_prompt:
                        "You are a senior interviewer. Ask targeted questions about the role."
                            .into(),
                    kind: "openai".into(),
                    model: String::new(),
                    api_key: None,
                    base_url: None,
                    color: Some("#4D6BFE".into()),
                    avatar: Some("person.fill.viewfinder".into()),
                },
                BusScenarioMember {
                    id: "coach".into(),
                    name: "Coach".into(),
                    role: Some("Coach".into()),
                    system_prompt:
                        "You are a career coach. After the interviewer's turn, give brief, specific feedback to the user."
                            .into(),
                    kind: "openai".into(),
                    model: String::new(),
                    api_key: None,
                    base_url: None,
                    color: Some("#34C759".into()),
                    avatar: Some("graduationcap.fill".into()),
                },
            ],
            turn_policy: "moderator".into(),
            script_order: None,
            moderator_id: Some("interviewer".into()),
            opener_agent_id: Some("interviewer".into()),
            opener_line: Some("Let's begin. Tell me about yourself.".into()),
            topic_fields: Some(vec![BusTopicField {
                id: "role".into(),
                label: "Target role".into(),
                placeholder: Some("e.g. Senior Backend Engineer".into()),
                visible_to: None,
            }]),
            debrief: Some(BusDebriefConfig {
                button_label: "End interview".into(),
                summary_prompt: "Summarize the user's performance and give 3 improvement points."
                    .into(),
                debrief_member_id: "coach".into(),
            }),
            review_loop: None,
            locale: None,
        },
        BusScenario {
            id: "preset-writing-workshop".into(),
            name: "Writing Workshop".into(),
            icon: Some("pencil.and.ruler".into()),
            members: vec![
                BusScenarioMember {
                    id: "writer".into(),
                    name: "Writer".into(),
                    role: Some("Writer".into()),
                    system_prompt: "You are a writer. Draft the requested passage.".into(),
                    kind: "openai".into(),
                    model: String::new(),
                    api_key: None,
                    base_url: None,
                    color: Some("#4D6BFE".into()),
                    avatar: Some("pencil".into()),
                },
                BusScenarioMember {
                    id: "editor".into(),
                    name: "Editor".into(),
                    role: Some("Editor".into()),
                    system_prompt:
                        "You are an editor. Review the draft; emit `定稿` when you approve."
                            .into(),
                    kind: "openai".into(),
                    model: String::new(),
                    api_key: None,
                    base_url: None,
                    color: Some("#FF9500".into()),
                    avatar: Some("checkmark.seal".into()),
                },
            ],
            turn_policy: "scripted".into(),
            script_order: Some(vec!["writer".into(), "editor".into()]),
            moderator_id: None,
            opener_agent_id: Some("writer".into()),
            opener_line: None,
            topic_fields: Some(vec![BusTopicField {
                id: "topic".into(),
                label: "Writing topic".into(),
                placeholder: None,
                visible_to: None,
            }]),
            debrief: None,
            review_loop: Some(oneai_bus::BusReviewLoop {
                reviewer_id: "editor".into(),
                approve_marker: "定稿".into(),
                max_rounds: 3,
            }),
            locale: None,
        },
    ]
}

/// An in-memory [`ScenarioStore`] for tests — no file IO, deterministic.
pub struct InMemoryScenarioStore {
    scenarios: Mutex<Vec<BusScenario>>,
}

impl Default for InMemoryScenarioStore {
    fn default() -> Self {
        Self {
            scenarios: Mutex::new(Vec::new()),
        }
    }
}

impl InMemoryScenarioStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn from_seed(scenarios: Vec<BusScenario>) -> Self {
        Self {
            scenarios: Mutex::new(scenarios),
        }
    }
}

#[async_trait]
impl ScenarioStore for InMemoryScenarioStore {
    async fn list(&self) -> std::io::Result<Vec<BusScenario>> {
        Ok(self.scenarios.lock().await.clone())
    }
    async fn get(&self, id: &str) -> std::io::Result<Option<BusScenario>> {
        Ok(self
            .scenarios
            .lock()
            .await
            .iter()
            .find(|s| s.id == id)
            .cloned())
    }
    async fn upsert(&self, scenario: BusScenario) -> std::io::Result<()> {
        let mut guard = self.scenarios.lock().await;
        if let Some(existing) = guard.iter_mut().find(|s| s.id == scenario.id) {
            *existing = scenario;
        } else {
            guard.push(scenario);
        }
        Ok(())
    }
    async fn delete(&self, id: &str) -> std::io::Result<()> {
        self.scenarios.lock().await.retain(|s| s.id != id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    async fn fresh_file_store(dir: &Path) -> FileScenarioStore {
        FileScenarioStore::new(dir.join("scenarios.json"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn new_file_seeds_builtin_presets() {
        let tmp = tempfile::tempdir().unwrap();
        let store = fresh_file_store(tmp.path()).await;
        let list = store.list().await.unwrap();
        assert!(!list.is_empty(), "a fresh store seeds defaults");
        assert!(list.iter().any(|s| s.id == "preset-interview"));
        assert!(list.iter().any(|s| s.id == "preset-writing-workshop"));
    }

    #[tokio::test]
    async fn builtin_presets_validate_clean() {
        // The seeded defaults must be launchable — no drift between seeder
        // and the canonical validator.
        for s in builtin_presets() {
            assert!(s.is_valid(), "preset {} invalid: {:?}", s.id, s.validate());
        }
    }

    #[tokio::test]
    async fn upsert_inserts_then_replaces_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        let store = fresh_file_store(tmp.path()).await;
        let mut s = BusScenario {
            id: "custom".into(),
            name: "Custom".into(),
            icon: None,
            members: vec![BusScenarioMember {
                id: "a".into(),
                name: "A".into(),
                role: None,
                system_prompt: "p".into(),
                kind: "openai".into(),
                model: String::new(),
                api_key: None,
                base_url: None,
                color: None,
                avatar: None,
            }],
            turn_policy: "roundrobin".into(),
            script_order: None,
            moderator_id: None,
            opener_agent_id: None,
            opener_line: None,
            topic_fields: None,
            debrief: None,
            review_loop: None,
            locale: None,
        };
        store.upsert(s.clone()).await.unwrap();
        assert_eq!(store.get("custom").await.unwrap().unwrap().name, "Custom");
        // Replace by id.
        s.name = "Custom v2".into();
        store.upsert(s).await.unwrap();
        let all = store.list().await.unwrap();
        assert_eq!(all.iter().filter(|x| x.id == "custom").count(), 1);
        assert_eq!(
            store.get("custom").await.unwrap().unwrap().name,
            "Custom v2"
        );
    }

    #[tokio::test]
    async fn delete_removes_and_is_noop_for_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let store = fresh_file_store(tmp.path()).await;
        let before = store.list().await.unwrap().len();
        store.delete("preset-interview").await.unwrap();
        let after = store.list().await.unwrap().len();
        assert_eq!(after, before - 1);
        // Unknown id — no-op, no error.
        store.delete("nope").await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), after);
    }

    #[tokio::test]
    async fn atomic_rename_leaves_no_tmp_on_crash_simulation() {
        // After a clean write, the temp file must not linger (rename moved it).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let store = fresh_file_store(&dir).await;
        let s = builtin_presets().into_iter().next().unwrap();
        store.upsert(s).await.unwrap();
        let tmp_file = dir.join("scenarios.json.tmp");
        assert!(
            !tmp_file.exists(),
            "temp file should not linger after rename"
        );
        assert!(dir.join("scenarios.json").exists());
    }

    #[tokio::test]
    async fn in_memory_store_round_trips() {
        let store = InMemoryScenarioStore::from_seed(builtin_presets());
        assert_eq!(store.list().await.unwrap().len(), 2);
        assert!(store.get("preset-interview").await.unwrap().is_some());
        store.delete("preset-interview").await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), 1);
    }
}
