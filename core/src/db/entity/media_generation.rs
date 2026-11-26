//! `media_generations`: persistent image and video generation jobs.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, checked_text_enum};

checked_text_enum!(
    /// Capability domain of a media generation job.
    MediaGenerationDomain {
        ImageGen = "image_gen",
        VideoGen = "video_gen",
    }
);

checked_text_enum!(
    /// Lifecycle status of a media generation job.
    MediaGenerationStatus {
        Queued = "queued",
        Running = "running",
        Succeeded = "succeeded",
        Failed = "failed",
        Cancelled = "cancelled",
    }
);

checked_text_enum!(
    /// Entry point that created a media generation job.
    MediaGenerationSource {
        Studio = "studio",
        AgentTool = "agent_tool",
    }
);

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "media_generations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub capability_domain: MediaGenerationDomain,
    pub logical_model_id: Option<String>,
    pub deployment_id: Option<String>,
    pub provider_id: Option<String>,
    pub remote_model_id: Option<String>,
    pub protocol_id: Option<String>,
    pub prompt: String,
    pub params_json: String,
    pub status: MediaGenerationStatus,
    pub progress_pct: Option<i32>,
    pub upstream_job_id: Option<String>,
    pub result_files_json: Option<String>,
    pub error_message: Option<String>,
    pub conversation_id: Option<String>,
    pub source: MediaGenerationSource,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
    pub completed_at: Option<EpochMs>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::model_profile::Entity",
        from = "Column::LogicalModelId",
        to = "super::model_profile::Column::Id",
        on_delete = "SetNull"
    )]
    ModelProfile,
    #[sea_orm(
        belongs_to = "super::model_config::Entity",
        from = "Column::DeploymentId",
        to = "super::model_config::Column::Id",
        on_delete = "SetNull"
    )]
    ModelConfig,
    #[sea_orm(
        belongs_to = "super::provider::Entity",
        from = "Column::ProviderId",
        to = "super::provider::Column::Id",
        on_delete = "SetNull"
    )]
    Provider,
    #[sea_orm(
        belongs_to = "super::conversation::Entity",
        from = "Column::ConversationId",
        to = "super::conversation::Column::Id",
        on_delete = "SetNull"
    )]
    Conversation,
}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use sea_orm::Iterable;

    use super::*;
    use crate::db::entity::allowed_by_check;

    fn stored<E: Iterable + ActiveEnum<Value = String> + Copy + std::fmt::Debug + PartialEq>(
        as_str: impl Fn(E) -> &'static str,
    ) -> BTreeSet<String> {
        E::iter()
            .map(|variant| {
                let db = variant.to_value();
                assert_eq!(as_str(variant), db);
                assert_eq!(E::try_from_value(&db).unwrap(), variant);
                db
            })
            .collect()
    }

    #[test]
    fn stored_spellings_match_check_constraints() {
        assert_eq!(
            stored::<MediaGenerationDomain>(MediaGenerationDomain::as_str),
            allowed_by_check("media_generations", "capability_domain")
        );
        assert_eq!(
            stored::<MediaGenerationStatus>(MediaGenerationStatus::as_str),
            allowed_by_check("media_generations", "status")
        );
        assert_eq!(
            stored::<MediaGenerationSource>(MediaGenerationSource::as_str),
            allowed_by_check("media_generations", "source")
        );
    }
}
