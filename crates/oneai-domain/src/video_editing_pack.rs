//! VideoEditingPack — the video-editing domain configuration pack.
//!
//! The third concrete DomainPack implementation, built from the
//! "剪辑Agent设计方案" (§34003d23). It turns a generic agent into a video editor
//! that clips one-or-more video/image materials into a themed finished video,
//! driven by an optional user brief (构想).
//!
//! The design doc's architecture conclusion is a **hybrid**: a deterministic
//! directed-graph workflow as the skeleton, with LLM nodes making the creative
//! decisions. The EDL (edit timeline JSON) is the single source of truth — all
//! edit decisions land in it, and `render` is its deterministic execution.
//!
//! Layer mapping (vs. CodingPack):
//! - **Tools** — the ffmpeg/ffprobe media tools (`oneai_tool::media_tools`),
//!   gated on binary presence so they vanish from the schema when ffmpeg is
//!   absent (Footprint gate).
//! - **ContextSources** — a [`MediaAssetInventorySource`] that senses the
//!   material on disk (what files exist, their type/size/duration).
//! - **PermissionProfile** — read probes/validators auto-approved; render and
//!   motion-generation require confirmation (they are long, resource-heavy).
//! - **ParadigmStrategies** — edit tasks → Plan+ReAct(+Reflect); material
//!   analysis → Explore.
//! - **CompressionTemplate** — preserve the brief, material index, timeline,
//!   and QA defects across context compression.
//! - **MemoryProfile** — taste/habit extraction (画幅/转场/配乐口味) + project
//!   templates, decay-enabled (see `MemoryProfile::video_editing`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::Result;
use oneai_core::traits::Tool;
use oneai_core::PermissionLevel;
use oneai_tool::{
    ImageToMotionTool, MakeThumbnailTool, ProbeMediaTool, RenderTool, SampleFramesTool,
    ValidateTimelineTool, VerifyOutputTool,
};

use oneai_workflow::{
    EdgeCondition, GraphEdge, GraphNode, NodeAction, StateGraph, StepConfig, WorkflowConfig,
};

use crate::compression_template::CompressionTemplate;
use crate::context_source::{ContextPosition, ContextSource, RefreshPolicy};
use crate::domain_pack::DomainPack;
use crate::paradigm_strategy::{
    DomainParadigmKind, ParadigmStrategy, SubAgentMergeStrategy, SubAgentTypeDefinition,
};
use crate::permission_profile::PermissionProfile;

// ─── Video Editing System Prompt ───────────────────────────────────────────────

/// The video-editing domain system prompt template.
///
/// Adapted from the design doc §1 (角色/输入/决策优先级/工作纪律/硬性约束/输出契约).
/// Like [`crate::coding_pack::CODING_SYSTEM_PROMPT`], tool-selection specifics are
/// deferred to the `{{TOOL_PREFERENCE_RULES}}` marker (resolved against the
/// visible tool set at turn start), so a paradigm/tool filter can never leave a
/// stale "use X" promise. The workflow prose names *stages*, not commands.
pub const VIDEO_EDITING_SYSTEM_PROMPT: &str = "\
You are a professional video editor. Your job is to clip the user's video/image \
materials into a single finished video that is thematically coherent and \
well-paced, and — when the user provides a brief (构想) — to make every editing \
decision serve that brief.

# Inputs
- materials: one or more video/image files (surfaced to you via the material \
  inventory context).
- brief (optional): the user's creative concept.

{{TOOL_PREFERENCE_RULES}}

# Decision priority (must be obeyed, in order)
1. The user's explicit brief > the theme the material itself suggests > default strategy.
2. When a brief exists, structure everything (clip order, pacing, mood, captions) \
   around it. If the material cannot support part of the brief, say so explicitly \
   and do NOT pad the gap with unrelated footage.
3. When there is no brief, first analyze the material to infer its theme and mood, \
   then propose an edit plan. If theme confidence is low, ask the user to confirm \
   rather than assuming.

