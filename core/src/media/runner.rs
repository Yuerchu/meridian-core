//! Unified `MediaRunner` for `image_gen` and `video_gen` across Studio and
//! built-in agent tools.
//!
//! Enforces:
//! 1. **Stateless Multi-Provider Failover**: ranks enabled deployments via
//!    [`crate::agent::routing::rank_enabled_candidates`] (official standard
//!    protocols ahead of degraded relays, then ascending priority) and fails
//!    over automatically ONLY on [`MediaGenError::Retryable`] errors via
//!    [`crate::agent::routing::decide_failover`].
//! 2. **Clean Originals + Core Compliance Port (D-001)**: upstream `watermark`
//!    defaults to `false`; generated assets are saved under
//!    `<data_dir>/media_generations/<id>/`, stamped via
//!    [`MediaCompliancePort::on_generated_original`], and exported solely via
//!    [`MediaCompliancePort::prepare_export`].
//! 3. **Cancellable Async Jobs**: tracks active `CancellationToken`s per
//!    generation ID so either Studio UI or an aborted chat turn cancels
//!    long-running video polling immediately.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use meridian_provider_sdk::chat::Credential;
use meridian_provider_sdk::domain::{CapabilityDomain, RoutingPolicy, SwitchImpact};
use meridian_provider_sdk::media_gen::{
    GeneratedMedia, ImageGenProvider, ImageGenRequest, MediaGenError, MediaReference, VideoGenProvider, VideoGenRequest,
};
use meridian_providers::media_registry::{
    create_image_gen_provider, create_video_gen_provider, resolve_media_protocol,
};
use meridian_providers::mixins::AsyncMediaJobMixin;
use meridian_providers::models::evaluate_protocol_compliance;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agent::provider_secret_name;
use crate::agent::routing::{DeploymentCandidate, FailoverDecision, decide_failover, rank_enabled_candidates};
use crate::db::entity::media_generation::{self, MediaGenerationDomain, MediaGenerationSource, MediaGenerationStatus};
use crate::db::sea::cap::Db;
use crate::db::sea::ops;
use crate::db::sea::ops::media_generation::MediaGenerationProgressChangeset;
use crate::events::{EventBus, MediaGenerationUpdatedEvent};
use crate::media::compliance::{
    CleanOriginalCompliancePort, ExportWatermarkPolicy, GeneratedAssetMeta, MediaCompliancePort,
};
use crate::media::storage::MediaAssetStore;
use crate::provider::registry::CredentialKind;
use crate::secrets::{SecretName, SecretScope, SecretsManager};
use crate::util::now_ms;

/// Preference key holding the default logical model ID for `image_gen`.
pub const DEFAULT_IMAGE_MODEL_PREF: &str = "media.default_image_model_id";
/// Preference key holding the default logical model ID for `video_gen`.
pub const DEFAULT_VIDEO_MODEL_PREF: &str = "media.default_video_model_id";

/// Persisted JSON metadata for one stored asset in `media_generations.result_files_json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredMediaAsset {
    pub file_uri: String,
    pub relative_path: String,
    pub mime_type: String,
    pub bytes_len: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_seconds: Option<f64>,
    pub revised_prompt: Option<String>,
    pub produce_id: String,
    pub is_ai_generated: bool,
    pub watermark_snapshot: Option<String>,
}

/// Request parameters persisted as JSON in `media_generations.params_json` and
/// accepted by [`MediaRunner`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaGenerationParams {
    pub aspect_ratio: Option<String>,
    pub size: Option<String>,
    pub quality: Option<String>,
    pub count: Option<u32>,
    pub resolution: Option<String>,
    pub duration_seconds: Option<u32>,
    pub seed: Option<i64>,
    pub generate_audio: Option<bool>,
}

/// Input bundle passed to [`MediaRunner::submit`] or [`MediaRunner::run_sync`].
#[derive(Debug, Clone)]
pub struct MediaGenerationJobSpec {
    pub domain: MediaGenerationDomain,
    pub logical_model_id: Option<String>,
    pub prompt: String,
    pub reference_images: Vec<MediaReference>,
    pub params: MediaGenerationParams,
    pub source: MediaGenerationSource,
    pub conversation_id: Option<String>,
}

