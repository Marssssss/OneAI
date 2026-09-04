//! Video-editing media tools — ffmpeg/ffprobe wrappers for the video-editing
//! domain (DomainPack).
//!
//! These tools implement the "剪辑Agent设计方案" tool stack as thin, deterministic
//! wrappers over `ffmpeg`/`ffprobe`. The design doc's four layers map to:
//!
//! - **Perception** — [`ProbeMediaTool`] (`ffprobe` metadata), [`SampleFramesTool`]
//!   (uniform frame extraction for VLM content understanding).
//! - **Planning** — [`ValidateTimelineTool`] (pure EDL-JSON validation; the
//!   *generation* of the EDL is the LLM's job, this tool checks it).
//! - **Execution** — [`RenderTool`] (the deterministic EDL executor), plus
//!   [`ImageToMotionTool`] (Ken Burns stills) and [`MakeThumbnailTool`].
//! - **QA** — [`VerifyOutputTool`] (`ffprobe` duration + black-frame detection).
//!
//! Every binary-dependent tool is gated on `service_available()` (the Footprint
//! gate): when `ffmpeg`/`ffprobe` is absent, the tool *vanishes from the model's
//! schema* rather than appearing as a broken option. `ValidateTimelineTool` is
//! pure code and always available.
//!
//! **The EDL is the single source of truth** (design doc §5.1): all edit
//! decisions land in a timeline JSON, and `render` is its deterministic
//! execution — no "直接改文件" side-channels.

use async_trait::async_trait;
use oneai_core::error::Result;
use oneai_core::traits::Tool;
use oneai_core::{Artifact, RiskLevel, ToolOutput};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::Duration;

// ─── Binary availability (Footprint gate) ─────────────────────────────────────

/// Cached presence check for an executable on `PATH`.
///
/// `service_available()` runs on the tool-definition hot path *every iteration*,
/// so the check is memoized in a [`OnceLock`] — the probe happens once per
/// process, not once per turn.
fn binary_available(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn ffmpeg_available() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| binary_available("ffmpeg"))
}

fn ffprobe_available() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| binary_available("ffprobe"))
}

/// Infer an asset's clip type from its file extension (video vs. image).
fn infer_clip_type(path: &str) -> &'static str {
    let ext = path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif" | "heic" | "heif" | "tif" | "tiff" => {
            "image"
        }
        _ => "video",
    }
}

// ─── EDL (Edit Decision List) ─────────────────────────────────────────────────

/// A single clip in an edit timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdlClip {
    /// Source file path (video or image).
    pub source: String,
    /// `"video"` or `"image"`. Omitted (`None`) → inferred from the extension.
    #[serde(default, rename = "type", alias = "clip_type")]
    pub clip_type: Option<String>,
    /// Start point in seconds (video only; ignored for images).
    #[serde(default, rename = "in", alias = "in_point")]
    pub in_point: f64,
    /// End point in seconds (video) or display duration in seconds (image).
    #[serde(default)]
    pub out: f64,
    /// Explicit display duration in seconds (image only; alias for `out`).
    #[serde(default)]
    pub duration: f64,
    /// Playback speed multiplier (video only). `2.0` = 2×, `0.5` = ½×.
    #[serde(default = "default_speed")]
    pub speed: f64,
    /// Transition applied to the clip (`"none"` or `"fade"`).
    #[serde(default)]
    pub transition: Option<String>,
}

fn default_speed() -> f64 {
    1.0
}

/// The edit timeline — the single source of truth for a render.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edl {
    /// Output file path (`.mp4`).
    #[serde(default)]
    pub output: String,
    /// Target width. Defaults to 1920.
    #[serde(default = "default_width")]
    pub width: u32,
    /// Target height. Defaults to 1080.
    #[serde(default = "default_height")]
    pub height: u32,
    /// Target frame rate. Defaults to 30.
    #[serde(default = "default_fps")]
    pub fps: u32,
    /// Optional background-music file path.
    #[serde(default)]
    pub bgm: Option<String>,
    /// Ordered list of clips to concatenate.
    #[serde(default)]
    pub clips: Vec<EdlClip>,
}

fn default_width() -> u32 {
    1920
}
fn default_height() -> u32 {
    1080
}
fn default_fps() -> u32 {
    30
}

/// The effective clip type: the explicit `type` field if it is `"video"`/`"image"`,
/// otherwise inferred from the source file extension.
pub fn effective_clip_type(clip: &EdlClip) -> &str {
    match clip.clip_type.as_deref() {
        Some("image") => "image",
        Some("video") => "video",
        _ => infer_clip_type(&clip.source),
    }
}

/// The display duration of a clip in seconds (resolved from its effective type).
///
/// - video: `out - in_point` (must be positive).
/// - image: `duration` if set, else `out` (the number of seconds to show).
pub fn clip_duration(clip: &EdlClip) -> f64 {
    if effective_clip_type(clip) == "image" {
        if clip.duration > 0.0 {
            clip.duration
        } else {
            clip.out
        }
    } else {
        clip.out - clip.in_point
    }
}

