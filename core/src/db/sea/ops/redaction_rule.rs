//! Reading and writing `redaction_rules`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`. That is
//! what lets the add-rule tool check the name and the per-scope limit and then
//! insert under one lock, so two concurrent adds cannot both pass the count.

use sea_orm::{ColumnTrait, DbErr, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder};

use crate::db::entity::redaction_rule;
use crate::db::entity::redaction_rule::{MAX_RULES_PER_SCOPE, RuleScope};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::SqlBool;
use crate::redaction::rule::RuleSpec;

/// The enabled rules, global ones first, each scope in the order its rules
/// were added.
pub async fn list_enabled_rules(db: &impl Read) -> Result<Vec<redaction_rule::Model>, DbErr> {
    redaction_rule::Entity::find()
        .filter(redaction_rule::Column::IsEnabled.eq(SqlBool::TRUE))
        .order_by_asc(redaction_rule::Column::ScopeType)
        .order_by_asc(redaction_rule::Column::ScopeId)
        .order_by_asc(redaction_rule::Column::CreatedAt)
        .order_by_asc(redaction_rule::Column::Id)
        .all(db.conn()?)
        .await
}

pub async fn get_rule_by_name(
    db: &impl Read,
    scope_type: RuleScope,
    scope_id: &str,
    name: &str,
) -> Result<Option<redaction_rule::Model>, DbErr> {
    redaction_rule::Entity::find()
        .filter(redaction_rule::Column::ScopeType.eq(scope_type))
        .filter(redaction_rule::Column::ScopeId.eq(scope_id))
        .filter(redaction_rule::Column::Name.eq(name))
        .one(db.conn()?)
        .await
}

/// Whether a rule with this spec may be added to the scope: not a built-in's
/// name, not a name the scope already has (enabled or not), and under the
/// per-scope limit. Takes the write transaction the insert will run in, so the
/// answer still holds when the row goes in.
pub async fn validate_new_rule(
    tx: &WriteTx,
    scope_type: RuleScope,
    scope_id: &str,
    spec: &RuleSpec,
) -> Result<(), String> {
    use crate::redaction::builtin::BUILTIN_RULES;

    if BUILTIN_RULES.iter().any(|b| b.name == spec.name) {
        return Err(format!("`{}` is a built-in rule and cannot be overridden", spec.name));
    }

    if let Some(existing) = get_rule_by_name(tx, scope_type, scope_id, &spec.name)
        .await
        .map_err(|e| e.to_string())?
    {
        return Err(if existing.is_enabled.get() {
            format!(
                "a rule named `{}` already exists in this scope (id: {})",
                spec.name, existing.id
            )
        } else {
            format!(
                "a rule named `{}` was previously disabled in this scope; remove it first or choose a different name",
                spec.name
            )
        });
    }

    let count = redaction_rule::Entity::find()
        .filter(redaction_rule::Column::ScopeType.eq(scope_type))
        .filter(redaction_rule::Column::ScopeId.eq(scope_id))
        .count(tx.conn().map_err(|e| e.to_string())?)
        .await
        .map_err(|e| e.to_string())?;
    if count as usize >= MAX_RULES_PER_SCOPE {
        return Err(format!(
            "this scope already has {count} rules (limit: {MAX_RULES_PER_SCOPE})"
        ));
    }
    Ok(())
}