/// Hydrated generation record returned to IPC commands and built-in tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaGenerationRecord {
    pub id: String,
    pub capability_domain: String,
    pub logical_model_id: Option<String>,
    pub deployment_id: Option<String>,
    pub provider_id: Option<String>,
    pub remote_model_id: Option<String>,
    pub protocol_id: Option<String>,
    pub prompt: String,
    pub params_json: String,
    pub status: String,
    pub progress_pct: Option<i32>,
    pub upstream_job_id: Option<String>,
    pub assets: Vec<StoredMediaAsset>,
    pub error_message: Option<String>,
    pub conversation_id: Option<String>,
    pub source: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
}

impl MediaGenerationRecord {
    pub fn try_from_row(row: media_generation::Model) -> Result<Self, String> {
        let assets: Vec<StoredMediaAsset> = match row.result_files_json.as_deref() {
            None => Vec::new(),
            Some(raw) => serde_json::from_str(raw)
                .map_err(|err| format!("media generation `{}` has malformed result_files_json: {err}", row.id))?,
        };
        Ok(Self {
            id: row.id,
            capability_domain: row.capability_domain.as_str().to_string(),
            logical_model_id: row.logical_model_id,
            deployment_id: row.deployment_id,
            provider_id: row.provider_id,
            remote_model_id: row.remote_model_id,
            protocol_id: row.protocol_id,
            prompt: row.prompt,
            params_json: row.params_json,
            status: row.status.as_str().to_string(),
            progress_pct: row.progress_pct,
            upstream_job_id: row.upstream_job_id,
            assets,
            error_message: row.error_message,
            conversation_id: row.conversation_id,
            source: row.source.as_str().to_string(),
            created_at: row.created_at,
            updated_at: row.updated_at,
            completed_at: row.completed_at,
        })
    }
}

/// Resolved executable candidate deployment.
#[derive(Clone)]
struct ExecutableDeployment {
    candidate: DeploymentCandidate,
    base_url: String,
    credential: Credential,
}

/// Coordinates media generation execution, failover, cancellation, storage, and
/// compliance hooks.
pub struct MediaRunner {
    db: Db,
    secrets: Arc<SecretsManager>,
    events: EventBus,
    store: MediaAssetStore,
    compliance: Arc<dyn MediaCompliancePort>,
    active_cancels: Mutex<HashMap<String, CancellationToken>>,
}

impl MediaRunner {
    pub fn new(db: Db, secrets: Arc<SecretsManager>, events: EventBus, data_dir: &Path) -> Self {
        Self {
            db,
            secrets,
            events,
            store: MediaAssetStore::new(data_dir),
            compliance: Arc::new(CleanOriginalCompliancePort),
            active_cancels: Mutex::new(HashMap::new()),
        }
    }

    pub fn store(&self) -> &MediaAssetStore {
        &self.store
    }

    pub fn compliance(&self) -> &Arc<dyn MediaCompliancePort> {
        &self.compliance
    }

    /// Create a durable `media_generations` row in `queued` state and spawn the
    /// background execution task. Returns the initial record immediately so
    /// Studio UI can display a live progress card right away.
    pub async fn submit_background(
        self: &Arc<Self>,
        spec: MediaGenerationJobSpec,
    ) -> Result<MediaGenerationRecord, String> {
        let (logical_model_id, routing_policy, candidates) = self.resolve_candidates(&spec).await?;
        let record = self.create_queued_row(&spec, logical_model_id.clone()).await?;
        let gen_id = record.id.clone();

        let token = CancellationToken::new();
        self.active_cancels.lock().await.insert(gen_id.clone(), token.clone());
        self.emit_update(&record);

        let runner = Arc::clone(self);
        tokio::spawn(async move {
            let _ = runner
                .execute_job(&gen_id, &spec, routing_policy, candidates, token)
                .await;
            runner.active_cancels.lock().await.remove(&gen_id);
        });

        Ok(record)
    }