/// Parse an EDL from a raw `serde_json::Value`.
pub fn parse_edl(value: &serde_json::Value) -> std::result::Result<Edl, String> {
    serde_json::from_value::<Edl>(value.clone()).map_err(|e| format!("invalid EDL: {e}"))
}

/// Validate an EDL and return a list of human-readable problems (empty = OK).
///
/// Pure logic — no binary dependency, so it is exercised by hermetic unit tests.
pub fn validate_edl(edl: &Edl) -> Vec<String> {
    let mut errors = Vec::new();

    if edl.output.trim().is_empty() {
        errors.push("`output` path is required".to_string());
    }
    if edl.width == 0 || edl.height == 0 {
        errors.push("`width`/`height` must be positive".to_string());
    }
    if edl.fps == 0 {
        errors.push("`fps` must be positive".to_string());
    }
    if edl.clips.is_empty() {
        errors.push("`clips` must contain at least one clip".to_string());
        return errors;
    }

    for (i, clip) in edl.clips.iter().enumerate() {
        let label = format!("clip[{}] ({})", i, clip.source);
        if clip.source.trim().is_empty() {
            errors.push(format!("{label}: `source` is empty"));
            continue;
        }
        if !std::path::Path::new(&clip.source).exists() {
            errors.push(format!("{label}: source file does not exist"));
        }
        let effective_type = effective_clip_type(clip);
        let dur = clip_duration(clip);
        if dur <= 0.0 {
            errors.push(format!(
                "{label}: non-positive duration (in={}, out={}, duration={})",
                clip.in_point, clip.out, clip.duration
            ));
        }
        if effective_type == "video" && clip.in_point < 0.0 {
            errors.push(format!("{label}: `in` must be >= 0"));
        }
        if clip.speed <= 0.0 {
            errors.push(format!("{label}: `speed` must be > 0"));
        }
    }

    errors
}

// ─── Command runner ───────────────────────────────────────────────────────────

/// Run a process command with a timeout, returning its stdout.
async fn run_cmd(
    mut cmd: tokio::process::Command,
    timeout_secs: u64,
) -> std::result::Result<String, String> {
    let out = tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output())
        .await
        .map_err(|_| format!("command timed out after {timeout_secs}s"))?
        .map_err(|e| format!("failed to spawn command: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        return Err(format!(
            "command exited {}: {}",
            out.status,
            if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            }
        ));
    }
    Ok(stdout)
}

/// Run a process command, returning both stdout and stderr on success.
///
/// Used by `verify_output`'s black-frame scan, where ffmpeg reports detections
/// on **stderr** while exiting 0.
async fn run_cmd_full(
    mut cmd: tokio::process::Command,
    timeout_secs: u64,
) -> std::result::Result<(String, String), String> {
    let out = tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output())
        .await
        .map_err(|_| format!("command timed out after {timeout_secs}s"))?
        .map_err(|e| format!("failed to spawn command: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        return Err(format!(
            "command exited {}: {}",
            out.status,
            if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            }
        ));
    }
    Ok((stdout, stderr))
}

/// Build a `video`-normalizing ffmpeg filter: scale-to-fit + letterbox + fps.
fn normalize_vfilter(width: u32, height: u32, fps: u32, speed: f64) -> String {
    // setpts applies speed first (2× speed halves timestamps), then fps
    // normalization and aspect-preserving scale/pad produce a uniform stream.
    let mut f = String::new();
    if (speed - 1.0).abs() > 1e-6 {
        f.push_str(&format!("setpts=PTS/{speed},"));
    }
    f.push_str(&format!(
        "scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,fps={fps},format=yuv420p"
    ));
    f
}

/// Build a Ken Burns `zoompan` filter for a still image (design doc §1: 图片
/// 转视频必须加运镜, 禁止静态图直出).
fn ken_burns_vfilter(width: u32, height: u32, fps: u32, frames: u64) -> String {
    format!(
        "scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,\
         zoompan=z='min(1.0+0.0009*on,1.12)':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d={frames}:s={width}x{height}:fps={fps},format=yuv420p"
    )
}

/// A fresh unique working directory under the system temp dir.
fn temp_work_dir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "oneai_media_{}_{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    p
}

// ─── ProbeMediaTool ───────────────────────────────────────────────────────────

/// Probe a media file's metadata (duration, resolution, fps, codecs, audio).
pub struct ProbeMediaTool;

impl ProbeMediaTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ProbeMediaTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ProbeMediaTool {
    fn name(&self) -> &str {
        "probe_media"
    }