# Work discipline
1. Always 素材盘点 (inventory + probe the material) before 剪辑设计 (edit design), \
   and only then render. Never skip analysis and jump straight to concatenation.
2. Produce a structured timeline (EDL JSON) before rendering; every edit decision \
   must be expressed in the timeline — rendering is just its deterministic execution.
3. Each revision is an incremental re-edit (locate the affected clips), unless the \
   user asks to start over.
4. Every deliverable must pass self-check (duration, black frames, silence) before \
   delivery; if it fails, fix and re-check.

# Hard constraints
- Never fabricate content that is not in the material (no fake \"real footage\").
- Do not remove watermarks, do not use unlicensed music, and refuse face-swap / \
  impersonation / forged-evidence requests.
- Images must be animated with camera motion (Ken Burns etc.); never show a static \
  image for more than a few seconds.
- Default output spec when unspecified: 1080p, 25/30fps aligned to the source, \
  -14 LUFS loudness, H.264 + AAC.

# Output contract
- Report progress with one line after each stage.
- On delivery, give: the finished-file path, duration, resolution, a shot-structure \
  summary, and the list of materials not used.
{{MODEL_DRIVEN_CONTROL_TOOLS}}";

// ─── Video Editing Sub-Agent Types ─────────────────────────────────────────────

/// Sub-agent types available in the video-editing domain.
fn video_editing_sub_agent_types() -> Vec<SubAgentTypeDefinition> {
    vec![
        SubAgentTypeDefinition {
            name: "editor".to_string(),
            description: "Designs the edit timeline (EDL) and renders the finished video"
                .to_string(),
            system_prompt: "You are a video editing agent. Probe the material, design an \
                edit timeline (EDL JSON) that matches the brief (or the material's inferred \
                theme), validate it, render it, and verify the output. Return the output \
                path plus a shot-structure summary and any materials you could not use."
                .to_string(),
            available_tools: vec![
                "probe_media".to_string(),
                "sample_frames".to_string(),
                "validate_timeline".to_string(),
                "render".to_string(),
                "image_to_motion".to_string(),
                "make_thumbnail".to_string(),
                "verify_output".to_string(),
            ],
            permission_threshold: PermissionLevel::Standard,
            budget: 80_000,
            modifies_files: true,
            merge_strategy: SubAgentMergeStrategy::PreserveOnly,
            structured_output: None,
        },
        SubAgentTypeDefinition {
            name: "reviewer".to_string(),
            description: "Quality-checks a rendered video and reports defects".to_string(),
            system_prompt: "You are a video QA agent. Run verification on the rendered \
                output (duration, black frames, silence) and report pass/fail with the \
                specific defects to fix. Do not re-render yourself; report findings."
                .to_string(),
            available_tools: vec!["verify_output".to_string(), "probe_media".to_string()],
            permission_threshold: PermissionLevel::Read,
            budget: 20_000,
            modifies_files: false,
            merge_strategy: SubAgentMergeStrategy::PreserveOnly,
            structured_output: None,
        },
    ]
}

// ─── VideoEditingPack Factory ──────────────────────────────────────────────────

/// Create a VideoEditingPack DomainPack for the given working directory.
///
/// ```ignore
/// let app = AppBuilder::new()
///     .provider(provider)
///     .domain_pack(video_editing_pack("/path/to/material"))  // ← one-line domain switch
///     .build()?;
/// ```
///
/// The pack provides:
/// - 7 media tools (probe, sample, validate, render, image_to_motion, thumbnail, verify)
/// - A material-asset inventory context source (plus date/environment)
/// - A permission profile (probe/validate auto-approved; render confirmed)
/// - Edit/material-analysis paradigm strategies
/// - A video-editing compression template (preserve brief/timeline/defects)
/// - The video-editing memory profile (taste/habit extraction)
/// - A predefined edit-loop StateGraph + a deterministic deliver workflow
pub fn video_editing_pack(project_dir: &str) -> DomainPack {
    DomainPack {
        name: "video_editing".to_string(),
        description: "Video editing domain pack — clip video/image materials into a themed finished video, driven by an optional user brief".to_string(),

        // Layer 1: Domain-specific tools (ffmpeg/ffprobe media tools). Each is
        // gated on binary presence via `service_available()` so an ffmpeg-less
        // host sees zero broken options (Footprint gate).
        tools: vec![
            Arc::new(ProbeMediaTool::new()) as Arc<dyn Tool>,
            Arc::new(SampleFramesTool::new()) as Arc<dyn Tool>,
            Arc::new(ValidateTimelineTool::new()) as Arc<dyn Tool>,
            Arc::new(RenderTool::new()) as Arc<dyn Tool>,
            Arc::new(ImageToMotionTool::new()) as Arc<dyn Tool>,
            Arc::new(MakeThumbnailTool::new()) as Arc<dyn Tool>,
            Arc::new(VerifyOutputTool::new()) as Arc<dyn Tool>,
        ],

        // Layer 1 supplement: tool decorators. The media tools already carry
        // rich descriptions; no description overrides are needed here.
        tool_decorators: vec![],

        // Layer 2: Context sources — the material inventory is the primary
        // domain sense; date + environment provide ambient context.
        context_sources: vec![
            Arc::new(MediaAssetInventorySource::new(project_dir)), // priority 10
            Arc::new(crate::builtin_sources::DateSource::new()),
            Arc::new(crate::builtin_sources::EnvironmentInfoSource::new()),
        ],

        // Layer 3: Permission profile — probes/validation are read-only and
        // auto-approved; render and motion-generation are resource-heavy and
        // require confirmation.
        permission_profile: PermissionProfile {
            name: "video_editing".to_string(),
            auto_approve: HashSet::from([
                "probe_media".to_string(),
                "sample_frames".to_string(),
                "validate_timeline".to_string(),
                "verify_output".to_string(),
                "make_thumbnail".to_string(),
            ]),
            require_confirmation: HashSet::from([
                "render".to_string(),
                "image_to_motion".to_string(),
            ]),
            deny_by_default: Vec::new(),
            permission_overrides: HashMap::from([
                ("render".to_string(), PermissionLevel::Full),
                ("image_to_motion".to_string(), PermissionLevel::Standard),
            ]),
            default_threshold: PermissionLevel::Standard,
            approval_policy: oneai_core::ApprovalPolicy::OnFailure,
            trusted_dirs: Vec::new(),
            exec_policy: None,
            tool_exposure: HashMap::new(),
        },

        // Layer 4: Paradigm strategies — editing tasks are Plan+ReAct(+Reflect);
        // material analysis is Explore.
        paradigm_strategies: vec![
            ParadigmStrategy {
                trigger_pattern: "剪辑|剪视频|成片|视频|混剪|编辑|制作|剪接|配乐|字幕|montage|edit|clip|cut"
                    .to_string(),
                paradigm_sequence: vec![
                    DomainParadigmKind::Plan,
                    DomainParadigmKind::ReAct,
                    DomainParadigmKind::Reflect,
                ],
                sub_agent_types: video_editing_sub_agent_types(),
                description: "Editing tasks require planning the timeline, executing the edit, and QA review".to_string(),
            },
            ParadigmStrategy {
                trigger_pattern: "分析|素材|盘点|理解|画面|镜头|讲了什么|这是什么|里面"
                    .to_string(),
                paradigm_sequence: vec![DomainParadigmKind::Explore],
                sub_agent_types: vec![video_editing_sub_agent_types()[0].clone()],
                description: "Material analysis uses the exploration paradigm (probe + sample + describe)".to_string(),
            },
        ],

        // Layer 5: Compression template — preserve the brief, material index,
        // timeline (EDL), and QA defects across context compression.
        compression_template: CompressionTemplate {
            name: "video_editing".to_string(),
            preserve_fields: vec![
                "brief".to_string(),
                "material_index".to_string(),
                "timeline".to_string(),
                "edit_decisions".to_string(),
                "current_segment".to_string(),
                "defects".to_string(),
            ],
            template: VIDEO_EDITING_COMPRESSION_TEMPLATE.to_string(),
            truncate_rules: HashMap::from([
                ("probe_output".to_string(), 800),   // ffprobe summaries stay short
                ("frame_list".to_string(), 600),     // frame path lists
                ("timeline".to_string(), 4000),      // the EDL is the fact source — keep it
            ]),
            default_variables: HashMap::from([
                ("default_spec".to_string(), "1080p, 30fps, H.264+AAC".to_string()),
            ]),
        },

        // Layer 7: Memory profile — taste/habit extraction + decay (see preset).
        memory_profile: crate::memory_profile::MemoryProfile::video_editing(),

        // System prompt
        system_prompt_template: VIDEO_EDITING_SYSTEM_PROMPT.to_string(),

        // Layer 6: Predefined workflows and StateGraphs
        workflows: vec![video_deliver_workflow()],
        state_graphs: vec![video_edit_loop_graph(), video_qa_loop_graph()],

        // Sub-agent definitions: an editor (renders) + a reviewer (QA).
        sub_agent_definitions: video_editing_sub_agent_types(),
    }
}

// ─── MediaAssetInventorySource ─────────────────────────────────────────────────

/// Media extensions recognized as video vs. image material.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mov", "avi", "mkv", "webm", "m4v", "mpg", "mpeg", "flv", "wmv", "mts", "m2ts", "3gp",
];
const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "bmp", "gif", "heic", "heif", "tif", "tiff",
];

