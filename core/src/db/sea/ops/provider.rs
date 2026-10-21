//! Reading and writing `providers`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use sea_orm::ActiveValue::Unchanged;
use sea_orm::{ActiveModelTrait, DbErr, EntityTrait, IntoActiveModel, QueryOrder};

use crate::db::entity::provider;
use crate::db::entity::provider::ProviderChangeset;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};

pub async fn list_providers(db: &impl Read) -> Result<Vec<provider::Model>, DbErr> {
    let rows = provider::Entity::find()
        .order_by_asc(provider::Column::SortOrder)
        .all(db.conn()?)
        .await?;
    for row in &rows {
        validate(row)?;
    }
    Ok(rows)
}

pub async fn get_provider(db: &impl Read, id: &str) -> Result<Option<provider::Model>, DbErr> {
    let row = provider::Entity::find_by_id(id).one(db.conn()?).await?;
    if let Some(ref value) = row {
        validate(value)?;
    }
    Ok(row)
}

fn validate(row: &provider::Model) -> Result<(), DbErr> {
    crate::provider::registry::validate_local_backend(row.provider_type, row.local_backend).map_err(DbErr::Custom)
}

fn not_found(id: &str) -> DbErr {
    DbErr::RecordNotFound(format!("provider `{id}`"))
}

/// Inserts the row the caller built and reads it back.
pub async fn create_provider(tx: &WriteTx, model: provider::Model) -> Result<provider::Model, DbErr> {
    validate(&model)?;
    let id = model.id.clone();
    provider::Entity::insert(model.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    get_provider(tx, &id).await?.ok_or_else(|| not_found(&id))
}

/// `RecordNotFound` when there is no such row, before anything is written.
pub async fn update_provider(tx: &WriteTx, id: &str, changeset: ProviderChangeset) -> Result<provider::Model, DbErr> {
    let existing = get_provider(tx, id).await?.ok_or_else(|| not_found(id))?;
    crate::provider::registry::validate_local_backend(
        changeset.provider_type.unwrap_or(existing.provider_type),
        changeset.local_backend.unwrap_or(existing.local_backend),
    )
    .map_err(DbErr::Custom)?;
    let mut row = changeset.into_active_model();
    row.id = Unchanged(existing.id);
    row.update(tx.conn()?).await
}

/// How many rows went: 0 for an id that was already gone. Its cached models,
/// model configs and the rest go with it by cascade; assistants pointing at it
/// are set to no provider.
pub async fn delete_provider(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    Ok(provider::Entity::delete_by_id(id).exec(tx.conn()?).await?.rows_affected)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::sea_test_db;
    use crate::db::types::SqlBool;
    use crate::provider::registry::{ApiFormat, CredentialKind, ProviderType, TransportProfile};

    pub(crate) fn provider_row(id: &str, sort_order: i32) -> provider::Model {
        provider::Model {
            id: id.into(),
            name: id.to_uppercase(),
            provider_type: ProviderType::Openai,
            base_url: "https://example.invalid".into(),
            is_enabled: SqlBool::TRUE,
            sort_order,
            created_at: 1,
            updated_at: 1,
            api_format: ApiFormat::Responses,
            catalog_id: Some("openai".into()),
            credential_kind: CredentialKind::ApiKey,
            transport_profile: TransportProfile::Standard,
            icon: None,
            codex_request_shape: SqlBool::FALSE,
            local_backend: None,
        }
    }

    pub(crate) async fn insert(db: &Db, model: provider::Model) -> provider::Model {
        db.write(async |tx| create_provider(tx, model).await).await.unwrap()
    }

    #[tokio::test]
    async fn local_backend_round_trips_and_invalid_provider_pairs_fail() {
        use crate::provider::registry::LocalInferenceBackend;
        let db = sea_test_db().await;
        let mut local = provider_row("local", 0);
        local.provider_type = ProviderType::LitertLm;
        local.api_format = ApiFormat::LitertLm;
        local.credential_kind = CredentialKind::None;
        local.transport_profile = TransportProfile::LocalNative;
        assert!(
            db.write(async |tx| create_provider(tx, local.clone()).await)
                .await
                .is_err()
        );
        local.local_backend = Some(LocalInferenceBackend::Auto);
        insert(&db, local).await;
        for backend in [
            LocalInferenceBackend::Cpu,
            LocalInferenceBackend::Gpu,
            LocalInferenceBackend::Npu,
            LocalInferenceBackend::Auto,
        ] {
            let updated = db
                .write(async |tx| {
                    update_provider(
                        tx,
                        "local",
                        ProviderChangeset {
                            local_backend: Some(Some(backend)),
                            ..Default::default()
                        },
                    )
                    .await
                })
                .await
                .unwrap();
            assert_eq!(updated.local_backend, Some(backend));
            assert_eq!(
                get_provider(&db, "local").await.unwrap().unwrap().local_backend,
                Some(backend)
            );
        }
        assert!(
            db.write(async |tx| update_provider(
                tx,
                "local",
                ProviderChangeset {
                    local_backend: Some(None),
                    ..Default::default()
                }
            )
            .await)
                .await
                .is_err()
        );
        assert!(
            db.write(async |tx| update_provider(
                tx,
                "local",
                ProviderChangeset {
                    provider_type: Some(ProviderType::Openai),
                    ..Default::default()
                }
            )
            .await)
                .await
                .is_err()
        );
        let cloud = provider_row("cloud", 1);
        insert(&db, cloud).await;
        assert!(
            db.write(async |tx| update_provider(
                tx,
                "cloud",
                ProviderChangeset {
                    local_backend: Some(Some(LocalInferenceBackend::Gpu)),
                    ..Default::default()
                }
            )
            .await)
                .await
                .is_err()
        );
        crate::db::sea::execute_for_tests(&db, "UPDATE providers SET local_backend='gpu' WHERE id='cloud'")
            .await
            .unwrap();
        assert!(get_provider(&db, "cloud").await.is_err());
        assert!(list_providers(&db).await.is_err());
    }

    #[tokio::test]
    async fn providers_list_in_order_update_in_part_and_delete() {
        let db = sea_test_db().await;
        let written = provider_row("b", 1);
        assert_eq!(insert(&db, written.clone()).await, written);
        insert(&db, provider_row("a", 0)).await;
        let ids: Vec<_> = list_providers(&db).await.unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["a", "b"]);

        let updated = db
            .write(async |tx| {
                update_provider(
                    tx,
                    "b",
                    ProviderChangeset {
                        name: Some("Relay".into()),
                        catalog_id: Some(None),
                        icon: Some(Some("vertexai".into())),
                        updated_at: Some(9),
                        ..Default::default()
                    },
                )
                .await
            })
            .await
            .unwrap();
        assert_eq!(
            (
                updated.name.as_str(),
                updated.catalog_id,
                updated.icon.as_deref(),
                updated.updated_at
            ),
            ("Relay", None, Some("vertexai"), 9)
        );
        assert_eq!(updated.base_url, written.base_url, "a field left out is not written");

        let missing = db
            .write(async |tx| update_provider(tx, "nope", ProviderChangeset::default()).await)
            .await;
        assert!(matches!(missing, Err(DbErr::RecordNotFound(_))));

        assert_eq!(db.write(async |tx| delete_provider(tx, "b").await).await.unwrap(), 1);
        assert_eq!(get_provider(&db, "b").await.unwrap(), None);
    }
}