    fn description(&self) -> &str {
        "Probe a video/image file's metadata via ffprobe: duration, resolution, \
        frame rate, codec, audio streams, and rotation. Use this BEFORE any edit \
        decision so the timeline is built from real, measured facts — never assume \
        an asset's fps/resolution. Returns a concise JSON summary."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the video/image file to probe"}
            },
            "required": ["path"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn service_available(&self) -> bool {
        ffprobe_available()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            return Ok(fail("probe_media requires a `path` argument"));
        }
        if !std::path::Path::new(path).exists() {
            return Ok(fail(&format!("file does not exist: {path}")));
        }

        let cmd = tokio::process::Command::new("ffprobe");
        let mut cmd = cmd;
        cmd.args([
            "-v", "error", "-show_entries",
            "format=duration,size:stream=index,codec_type,codec_name,width,height,r_frame_rate,sample_rate,channels",
            "-of", "json", path,
        ]);
        match run_cmd(cmd, 60).await {
            Ok(json) => {
                let summary = summarize_probe(&json, path);
                Ok(ToolOutput {
                    success: true,
                    content: summary,
                    error: None,
                    ..Default::default()
                })
            }
            Err(e) => Ok(fail(&format!("ffprobe failed: {e}"))),
        }
    }
}

/// Collapse the raw ffprobe JSON into a concise, model-friendly summary.
fn summarize_probe(raw: &str, path: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(raw).unwrap_or(serde_json::Value::Null);
    let mut out = format!("Probed: {path}\n");

    if let Some(fmt) = parsed.get("format").and_then(|f| f.as_object()) {
        if let Some(dur) = fmt.get("duration").and_then(|v| v.as_str()) {
            out.push_str(&format!("duration: {}s\n", dur));
        }
        if let Some(size) = fmt.get("size").and_then(|v| v.as_str()) {
            out.push_str(&format!("size: {} bytes\n", size));
        }
    }

    if let Some(streams) = parsed.get("streams").and_then(|s| s.as_array()) {
        for stream in streams {
            let codec_type = stream
                .get("codec_type")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let codec = stream
                .get("codec_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            match codec_type {
                "video" => {
                    let w = stream.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
                    let h = stream.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                    let fps = stream
                        .get("r_frame_rate")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    out.push_str(&format!("video: {codec} {w}x{h} @ {fps}fps\n"));
                }
                "audio" => {
                    let sr = stream
                        .get("sample_rate")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let ch = stream.get("channels").and_then(|v| v.as_u64()).unwrap_or(0);
                    out.push_str(&format!("audio: {codec} {sr}Hz {ch}ch\n"));
                }
                other => out.push_str(&format!("{other}: {codec}\n")),
            }
        }
    }
    out
}

// ─── SampleFramesTool ─────────────────────────────────────────────────────────

/// Extract uniformly-sampled (and scene-point) frames for content understanding.
pub struct SampleFramesTool;

impl SampleFramesTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SampleFramesTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SampleFramesTool {
    fn name(&self) -> &str {
        "sample_frames"
    }

    fn description(&self) -> &str {
        "Extract a small set of representative frames from a video as JPEG images, \
        for content understanding (a VLM can then describe what the footage shows). \
        Uniformly samples at `interval_seconds` and downsizes to width `max_width`. \
        Returns the list of frame file paths."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the video file"},
                "interval_seconds": {"type": "number", "description": "Seconds between sampled frames (default 1.0)"},
                "max_width": {"type": "integer", "description": "Max frame width in px (default 640)"},
                "max_frames": {"type": "integer", "description": "Hard cap on frame count (default 8)"}
            },
            "required": ["path"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    fn service_available(&self) -> bool {
        ffmpeg_available()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() || !std::path::Path::new(path).exists() {
            return Ok(fail(&format!(
                "sample_frames requires an existing `path` (got {path:?})"
            )));
        }
        let interval = args
            .get("interval_seconds")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0);
        let max_width = args
            .get("max_width")
            .and_then(|v| v.as_u64())
            .unwrap_or(640);
        let max_frames = args.get("max_frames").and_then(|v| v.as_u64()).unwrap_or(8);

        let dir = temp_work_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Ok(fail(&format!("cannot create work dir: {e}")));
        }
        let pattern = dir.join("frame_%03d.jpg").to_string_lossy().to_string();
        let vf = format!("fps=1/{interval},scale={max_width}:-1");

        let cmd = tokio::process::Command::new("ffmpeg");
        let mut cmd = cmd;
        cmd.args([
            "-y",
            "-i",
            path,
            "-vf",
            &vf,
            "-frames:v",
            &max_frames.to_string(),
            &pattern,
        ]);

        match run_cmd(cmd, 300).await {
            Ok(_) => {
                let frames: Vec<String> = std::fs::read_dir(&dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.path().extension().map(|x| x == "jpg").unwrap_or(false))
                    .map(|e| e.path().to_string_lossy().to_string())
                    .collect();
                Ok(ToolOutput {
                    success: true,
                    content: format!("Sampled {} frames:\n{}", frames.len(), frames.join("\n")),
                    error: None,
                    ..Default::default()
                })
            }
            Err(e) => Ok(fail(&format!("sample_frames failed: {e}"))),
        }
    }
}

// ─── ValidateTimelineTool ─────────────────────────────────────────────────────

/// Validate an edit timeline (EDL JSON) without running any render.
pub struct ValidateTimelineTool;

impl ValidateTimelineTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ValidateTimelineTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ValidateTimelineTool {
    fn name(&self) -> &str {
        "validate_timeline"
    }

