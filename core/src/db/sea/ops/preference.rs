//! The `preferences` table through SeaORM.
//!
//! The twin of `db::ops::preference` for the transaction roots that have moved
//! over, under the same names on purpose: the transaction-graph checker pairs
//! a `db/sea/ops` function with the `db/ops` function of the same name and
//! keeps `docs/dual-impl.md` honest about how many Diesel callers the old one
//! still has. The Diesel version goes when that number reaches zero.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::OnConflict;
use sea_orm::{DbErr, EntityTrait};

use crate::db::entity::preference;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::EpochMs;

/// `None` when the key has no row, which every caller treats as "the shipped
/// default", as distinct from a row holding an empty string.
pub async fn get_preference(db: &impl Read, key: &str) -> Result<Option<String>, DbErr> {
    Ok(preference::Entity::find_by_id(key)
        .one(db.conn()?)
        .await?
        .map(|row| row.value))
}

/// Writes or overwrites the row. An upsert rather than Diesel's `REPLACE`,
/// which deletes and re-inserts: the same result here, where nothing
/// references the row, and the form every backend has.
pub async fn set_preference(tx: &WriteTx, key: &str, value: &str, now: EpochMs) -> Result<(), DbErr> {
    let row = preference::ActiveModel {
        key: Set(key.to_owned()),
        value: Set(value.to_owned()),
        updated_at: Set(now),
    };
    preference::Entity::insert(row)
        .on_conflict(
            OnConflict::column(preference::Column::Key)
                .update_columns([preference::Column::Value, preference::Column::UpdatedAt])
                .to_owned(),
        )
        .exec_without_returning(tx.conn()?)
        .await?;
    Ok(())
}

/// Removes the row; a key with no row is not an error, as with Diesel.
pub async fn delete_preference(tx: &WriteTx, key: &str) -> Result<(), DbErr> {
    preference::Entity::delete_by_id(key).exec(tx.conn()?).await?;
    Ok(())
}

pub fn parse_bool_preference(key: &str, value: Option<&str>, default: bool) -> Result<bool, String> {
    match value {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(value) => Err(format!("preference `{key}` must be `true` or `false`, got `{value}`")),
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::EntityTrait;

    #[test]
    fn bool_preferences_are_closed_but_keep_the_missing_default() {
        assert!(parse_bool_preference("feature.enabled", None, true).unwrap());
        assert!(!parse_bool_preference("feature.enabled", None, false).unwrap());
        assert!(parse_bool_preference("feature.enabled", Some("true"), false).unwrap());
        assert!(!parse_bool_preference("feature.enabled", Some("false"), true).unwrap());
        assert!(parse_bool_preference("feature.enabled", Some("1"), true).is_err());
        assert!(parse_bool_preference("feature.enabled", Some("TRUE"), true).is_err());
    }

    use super::*;
    use crate::db::sea::sea_test_db;

    #[tokio::test]
    async fn a_missing_key_reads_as_none_and_deleting_it_is_not_an_error() {
        let db = sea_test_db().await;
        assert_eq!(get_preference(&db, "never.set").await.unwrap(), None);
        db.write(async |tx| delete_preference(tx, "never.set").await)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_second_set_overwrites_the_value_and_the_time() {
        let db = sea_test_db().await;
        db.write(async |tx| set_preference(tx, "theme", "dark", 10).await)
            .await
            .unwrap();
        db.write(async |tx| set_preference(tx, "theme", "light", 20).await)
            .await
            .unwrap();

        assert_eq!(get_preference(&db, "theme").await.unwrap().as_deref(), Some("light"));
        let row = preference::Entity::find_by_id("theme")
            .one(db.conn().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.updated_at, 20);
        assert_eq!(
            preference::Entity::find().all(db.conn().unwrap()).await.unwrap().len(),
            1,
            "an overwrite is one row, not two"
        );
    }

    #[tokio::test]
    async fn an_empty_string_is_a_value_and_delete_makes_it_absent() {
        let db = sea_test_db().await;
        db.write(async |tx| set_preference(tx, "onebot.access_token", "", 1).await)
            .await
            .unwrap();
        assert_eq!(
            get_preference(&db, "onebot.access_token").await.unwrap().as_deref(),
            Some("")
        );
        db.write(async |tx| delete_preference(tx, "onebot.access_token").await)
            .await
            .unwrap();
        assert_eq!(get_preference(&db, "onebot.access_token").await.unwrap(), None);
    }

    /// The reads and the write of one key happen inside one `WriteTx`, as a
    /// caller that reads-then-writes (voice_corpus::storage_key) needs.
    #[tokio::test]
    async fn a_read_inside_a_write_transaction_sees_its_own_write() {
        let db = sea_test_db().await;
        let seen = db
            .write(async |tx| {
                set_preference(tx, "k", "v", 1).await?;
                get_preference(tx, "k").await
            })
            .await
            .unwrap();
        assert_eq!(seen.as_deref(), Some("v"));
    }
}
