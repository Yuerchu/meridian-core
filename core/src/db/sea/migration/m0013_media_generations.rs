//! `media_generations`: persistent history of image and video generation jobs
//! initiated from either the Creative Studio page (`studio`) or the built-in
//! agent tools (`agent_tool`).
//!
//! Clean original generated binaries live on disk under
//! `<app_data>/media_generations/<generation_id>/`; this table stores job
//! metadata, provenance (`logical_model_id`, `deployment_id`, `provider_id`,
//! `protocol_id`), live progress, and asset manifest JSON (`result_files_json`)
//! including compliance metadata (`produce_id`, `is_ai_generated`,
//! `watermark_snapshot`).

use sea_orm::sea_query::ForeignKeyAction::SetNull;
use sea_orm::sea_query::IndexOrder::{Asc, Desc};

use super::{Object, big_int, check, fk, index, integer, render_sqlite, t, table, text};

/// The statements SQLite runs for this migration, in order.
pub fn sqlite_statements() -> Vec<String> {
    render_sqlite(&objects())
}

#[rustfmt::skip]
pub(super) fn objects() -> Vec<Object> {
    vec![
        t(table("media_generations")
            .col(text("id").not_null().primary_key())
            .col(text("capability_domain").not_null())
            .col(text("logical_model_id"))
            .col(text("deployment_id"))
            .col(text("provider_id"))
            .col(text("remote_model_id"))
            .col(text("protocol_id"))
            .col(text("prompt").not_null())
            .col(text("params_json").not_null())
            .col(text("status").not_null())
            .col(integer("progress_pct"))
            .col(text("upstream_job_id"))
            .col(text("result_files_json"))
            .col(text("error_message"))
            .col(text("conversation_id"))
            .col(text("source").not_null())
            .col(big_int("created_at").not_null())
            .col(big_int("updated_at").not_null())
            .col(big_int("completed_at"))
            .foreign_key(&mut fk(&["logical_model_id"], "model_profiles", &["id"], SetNull))
            .foreign_key(&mut fk(&["deployment_id"], "model_configs", &["id"], SetNull))
            .foreign_key(&mut fk(&["provider_id"], "providers", &["id"], SetNull))
            .foreign_key(&mut fk(&["conversation_id"], "conversations", &["id"], SetNull))
            .check(check(r"capability_domain IN ('image_gen', 'video_gen')"))
            .check(check(r"status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')"))
            .check(check(r"source IN ('studio', 'agent_tool')"))
        ),
        index(
            "idx_media_generations_created_at",
            "media_generations",
            &[("created_at", Desc)],
            false,
            None,
        ),
        index(
            "idx_media_generations_domain_created",
            "media_generations",
            &[("capability_domain", Asc), ("created_at", Desc)],
            false,
            None,
        ),
        index(
            "idx_media_generations_conversation",
            "media_generations",
            &[("conversation_id", Asc)],
            false,
            None,
        ),
    ]
}