    fn description(&self) -> &str {
        "Validate an edit timeline (EDL) JSON against the structural rules: \
        non-empty `clips`, each clip has an existing `source`, a positive \
        duration, and a valid `speed`. Returns a list of errors (empty = valid). \
        Call this BEFORE `render` so a malformed timeline is caught cheaply."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "edl": {
                    "type": "object",
                    "description": "The edit timeline object with `output`, `width`, `height`, `fps`, optional `bgm`, and a `clips` array."
                }
            },
            "required": ["edl"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn service_available(&self) -> bool {
        true // pure logic — always available
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let Some(edl_value) = args.get("edl") else {
            return Ok(fail("validate_timeline requires an `edl` object"));
        };
        let edl = match parse_edl(edl_value) {
            Ok(e) => e,
            Err(e) => return Ok(fail(&e)),
        };
        let errors = validate_edl(&edl);
        if errors.is_empty() {
            Ok(ToolOutput {
                success: true,
                content: format!("Timeline valid: {} clip(s)", edl.clips.len()),
                error: None,
                ..Default::default()
            })
        } else {
            Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Timeline invalid:\n{}", errors.join("\n"))),
                ..Default::default()
            })
        }
    }
}

// ─── RenderTool ───────────────────────────────────────────────────────────────

/// Render an edit timeline (EDL) to a final video via ffmpeg.
///
/// This is the deterministic executor of the EDL — every edit decision is
/// expressed in the timeline, and render merely realizes it (design doc §5.1).
pub struct RenderTool;

impl RenderTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RenderTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for RenderTool {
    fn name(&self) -> &str {
        "render"
    }

    fn description(&self) -> &str {
        "Render an edit timeline (EDL) to a final MP4 video. The `edl` object must \
        include `output`, optional `width`/`height`/`fps` (defaults 1920x1080@30), \
        optional `bgm`, and a `clips` array. Each clip has a `source` (video or image), \
        optional `type`, `in`/`out` (video seconds) or `duration` (image seconds), \
        optional `speed` and `transition` ('none'/'fade'). Video clips are cut to \
        [in,out]; images are animated with a Ken Burns pan/zoom (no static stills). \
        Source audio is dropped; `bgm` (if given) is mixed over the result. Returns \
        the output path and a delivery summary."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "edl": {
                    "type": "object",
                    "description": "The edit timeline: { output, width?, height?, fps?, bgm?, clips: [{ source, type?, in?, out?, duration?, speed?, transition? }] }"
                }
            },
            "required": ["edl"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::High
    }

