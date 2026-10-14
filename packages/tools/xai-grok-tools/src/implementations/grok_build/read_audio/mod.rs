//! `read_audio`: attach a local media path to the external macOS inference server.
//!
//! Architecture map: resolve workspace path -> verify a regular audio/media
//! file -> validate the optional range and locale -> return a typed carrier.
//! The harness never analyzes or uploads bytes. q36 invokes the bundled
//! native executable, validates schema v2, and replaces this part with XML.

use serde::{Deserialize, Serialize};

use crate::types::output::{ReadAudioOutput, ToolOutput};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::{Cwd, DisplayCwd, resolve_model_path};
use crate::types::tool::{ToolKind, ToolNamespace};

pub const READ_AUDIO_TOOL_NAME: &str = "read_audio";

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReadAudioInput {
    #[schemars(
        description = "Path to a local audio file. Relative paths resolve against the workspace."
    )]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Optional window in seconds as \"start-end\", for example \"10-40\"."
    )]
    pub time_range: Option<String>,
    #[serde(default = "default_locale")]
    #[schemars(description = "Speech recognition locale. Default \"en-US\".")]
    pub locale: String,
}

fn default_locale() -> String {
    "en-US".to_owned()
}

#[derive(Debug, Default)]
pub struct ReadAudioTool;

impl crate::types::tool_metadata::ToolMetadata for ReadAudioTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }
    fn description_template(&self) -> &str {
        r#"Read a local audio file and attach native transcript, timestamped words, and sound classifications to the model context. Optionally pass time_range as \"start-end\" seconds and locale such as \"en-US\". This tool requires macOS and the compatible external inference server."#
    }
    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for ReadAudioTool {
    type Args = ReadAudioInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(READ_AUDIO_TOOL_NAME).expect("valid tool id")
    }
    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            READ_AUDIO_TOOL_NAME,
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

    #[tracing::instrument(name = "tool.read_audio", skip_all, fields(path = %input.path))]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: ReadAudioInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        if !cfg!(target_os = "macos") {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "read_audio is available only on macOS",
            ));
        }
        if input.locale.trim().is_empty() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "read_audio locale must not be empty",
            ));
        }
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
            .map_err(|error| {
                xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "read_audio: {} not found: {error}",
                    joined.display()
                ))
            })?;
        let metadata = tokio::fs::metadata(&path).await.map_err(|error| {
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "read_audio: cannot stat {}: {error}",
                path.display()
            ))
        })?;
        if !metadata.is_file() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "read_audio: {} is not a file",
                path.display()
            )));
        }
        let time_range = input
            .time_range
            .as_deref()
            .map(super::read_video::parse_time_range)
            .transpose()?;
        Ok(ToolOutput::ReadAudio(ReadAudioOutput {
            path,
            time_range,
            locale: input.locale,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_validation_are_explicit() {
        let input: ReadAudioInput = serde_json::from_str(r#"{"path":"a.m4a"}"#).unwrap();
        assert_eq!(input.locale, "en-US");
        assert_eq!(
            super::super::read_video::parse_time_range("10-40").unwrap(),
            [10.0, 40.0]
        );
        assert!(super::super::read_video::parse_time_range("40-10").is_err());
    }
}
