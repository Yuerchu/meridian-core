//! `emoji_embeddings`: persisted vector representations of confirmed stickers
//! computed by the configured on-device embedding model (`emoji.embedder`).
//!
//! Storing vectors on disk decouples indexing from `send_sticker` so a restart
//! reconciles only missing or modified stickers instead of re-embedding the
//! entire library synchronously on a turn.

use sea_orm::sea_query::ForeignKeyAction::Cascade;
use sea_orm::sea_query::IndexOrder::Asc;

use super::{Object, big_int, blob, check, fk, index, integer, render_sqlite, t, table, text};

/// The statements SQLite runs for this migration, in order.
pub fn sqlite_statements() -> Vec<String> {
    render_sqlite(&objects())
}

#[rustfmt::skip]
pub(super) fn objects() -> Vec<Object> {
    vec![
        t(table("emoji_embeddings")
            .col(text("emoji_id").not_null().primary_key())
            .col(text("model_fingerprint").not_null())
            .col(text("source_hash").not_null())
            .col(integer("dims").not_null())
            .col(blob("vector").not_null())
            .col(big_int("updated_at").not_null())
            .foreign_key(&mut fk(&["emoji_id"], "emojis", &["id"], Cascade))
            .check(check("dims > 0"))
        ),
        index(
            "idx_emoji_embeddings_model",
            "emoji_embeddings",
            &[("model_fingerprint", Asc)],
            false,
            None,
        ),
    ]
}