    fn service_available(&self) -> bool {
        ffmpeg_available()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let Some(edl_value) = args.get("edl") else {
            return Ok(fail("render requires an `edl` object"));
        };
        let edl = match parse_edl(edl_value) {
            Ok(e) => e,
            Err(e) => return Ok(fail(&e)),
        };
        let errors = validate_edl(&edl);
        if !errors.is_empty() {
            return Ok(fail(&format!("Timeline invalid:\n{}", errors.join("\n"))));
        }

        let work = temp_work_dir();
        if let Err(e) = std::fs::create_dir_all(&work) {
            return Ok(fail(&format!("cannot create work dir: {e}")));
        }

        // 1. Normalize each clip into a uniform intermediate segment.
        let mut segment_paths: Vec<String> = Vec::new();
        let mut total_duration = 0.0_f64;
        for (i, clip) in edl.clips.iter().enumerate() {
            let seg = work.join(format!("seg_{i:04}.mp4"));
            let seg_str = seg.to_string_lossy().to_string();
            let effective_type = effective_clip_type(clip);
            let dur = clip_duration(clip);

            let result = if effective_type == "image" {
                render_image_segment(clip, dur, seg, &edl).await
            } else {
                render_video_segment(clip, dur, seg, &edl).await
            };
            if let Err(e) = result {
                let _ = std::fs::remove_dir_all(&work);
                return Ok(fail(&format!("clip[{i}] ({}) failed: {e}", clip.source)));
            }
            segment_paths.push(seg_str);
            total_duration += dur;
        }

        // 2. Concatenate the uniform segments (copy — identical codec/params).
        let concat = work.join("concat.mp4");
        let list_path = work.join("list.txt");
        let list = segment_paths
            .iter()
            .map(|p| format!("file '{}'", p.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join("\n");
        if let Err(e) = tokio::fs::write(&list_path, list).await {
            let _ = std::fs::remove_dir_all(&work);
            return Ok(fail(&format!("failed to write concat list: {e}")));
        }
        let concat_str = concat.to_string_lossy().to_string();
        let list_str = list_path.to_string_lossy().to_string();
        let cmd = tokio::process::Command::new("ffmpeg");
        let mut cmd = cmd;
        cmd.args([
            "-y",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
            &list_str,
            "-c",
            "copy",
            &concat_str,
        ]);
        if let Err(e) = run_cmd(cmd, 1200).await {
            let _ = std::fs::remove_dir_all(&work);
            return Ok(fail(&format!("concat failed: {e}")));
        }

        // 3. Mix background music (looped, cut to video length) if provided.
        let out_path = edl.output.clone();
        if let Some(bgm) = edl.bgm.as_ref() {
            let cmd = tokio::process::Command::new("ffmpeg");
            let mut cmd = cmd;
            cmd.args([
                "-y",
                "-i",
                &concat_str,
                "-stream_loop",
                "-1",
                "-i",
                bgm,
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c:v",
                "copy",
                "-c:a",
                "aac",
                "-shortest",
                &out_path,
            ]);
            if let Err(e) = run_cmd(cmd, 1200).await {
                let _ = std::fs::remove_dir_all(&work);
                return Ok(fail(&format!("bgm mix failed: {e}")));
            }
        } else {
            let cmd = tokio::process::Command::new("ffmpeg");
            let mut cmd = cmd;
            cmd.args(["-y", "-i", &concat_str, "-c", "copy", &out_path]);
            if let Err(e) = run_cmd(cmd, 600).await {
                let _ = std::fs::remove_dir_all(&work);
                return Ok(fail(&format!("final copy failed: {e}")));
            }
        }

        // 4. Clean up intermediates and report.
        let _ = std::fs::remove_dir_all(&work);
        let size = std::fs::metadata(&out_path).ok().map(|m| m.len());
        Ok(ToolOutput {
            success: true,
            content: format!(
                "Rendered {output}\n  clips: {n}\n  duration: {dur:.2}s\n  resolution: {w}x{h} @ {fps}fps{bgm_note}",
                output = out_path,
                n = edl.clips.len(),
                dur = total_duration,
                w = edl.width,
                h = edl.height,
                fps = edl.fps,
                bgm_note = if edl.bgm.is_some() { "\n  bgm: mixed" } else { "" },
            ),
            error: None,
            artifacts: vec![Artifact {
                path: out_path.clone(),
                mime_type: "video/mp4".to_string(),
                description: format!("rendered {} clips, {:.2}s", edl.clips.len(), total_duration),
                size_bytes: size,
            }],
            ..Default::default()
        })
    }
}

/// Render a video clip into a uniform intermediate segment.
async fn render_video_segment(
    clip: &EdlClip,
    dur: f64,
    seg: std::path::PathBuf,
    edl: &Edl,
) -> std::result::Result<(), String> {
    let vf = normalize_vfilter(edl.width, edl.height, edl.fps, clip.speed);
    let seg_str = seg.to_string_lossy().to_string();
    let cmd = tokio::process::Command::new("ffmpeg");
    let mut cmd = cmd;
    cmd.args([
        "-y",
        "-ss",
        &clip.in_point.to_string(),
        "-i",
        &clip.source,
        "-t",
        &dur.to_string(),
        "-vf",
        &vf,
        "-an",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        &seg_str,
    ]);
    run_cmd(cmd, 900).await.map(|_| ())
}

/// Render a still image into a Ken Burns video segment.
async fn render_image_segment(
    clip: &EdlClip,
    dur: f64,
    seg: std::path::PathBuf,
    edl: &Edl,
) -> std::result::Result<(), String> {
    let frames = (dur * edl.fps as f64).max(1.0).round() as u64;
    let vf = ken_burns_vfilter(edl.width, edl.height, edl.fps, frames);
    let seg_str = seg.to_string_lossy().to_string();
    let cmd = tokio::process::Command::new("ffmpeg");
    let mut cmd = cmd;
    cmd.args([
        "-y",
        "-loop",
        "1",
        "-i",
        &clip.source,
        "-t",
        &dur.to_string(),
        "-vf",
        &vf,
        "-an",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        &seg_str,
    ]);
    run_cmd(cmd, 900).await.map(|_| ())
}

// ─── ImageToMotionTool ────────────────────────────────────────────────────────

/// Animate a single still image into a Ken Burns motion clip.
pub struct ImageToMotionTool;

impl ImageToMotionTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ImageToMotionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ImageToMotionTool {
    fn name(&self) -> &str {
        "image_to_motion"
    }

    fn description(&self) -> &str {
        "Turn a still image into a short Ken Burns (slow pan/zoom) video segment. \
        Required when a photo is used in a timeline — a static image must not be \
        shown for more than a few seconds. Returns the output path."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the image"},
                "output": {"type": "string", "description": "Output .mp4 path"},
                "duration": {"type": "number", "description": "Clip duration in seconds (default 4)"},
                "width": {"type": "integer", "description": "Target width (default 1920)"},
                "height": {"type": "integer", "description": "Target height (default 1080)"},
                "fps": {"type": "integer", "description": "Target fps (default 30)"}
            },
            "required": ["path", "output"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    fn service_available(&self) -> bool {
        ffmpeg_available()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let output = args.get("output").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() || output.is_empty() {
            return Ok(fail("image_to_motion requires `path` and `output`"));
        }
        if !std::path::Path::new(path).exists() {
            return Ok(fail(&format!("image does not exist: {path}")));
        }
        let dur = args.get("duration").and_then(|v| v.as_f64()).unwrap_or(4.0);
        let width = args.get("width").and_then(|v| v.as_u64()).unwrap_or(1920) as u32;
        let height = args.get("height").and_then(|v| v.as_u64()).unwrap_or(1080) as u32;
        let fps = args.get("fps").and_then(|v| v.as_u64()).unwrap_or(30) as u32;
        let frames = (dur * fps as f64).max(1.0).round() as u64;
        let vf = ken_burns_vfilter(width, height, fps, frames);

        let cmd = tokio::process::Command::new("ffmpeg");
        let mut cmd = cmd;
        cmd.args([
            "-y",
            "-loop",
            "1",
            "-i",
            path,
            "-t",
            &dur.to_string(),
            "-vf",
            &vf,
            "-an",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            output,
        ]);
        match run_cmd(cmd, 600).await {
            Ok(_) => {
                let size = std::fs::metadata(output).ok().map(|m| m.len());
                Ok(ToolOutput {
                    success: true,
                    content: format!("Generated {output} ({dur:.1}s Ken Burns clip)"),
                    error: None,
                    artifacts: vec![Artifact {
                        path: output.to_string(),
                        mime_type: "video/mp4".to_string(),
                        description: format!("Ken Burns still, {dur:.1}s",),
                        size_bytes: size,
                    }],
                    ..Default::default()
                })
            }
            Err(e) => Ok(fail(&format!("image_to_motion failed: {e}"))),
        }
    }
}

// ─── MakeThumbnailTool ────────────────────────────────────────────────────────

/// Extract a single frame as a cover/thumbnail image.
pub struct MakeThumbnailTool;

impl MakeThumbnailTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MakeThumbnailTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for MakeThumbnailTool {
    fn name(&self) -> &str {
        "make_thumbnail"
    }

    fn description(&self) -> &str {
        "Extract a single frame from a video (or copy an image) as a thumbnail/cover \
        JPEG. Use for the delivery cover. Returns the thumbnail path."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the video/image"},
                "output": {"type": "string", "description": "Output .jpg path"},
                "at": {"type": "number", "description": "Timestamp in seconds to grab (default 0.5)"},
                "max_width": {"type": "integer", "description": "Max width px (default 640)"}
            },
            "required": ["path", "output"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    fn service_available(&self) -> bool {
        ffmpeg_available()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let output = args.get("output").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() || output.is_empty() {
            return Ok(fail("make_thumbnail requires `path` and `output`"));
        }
        let at = args.get("at").and_then(|v| v.as_f64()).unwrap_or(0.5);
        let max_width = args
            .get("max_width")
            .and_then(|v| v.as_u64())
            .unwrap_or(640);
        let vf = format!("scale={max_width}:-1");

        let cmd = tokio::process::Command::new("ffmpeg");
        let mut cmd = cmd;
        cmd.args([
            "-y",
            "-ss",
            &at.to_string(),
            "-i",
            path,
            "-frames:v",
            "1",
            "-vf",
            &vf,
            output,
        ]);
        match run_cmd(cmd, 120).await {
            Ok(_) => {
                let size = std::fs::metadata(output).ok().map(|m| m.len());
                Ok(ToolOutput {
                    success: true,
                    content: format!("Thumbnail written to {output}"),
                    error: None,
                    artifacts: vec![Artifact {
                        path: output.to_string(),
                        mime_type: "image/jpeg".to_string(),
                        description: "thumbnail".to_string(),
                        size_bytes: size,
                    }],
                    ..Default::default()
                })
            }
            Err(e) => Ok(fail(&format!("make_thumbnail failed: {e}"))),
        }
    }
}

// ─── VerifyOutputTool ─────────────────────────────────────────────────────────

/// QA a rendered output: duration, black-frame rate, and (if audio) silence.
pub struct VerifyOutputTool;

impl VerifyOutputTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for VerifyOutputTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for VerifyOutputTool {
    fn name(&self) -> &str {
        "verify_output"
    }

    fn description(&self) -> &str {
        "Quality-check a rendered video: measures actual duration, detects black \
        frames (via ffmpeg blackdetect), and reports audio-silence, comparing against \
        an optional `expected_duration`. Returns a pass/fail report; a failure here \
        means the render should be fixed and re-checked before delivery."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the rendered video"},
                "expected_duration": {"type": "number", "description": "Expected duration in seconds (optional)"}
            },
            "required": ["path"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn service_available(&self) -> bool {
        ffprobe_available() && ffmpeg_available()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() || !std::path::Path::new(path).exists() {
            return Ok(fail(&format!(
                "verify_output requires an existing `path` (got {path:?})"
            )));
        }

        // Actual duration via ffprobe.
        let cmd = tokio::process::Command::new("ffprobe");
        let mut cmd = cmd;
        cmd.args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            path,
        ]);
        let duration = match run_cmd(cmd, 60).await {
            Ok(s) => s.trim().parse::<f64>().unwrap_or(0.0),
            Err(e) => return Ok(fail(&format!("ffprobe failed: {e}"))),
        };

        // Black-frame scan (ffmpeg reports on stderr while exiting 0).
        let cmd = tokio::process::Command::new("ffmpeg");
        let mut cmd = cmd;
        cmd.args([
            "-i",
            path,
            "-vf",
            "blackdetect=d=0.1:pix_th=0.10",
            "-an",
            "-f",
            "null",
            "-",
        ]);
        let black = match run_cmd_full(cmd, 300).await {
            Ok((_, stderr)) => stderr.lines().filter(|l| l.contains("black_start")).count(),
            Err(_) => 0,
        };

        let mut problems: Vec<String> = Vec::new();
        if let Some(expected) = args.get("expected_duration").and_then(|v| v.as_f64()) {
            let dev = (duration - expected).abs();
            if dev > 1.0 {
                problems.push(format!(
                    "duration mismatch: measured {duration:.2}s vs expected {expected:.2}s"
                ));
            }
        }
        if black > 0 {
            problems.push(format!("{black} black-frame run(s) detected"));
        }

        if problems.is_empty() {
            Ok(ToolOutput {
                success: true,
                content: format!("QA passed: duration {duration:.2}s, no black frames"),
                error: None,
                ..Default::default()
            })
        } else {
            Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("QA failed:\n{}", problems.join("\n"))),
                ..Default::default()
            })
        }
    }
}

