use diesel::SqliteConnection;
use diesel::prelude::*;

use crate::db::models::redaction_rule::*;
use crate::db::schema::redaction_rules;
use crate::redaction::rule::RuleSpec;

pub fn list_rules(conn: &mut SqliteConnection) -> QueryResult<Vec<RedactionRuleRow>> {
    redaction_rules::table
        .order((
            redaction_rules::scope_type.asc(),
            redaction_rules::scope_id.asc(),
            redaction_rules::created_at.asc(),
            redaction_rules::id.asc(),
        ))
        .load(conn)
}

pub fn list_enabled_rules(conn: &mut SqliteConnection) -> QueryResult<Vec<RedactionRuleRow>> {
    redaction_rules::table
        .filter(redaction_rules::is_enabled.eq(1))
        .order((
            redaction_rules::scope_type.asc(),
            redaction_rules::scope_id.asc(),
            redaction_rules::created_at.asc(),
            redaction_rules::id.asc(),
        ))
        .load(conn)
}

pub fn list_visible(conn: &mut SqliteConnection, project_id: Option<&str>) -> QueryResult<Vec<RedactionRuleRow>> {
    let mut query = redaction_rules::table
        .into_boxed()
        .filter(redaction_rules::scope_type.eq("global"));
    if let Some(pid) = project_id {
        query = redaction_rules::table.into_boxed().filter(
            redaction_rules::scope_type.eq("global").or(redaction_rules::scope_type
                .eq("project")
                .and(redaction_rules::scope_id.eq(pid))),
        );
    }
    query
        .order((
            redaction_rules::scope_type.asc(),
            redaction_rules::created_at.asc(),
            redaction_rules::id.asc(),
        ))
        .load(conn)
}

pub fn get_rule_by_name(
    conn: &mut SqliteConnection,
    scope_type: &str,
    scope_id: &str,
    name: &str,
) -> QueryResult<Option<RedactionRuleRow>> {
    redaction_rules::table
        .filter(redaction_rules::scope_type.eq(scope_type))
        .filter(redaction_rules::scope_id.eq(scope_id))
        .filter(redaction_rules::name.eq(name))
        .first(conn)
        .optional()
}

pub fn validate_new_rule(
    conn: &mut SqliteConnection,
    scope_type: &str,
    scope_id: &str,
    spec: &RuleSpec,
) -> Result<(), String> {
    use crate::redaction::builtin::BUILTIN_RULES;

    if BUILTIN_RULES.iter().any(|b| b.name == spec.name) {
        return Err(format!("`{}` is a built-in rule and cannot be overridden", spec.name));
    }

    if let Some(existing) = get_rule_by_name(conn, scope_type, scope_id, &spec.name).map_err(|e| e.to_string())? {
        if existing.is_enabled() {
            return Err(format!(
                "a rule named `{}` already exists in this scope (id: {})",
                spec.name, existing.id
            ));
        } else {
            return Err(format!(
                "a rule named `{}` was previously disabled in this scope; remove it first or choose a different name",
                spec.name
            ));
        }
    }

    let count: i64 = redaction_rules::table
        .filter(redaction_rules::scope_type.eq(scope_type))
        .filter(redaction_rules::scope_id.eq(scope_id))
        .count()
        .get_result(conn)
        .map_err(|e| e.to_string())?;

    if count as usize >= MAX_RULES_PER_SCOPE {
        return Err(format!(
            "this scope already has {count} rules (limit: {MAX_RULES_PER_SCOPE})"
        ));
    }

    Ok(())
}

pub fn create_rule(conn: &mut SqliteConnection, insert: &RedactionRuleInsert) -> QueryResult<RedactionRuleRow> {
    diesel::insert_into(redaction_rules::table)
        .values(insert)
        .execute(conn)?;
    redaction_rules::table.find(&insert.id).first(conn)
}

pub fn update_rule(
    conn: &mut SqliteConnection,
    id: &str,
    changeset: &RedactionRuleChangeset,
) -> QueryResult<RedactionRuleRow> {
    diesel::update(redaction_rules::table.find(id))
        .set(changeset)
        .execute(conn)?;
    redaction_rules::table.find(id).first(conn)
}

pub fn delete_rule(conn: &mut SqliteConnection, id: &str) -> QueryResult<()> {
    diesel::delete(redaction_rules::table.find(id)).execute(conn)?;
    Ok(())
}

pub fn delete_project_rules(conn: &mut SqliteConnection, project_id: &str) -> QueryResult<usize> {
    diesel::delete(
        redaction_rules::table
            .filter(redaction_rules::scope_type.eq("project"))
            .filter(redaction_rules::scope_id.eq(project_id)),
    )
    .execute(conn)
}

pub fn purge_orphan_project_rules(conn: &mut SqliteConnection) -> QueryResult<usize> {
    use crate::db::schema::projects;

    let orphan_ids: Vec<String> = redaction_rules::table
        .filter(redaction_rules::scope_type.eq("project"))
        .filter(diesel::dsl::not(diesel::dsl::exists(
            projects::table.filter(projects::id.eq(redaction_rules::scope_id)),
        )))
        .select(redaction_rules::id)
        .load(conn)?;

    if orphan_ids.is_empty() {
        return Ok(0);
    }

    diesel::delete(redaction_rules::table.filter(redaction_rules::id.eq_any(&orphan_ids))).execute(conn)
}
