//! Built-in agent tools for image and video generation (`generate_image` and
//! `generate_video`).
//!
//! Both tools delegate execution to [`crate::media::MediaRunner`], which:
//! - enforces `SwitchImpact::Stateless` automatic priority failover across
//!   official provider deployments,
//! - stores clean originals in `<data_dir>/media_generations/<id>/`,
//! - stamps D-001 compliance provenance via `MediaCompliancePort`,
//! - and observes `ToolContext::cancel` when a chat turn is aborted.
//!
//! Outputs are returned through [`Tool::execute_rich`] as [`MediaOut::Bytes`]
//! so `agent::tool_media::store` renders images and videos inline in the chat
//! transcript (`schema_version = 3`).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use crate::db::entity::media_generation::{MediaGenerationDomain, MediaGenerationSource};
use crate::media::storage::extension_for_mime;
use crate::media::{MediaGenerationJobSpec, MediaGenerationParams, MediaRunner};

use super::spec::{Effect, ToolSpec};
use super::{MediaOut, Permission, Tool, ToolContext, ToolOutput, description_property};

pub type SharedMediaRunner = Arc<OnceLock<Arc<MediaRunner>>>;

pub struct GenerateImageTool {
    runner: SharedMediaRunner,
}

impl GenerateImageTool {
    pub fn new(runner: SharedMediaRunner) -> Self {
        Self { runner }
    }
}

pub struct GenerateVideoTool {
    runner: SharedMediaRunner,
}

impl GenerateVideoTool {
    pub fn new(runner: SharedMediaRunner) -> Self {
        Self { runner }
    }
}

async fn run_media_tool(
    runner_slot: &SharedMediaRunner,
    domain: MediaGenerationDomain,
    args: serde_json::Value,
    context: &ToolContext,
) -> Result<ToolOutput, String> {
    let runner = runner_slot
        .get()
        .ok_or_else(|| "media generation service is not initialized in this runtime".to_string())?;
    let prompt = args["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("missing non-empty 'prompt' argument")?
        .to_string();

    let params = MediaGenerationParams {
        aspect_ratio: args["aspect_ratio"].as_str().map(str::to_string),
        size: args["size"].as_str().map(str::to_string),
        quality: args["quality"].as_str().map(str::to_string),
        count: args["count"].as_u64().map(|n| n.clamp(1, 4) as u32),
        resolution: args["resolution"].as_str().map(str::to_string),
        duration_seconds: args["duration_seconds"].as_u64().map(|n| n.clamp(1, 30) as u32),
        seed: args["seed"].as_i64(),
        generate_audio: args["generate_audio"].as_bool(),
    };

    let spec = MediaGenerationJobSpec {
        domain,
        logical_model_id: args["model_id"].as_str().map(str::to_string),
        prompt,
        reference_images: Vec::new(),
        params,
        source: MediaGenerationSource::AgentTool,
        conversation_id: context.conversation_id.clone(),
    };

    let record = runner.run_to_completion(spec, context.cancel.clone()).await?;
    let mut media = Vec::with_capacity(record.assets.len());
    for (idx, asset) in record.assets.iter().enumerate() {
        let abs = runner.store().resolve_relative(&asset.relative_path)?;
        let bytes = tokio::fs::read(&abs)
            .await
            .map_err(|err| format!("failed to read generated media asset: {err}"))?;
        let ext = extension_for_mime(&asset.mime_type);
        let prefix = match domain {
            MediaGenerationDomain::ImageGen => "generated_image",
            MediaGenerationDomain::VideoGen => "generated_video",
        };
        media.push(MediaOut::Bytes {
            mime: asset.mime_type.clone(),
            bytes,
            name: Some(format!("{prefix}_{}_{idx}.{ext}", &record.id[..record.id.len().min(8)])),
        });
    }

    let summary = serde_json::json!({
        "generation_id": record.id,
        "domain": record.capability_domain,
        "model": record.remote_model_id,
        "protocol": record.protocol_id,
        "asset_count": record.assets.len(),
        "assets": record.assets.iter().map(|a| serde_json::json!({
            "produce_id": a.produce_id,
            "mime_type": a.mime_type,
            "width": a.width,
            "height": a.height,
            "duration_seconds": a.duration_seconds,
            "revised_prompt": a.revised_prompt,
        })).collect::<Vec<_>>(),
    });

    Ok(ToolOutput {
        text: serde_json::to_string_pretty(&summary).unwrap_or_else(|_| format!("Generated {} asset(s)", media.len())),
        media,
    })
}

#[async_trait]
impl Tool for GenerateImageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            effect: Effect::Network,
            loop_handled: false,
            plan_mode: false,
            explore: false,
            reviewer: false,
            parallel: true,
        }
    }

    fn name(&self) -> &str {
        "generate_image"
    }

    fn description(&self) -> &str {
        "Generate one or more images from a text prompt using the configured image generation model (OpenAI Images, Google Gemini Image, or Volcengine Seedream). Returns generated images inline."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Detailed visual description of the image to generate."
                },
                "description": description_property(),
                "model_id": {
                    "type": "string",
                    "description": "Optional logical image model ID. Omit to use the user's default image model."
                },
                "aspect_ratio": {
                    "type": "string",
                    "description": "Optional aspect ratio such as '1:1', '16:9', '9:16', '4:3', or '3:4'."
                },
                "size": {
                    "type": "string",
                    "description": "Optional explicit resolution/size string (e.g. '1024x1024', '2048x2048', '2K')."
                },
                "quality": {
                    "type": "string",
                    "description": "Optional quality tier (e.g. 'low', 'medium', 'high', 'auto')."
                },
                "count": {
                    "type": "integer",
                    "description": "Optional number of images to generate (1-4)."
                },
                "seed": {
                    "type": "integer",
                    "description": "Optional random seed for reproducibility."
                }
            },
            "required": ["prompt"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        self.execute_rich(args, context).await.map(|out| out.text)
    }

    async fn execute_rich(&self, args: serde_json::Value, context: &ToolContext) -> Result<ToolOutput, String> {
        run_media_tool(&self.runner, MediaGenerationDomain::ImageGen, args, context).await
    }
}