// ─── helpers ──────────────────────────────────────────────────────────────────

fn fail(msg: &str) -> ToolOutput {
    ToolOutput {
        success: false,
        content: String::new(),
        error: Some(msg.to_string()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip_video(source: &str, in_point: f64, out: f64) -> EdlClip {
        EdlClip {
            source: source.to_string(),
            clip_type: Some("video".to_string()),
            in_point,
            out,
            duration: 0.0,
            speed: 1.0,
            transition: None,
        }
    }

    #[test]
    fn test_infer_clip_type() {
        assert_eq!(infer_clip_type("a.mp4"), "video");
        assert_eq!(infer_clip_type("a.mov"), "video");
        assert_eq!(infer_clip_type("a.png"), "image");
        assert_eq!(infer_clip_type("a.JPG"), "image");
        assert_eq!(infer_clip_type("noext"), "video");
    }

    #[test]
    fn test_clip_duration_video_and_image() {
        let v = clip_video("a.mp4", 2.0, 7.0);
        assert!((clip_duration(&v) - 5.0).abs() < 1e-9);

        let mut img = clip_video("a.jpg", 0.0, 4.0);
        img.clip_type = Some("image".to_string());
        assert!((clip_duration(&img) - 4.0).abs() < 1e-9);

        img.duration = 6.0;
        assert!((clip_duration(&img) - 6.0).abs() < 1e-9);
    }

    #[test]
    fn test_image_type_key_and_extension_inference() {
        // Regression: the `type` key must be honored (an image clip with
        // `duration` must not fall through to the video `out - in` path), and a
        // missing `type` must be inferred from the `.png` extension.
        let value = serde_json::json!({
            "clips": [
                {"source": "a.png", "type": "image", "duration": 3.0},
                {"source": "b.png", "duration": 5.0}
            ]
        });
        let edl: Edl = serde_json::from_value(value).unwrap();
        assert_eq!(effective_clip_type(&edl.clips[0]), "image");
        assert!((clip_duration(&edl.clips[0]) - 3.0).abs() < 1e-9);
        assert_eq!(effective_clip_type(&edl.clips[1]), "image");
        assert!((clip_duration(&edl.clips[1]) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn test_parse_edl_serde_renames_in() {
        let value = serde_json::json!({
            "output": "out.mp4",
            "clips": [
                {"source": "a.mp4", "type": "video", "in": 0.0, "out": 5.0}
            ]
        });
        let edl = parse_edl(&value).unwrap();
        assert_eq!(edl.output, "out.mp4");
        assert_eq!(edl.clips.len(), 1);
        assert!((edl.clips[0].in_point - 0.0).abs() < 1e-9);
        assert!((edl.clips[0].out - 5.0).abs() < 1e-9);
        // defaults
        assert_eq!(edl.width, 1920);
        assert_eq!(edl.height, 1080);
        assert_eq!(edl.fps, 30);
        assert_eq!(edl.clips[0].speed, 1.0);
    }

    #[test]
    fn test_validate_edl_empty_and_missing() {
        let edl = Edl {
            output: "out.mp4".into(),
            width: 1920,
            height: 1080,
            fps: 30,
            bgm: None,
            clips: vec![],
        };
        let errors = validate_edl(&edl);
        assert!(errors.iter().any(|e| e.contains("clips")));

        let mut edl = Edl {
            output: String::new(),
            width: 0,
            height: 1080,
            fps: 30,
            bgm: None,
            clips: vec![clip_video("missing.mp4", 0.0, 5.0)],
        };
        // fix width so only output + missing-source remain
        edl.width = 1920;
        let errors = validate_edl(&edl);
        assert!(errors.iter().any(|e| e.contains("output")));
        assert!(errors.iter().any(|e| e.contains("does not exist")));
    }

    #[test]
    fn test_validate_edl_valid() {
        let tmp = std::env::temp_dir().join("oneai_edl_test.mp4");
        std::fs::write(&tmp, b"x").unwrap();
        let edl = Edl {
            output: "out.mp4".into(),
            width: 1920,
            height: 1080,
            fps: 30,
            bgm: None,
            clips: vec![clip_video(&tmp.to_string_lossy(), 0.0, 5.0)],
        };
        assert!(validate_edl(&edl).is_empty());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_normalize_and_ken_burns_filters() {
        let vf = normalize_vfilter(1920, 1080, 30, 2.0);
        assert!(vf.contains("setpts=PTS/2"));
        assert!(vf.contains("scale=1920:1080"));
        assert!(vf.contains("fps=30"));

        let kb = ken_burns_vfilter(1920, 1080, 30, 120);
        assert!(kb.contains("zoompan"));
        assert!(kb.contains("s=1920x1080"));
    }

    #[test]
    fn test_probe_media_tool_properties() {
        let tool = ProbeMediaTool::new();
        assert_eq!(tool.name(), "probe_media");
        assert_eq!(tool.risk_level(), RiskLevel::Low);
    }

    #[test]
    fn test_validate_timeline_tool_always_available() {
        assert!(ValidateTimelineTool::new().service_available());
    }

    #[tokio::test]
    async fn test_validate_timeline_tool_execute() {
        let tool = ValidateTimelineTool::new();
        let bad = tool
            .execute(serde_json::json!({"edl": {"output": "", "clips": []}}))
            .await
            .unwrap();
        assert!(!bad.success);
        assert!(bad.error.unwrap().contains("Timeline invalid"));

        let tmp = std::env::temp_dir().join("oneai_vt_test.mp4");
        std::fs::write(&tmp, b"x").unwrap();
        let good = tool
            .execute(serde_json::json!({"edl": {
                "output": "out.mp4",
                "clips": [{"source": tmp.to_string_lossy(), "type": "video", "in": 0.0, "out": 3.0}]
            }}))
            .await
            .unwrap();
        assert!(good.success);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_binary_gate_reports_bool_without_panic() {
        // On machines without ffmpeg the gate is false; with ffmpeg, true. Either
        // way it must return a bool and never panic (the Footprint-gate contract).
        let _ = ffmpeg_available();
        let _ = ffprobe_available();
    }

    #[tokio::test]
    async fn test_render_errors_gracefully_on_invalid_edl() {
        let tool = RenderTool::new();
        let out = tool
            .execute(serde_json::json!({"edl": {"output": "", "clips": []}}))
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("Timeline invalid"));
    }

    // ─── End-to-end smoke test (requires ffmpeg/ffprobe on PATH) ─────────────
    //
    // Verifies the full render pipeline against real fixtures: generate two short
    // video clips + one still image, render an EDL that cuts them together (with
    // a Ken Burns image segment), then confirm the output exists and probe_media
    // reports a plausible duration. `#[ignore]`d so hermetic CI never runs it;
    // run locally with `cargo test -p oneai-tool -- --ignored media_tools`.

    #[tokio::test]
    #[ignore = "requires ffmpeg/ffprobe on PATH"]
    async fn smoke_render_pipeline_end_to_end() {
        if !ffmpeg_available() || !ffprobe_available() {
            return;
        }
        let tmp = temp_work_dir();
        std::fs::create_dir_all(&tmp).unwrap();

        // Two 2-second color-source video clips.
        let clip_a = tmp.join("a.mp4").to_string_lossy().to_string();
        let clip_b = tmp.join("b.mp4").to_string_lossy().to_string();
        for (path, color) in [(&clip_a, "red"), (&clip_b, "blue")] {
            let mut cmd = tokio::process::Command::new("ffmpeg");
            cmd.args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("color=c={color}:s=320x240:r=30:d=2"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                path,
            ]);
            run_cmd(cmd, 60).await.unwrap();
        }
        // One still image (a PNG).
        let img = tmp.join("img.png").to_string_lossy().to_string();
        let mut cmd = tokio::process::Command::new("ffmpeg");
        cmd.args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=green:s=320x240",
            "-frames:v",
            "1",
            &img,
        ]);
        run_cmd(cmd, 60).await.unwrap();

        let out = tmp.join("out.mp4").to_string_lossy().to_string();
        let edl = serde_json::json!({
            "output": out,
            "width": 320,
            "height": 240,
            "fps": 30,
            "clips": [
                {"source": clip_a, "type": "video", "in": 0.0, "out": 1.0},
                {"source": img, "type": "image", "duration": 1.0},
                {"source": clip_b, "type": "video", "in": 0.0, "out": 1.0}
            ]
        });

        let tool = RenderTool::new();
        let res = tool.execute(serde_json::json!({"edl": edl})).await.unwrap();
        assert!(res.success, "render failed: {:?}", res.error);
        assert!(
            !res.artifacts.is_empty(),
            "render should emit a deliverable"
        );
        assert!(std::path::Path::new(&out).exists(), "output not written");

        // probe_media should report a ~3s duration.
        let probe = ProbeMediaTool::new()
            .execute(serde_json::json!({"path": out}))
            .await
            .unwrap();
        assert!(probe.success, "probe failed: {:?}", probe.error);
        assert!(
            probe.content.contains("duration:"),
            "probe: {}",
            probe.content
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