/// A context source that inventories the video/image material in the working
/// directory — the domain's equivalent of the coding `git_status`/`repo_map`.
///
/// It scans (shallowly) for media files, reports each asset's type and size, and
/// — when `ffprobe` is present — probes the first few for duration. This is what
/// lets the agent "盘点素材" before planning an edit.
///
/// Refresh policy: OnceAtStart (the material set is stable during a session).
/// Position: Tail (re-scanned and can grow as the agent renders intermediates).
pub struct MediaAssetInventorySource {
    project_dir: Arc<std::sync::RwLock<PathBuf>>,
}

impl MediaAssetInventorySource {
    pub fn new(project_dir: &str) -> Self {
        Self {
            project_dir: Arc::new(std::sync::RwLock::new(PathBuf::from(project_dir))),
        }
    }
}

/// Recursively collect media files up to `max_depth`, capped at `limit` entries.
fn collect_media_files(root: &Path, max_depth: usize, limit: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > max_depth || found.len() >= limit {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip hidden + common non-material dirs to keep the scan cheap.
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "target" || name == "node_modules" {
                    continue;
                }
                stack.push((path, depth + 1));
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let ext = ext.to_ascii_lowercase();
                if VIDEO_EXTS.contains(&ext.as_str()) || IMAGE_EXTS.contains(&ext.as_str()) {
                    found.push(path);
                }
            }
        }
    }
    found.sort();
    found
}

