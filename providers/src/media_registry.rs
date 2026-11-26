//! Media generation provider registry (`image_gen` & `video_gen`).
//!
//! Instantiates official image/video protocol adapters from a canonical
//! protocol ID (or inferred from `(domain, vendor_id / provider_type / model)`)
//! plus endpoint URL and credential.

use meridian_provider_sdk::chat::Credential;
use meridian_provider_sdk::domain::CapabilityDomain;
use meridian_provider_sdk::media_gen::{ImageGenProvider, VideoGenProvider};

use crate::protocols::bailian_wan_video::BailianWanVideoProvider;
use crate::protocols::google_image_gen::GoogleImageGenProvider;
use crate::protocols::google_veo::GoogleVeoProvider;
use crate::protocols::kling_video::KlingVideoProvider;
use crate::protocols::minimax_video::MiniMaxVideoProvider;
use crate::protocols::openai_images::OpenAIImagesProvider;
use crate::protocols::vidu_video::ViduVideoProvider;
use crate::protocols::volcengine_seedance::VolcengineSeedanceProvider;
use crate::protocols::volcengine_seedream::VolcengineSeedreamProvider;

/// Resolve the official media protocol ID for a deployment in `image_gen` or
/// `video_gen`. Explicit `protocol_override` takes precedence if it matches a
/// registered protocol for `domain`; otherwise canonical model metadata or
/// vendor/provider identity selects the official protocol.
pub fn resolve_media_protocol(
    domain: CapabilityDomain,
    protocol_override: Option<&str>,
    provider_type: &str,
    vendor_id: Option<&str>,
    remote_model_id: &str,
) -> Result<&'static str, String> {
    if let Some(explicit) = protocol_override.map(str::trim).filter(|s| !s.is_empty())
        && let Some(desc) = crate::protocols::find_protocol(explicit)
        && desc.domain == domain
    {
        return Ok(desc.id);
    }

    if let Some(matched) = crate::models::canonicalize_model_id(remote_model_id) {
        for proto in &matched.canonical_protocols {
            if let Some(desc) = crate::protocols::find_protocol(proto)
                && desc.domain == domain
            {
                return Ok(desc.id);
            }
        }
    }

    let hint = vendor_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(provider_type)
        .to_ascii_lowercase();

    match (domain, hint.as_str()) {
        (CapabilityDomain::ImageGen, "openai") => Ok("openai_images"),
        (CapabilityDomain::ImageGen, "google") => Ok("google_image_gen"),
        (CapabilityDomain::ImageGen, "volcengine" | "ark" | "seedream") => Ok("volcengine_seedream"),
        (CapabilityDomain::VideoGen, "google") => Ok("google_veo"),
        (CapabilityDomain::VideoGen, "volcengine" | "ark" | "seedance") => Ok("volcengine_seedance"),
        (CapabilityDomain::VideoGen, "kling") => Ok("kling_video"),
        (CapabilityDomain::VideoGen, "minimax" | "hailuo") => Ok("minimax_video"),
        (CapabilityDomain::VideoGen, "vidu") => Ok("vidu_video"),
        (CapabilityDomain::VideoGen, "bailian" | "dashscope" | "wan") => Ok("bailian_wan_video"),
        _ => Err(format!(
            "cannot resolve official `{}` protocol for provider `{hint}` and model `{remote_model_id}`",
            domain.as_str()
        )),
    }
}

/// Instantiate an official [`ImageGenProvider`] for `protocol`.
pub fn create_image_gen_provider(
    protocol: &str,
    base_url: &str,
    credential: &Credential,
) -> Result<Box<dyn ImageGenProvider>, String> {
    match protocol {
        "openai_images" => match credential {
            Credential::ApiKey(key) => Ok(Box::new(OpenAIImagesProvider::new(base_url, key))),
            _ => Err("OpenAI Images requires an API key credential".into()),
        },
        "google_image_gen" => match credential {
            Credential::ApiKey(key) => Ok(Box::new(GoogleImageGenProvider::new(base_url, key))),
            Credential::GoogleVertexServiceAccount(json) => {
                Ok(Box::new(GoogleImageGenProvider::new_vertex(base_url, json)?))
            }
            _ => Err("Google Image Generation requires an API key or Vertex Service Account credential".into()),
        },
        "volcengine_seedream" => match credential {
            Credential::ApiKey(key) => Ok(Box::new(VolcengineSeedreamProvider::new(base_url, key))),
            _ => Err("Volcengine Seedream requires an API key credential".into()),
        },
        other => Err(format!("unknown image_gen protocol `{other}`")),
    }
}

/// Instantiate an official [`VideoGenProvider`] for `protocol`.
pub fn create_video_gen_provider(
    protocol: &str,
    base_url: &str,
    credential: &Credential,
) -> Result<Box<dyn VideoGenProvider>, String> {
    match protocol {
        "google_veo" => match credential {
            Credential::ApiKey(key) => Ok(Box::new(GoogleVeoProvider::new(base_url, key))),
            Credential::GoogleVertexServiceAccount(json) => {
                Ok(Box::new(GoogleVeoProvider::new_vertex(base_url, json)?))
            }
            _ => Err("Google Veo requires an API key or Vertex Service Account credential".into()),
        },
        "volcengine_seedance" => match credential {
            Credential::ApiKey(key) => Ok(Box::new(VolcengineSeedanceProvider::new(base_url, key))),
            _ => Err("Volcengine Seedance requires an API key credential".into()),
        },
        "kling_video" => match credential {
            Credential::ApiKey(key) => Ok(Box::new(KlingVideoProvider::new(base_url, key))),
            _ => Err("Kling Video requires a static API key credential".into()),
        },
        "minimax_video" => match credential {
            Credential::ApiKey(key) => Ok(Box::new(MiniMaxVideoProvider::new(base_url, key))),
            _ => Err("MiniMax Video requires an API key credential".into()),
        },
        "vidu_video" => match credential {
            Credential::ApiKey(key) => Ok(Box::new(ViduVideoProvider::new(base_url, key))),
            _ => Err("Vidu Video requires an API key credential".into()),
        },
        "bailian_wan_video" => match credential {
            Credential::ApiKey(key) => Ok(Box::new(BailianWanVideoProvider::new(base_url, key))),
            _ => Err("Alibaba Bailian Wan requires an API key credential".into()),
        },
        other => Err(format!("unknown video_gen protocol `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use meridian_provider_sdk::conformance::{
        assert_image_gen_provider_conformance, assert_video_gen_provider_conformance,
    };

    use super::*;

    #[test]
    fn all_nine_official_media_protocols_instantiate_and_pass_conformance() {
        let key = Credential::ApiKey("test-key".into());
        for protocol in ["openai_images", "google_image_gen", "volcengine_seedream"] {
            let p = create_image_gen_provider(protocol, "", &key).unwrap();
            assert_eq!(p.provider_id(), protocol);
            assert_image_gen_provider_conformance(p.as_ref());
        }
        for protocol in [
            "google_veo",
            "volcengine_seedance",
            "kling_video",
            "minimax_video",
            "vidu_video",
            "bailian_wan_video",
        ] {
            let p = create_video_gen_provider(protocol, "", &key).unwrap();
            assert_eq!(p.provider_id(), protocol);
            assert_video_gen_provider_conformance(p.as_ref());
        }
    }
}
