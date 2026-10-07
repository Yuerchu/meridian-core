//! Which skills are offered where: the three binding tables.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`. That is
//! what lets the binding command count an anchor's bindings and add one under
//! a single lock, so two concurrent binds cannot both pass the cap.

use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};

use crate::db::entity::{skill, skill_binding_assistant, skill_binding_global, skill_binding_project};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, Snapshot, WriteTx};
use crate::db::types::SqlBool;

/// Bindings cost context on every request (each one contributes a name and a
/// description to the tool schema), so the cap is per anchor rather than a
/// global storage quota.
pub const MAX_BINDINGS_PER_ANCHOR: usize = 50;

/// Which anchor a skill binding hangs off. Bindings are layered rather than
/// collected into one central set: a skill pinned globally stays available even
/// on an assistant the user cannot (or does not want to) edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, strum::IntoStaticStr)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "snake_case")]
pub enum SkillLayer {
    Global,
    Project,
    Assistant,
}

impl SkillLayer {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// Bind a skill at a layer; binding it twice is not an error. A project or
/// assistant binding with no anchor id does nothing.
pub async fn bind(tx: &WriteTx, layer: SkillLayer, anchor_id: Option<&str>, dir_name: &str) -> Result<(), DbErr> {
    let conn = tx.conn()?;
    match (layer, anchor_id) {
        (SkillLayer::Global, _) => {
            skill_binding_global::Entity::insert(skill_binding_global::ActiveModel {
                dir_name: Set(dir_name.to_owned()),
            })
            .on_conflict(
                OnConflict::column(skill_binding_global::Column::DirName)
                    .do_nothing()
                    .to_owned(),
            )
            .exec_without_returning(conn)
            .await?;
        }
        (SkillLayer::Project, Some(id)) => {
            skill_binding_project::Entity::insert(skill_binding_project::ActiveModel {
                project_id: Set(id.to_owned()),
                dir_name: Set(dir_name.to_owned()),
            })
            .on_conflict(
                OnConflict::columns([
                    skill_binding_project::Column::ProjectId,
                    skill_binding_project::Column::DirName,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec_without_returning(conn)
            .await?;
        }
        (SkillLayer::Assistant, Some(id)) => {
            skill_binding_assistant::Entity::insert(skill_binding_assistant::ActiveModel {
                assistant_id: Set(id.to_owned()),
                dir_name: Set(dir_name.to_owned()),
            })
            .on_conflict(
                OnConflict::columns([
                    skill_binding_assistant::Column::AssistantId,
                    skill_binding_assistant::Column::DirName,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec_without_returning(conn)
            .await?;
        }
        (SkillLayer::Project | SkillLayer::Assistant, None) => {}
    }
    Ok(())
}

/// Remove a binding; removing one that is not there is not an error.
pub async fn unbind(tx: &WriteTx, layer: SkillLayer, anchor_id: Option<&str>, dir_name: &str) -> Result<(), DbErr> {
    let conn = tx.conn()?;
    match (layer, anchor_id) {
        (SkillLayer::Global, _) => {
            skill_binding_global::Entity::delete_by_id(dir_name).exec(conn).await?;
        }
        (SkillLayer::Project, Some(id)) => {
            skill_binding_project::Entity::delete_by_id((id.to_owned(), dir_name.to_owned()))
                .exec(conn)
                .await?;
        }
        (SkillLayer::Assistant, Some(id)) => {
            skill_binding_assistant::Entity::delete_by_id((id.to_owned(), dir_name.to_owned()))
                .exec(conn)
                .await?;
        }
        (SkillLayer::Project | SkillLayer::Assistant, None) => {}
    }
    Ok(())
}

/// Directory names bound at one specific layer, in order, for the settings UI.
pub async fn list_layer(db: &impl Read, layer: SkillLayer, anchor_id: Option<&str>) -> Result<Vec<String>, DbErr> {
    let conn = db.conn()?;
    match (layer, anchor_id) {
        (SkillLayer::Global, _) => {
            skill_binding_global::Entity::find()
                .select_only()
                .column(skill_binding_global::Column::DirName)
                .order_by_asc(skill_binding_global::Column::DirName)
                .into_tuple()
                .all(conn)
                .await
        }
        (SkillLayer::Project, Some(id)) => {
            skill_binding_project::Entity::find()
                .select_only()
                .column(skill_binding_project::Column::DirName)
                .filter(skill_binding_project::Column::ProjectId.eq(id))
                .order_by_asc(skill_binding_project::Column::DirName)
                .into_tuple()
                .all(conn)
                .await
        }
        (SkillLayer::Assistant, Some(id)) => {
            skill_binding_assistant::Entity::find()
                .select_only()
                .column(skill_binding_assistant::Column::DirName)
                .filter(skill_binding_assistant::Column::AssistantId.eq(id))
                .order_by_asc(skill_binding_assistant::Column::DirName)
                .into_tuple()
                .all(conn)
                .await
        }
        (SkillLayer::Project | SkillLayer::Assistant, None) => Ok(Vec::new()),
    }
}

pub async fn count_layer(db: &impl Read, layer: SkillLayer, anchor_id: Option<&str>) -> Result<usize, DbErr> {
    Ok(list_layer(db, layer, anchor_id).await?.len())
}

/// Every enabled skill reachable from the three anchors, deduplicated and
/// ordered by `dir_name`. The ordering is load-bearing: this list feeds the
/// tool schema, which sits at the front of the prompt cache prefix for every
/// provider — a non-deterministic order would invalidate that cache on each
/// request. Four statements joined in Rust, so it takes a [`Snapshot`]: on the
/// pool, a skill unbound and deleted between them could still be offered.
pub async fn resolve_available(
    db: &impl Snapshot,
    project_id: Option<&str>,
    assistant_id: Option<&str>,
) -> Result<Vec<skill::Model>, DbErr> {
    let mut bound = list_layer(db, SkillLayer::Global, None).await?;
    bound.extend(list_layer(db, SkillLayer::Project, project_id).await?);
    bound.extend(list_layer(db, SkillLayer::Assistant, assistant_id).await?);
    bound.sort();
    bound.dedup();
    if bound.is_empty() {
        return Ok(Vec::new());
    }
    skill::Entity::find()
        .filter(skill::Column::DirName.is_in(bound))
        .filter(skill::Column::IsEnabled.eq(SqlBool::TRUE))
        .order_by_asc(skill::Column::DirName)
        .all(db.conn()?)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::ops::skill::{update_skill, upsert_skill};
    use crate::db::sea::{execute_for_tests, sea_test_db};

    async fn index(db: &Db, dir: &str) {
        let mut model = crate::db::sea::ops::skill::tests::make_skill(dir, dir);
        model.display_name = dir.into();
        db.write(async |tx| upsert_skill(tx, model).await).await.unwrap();
    }

    async fn anchors(db: &Db) {
        execute_for_tests(
            db,
            "INSERT INTO projects (id, name, source_type, created_at, updated_at) VALUES ('p1', 'P', 'local', 1, 1);
             INSERT INTO assistants (id, name, created_at, updated_at) VALUES ('a1', 'A', 1, 1);",
        )
        .await
        .unwrap();
    }

    async fn set(db: &Db, layer: SkillLayer, anchor: Option<&str>, dir: &str, bound: bool) {
        db.write(async |tx| {
            if bound {
                bind(tx, layer, anchor, dir).await
            } else {
                unbind(tx, layer, anchor, dir).await
            }
        })
        .await
        .unwrap();
    }

    async fn available(db: &Db, project: Option<&str>, assistant: Option<&str>) -> Vec<String> {
        db.read(async |tx| resolve_available(tx, project, assistant).await)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.dir_name)
            .collect()
    }

    #[tokio::test]
    async fn bind_is_idempotent_and_unbind_removes() {
        let db = sea_test_db().await;
        anchors(&db).await;
        index(&db, "s").await;
        for layer in [SkillLayer::Global, SkillLayer::Project, SkillLayer::Assistant] {
            let anchor = match layer {
                SkillLayer::Global => None,
                SkillLayer::Project => Some("p1"),
                SkillLayer::Assistant => Some("a1"),
            };
            set(&db, layer, anchor, "s", true).await;
            set(&db, layer, anchor, "s", true).await;
            assert_eq!(list_layer(&db, layer, anchor).await.unwrap(), ["s"], "{layer:?}");
            assert_eq!(count_layer(&db, layer, anchor).await.unwrap(), 1);
            set(&db, layer, anchor, "s", false).await;
            assert!(list_layer(&db, layer, anchor).await.unwrap().is_empty(), "{layer:?}");
        }
        // An anchored layer with no anchor is a no-op both ways.
        set(&db, SkillLayer::Project, None, "s", true).await;
        assert!(list_layer(&db, SkillLayer::Project, None).await.unwrap().is_empty());
    }

    /// Union of the three layers, deduplicated, sorted, enabled only; a
    /// project's or assistant's bindings only where that anchor applies.
    #[tokio::test]
    async fn available_is_the_sorted_union_of_the_applicable_layers() {
        let db = sea_test_db().await;
        anchors(&db).await;
        for dir in ["g", "p", "a", "off"] {
            index(&db, dir).await;
        }
        set(&db, SkillLayer::Global, None, "g", true).await;
        set(&db, SkillLayer::Project, Some("p1"), "p", true).await;
        set(&db, SkillLayer::Project, Some("p1"), "g", true).await;
        set(&db, SkillLayer::Assistant, Some("a1"), "a", true).await;
        set(&db, SkillLayer::Global, None, "off", true).await;
        db.write(async |tx| {
            update_skill(
                tx,
                "off",
                crate::db::entity::skill::SkillChangeset {
                    is_enabled: Some(SqlBool::FALSE),
                    ..Default::default()
                },
            )
            .await
        })
        .await
        .unwrap();

        assert_eq!(available(&db, None, None).await, ["g"]);
        assert_eq!(available(&db, Some("p1"), Some("a1")).await, ["a", "g", "p"]);
    }

    /// Deleting a skill takes its bindings with it.
    #[tokio::test]
    async fn bindings_cascade_with_the_skill() {
        let db = sea_test_db().await;
        index(&db, "s").await;
        set(&db, SkillLayer::Global, None, "s", true).await;
        db.write(async |tx| crate::db::sea::ops::skill::delete_skill(tx, "s").await)
            .await
            .unwrap();
        assert!(list_layer(&db, SkillLayer::Global, None).await.unwrap().is_empty());
    }
}