    /// Run a media generation job to completion in the caller's task (used by
    /// built-in `generate_image` / `generate_video` agent tools), observing
    /// `external_cancel` when the owning chat turn stops.
    pub async fn run_to_completion(
        &self,
        spec: MediaGenerationJobSpec,
        external_cancel: CancellationToken,
    ) -> Result<MediaGenerationRecord, String> {
        let (logical_model_id, routing_policy, candidates) = self.resolve_candidates(&spec).await?;
        let record = self.create_queued_row(&spec, logical_model_id.clone()).await?;
        let gen_id = record.id.clone();

        let token = external_cancel.child_token();
        self.active_cancels.lock().await.insert(gen_id.clone(), token.clone());
        self.emit_update(&record);

        let result = self
            .execute_job(&gen_id, &spec, routing_policy, candidates, token)
            .await;
        self.active_cancels.lock().await.remove(&gen_id);
        result
    }

    /// Request cancellation of an in-flight generation job.
    pub async fn cancel(&self, generation_id: &str) -> Result<bool, String> {
        if let Some(token) = self.active_cancels.lock().await.get(generation_id) {
            token.cancel();
            return Ok(true);
        }
        // If not currently running in memory, atomically check & mark queued/running stale rows cancelled.
        let now = now_ms();
        let cancelled = self
            .db
            .write(async |tx| {
                let Some(row) = ops::media_generation::get(tx, generation_id).await? else {
                    return Ok::<_, sea_orm::DbErr>(None);
                };
                if !matches!(
                    row.status,
                    MediaGenerationStatus::Queued | MediaGenerationStatus::Running
                ) {
                    return Ok(None);
                }
                let updated = ops::media_generation::update(
                    tx,
                    generation_id,
                    MediaGenerationProgressChangeset {
                        status: Some(MediaGenerationStatus::Cancelled),
                        updated_at: now,
                        completed_at: Some(Some(now)),
                        ..Default::default()
                    },
                )
                .await?;
                Ok(Some(updated))
            })
            .await
            .map_err(|err| format!("failed to cancel media generation: {err}"))?;
        if let Some(updated) = cancelled {
            let record = MediaGenerationRecord::try_from_row(updated)?;
            self.emit_update(&record);
            return Ok(true);
        }
        Ok(false)
    }

    pub async fn get(&self, generation_id: &str) -> Result<Option<MediaGenerationRecord>, String> {
        let row = ops::media_generation::get(&self.db, generation_id)
            .await
            .map_err(|err| format!("failed to load media generation: {err}"))?;
        row.map(MediaGenerationRecord::try_from_row).transpose()
    }

    pub async fn list(
        &self,
        domain: Option<MediaGenerationDomain>,
        limit: u64,
        offset: u64,
    ) -> Result<Vec<MediaGenerationRecord>, String> {
        let rows = ops::media_generation::list(&self.db, domain, limit, offset)
            .await
            .map_err(|err| format!("failed to list media generations: {err}"))?;
        rows.into_iter().map(MediaGenerationRecord::try_from_row).collect()
    }

    pub async fn delete(&self, generation_id: &str) -> Result<bool, String> {
        let _ = self.cancel(generation_id).await;
        let deleted = self
            .db
            .write(async |tx| ops::media_generation::delete(tx, generation_id).await)
            .await
            .map_err(|err| format!("failed to delete media generation: {err}"))?;
        let _ = self.store.delete_generation(generation_id).await;
        Ok(deleted)
    }

    /// Export one generated asset through [`MediaCompliancePort::prepare_export`].
    pub async fn export_asset(
        &self,
        generation_id: &str,
        asset_index: usize,
        policy: ExportWatermarkPolicy,
    ) -> Result<(Vec<u8>, StoredMediaAsset), String> {
        let record = self
            .get(generation_id)
            .await?
            .ok_or_else(|| format!("media generation `{generation_id}` not found"))?;
        let asset = record
            .assets
            .get(asset_index)
            .cloned()
            .ok_or_else(|| format!("asset index {asset_index} out of range for `{generation_id}`"))?;
        let abs_path = self.store.resolve_relative(&asset.relative_path)?;
        let meta = GeneratedAssetMeta {
            produce_id: asset.produce_id.clone(),
            is_ai_generated: asset.is_ai_generated,
            watermark_snapshot: asset.watermark_snapshot.clone(),
            provider_id: record.provider_id.unwrap_or_default(),
            model_id: record.remote_model_id.unwrap_or_default(),
            created_at: record.created_at,
        };
        let bytes = self.compliance.prepare_export(&abs_path, &meta, policy).await?;
        Ok((bytes, asset))
    }

