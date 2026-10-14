//! `read_video`: hand a local video file to the model's vision tower.
//!
//! Architecture map
//! ----------------
//! `read_file` reads images by inlining their bytes as a data URL. Video is
//! different in three ways, which is why it is a separate tool instead of a
//! `read_file` branch:
//!
//! 1. The bytes never travel through the harness. The compatible external inference server
//!    (q36 `app.py`, `_prepare_video_part`) ingests a server-local FILE PATH and
//!    runs its own ffmpeg decode, so this tool only validates the file and
//!    forwards the absolute path.
//! 2. Video has knobs images do not: a resolution class, a sampling fps, and
//!    an optional time window. They are closed string enums here and the
//!    ENGINE owns the conversion to pixels / floats (train = serve: the
//!    engine's `RESOLUTION_CLASS_PIXELS` / `FPS_CHOICES` are the single source
//!    of truth; no pixel constants are duplicated in Rust).
//! 3. The result is two carriers, like `read_file`'s image path: the prose the
//!    model reads (`ToolOutput::ReadVideo` -> `to_prompt_format`) and the
//!    `ContentPart::Video` the session attaches to the tool result so the
//!    tower actually ingests the frames (`tool_calls.rs`).
//!
//! Stages: resolve the model-provided path against the workspace -> sniff the
//! magic bytes (mp4 / mov / webm / mkv / avi only) -> parse the knobs ->
//! return the typed output. Aspect band (9:16..16:9), duration and token
//! budget are enforced by the engine, which fails loudly with a 400 that
//! surfaces through the API at sampling time; the harness stays thin.
//!
//! Only meaningful on the local chat-completions backend (shared filesystem).
//! The session refuses to attach the part on any other backend, and every
//! incompatible serializer renders a text placeholder instead of the q36 shape.

use serde::{Deserialize, Serialize};

use crate::types::output::{ReadVideoOutput, ToolOutput};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::{Cwd, DisplayCwd, resolve_model_path};
use crate::types::tool::{ToolKind, ToolNamespace};

pub const READ_VIDEO_TOOL_NAME: &str = "read_video";

/// Bytes sniffed for the container magic. Every supported container declares
/// itself well inside this window (mp4/mov `ftyp` at offset 4, RIFF/AVI at
/// 0, EBML doctype for mkv/webm within the first few hundred bytes).
const MAGIC_SNIFF_BYTES: usize = 64 * 1024;

/// Accepted container MIME types as reported by the `infer` crate, paired
/// with the friendly name used in errors.
const SUPPORTED_CONTAINERS: &[(&str, &str)] = &[
    ("video/mp4", "mp4"),
    ("video/quicktime", "mov"),
    ("video/webm", "webm"),
    ("video/x-matroska", "mkv"),
    ("video/x-msvideo", "avi"),
];

/// Resolution class sent to the engine verbatim. The values are the audio
/// bridge's trained policy menu; 720p exists in training but is deliberately
/// not exposed, and 4K sources are accepted and downscaled.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
pub enum VideoResolution {
    #[serde(rename = "240p")]
    R240p,
    #[default]
    #[serde(rename = "360p")]
    R360p,
    #[serde(rename = "480p")]
    R480p,
}

impl VideoResolution {
    pub fn as_engine_str(self) -> &'static str {
        match self {
            Self::R240p => "240p",
            Self::R360p => "360p",
            Self::R480p => "480p",
        }
    }
}

/// Sampling rate in frames per second, as a STRING enum so the API is a
/// closed set (the engine rejects numbers on purpose). 0.5 / 1 / 2 / 3 sit on
/// the trained menu; 0.25 and 4 are format-safe best-effort extrapolations.
///
/// Deserialization is lenient on purpose: the harness's argument layer turns
/// numeric-looking strings ("2") into JSON numbers before serde sees them, so
/// `2`, `2.0` and `"2"` must all mean [`VideoFps::Two`]. The schema stays a
/// string enum; the engine still receives the canonical string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema, Default)]
pub enum VideoFps {
    #[serde(rename = "0.25")]
    Quarter,
    #[serde(rename = "0.5")]
    Half,
    #[default]
    #[serde(rename = "1")]
    One,
    #[serde(rename = "2")]
    Two,
    #[serde(rename = "3")]
    Three,
    #[serde(rename = "4")]
    Four,
}

impl VideoFps {
    const ALL: [VideoFps; 6] = [
        Self::Quarter,
        Self::Half,
        Self::One,
        Self::Two,
        Self::Three,
        Self::Four,
    ];

    /// Canonical string ("0.25" .. "4") or a number equal to one of them.
    pub fn parse_lenient(text: &str) -> Option<Self> {
        let t = text.trim();
        if let Some(v) = Self::ALL.iter().find(|v| v.as_engine_str() == t) {
            return Some(*v);
        }
        let n: f64 = t.parse().ok()?;
        Self::ALL
            .iter()
            .copied()
            .find(|v| (v.as_engine_str().parse::<f64>().unwrap() - n).abs() < 1e-9)
    }