/// Best-effort duration probe via `ffprobe` (blocking; used for a small subset).
fn probe_duration(path: &Path) -> Option<f64> {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .ok()
}

#[async_trait]
impl ContextSource for MediaAssetInventorySource {
    fn key(&self) -> &str {
        "media_asset_inventory"
    }

    async fn load(&self) -> Result<String> {
        let dir = self.project_dir.read().unwrap().clone();
        let files = collect_media_files(&dir, 2, 60);
        if files.is_empty() {
            return Ok(
                "Material Inventory: no video/image files found in the working directory"
                    .to_string(),
            );
        }

        let mut out = format!("Material Inventory ({} media files):\n", files.len());
        let has_ffprobe = std::process::Command::new("ffprobe")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        for (i, path) in files.iter().enumerate() {
            let rel = path.strip_prefix(&dir).unwrap_or(path);
            let kind = if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let e = ext.to_ascii_lowercase();
                if IMAGE_EXTS.contains(&e.as_str()) {
                    "image"
                } else {
                    "video"
                }
            } else {
                "media"
            };
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let mut line = format!("  - [{}] {} ({} KB)", kind, rel.display(), size / 1024);
            // Probe only the first few to bound startup cost (design doc §5.7).
            if has_ffprobe && i < 8 {
                if let Some(d) = probe_duration(path) {
                    line.push_str(&format!(" — {:.2}s", d));
                }
            }
            out.push_str(&line);
            out.push('\n');
        }
        Ok(out)
    }

    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::OnceAtStart
    }

    fn priority(&self) -> u32 {
        10
    }

    fn position(&self) -> ContextPosition {
        // Re-scanned and can grow as the agent renders intermediates into the
        // working dir — keep it out of the cached prefix.
        ContextPosition::Tail
    }

    fn is_path_bound(&self) -> bool {
        true
    }

    fn rebind_project_dir(&self, dir: &Path) -> bool {
        *self.project_dir.write().unwrap() = dir.to_path_buf();
        true
    }
}