    async fn create_queued_row(
        &self,
        spec: &MediaGenerationJobSpec,
        logical_model_id: Option<String>,
    ) -> Result<MediaGenerationRecord, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let params_json =
            serde_json::to_string(&spec.params).map_err(|err| format!("failed to serialize media params: {err}"))?;
        let row = media_generation::Model {
            id: id.clone(),
            capability_domain: spec.domain,
            logical_model_id,
            deployment_id: None,
            provider_id: None,
            remote_model_id: None,
            protocol_id: None,
            prompt: spec.prompt.clone(),
            params_json,
            status: MediaGenerationStatus::Queued,
            progress_pct: Some(0),
            upstream_job_id: None,
            result_files_json: None,
            error_message: None,
            conversation_id: spec.conversation_id.clone(),
            source: spec.source,
            created_at: now,
            updated_at: now,
            completed_at: None,
        };
        let inserted = self
            .db
            .write(async |tx| ops::media_generation::insert(tx, row).await)
            .await
            .map_err(|err| format!("failed to persist media generation job: {err}"))?;
        MediaGenerationRecord::try_from_row(inserted)
    }

    async fn resolve_candidates(
        &self,
        spec: &MediaGenerationJobSpec,
    ) -> Result<(Option<String>, RoutingPolicy, Vec<ExecutableDeployment>), String> {
        let domain = match spec.domain {
            MediaGenerationDomain::ImageGen => CapabilityDomain::ImageGen,
            MediaGenerationDomain::VideoGen => CapabilityDomain::VideoGen,
        };
        let pref_key = match spec.domain {
            MediaGenerationDomain::ImageGen => DEFAULT_IMAGE_MODEL_PREF,
            MediaGenerationDomain::VideoGen => DEFAULT_VIDEO_MODEL_PREF,
        };

        let requested_id = match spec.logical_model_id.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(id) => Some(id.to_string()),
            None => ops::preference::get(&self.db, pref_key)
                .await
                .map_err(|err| format!("failed to read `{pref_key}` preference: {err}"))?,
        };

        let (models_with_deps, providers) = self
            .db
            .read(async |snap| {
                let models = ops::model_profile::list_with_deployments(snap, false).await?;
                let providers = ops::provider::list(snap).await?;
                Ok::<_, sea_orm::DbErr>((models, providers))
            })
            .await
            .map_err(|err| format!("failed to load media models and providers: {err}"))?;

        let providers_by_id: HashMap<String, _> = providers.into_iter().map(|p| (p.id.clone(), p)).collect();

        let matched_model = if let Some(ref target_id) = requested_id {
            models_with_deps
                .into_iter()
                .find(|(m, _)| m.id == *target_id || m.canonical_id.as_deref() == Some(target_id.as_str()))
                .ok_or_else(|| format!("logical media model `{target_id}` not found"))?
        } else {
            models_with_deps
                .into_iter()
                .find(|(m, deps)| m.capability_domain == domain && deps.iter().any(|d| d.is_enabled.get()))
                .ok_or_else(|| {
                    format!(
                        "no enabled `{}` model is configured. Add one in Settings > Models first.",
                        domain.as_str()
                    )
                })?
        };

        let (profile, deployments) = matched_model;
        if profile.capability_domain != domain {
            return Err(format!(
                "model `{}` belongs to domain `{}`, expected `{}`",
                profile.name,
                profile.capability_domain.as_str(),
                domain.as_str()
            ));
        }

        let mut executable = Vec::new();
        for dep in deployments {
            if !dep.is_enabled.get() {
                continue;
            }
            let Some(provider) = providers_by_id.get(&dep.provider_id) else {
                continue;
            };
            if !provider.is_enabled.get() {
                continue;
            }
            let protocol_id = resolve_media_protocol(
                domain,
                dep.protocol_override.as_deref(),
                provider.provider_type.as_str(),
                provider.catalog_id.as_deref(),
                &dep.model_id,
            )?;
            let credential = self.load_media_credential(provider, protocol_id)?;
            let compliance = evaluate_protocol_compliance(&dep.model_id, protocol_id);
            executable.push(ExecutableDeployment {
                candidate: DeploymentCandidate {
                    deployment_id: dep.id.clone(),
                    provider_id: provider.id.clone(),
                    provider_name: provider.name.clone(),
                    remote_model_id: dep.model_id.clone(),
                    protocol_id: protocol_id.to_string(),
                    is_enabled: true,
                    priority: dep.priority,
                    compliance,
                },
                base_url: provider.base_url.clone(),
                credential,
            });
        }

        if executable.is_empty() {
            return Err(format!(
                "logical model `{}` has no enabled provider deployments with valid credentials",
                profile.name
            ));
        }

        // Order strictly by `rank_enabled_candidates` (official standard first, then priority).
        let raw_candidates: Vec<DeploymentCandidate> = executable.iter().map(|e| e.candidate.clone()).collect();
        let ranked = rank_enabled_candidates(&raw_candidates);
        let mut by_dep: HashMap<String, ExecutableDeployment> = executable
            .into_iter()
            .map(|e| (e.candidate.deployment_id.clone(), e))
            .collect();
        let ordered: Vec<ExecutableDeployment> = ranked
            .into_iter()
            .filter_map(|c| by_dep.remove(&c.deployment_id))
            .collect();

        Ok((Some(profile.id), profile.routing_policy, ordered))
    }

    fn load_media_credential(
        &self,
        provider: &crate::db::entity::provider::Model,
        protocol_id: &str,
    ) -> Result<Credential, String> {
        let secret_key = provider_secret_name(&provider.id);
        let secret_name = SecretName::new(&secret_key)
            .map_err(|err| format!("invalid provider secret name `{secret_key}`: {err}"))?;
        let stored = self
            .secrets
            .get(&SecretScope::Global, &secret_name)
            .map_err(|err| format!("failed to read credential for provider `{}`: {err}", provider.name))?;

        let raw_secret = match stored.filter(|s| !s.trim().is_empty()) {
            Some(s) => s,
            None => {
                // Fallback to standalone vendor service secret (e.g. SERVICE_KLING_KEY) if configured.
                let fallback_key = match protocol_id {
                    "volcengine_seedream" | "volcengine_seedance" => Some("SERVICE_VOLCENGINE_ARK_KEY"),
                    "kling_video" => Some("SERVICE_KLING_KEY"),
                    "minimax_video" => Some("SERVICE_MINIMAX_KEY"),
                    "vidu_video" => Some("SERVICE_VIDU_KEY"),
                    "bailian_wan_video" => Some("SERVICE_BAILIAN_KEY"),
                    _ => None,
                };
                if let Some(name) = fallback_key
                    && let Ok(secret_id) = SecretName::new(name)
                    && let Ok(Some(value)) = self.secrets.get(&SecretScope::Global, &secret_id)
                    && !value.trim().is_empty()
                {
                    value
                } else {
                    return Err(format!("API key is missing for provider `{}`", provider.name));
                }
            }
        };

        match provider.credential_kind {
            CredentialKind::GoogleVertexServiceAccount => Ok(Credential::GoogleVertexServiceAccount(raw_secret)),
            _ => Ok(Credential::ApiKey(raw_secret)),
        }
    }

    async fn execute_job(
        &self,
        generation_id: &str,
        spec: &MediaGenerationJobSpec,
        routing_policy: RoutingPolicy,
        deployments: Vec<ExecutableDeployment>,
        cancel: CancellationToken,
    ) -> Result<MediaGenerationRecord, String> {
        let all_candidates: Vec<DeploymentCandidate> = deployments.iter().map(|d| d.candidate.clone()).collect();
        let mut remaining_pool = all_candidates.clone();
        let by_dep: HashMap<String, ExecutableDeployment> = deployments
            .into_iter()
            .map(|d| (d.candidate.deployment_id.clone(), d))
            .collect();

        let mut current = match remaining_pool.first().cloned() {
            Some(c) => c,
            None => return Err("no candidate deployment available".into()),
        };

        loop {
            if cancel.is_cancelled() {
                return self.finish_cancelled(generation_id).await;
            }
            let exec = by_dep
                .get(&current.deployment_id)
                .ok_or_else(|| "missing executable deployment".to_string())?;

            let _ = self
                .update_progress(
                    generation_id,
                    MediaGenerationProgressChangeset {
                        deployment_id: Some(Some(current.deployment_id.clone())),
                        provider_id: Some(Some(current.provider_id.clone())),
                        remote_model_id: Some(Some(current.remote_model_id.clone())),
                        protocol_id: Some(Some(current.protocol_id.clone())),
                        status: Some(MediaGenerationStatus::Running),
                        progress_pct: Some(Some(5)),
                        updated_at: now_ms(),
                        ..Default::default()
                    },
                )
                .await;

            let attempt = match spec.domain {
                MediaGenerationDomain::ImageGen => {
                    let provider = create_image_gen_provider(&current.protocol_id, &exec.base_url, &exec.credential)
                        .map_err(MediaGenError::InvalidRequest);
                    match provider {
                        Ok(p) => {
                            self.run_image_attempt(p.as_ref(), &current.remote_model_id, spec, &cancel)
                                .await
                        }
                        Err(e) => Err(e),
                    }
                }
                MediaGenerationDomain::VideoGen => {
                    let provider = create_video_gen_provider(&current.protocol_id, &exec.base_url, &exec.credential)
                        .map_err(MediaGenError::InvalidRequest);
                    match provider {
                        Ok(p) => {
                            self.run_video_attempt(generation_id, p.as_ref(), &current.remote_model_id, spec, &cancel)
                                .await
                        }
                        Err(e) => Err(e),
                    }
                }
            };

            match attempt {
                Ok(generated) => {
                    return self
                        .finish_succeeded(generation_id, &current.provider_id, &current.remote_model_id, generated)
                        .await;
                }
                Err(MediaGenError::Cancelled) => {
                    return self.finish_cancelled(generation_id).await;
                }
                Err(err) => {
                    let decision = decide_failover(
                        routing_policy,
                        SwitchImpact::Stateless,
                        &current.deployment_id,
                        &remaining_pool,
                        err.allows_failover(),
                    );
                    match decision {
                        FailoverDecision::AutoSwitch { next } => {
                            tracing::warn!(
                                generation_id = %generation_id,
                                failed_deployment = %current.deployment_id,
                                next_deployment = %next.deployment_id,
                                error = %err,
                                "stateless media failover switching to next priority deployment"
                            );
                            remaining_pool.retain(|c| c.deployment_id != current.deployment_id);
                            current = next;
                            continue;
                        }
                        _ => {
                            let message = err.to_string();
                            let _ = self.finish_failed(generation_id, &message).await;
                            return Err(message);
                        }
                    }
                }
            }
        }
    }

    async fn run_image_attempt(
        &self,
        provider: &dyn ImageGenProvider,
        remote_model_id: &str,
        spec: &MediaGenerationJobSpec,
        cancel: &CancellationToken,
    ) -> Result<Vec<GeneratedMedia>, MediaGenError> {
        let mut req = ImageGenRequest::new(remote_model_id, &spec.prompt);
        req.reference_images = spec.reference_images.clone();
        req.aspect_ratio = spec.params.aspect_ratio.clone();
        req.size = spec.params.size.clone();
        req.quality = spec.params.quality.clone();
        req.count = spec.params.count;
        req.seed = spec.params.seed;
        // D-001: always request clean originals from upstream providers.
        req.watermark = false;

        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(MediaGenError::Cancelled),
            res = provider.generate_image(&req) => res?,
        };
        Ok(resp.images)
    }

    async fn run_video_attempt(
        &self,
        generation_id: &str,
        provider: &dyn VideoGenProvider,
        remote_model_id: &str,
        spec: &MediaGenerationJobSpec,
        cancel: &CancellationToken,
    ) -> Result<Vec<GeneratedMedia>, MediaGenError> {
        let mut req = VideoGenRequest::new(remote_model_id, &spec.prompt);
        req.reference_images = spec.reference_images.clone();
        req.aspect_ratio = spec.params.aspect_ratio.clone();
        req.resolution = spec.params.resolution.clone();
        req.duration_seconds = spec.params.duration_seconds;
        req.seed = spec.params.seed;
        req.generate_audio = spec.params.generate_audio;
        // D-001: always request clean originals from upstream providers.
        req.watermark = false;

        let handle = tokio::select! {
            _ = cancel.cancelled() => return Err(MediaGenError::Cancelled),
            res = provider.submit_video_job(&req) => res?,
        };

        let _ = self
            .update_progress(
                generation_id,
                MediaGenerationProgressChangeset {
                    upstream_job_id: Some(Some(handle.job_id.clone())),
                    progress_pct: Some(Some(10)),
                    updated_at: now_ms(),
                    ..Default::default()
                },
            )
            .await;

        let poller = AsyncMediaJobMixin::default();
        let db = self.db.clone();
        let events = self.events.clone();
        let gen_id = generation_id.to_string();
        let domain_str = spec.domain.as_str().to_string();

        let resp = poller
            .poll_until_done(
                provider,
                &handle,
                move |pct| {
                    if let Some(pct) = pct {
                        let _ = events.emit_media_generation_updated(&MediaGenerationUpdatedEvent {
                            generation_id: gen_id.clone(),
                            capability_domain: domain_str.clone(),
                            status: "running".into(),
                            progress_pct: Some(i32::from(pct)),
                            error_message: None,
                        });
                        let db = db.clone();
                        let gen_id = gen_id.clone();
                        tokio::spawn(async move {
                            let _ = db
                                .write(async |tx| {
                                    ops::media_generation::update(
                                        tx,
                                        &gen_id,
                                        MediaGenerationProgressChangeset {
                                            progress_pct: Some(Some(i32::from(pct))),
                                            updated_at: now_ms(),
                                            ..Default::default()
                                        },
                                    )
                                    .await
                                })
                                .await;
                        });
                    }
                },
                cancel.cancelled(),
            )
            .await?;

        Ok(resp.videos)
    }

    async fn finish_succeeded(
        &self,
        generation_id: &str,
        provider_id: &str,
        remote_model_id: &str,
        generated: Vec<GeneratedMedia>,
    ) -> Result<MediaGenerationRecord, String> {
        let now = now_ms();
        let mut stored_assets = Vec::with_capacity(generated.len());
        for (idx, item) in generated.into_iter().enumerate() {
            let (abs_path, relative_path, file_uri) = self
                .store
                .write_original(generation_id, idx, &item.mime_type, &item.bytes)
                .await?;
            let produce_id = uuid::Uuid::new_v4().to_string();
            let raw_meta = GeneratedAssetMeta {
                produce_id,
                is_ai_generated: true,
                watermark_snapshot: None,
                provider_id: provider_id.to_string(),
                model_id: remote_model_id.to_string(),
                created_at: now,
            };
            let stamped = self.compliance.on_generated_original(&abs_path, &raw_meta).await?;
            stored_assets.push(StoredMediaAsset {
                file_uri,
                relative_path,
                mime_type: item.mime_type,
                bytes_len: item.bytes.len() as u64,
                width: item.width,
                height: item.height,
                duration_seconds: item.duration_seconds,
                revised_prompt: item.revised_prompt,
                produce_id: stamped.produce_id,
                is_ai_generated: stamped.is_ai_generated,
                watermark_snapshot: stamped.watermark_snapshot,
            });
        }

        let result_files_json = serde_json::to_string(&stored_assets)
            .map_err(|err| format!("failed to serialize stored media assets: {err}"))?;
        let record = self
            .update_progress(
                generation_id,
                MediaGenerationProgressChangeset {
                    status: Some(MediaGenerationStatus::Succeeded),
                    progress_pct: Some(Some(100)),
                    result_files_json: Some(Some(result_files_json)),
                    error_message: Some(None),
                    updated_at: now,
                    completed_at: Some(Some(now)),
                    ..Default::default()
                },
            )
            .await?;
        Ok(record)
    }

    async fn finish_failed(&self, generation_id: &str, message: &str) -> Result<MediaGenerationRecord, String> {
        let now = now_ms();
        self.update_progress(
            generation_id,
            MediaGenerationProgressChangeset {
                status: Some(MediaGenerationStatus::Failed),
                error_message: Some(Some(message.to_string())),
                updated_at: now,
                completed_at: Some(Some(now)),
                ..Default::default()
            },
        )
        .await
    }

    async fn finish_cancelled(&self, generation_id: &str) -> Result<MediaGenerationRecord, String> {
        let now = now_ms();
        self.update_progress(
            generation_id,
            MediaGenerationProgressChangeset {
                status: Some(MediaGenerationStatus::Cancelled),
                updated_at: now,
                completed_at: Some(Some(now)),
                ..Default::default()
            },
        )
        .await
    }

    async fn update_progress(
        &self,
        generation_id: &str,
        changes: MediaGenerationProgressChangeset,
    ) -> Result<MediaGenerationRecord, String> {
        let updated = self
            .db
            .write(async |tx| ops::media_generation::update(tx, generation_id, changes).await)
            .await
            .map_err(|err| format!("failed to update media generation: {err}"))?;
        let record = MediaGenerationRecord::try_from_row(updated)?;
        self.emit_update(&record);
        Ok(record)
    }

    fn emit_update(&self, record: &MediaGenerationRecord) {
        let _ = self.events.emit_media_generation_updated(&MediaGenerationUpdatedEvent {
            generation_id: record.id.clone(),
            capability_domain: record.capability_domain.clone(),
            status: record.status.clone(),
            progress_pct: record.progress_pct,
            error_message: record.error_message.clone(),
        });
    }
}