#[async_trait]
impl Tool for GenerateVideoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            effect: Effect::Network,
            loop_handled: false,
            plan_mode: false,
            explore: false,
            reviewer: false,
            parallel: true,
        }
    }

    fn name(&self) -> &str {
        "generate_video"
    }

    fn description(&self) -> &str {
        "Generate a video from a text prompt using the configured video generation model (Google Veo, Volcengine Seedance, Kling, MiniMax Hailuo, Vidu, or Alibaba Bailian Wan). Polls asynchronously until complete and returns the generated video inline."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Detailed motion and scene description of the video to generate."
                },
                "description": description_property(),
                "model_id": {
                    "type": "string",
                    "description": "Optional logical video model ID. Omit to use the user's default video model."
                },
                "aspect_ratio": {
                    "type": "string",
                    "description": "Optional aspect ratio such as '16:9', '9:16', '1:1', '4:3', or '3:4'."
                },
                "resolution": {
                    "type": "string",
                    "description": "Optional video resolution such as '480p', '720p', '1080p', or '4k'."
                },
                "duration_seconds": {
                    "type": "integer",
                    "description": "Optional target video duration in seconds (e.g. 5, 6, 8, 10)."
                },
                "generate_audio": {
                    "type": "boolean",
                    "description": "Optional flag requesting native audio generation when supported by the model."
                },
                "seed": {
                    "type": "integer",
                    "description": "Optional random seed for reproducibility."
                }
            },
            "required": ["prompt"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        self.execute_rich(args, context).await.map(|out| out.text)
    }

    async fn execute_rich(&self, args: serde_json::Value, context: &ToolContext) -> Result<ToolOutput, String> {
        run_media_tool(&self.runner, MediaGenerationDomain::VideoGen, args, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_tools_require_ask_permission_and_stay_out_of_read_only_modes() {
        let slot: SharedMediaRunner = Arc::new(OnceLock::new());
        let img = GenerateImageTool::new(slot.clone());
        let vid = GenerateVideoTool::new(slot);

        for tool in [&img as &dyn Tool, &vid as &dyn Tool] {
            assert_eq!(tool.default_permission(), Permission::Ask);
            let spec = tool.spec();
            assert_eq!(spec.effect, Effect::Network);
            assert!(!spec.plan_mode, "{} must not run in plan mode", tool.name());
            assert!(!spec.explore, "{} must not run in explore mode", tool.name());
            assert!(!spec.reviewer, "{} must not run in reviewer mode", tool.name());
            assert!(spec.parallel);
        }
    }
}