// ─── Predefined Workflows (Layer 6) ────────────────────────────────────────────

/// Deliver workflow — deterministic post-render DAG: verify → thumbnail.
///
/// Two steps: `verify_output` (QA pass) then `make_thumbnail` (cover). Both are
/// string-interpolated tool steps, so `/wf run video-deliver` is fully
/// deterministic given `{{output}}` / `{{thumbnail_path}}`.
fn video_deliver_workflow() -> WorkflowConfig {
    WorkflowConfig {
        name: "video-deliver".to_string(),
        description: "Deterministic post-render delivery: verify → thumbnail".to_string(),
        version: "1.0".to_string(),
        steps: vec![
            StepConfig {
                id: "verify".to_string(),
                description: "Quality-check the rendered output".to_string(),
                depends_on: vec![],
                tool: Some("verify_output".to_string()),
                tool_args: Some(serde_json::json!({"path": "{{output}}"})),
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(300),
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "thumbnail".to_string(),
                description: "Extract a cover thumbnail".to_string(),
                depends_on: vec!["verify".to_string()],
                tool: Some("make_thumbnail".to_string()),
                tool_args: Some(
                    serde_json::json!({"path": "{{output}}", "output": "{{thumbnail_path}}"}),
                ),
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(120),
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ],
        variables: HashMap::new(),
        timeout_secs: Some(600),
        default_retry_policy: oneai_workflow::RetryPolicy::default(),
        continue_on_failure: false,
    }
}

/// Video edit loop — cyclic think → act → think/end, the video-editing ReAct.
///
/// This is the primary execution graph: the model probes material, designs an
/// EDL, validates it, renders, and verifies — driving each step via tool calls.
/// It mirrors [`crate::coding_pack`]'s `react-loop` but with a video-editing
/// system prompt and the full media tool set.
fn video_edit_loop_graph() -> StateGraph {
    let mut graph = StateGraph::new("video-edit-loop", "think");

    graph.add_node(GraphNode {
        id: "think".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some(
                "You are a video editor. 盘点 the material, then design an edit \
                 timeline (EDL JSON) matching the brief (or the material's inferred \
                 theme). Validate the timeline, render it, and verify the output. \
                 When you have delivered a finished video, give the delivery summary \
                 as your final answer."
                    .to_string(),
            ),
            use_streaming: true,
            include_tool_definitions: true,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    graph.add_node(GraphNode {
        id: "act".to_string(),
        action: NodeAction::ToolCall {
            tool_name: "{{selected_tool}}".to_string(),
            args_template: Some("{{tool_args}}".to_string()),
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    graph.add_node(GraphNode {
        id: "end".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some(
                "Provide the delivery summary: finished-file path, duration, \
                 resolution, shot-structure summary, and the materials not used."
                    .to_string(),
            ),
            use_streaming: true,
            include_tool_definitions: false,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    graph.add_edge(GraphEdge {
        from: "think".to_string(),
        to: "act".to_string(),
        condition: Some(EdgeCondition::HasToolCalls),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "think".to_string(),
        to: "end".to_string(),
        condition: Some(EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "act".to_string(),
        to: "think".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });

    graph.add_terminal("end".to_string());
    graph
}

/// Video QA loop — verify → fix → verify until clean (reflect-style).
///
/// A `verify` LlmInfer node reasons about the last render's defects; if a fix is
/// needed it re-renders/re-runs via `act`, else it concludes. Termination is
/// bounded by `StateGraphExecutor::max_iterations`.
fn video_qa_loop_graph() -> StateGraph {
    let mut graph = StateGraph::new("video-qa-loop", "verify");

    graph.add_node(GraphNode {
        id: "verify".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some(
                "You are a video QA agent. Check the rendered output (duration, \
                 black frames, silence). If a defect can be fixed by adjusting the \
                 timeline or re-rendering, do so; otherwise report pass/fail as \
                 your final answer."
                    .to_string(),
            ),
            use_streaming: true,
            include_tool_definitions: true,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    graph.add_node(GraphNode {
        id: "act".to_string(),
        action: NodeAction::ToolCall {
            tool_name: "{{selected_tool}}".to_string(),
            args_template: Some("{{tool_args}}".to_string()),
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    graph.add_node(GraphNode {
        id: "end".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some("Report the final QA verdict.".to_string()),
            use_streaming: true,
            include_tool_definitions: false,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    graph.add_edge(GraphEdge {
        from: "verify".to_string(),
        to: "act".to_string(),
        condition: Some(EdgeCondition::HasToolCalls),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "verify".to_string(),
        to: "end".to_string(),
        condition: Some(EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "act".to_string(),
        to: "verify".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });

    graph.add_terminal("end".to_string());
    graph
}

// ─── Video Editing Compression Template ────────────────────────────────────────

/// The video-editing compression template.
///
/// Preserves the information most critical to continuing an edit: the brief, the
/// material index, the timeline (EDL), and the QA defects — the latter two being
/// the "single source of truth" and the re-edit targets respectively.
pub const VIDEO_EDITING_COMPRESSION_TEMPLATE: &str = "\
## Video Editing Progress Summary

### Brief (构想)
{{brief}}

### Material Index
{{material_index}}

### Timeline (EDL)
{{timeline}}

### Edit Decisions
{{edit_decisions}}

### Current Segment
{{current_segment}}

### Defects To Fix
{{defects}}

---
Default spec: {{default_spec}}";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_video_editing_pack_creation() {
        let pack = video_editing_pack("/tmp/test_project");

        assert_eq!(pack.name, "video_editing");
        assert_eq!(pack.tools.len(), 7);
        assert_eq!(pack.context_sources.len(), 3);
        assert!(!pack.system_prompt_template.is_empty());
    }

    #[test]
    fn test_system_prompt_uses_alignment_markers() {
        // Same discipline as the coding prompt: tool-specific promises route
        // through the markers so a paradigm/tool filter can't leave stale ones.
        assert!(VIDEO_EDITING_SYSTEM_PROMPT.contains("{{TOOL_PREFERENCE_RULES}}"));
        assert!(VIDEO_EDITING_SYSTEM_PROMPT.contains("{{MODEL_DRIVEN_CONTROL_TOOLS}}"));
    }

    #[test]
    fn test_video_editing_pack_permission_profile() {
        let pack = video_editing_pack("/tmp/test");

        // Read-only probes/validators auto-approved.
        assert!(pack.permission_profile.auto_approve.contains("probe_media"));
        assert!(pack
            .permission_profile
            .auto_approve
            .contains("validate_timeline"));
        assert!(pack
            .permission_profile
            .auto_approve
            .contains("verify_output"));

        // Expensive, resource-heavy render confirmed.
        assert!(pack
            .permission_profile
            .require_confirmation
            .contains("render"));
        assert_eq!(
            pack.permission_profile.permission_overrides.get("render"),
            Some(&PermissionLevel::Full)
        );
    }

    #[test]
    fn test_video_editing_pack_paradigm_strategies() {
        let pack = video_editing_pack("/tmp/test");
        assert_eq!(pack.paradigm_strategies.len(), 2);

        let edit = pack
            .paradigm_strategies
            .iter()
            .find(|s| s.matches("帮我剪辑一个旅行 vlog"))
            .unwrap();
        assert_eq!(edit.paradigm_sequence.len(), 3);

        let analyze = pack
            .paradigm_strategies
            .iter()
            .find(|s| s.matches("分析这些素材讲了什么"))
            .unwrap();
        assert_eq!(analyze.paradigm_sequence.len(), 1);
    }

    #[test]
    fn test_video_editing_pack_compression_template() {
        let pack = video_editing_pack("/tmp/test");

        assert_eq!(pack.compression_template.name, "video_editing");
        for field in ["brief", "material_index", "timeline", "defects"] {
            assert!(
                pack.compression_template
                    .preserve_fields
                    .contains(&field.to_string()),
                "missing preserve field: {field}"
            );
        }
        assert!(pack
            .compression_template
            .truncate_rules
            .contains_key("timeline"));
    }

    #[test]
    fn test_video_editing_pack_memory_profile() {
        let pack = video_editing_pack("/tmp/test");
        assert_eq!(pack.memory_profile.name, "video_editing");
        assert!(pack.memory_profile.enable_memory_tools);
    }

    #[test]
    fn test_video_editing_pack_state_graphs() {
        let pack = video_editing_pack("/tmp/test");
        assert_eq!(pack.state_graphs.len(), 2);
        let names: Vec<&str> = pack.state_graphs.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"video-edit-loop"));
        assert!(names.contains(&"video-qa-loop"));

        for g in &pack.state_graphs {
            assert!(!g.entry_point.is_empty(), "graph '{}' no entry", g.name);
            assert!(
                !g.terminal_nodes.is_empty(),
                "graph '{}' no terminal",
                g.name
            );
        }
    }

    #[test]
    fn test_video_editing_pack_workflows() {
        let pack = video_editing_pack("/tmp/test");
        assert_eq!(pack.workflows.len(), 1);
        assert_eq!(pack.workflows[0].name, "video-deliver");
    }

    #[test]
    fn test_sub_agent_definitions() {
        let pack = video_editing_pack("/tmp/test");
        let editor = pack.get_sub_agent_definition("editor").unwrap();
        assert!(editor.modifies_files);
        assert!(editor.available_tools.contains(&"render".to_string()));
        let reviewer = pack.get_sub_agent_definition("reviewer").unwrap();
        assert!(!reviewer.modifies_files);
    }

    #[test]
    fn test_collect_media_files_ignores_hidden_and_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.mp4"), b"x").unwrap();
        std::fs::write(tmp.path().join("b.PNG"), b"x").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), b"x").unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git").join("c.mp4"), b"x").unwrap();

        let found = collect_media_files(tmp.path(), 2, 60);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"a.mp4".to_string()));
        assert!(names.contains(&"b.PNG".to_string()));
        assert!(!names.iter().any(|n| n == "notes.txt"));
        assert!(!names.iter().any(|n| n == "c.mp4"));
    }

    #[tokio::test]
    async fn test_media_asset_inventory_source() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("clip.mp4"), b"x").unwrap();
        let source = MediaAssetInventorySource::new(tmp.path().to_str().unwrap());
        assert_eq!(source.key(), "media_asset_inventory");
        assert_eq!(source.position(), ContextPosition::Tail);
        assert!(source.is_path_bound());

        let content = source.load().await.unwrap();
        assert!(content.contains("Material Inventory"));
        assert!(content.contains("clip.mp4"));
        assert!(content.contains("video"));
    }
}
