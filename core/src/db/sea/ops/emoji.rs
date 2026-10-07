//! Reading and writing `emojis`.
//!
//! `message_stickers` is not here: it references `messages`, which is still
//! Diesel's, so linking a sticker to a message stays in `db::ops::emoji`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{
    ColumnTrait, Condition, DbErr, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect,
};

use crate::db::entity::emoji;
use crate::db::entity::emoji::{EmojiSemanticStatus, EmojiSource};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::{EpochMs, Json};

pub async fn list_by_pack(db: &impl Read, pack_id: &str) -> Result<Vec<emoji::Model>, DbErr> {
    emoji::Entity::find()
        .filter(emoji::Column::PackId.eq(pack_id))
        .order_by_asc(emoji::Column::SortOrder)
        .order_by_asc(emoji::Column::Id)
        .all(db.conn()?)
        .await
}

pub async fn get_emoji(db: &impl Read, id: &str) -> Result<Option<emoji::Model>, DbErr> {
    emoji::Entity::find_by_id(id).one(db.conn()?).await
}

fn not_found(id: &str) -> DbErr {
    DbErr::RecordNotFound(format!("sticker `{id}`"))
}

async fn existing(tx: &WriteTx, id: &str) -> Result<emoji::Model, DbErr> {
    get_emoji(tx, id).await?.ok_or_else(|| not_found(id))
}