/// Inserts the row the caller built.
pub async fn create_rule(tx: &WriteTx, model: redaction_rule::Model) -> Result<(), DbErr> {
    redaction_rule::Entity::insert(model.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    Ok(())
}

/// How many rows went: 0 for an id that was already gone.
pub async fn delete_rule(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    Ok(redaction_rule::Entity::delete_by_id(id)
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::redaction_rule::{GLOBAL_SCOPE_ID, RedactionExample, RuleCategory, RuleOrigin};
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::db::types::Json;

    fn spec(name: &str) -> RuleSpec {
        RuleSpec {
            name: name.into(),
            description: "d".into(),
            pattern: "tok_[a-z]+".into(),
            category: RuleCategory::Secret,
            examples: vec![RedactionExample {
                text: "tok_abc".into(),
                should_match: true,
            }],
        }
    }

    fn rule(id: &str, scope: RuleScope, scope_id: &str, name: &str, created_at: i64) -> redaction_rule::Model {
        let spec = spec(name);
        redaction_rule::Model {
            id: id.into(),
            scope_type: scope,
            scope_id: scope_id.into(),
            name: spec.name,
            description: spec.description,
            pattern: spec.pattern,
            category: spec.category,
            examples: Json(spec.examples),
            origin: RuleOrigin::Model,
            source_conversation_id: None,
            is_enabled: SqlBool::TRUE,
            created_at,
            updated_at: created_at,
        }
    }

    async fn insert(db: &Db, model: redaction_rule::Model) {
        db.write(async |tx| create_rule(tx, model).await).await.unwrap();
    }

    async fn validate(db: &Db, scope: RuleScope, scope_id: &str, name: &str) -> Result<(), String> {
        let spec = spec(name);
        db.write(async |tx| Ok::<_, DbErr>(validate_new_rule(tx, scope, scope_id, &spec).await))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn enabled_rules_come_back_global_first_and_in_the_order_added() {
        let db = sea_test_db().await;
        insert(&db, rule("p1", RuleScope::Project, "proj", "b", 1)).await;
        insert(&db, rule("g2", RuleScope::Global, GLOBAL_SCOPE_ID, "c", 3)).await;
        insert(&db, rule("g1", RuleScope::Global, GLOBAL_SCOPE_ID, "a", 2)).await;
        let mut off = rule("g0", RuleScope::Global, GLOBAL_SCOPE_ID, "off", 0);
        off.is_enabled = SqlBool::FALSE;
        insert(&db, off).await;

        let ids: Vec<_> = list_enabled_rules(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, ["g1", "g2", "p1"]);
        let found = get_rule_by_name(&db, RuleScope::Global, GLOBAL_SCOPE_ID, "a")
            .await
            .unwrap();
        assert_eq!(found, Some(rule("g1", RuleScope::Global, GLOBAL_SCOPE_ID, "a", 2)));
        assert_eq!(db.write(async |tx| delete_rule(tx, "g1").await).await.unwrap(), 1);
        assert_eq!(
            get_rule_by_name(&db, RuleScope::Global, GLOBAL_SCOPE_ID, "a")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn validation_refuses_builtins_repeats_disabled_names_and_a_full_scope() {
        let db = sea_test_db().await;
        let builtin = crate::redaction::builtin::BUILTIN_RULES[0].name;
        assert!(
            validate(&db, RuleScope::Global, GLOBAL_SCOPE_ID, builtin)
                .await
                .unwrap_err()
                .contains("built-in")
        );

        insert(&db, rule("r1", RuleScope::Global, GLOBAL_SCOPE_ID, "taken", 1)).await;
        assert!(
            validate(&db, RuleScope::Global, GLOBAL_SCOPE_ID, "taken")
                .await
                .unwrap_err()
                .contains("already exists")
        );
        // The same name in another scope is a different rule.
        validate(&db, RuleScope::Project, "proj", "taken").await.unwrap();

        let mut off = rule("r2", RuleScope::Global, GLOBAL_SCOPE_ID, "off", 1);
        off.is_enabled = SqlBool::FALSE;
        insert(&db, off).await;
        assert!(
            validate(&db, RuleScope::Global, GLOBAL_SCOPE_ID, "off")
                .await
                .unwrap_err()
                .contains("disabled")
        );

        for i in 0..MAX_RULES_PER_SCOPE {
            insert(
                &db,
                rule(&format!("f{i}"), RuleScope::Project, "full", &format!("n{i}"), 1),
            )
            .await;
        }
        assert!(
            validate(&db, RuleScope::Project, "full", "one_more")
                .await
                .unwrap_err()
                .contains("limit")
        );
    }

    /// Examples that do not decode fail the read rather than arriving empty.
    #[tokio::test]
    async fn a_malformed_row_fails_the_read() {
        let db = sea_test_db().await;
        insert(&db, rule("r1", RuleScope::Global, GLOBAL_SCOPE_ID, "a", 1)).await;
        execute_for_tests(&db, r#"UPDATE redaction_rules SET examples = '[{"text": "x"}]'"#)
            .await
            .unwrap();
        assert!(list_enabled_rules(&db).await.is_err());
    }
}