/// Pure stateless failover harness used by unit & mutation tests to verify
/// `SwitchImpact::Stateless` multi-deployment failover without live network.
#[cfg(test)]
pub(crate) async fn execute_stateless_failover<T, F, Fut>(
    routing_policy: RoutingPolicy,
    candidates: &[DeploymentCandidate],
    mut run_candidate: F,
) -> Result<(DeploymentCandidate, T), MediaGenError>
where
    F: FnMut(DeploymentCandidate) -> Fut,
    Fut: std::future::Future<Output = Result<T, MediaGenError>>,
{
    let mut pool = rank_enabled_candidates(candidates);
    let mut current = pool
        .first()
        .cloned()
        .ok_or_else(|| MediaGenError::InvalidRequest("no enabled deployment".into()))?;

    loop {
        match run_candidate(current.clone()).await {
            Ok(value) => return Ok((current, value)),
            Err(err) => {
                match decide_failover(
                    routing_policy,
                    SwitchImpact::Stateless,
                    &current.deployment_id,
                    &pool,
                    err.allows_failover(),
                ) {
                    FailoverDecision::AutoSwitch { next } => {
                        pool.retain(|c| c.deployment_id != current.deployment_id);
                        current = next;
                    }
                    _ => return Err(err),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use meridian_provider_sdk::domain::ProtocolCompliance;

    use super::*;

    fn dep(id: &str, priority: i32) -> DeploymentCandidate {
        DeploymentCandidate {
            deployment_id: id.into(),
            provider_id: format!("prov-{id}"),
            provider_name: format!("Provider {id}"),
            remote_model_id: "gpt-image-1".into(),
            protocol_id: "openai_images".into(),
            is_enabled: true,
            priority,
            compliance: ProtocolCompliance::Standard,
        }
    }

    #[tokio::test]
    async fn stateless_media_failover_switches_on_retryable_and_stops_on_content_filtered() {
        let candidates = vec![dep("primary", 0), dep("backup", 1)];

        // Retryable 429 on primary automatically fails over to backup
        let mut tried = Vec::new();
        let (winner, out) = execute_stateless_failover(RoutingPolicy::AutoFailoverStateless, &candidates, |c| {
            tried.push(c.deployment_id.clone());
            async move {
                if c.deployment_id == "primary" {
                    Err(MediaGenError::Retryable("HTTP 429 rate limited".into()))
                } else {
                    Ok("clean-original-bytes")
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(tried, vec!["primary", "backup"]);
        assert_eq!(winner.deployment_id, "backup");
        assert_eq!(out, "clean-original-bytes");

        // ContentFiltered MUST NOT silently jump to another provider
        let mut tried_safety = Vec::new();
        let err = execute_stateless_failover(RoutingPolicy::AutoFailoverStateless, &candidates, |c| {
            tried_safety.push(c.deployment_id.clone());
            async move { Err::<(), _>(MediaGenError::ContentFiltered("safety rejected".into())) }
        })
        .await
        .unwrap_err();
        assert_eq!(tried_safety, vec!["primary"]);
        assert!(matches!(err, MediaGenError::ContentFiltered(_)));
    }
}