/// Inserts the row the caller built and reads it back.
pub async fn create_emoji(tx: &WriteTx, model: emoji::Model) -> Result<emoji::Model, DbErr> {
    let id = model.id.clone();
    emoji::Entity::insert(model.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    existing(tx, &id).await
}

/// Sets columns on one row by expression and reads it back; `RecordNotFound`
/// when there is no such row.
async fn set(tx: &WriteTx, id: &str, columns: Vec<(emoji::Column, Expr)>) -> Result<emoji::Model, DbErr> {
    let mut update = emoji::Entity::update_many().filter(emoji::Column::Id.eq(id));
    for (column, value) in columns {
        update = update.col_expr(column, value);
    }
    if update.exec(tx.conn()?).await?.rows_affected == 0 {
        return Err(not_found(id));
    }
    existing(tx, id).await
}

pub async fn rename_emoji(tx: &WriteTx, id: &str, new_name: &str) -> Result<emoji::Model, DbErr> {
    set(tx, id, vec![(emoji::Column::Name, Expr::value(new_name))]).await
}

/// How many rows went: 0 for a sticker already gone. A sticker some message
/// still shows is refused by `message_stickers`' `ON DELETE RESTRICT`, as an
/// error.
pub async fn delete_emoji(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    Ok(emoji::Entity::delete_by_id(id).exec(tx.conn()?).await?.rows_affected)
}

/// Name or tags containing `query`, at most 50.
pub async fn search_emojis(db: &impl Read, query: &str) -> Result<Vec<emoji::Model>, DbErr> {
    emoji::Entity::find()
        .filter(
            Condition::any()
                .add(emoji::Column::Name.contains(query))
                .add(emoji::Column::Tags.contains(query)),
        )
        .order_by_asc(emoji::Column::SortOrder)
        .order_by_asc(emoji::Column::Id)
        .limit(50)
        .all(db.conn()?)
        .await
}

/// The stickers an assistant can send from these packs: confirmed, and not a
/// Lottie animation, which no provider can be shown.
pub async fn list_confirmed_for_packs(db: &impl Read, pack_ids: &[String]) -> Result<Vec<emoji::Model>, DbErr> {
    emoji::Entity::find()
        .filter(emoji::Column::PackId.is_in(pack_ids.iter().map(String::as_str)))
        .filter(emoji::Column::SemanticStatus.eq(EmojiSemanticStatus::Confirmed))
        .filter(emoji::Column::FileFormat.ne("lottie"))
        .order_by_asc(emoji::Column::PackId)
        .order_by_asc(emoji::Column::SortOrder)
        .order_by_asc(emoji::Column::Id)
        .all(db.conn()?)
        .await
}

/// A pack's stickers that still lack a confirmed meaning, most recently and
/// most often seen first.
pub async fn list_candidates(db: &impl Read, pack_id: &str) -> Result<Vec<emoji::Model>, DbErr> {
    emoji::Entity::find()
        .filter(emoji::Column::PackId.eq(pack_id))
        .filter(emoji::Column::SemanticStatus.ne(EmojiSemanticStatus::Confirmed))
        .order_by_desc(emoji::Column::LastSeenAt)
        .order_by_desc(emoji::Column::SeenCount)
        .order_by_asc(emoji::Column::Id)
        .all(db.conn()?)
        .await
}

/// A model's guess at a sticker's meaning, kept beside the confirmed fields
/// until a person accepts it.
pub async fn update_suggestion(tx: &WriteTx, id: &str, name: &str, tags: Option<&str>) -> Result<emoji::Model, DbErr> {
    set(
        tx,
        id,
        vec![
            (emoji::Column::SuggestedName, Expr::value(name)),
            (emoji::Column::SuggestedTags, Expr::value(tags)),
            (
                emoji::Column::SemanticStatus,
                Expr::value(EmojiSemanticStatus::Suggested),
            ),
        ],
    )
    .await
}

/// A person's word on what a sticker means: it becomes sendable, and any
/// pending suggestion is cleared.
pub async fn confirm_semantics(tx: &WriteTx, id: &str, name: &str, tags: Option<&str>) -> Result<emoji::Model, DbErr> {
    set(
        tx,
        id,
        vec![
            (emoji::Column::Name, Expr::value(name)),
            (emoji::Column::Tags, Expr::value(tags)),
            (emoji::Column::SuggestedName, Expr::value(Option::<String>::None)),
            (emoji::Column::SuggestedTags, Expr::value(Option::<String>::None)),
            (
                emoji::Column::SemanticStatus,
                Expr::value(EmojiSemanticStatus::Confirmed),
            ),
        ],
    )
    .await
}

pub async fn find_by_source_key(
    db: &impl Read,
    pack_id: &str,
    source: EmojiSource,
    source_key: &str,
) -> Result<Option<emoji::Model>, DbErr> {
    emoji::Entity::find()
        .filter(emoji::Column::PackId.eq(pack_id))
        .filter(emoji::Column::Source.eq(source))
        .filter(emoji::Column::SourceKey.eq(source_key))
        .one(db.conn()?)
        .await
}

/// Whether a pack already has a sticker of this name; names are unique per
/// pack.
pub async fn name_in_use(db: &impl Read, pack_id: &str, name: &str) -> Result<bool, DbErr> {
    Ok(emoji::Entity::find()
        .filter(emoji::Column::PackId.eq(pack_id))
        .filter(emoji::Column::Name.eq(name))
        .count(db.conn()?)
        .await?
        > 0)
}

/// Count a sighting: one more, and when.
pub async fn mark_seen(tx: &WriteTx, id: &str, now: EpochMs) -> Result<emoji::Model, DbErr> {
    set(
        tx,
        id,
        vec![
            (emoji::Column::SeenCount, Expr::col(emoji::Column::SeenCount).add(1)),
            (emoji::Column::LastSeenAt, Expr::value(Some(now))),
        ],
    )
    .await
}

/// The image of a sticker first seen without one, once it has downloaded.
pub async fn attach_captured_media(
    tx: &WriteTx,
    id: &str,
    file_name: &str,
    file_format: &str,
    file_size: i64,
    native_payload: Json<serde_json::Map<String, serde_json::Value>>,
) -> Result<emoji::Model, DbErr> {
    set(
        tx,
        id,
        vec![
            (emoji::Column::FileName, Expr::value(file_name)),
            (emoji::Column::FileFormat, Expr::value(file_format)),
            (emoji::Column::FileSize, Expr::value(file_size)),
            (emoji::Column::NativePayload, Expr::value(native_payload)),
        ],
    )
    .await
}

pub async fn count_by_pack(db: &impl Read, pack_id: &str) -> Result<u64, DbErr> {
    emoji::Entity::find()
        .filter(emoji::Column::PackId.eq(pack_id))
        .count(db.conn()?)
        .await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::entity::emoji_pack::EmojiPackKind;
    use crate::db::sea::cap::Db;
    use crate::db::sea::ops::emoji_pack::tests::{insert as insert_pack, pack};
    use crate::db::sea::{execute_for_tests, sea_test_db};

    pub(crate) fn sticker(id: &str, pack_id: &str, status: EmojiSemanticStatus) -> emoji::Model {
        emoji::Model {
            id: id.into(),
            pack_id: pack_id.into(),
            name: id.into(),
            tags: Some("happy".into()),
            file_name: format!("{id}.gif"),
            file_format: "gif".into(),
            sort_order: 0,
            created_at: 1,
            source: EmojiSource::OnebotImage,
            source_key: Some(format!("key-{id}")),
            native_payload: Some(Json(serde_json::Map::from_iter([(
                "url".to_owned(),
                serde_json::Value::from("https://example.invalid/s.gif"),
            )]))),
            semantic_status: status,
            suggested_name: None,
            suggested_tags: None,
            file_size: 10,
            seen_count: 1,
            last_seen_at: Some(1),
        }
    }

    pub(crate) async fn insert(db: &Db, model: emoji::Model) -> emoji::Model {
        db.write(async |tx| create_emoji(tx, model).await).await.unwrap()
    }

    async fn with_pack() -> Db {
        let db = sea_test_db().await;
        insert_pack(&db, pack("p", EmojiPackKind::Onebot, Some("42"), 0)).await;
        db
    }

    #[tokio::test]
    async fn a_created_sticker_reads_back_and_is_found_by_its_source_key() {
        let db = with_pack().await;
        let written = sticker("s1", "p", EmojiSemanticStatus::Pending);
        assert_eq!(insert(&db, written.clone()).await, written);
        assert_eq!(
            find_by_source_key(&db, "p", EmojiSource::OnebotImage, "key-s1")
                .await
                .unwrap(),
            Some(written)
        );
        assert_eq!(
            find_by_source_key(&db, "p", EmojiSource::OnebotFace, "key-s1")
                .await
                .unwrap(),
            None
        );
        assert!(name_in_use(&db, "p", "s1").await.unwrap());
        assert!(!name_in_use(&db, "p", "other").await.unwrap());
        assert_eq!(count_by_pack(&db, "p").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_sticker_moves_from_pending_to_suggested_to_confirmed() {
        let db = with_pack().await;
        insert(&db, sticker("s1", "p", EmojiSemanticStatus::Pending)).await;
        assert_eq!(list_candidates(&db, "p").await.unwrap().len(), 1);
        assert!(list_confirmed_for_packs(&db, &["p".into()]).await.unwrap().is_empty());

        let suggested = db
            .write(async |tx| update_suggestion(tx, "s1", "smile", Some("joy")).await)
            .await
            .unwrap();
        assert_eq!(suggested.semantic_status, EmojiSemanticStatus::Suggested);
        assert_eq!(suggested.suggested_name.as_deref(), Some("smile"));
        assert_eq!(suggested.name, "s1", "a suggestion does not rename");

        let confirmed = db
            .write(async |tx| confirm_semantics(tx, "s1", "grin", None).await)
            .await
            .unwrap();
        assert_eq!(confirmed.semantic_status, EmojiSemanticStatus::Confirmed);
        assert_eq!((confirmed.name.as_str(), confirmed.tags.as_deref()), ("grin", None));
        assert_eq!((confirmed.suggested_name, confirmed.suggested_tags), (None, None));
        assert!(list_candidates(&db, "p").await.unwrap().is_empty());
        assert_eq!(list_confirmed_for_packs(&db, &["p".into()]).await.unwrap().len(), 1);

        let missing = db.write(async |tx| rename_emoji(tx, "nope", "x").await).await;
        assert!(matches!(missing, Err(DbErr::RecordNotFound(_))), "{missing:?}");
    }

    #[tokio::test]
    async fn seeing_counts_and_media_attaches() {
        let db = with_pack().await;
        let mut bare = sticker("s1", "p", EmojiSemanticStatus::Pending);
        bare.file_name = String::new();
        bare.file_format = String::new();
        bare.native_payload = None;
        insert(&db, bare).await;
        let seen = db.write(async |tx| mark_seen(tx, "s1", 9).await).await.unwrap();
        assert_eq!((seen.seen_count, seen.last_seen_at), (2, Some(9)));

        let payload = Json(serde_json::Map::from_iter([(
            "file".to_owned(),
            serde_json::Value::from("f"),
        )]));
        let attached = db
            .write(async |tx| attach_captured_media(tx, "s1", "s1.png", "png", 99, payload).await)
            .await
            .unwrap();
        assert_eq!(
            (
                attached.file_name.as_str(),
                attached.file_format.as_str(),
                attached.file_size
            ),
            ("s1.png", "png", 99)
        );
        assert_eq!(attached.native_payload()["file"], "f");
    }

    /// Lottie stickers are never offered; search covers names and tags.
    #[tokio::test]
    async fn the_sendable_list_and_search() {
        let db = with_pack().await;
        insert(&db, sticker("gif", "p", EmojiSemanticStatus::Confirmed)).await;
        let mut lottie = sticker("lottie", "p", EmojiSemanticStatus::Confirmed);
        lottie.file_format = "lottie".into();
        insert(&db, lottie).await;
        let sendable: Vec<_> = list_confirmed_for_packs(&db, &["p".into()])
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(sendable, ["gif"]);
        assert_eq!(search_emojis(&db, "lot").await.unwrap().len(), 1);
        assert_eq!(search_emojis(&db, "happ").await.unwrap().len(), 2, "tags match too");
    }

    /// A payload that is not a JSON object fails the read rather than arriving
    /// as `{}`; a sticker a message shows cannot be deleted.
    #[tokio::test]
    async fn a_malformed_payload_fails_and_a_shown_sticker_stays() {
        let db = with_pack().await;
        insert(&db, sticker("s1", "p", EmojiSemanticStatus::Confirmed)).await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, created_at, updated_at) VALUES ('c', 1, 1);
             INSERT INTO messages (id, conversation_id, role, content, sort_order, created_at)
                 VALUES ('m', 'c', 'user', '', 0, 1);
             INSERT INTO message_stickers (message_id, sticker_id, position) VALUES ('m', 's1', 0);",
        )
        .await
        .unwrap();
        assert!(db.write(async |tx| delete_emoji(tx, "s1").await).await.is_err());

        execute_for_tests(&db, "UPDATE emojis SET native_payload = '[1]'")
            .await
            .unwrap();
        assert!(get_emoji(&db, "s1").await.is_err());
    }
}