    pub fn as_engine_str(self) -> &'static str {
        match self {
            Self::Quarter => "0.25",
            Self::Half => "0.5",
            Self::One => "1",
            Self::Two => "2",
            Self::Three => "3",
            Self::Four => "4",
        }
    }
}

impl<'de> Deserialize<'de> for VideoFps {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Num(f64),
            Str(String),
        }
        let text = match Raw::deserialize(d)? {
            Raw::Num(n) => n.to_string(),
            Raw::Str(s) => s,
        };
        Self::parse_lenient(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "fps must be one of \"0.25\", \"0.5\", \"1\", \"2\", \"3\", \"4\"; got {text:?}"
            ))
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReadVideoInput {
    #[schemars(
        description = "Path of the video file (mp4, mov, webm, mkv, avi). Relative paths resolve against the workspace; absolute paths are used as is."
    )]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Frame resolution class the model sees. Default \"360p\". Use \"240p\" for long clips (fewer tokens) and \"480p\" for fine detail. Source aspect must be between 9:16 and 16:9; wider or taller sources are rejected."
    )]
    pub resolution: Option<VideoResolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Frames sampled per second, as a string. Default \"1\". Lower (\"0.5\", \"0.25\") stretches the token budget over longer clips; higher (\"2\", \"3\", \"4\") captures fast motion in short clips."
    )]
    pub fps: Option<VideoFps>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Optional window in seconds as \"start-end\" (e.g. \"30-90\"). Omit to read the whole clip. Required when a whole clip exceeds the token budget."
    )]
    pub time_range: Option<String>,
}

/// `"30-90"` -> `[30.0, 90.0]`. Loud on anything else; the engine re-validates
/// the same invariants (0 <= start < end) against the real duration.
pub fn parse_time_range(text: &str) -> Result<[f64; 2], xai_tool_runtime::ToolError> {
    let bad = |why: &str| {
        xai_tool_runtime::ToolError::invalid_arguments(format!(
            "time_range must be \"start-end\" in seconds (e.g. \"30-90\"); got {text:?}: {why}"
        ))
    };
    let (a, b) = text
        .trim()
        .split_once('-')
        .ok_or_else(|| bad("missing '-' separator"))?;
    let start: f64 = a.trim().parse().map_err(|_| bad("start is not a number"))?;
    let end: f64 = b.trim().parse().map_err(|_| bad("end is not a number"))?;
    if !start.is_finite() || !end.is_finite() || start < 0.0 || end <= start {
        return Err(bad("need 0 <= start < end"));
    }
    Ok([start, end])
}

/// Sniff the container from the file head. Returns the friendly container
/// name, or a loud error naming what was found instead.
pub fn detect_video_container(head: &[u8]) -> Result<&'static str, xai_tool_runtime::ToolError> {
    let Some(kind) = infer::get(head) else {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(
            "read_video: could not identify the file type from its magic bytes; supported containers are mp4, mov, webm, mkv, avi",
        ));
    };
    let mime = kind.mime_type();
    SUPPORTED_CONTAINERS
        .iter()
        .find(|(m, _)| *m == mime)
        .map(|(_, name)| *name)
        .ok_or_else(|| {
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "read_video: {mime} is not a supported video container (mp4, mov, webm, mkv, avi){}",
                if mime.starts_with("image/") {
                    "; use read_file for images"
                } else {
                    ""
                }
            ))
        })
}

async fn read_file_head(path: &std::path::Path) -> Result<Vec<u8>, xai_tool_runtime::ToolError> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await.map_err(|e| {
        xai_tool_runtime::ToolError::invalid_arguments(format!(
            "read_video: cannot open {}: {e}",
            path.display()
        ))
    })?;
    let meta = file.metadata().await.map_err(|e| {
        xai_tool_runtime::ToolError::invalid_arguments(format!(
            "read_video: cannot stat {}: {e}",
            path.display()
        ))
    })?;
    if meta.is_dir() {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
            "read_video: {} is a directory, not a video file",
            path.display()
        )));
    }
    let mut head = vec![0u8; MAGIC_SNIFF_BYTES];
    let mut filled = 0usize;
    while filled < head.len() {
        let n = file.read(&mut head[filled..]).await.map_err(|e| {
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "read_video: cannot read {}: {e}",
                path.display()
            ))
        })?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    head.truncate(filled);
    Ok(head)
}

#[derive(Debug, Default)]
pub struct ReadVideoTool;

impl crate::types::tool_metadata::ToolMetadata for ReadVideoTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r##"Read a local video file (mp4, mov, webm, mkv, avi) so you can see its frames. The frames are attached to this tool result and sampled at `fps` frames per second at the `resolution` class; timestamps are carried as text between frames, so refer to moments by seconds. Defaults: resolution "360p", fps "1", whole clip. Choose lower fps / 240p for long clips and higher fps / 480p for short, detailed ones; pass `time_range` ("start-end" seconds) to read one window of a long clip. Sources wider than 16:9 or taller than 9:16 are rejected rather than cropped. Only available when running against the compatible external inference server. Example: read_video(path="clips/demo.mp4", resolution="480p", fps="2", time_range="10-40")"##
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for ReadVideoTool {
    type Args = ReadVideoInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(READ_VIDEO_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            READ_VIDEO_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    /// Step by step: resolve the path the way `read_file` does (workspace
    /// relative, display-cwd rewrite, canonicalize); refuse anything that is
    /// not one of the five containers by magic bytes; parse `time_range`;
    /// return the engine-facing strings. Nothing is decoded here.
    ///
    /// Main callers: the tool runner, on a model `read_video` call.
    #[tracing::instrument(name = "tool.read_video", skip_all, fields(path = %input.path))]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: ReadVideoInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let (cwd, display_cwd) = {
            let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
            let res = resources.lock().await;
            (
                res.require::<Cwd>()?.0.clone(),
                res.get::<DisplayCwd>().map(|d| d.0.clone()),
            )
        };
        let joined = resolve_model_path(&cwd, display_cwd.as_deref(), &input.path);
        let path = crate::util::fs::try_canonicalize(&joined)
            .await
            .map_err(|e| {
                xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "read_video: {} not found: {e}",
                    joined.display()
                ))
            })?;
        let head = read_file_head(&path).await?;
        let container = detect_video_container(&head)?;
        let time_range = match input.time_range.as_deref() {
            Some(text) => Some(parse_time_range(text)?),
            None => None,
        };
        let output = ReadVideoOutput {
            path,
            container: container.to_owned(),
            resolution: input
                .resolution
                .unwrap_or_default()
                .as_engine_str()
                .to_owned(),
            fps: input.fps.unwrap_or_default().as_engine_str().to_owned(),
            time_range,
        };
        Ok(ToolOutput::ReadVideo(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_exposes_string_enums_with_defaults() {
        let schema = serde_json::to_value(schemars::schema_for!(ReadVideoInput)).unwrap();
        let text = schema.to_string();
        for v in [
            "\"240p\"", "\"360p\"", "\"480p\"", "\"0.25\"", "\"0.5\"", "\"4\"",
        ] {
            assert!(text.contains(v), "schema missing {v}: {text}");
        }
        let parsed: ReadVideoInput = serde_json::from_str(
            r#"{"path":"a.mp4","resolution":"240p","fps":"0.5","time_range":"3-9"}"#,
        )
        .unwrap();
        assert_eq!(parsed.resolution, Some(VideoResolution::R240p));
        assert_eq!(parsed.fps, Some(VideoFps::Half));
        // the argument layer may hand us numbers: 2, 2.0 and "2" are all Two
        for raw in [
            r#"{"path":"a.mp4","fps":2}"#,
            r#"{"path":"a.mp4","fps":2.0}"#,
            r#"{"path":"a.mp4","fps":"2"}"#,
        ] {
            let p: ReadVideoInput = serde_json::from_str(raw).unwrap();
            assert_eq!(p.fps, Some(VideoFps::Two), "{raw}");
        }
        let p: ReadVideoInput = serde_json::from_str(r#"{"path":"a.mp4","fps":0.25}"#).unwrap();
        assert_eq!(p.fps, Some(VideoFps::Quarter));
        assert!(serde_json::from_str::<ReadVideoInput>(r#"{"path":"a.mp4","fps":1.5}"#).is_err());
        assert!(serde_json::from_str::<ReadVideoInput>(r#"{"path":"a.mp4","fps":"5"}"#).is_err());
        assert!(
            serde_json::from_str::<ReadVideoInput>(r#"{"path":"a.mp4","resolution":"720p"}"#)
                .is_err()
        );
        assert_eq!(VideoResolution::default().as_engine_str(), "360p");
        assert_eq!(VideoFps::default().as_engine_str(), "1");
    }

    #[test]
    fn time_range_parses_and_rejects() {
        assert_eq!(parse_time_range("30-90").unwrap(), [30.0, 90.0]);
        assert_eq!(parse_time_range(" 0 - 12.5 ").unwrap(), [0.0, 12.5]);
        for bad in ["30", "90-30", "5-5", "-1-4", "a-b", "30:90"] {
            assert!(parse_time_range(bad).is_err(), "{bad} should fail");
        }
    }

    #[test]
    fn container_detection_accepts_five_containers_and_rejects_others() {
        let mut mp4 = vec![0, 0, 0, 0x18];
        mp4.extend_from_slice(b"ftypisom");
        mp4.extend_from_slice(&[0; 64]);
        assert_eq!(detect_video_container(&mp4).unwrap(), "mp4");
        let mut mov = vec![0, 0, 0, 0x14];
        mov.extend_from_slice(b"ftypqt  ");
        mov.extend_from_slice(&[0; 64]);
        assert_eq!(detect_video_container(&mov).unwrap(), "mov");
        let mut avi = b"RIFF\x00\x00\x00\x00AVI LIST".to_vec();
        avi.extend_from_slice(&[0; 64]);
        assert_eq!(detect_video_container(&avi).unwrap(), "avi");
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        let err = detect_video_container(png).unwrap_err().to_string();
        assert!(err.contains("read_file"), "{err}");
        assert!(detect_video_container(b"plain text").is_err());
    }
}
