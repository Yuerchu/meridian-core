//! What is left of the Diesel skill-binding ops: `resolve_available`, which
//! `turn_config::resolve` still calls on the connection it shares with the
//! rest of the turn's reads. Everything else is `db::sea::ops::skill_binding`;
//! `docs/dual-impl.md` counts what still holds this one.
//!
//! It answers in `skill::Model` rather than a Diesel row, so the catalog that
//! consumes it (`agent::tool_defs::apply_skill_catalog`) has one input type
//! whichever side produced the list.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::entity::skill;
use crate::db::entity::skill::SkillSource;
use crate::db::models::skill::SkillRow;
use crate::db::schema::{skill_bindings_assistant, skill_bindings_global, skill_bindings_project, skills};

/// Every enabled skill reachable from the three anchors, deduplicated and
/// ordered by `dir_name` — see the SeaORM version for why the order matters.
/// The statements run on the caller's connection, which `turn_config` uses for
/// its other reads in the same moment.
pub fn resolve_available(
    conn: &mut SqliteConnection,
    project_id: Option<&str>,
    assistant_id: Option<&str>,
) -> Result<Vec<skill::Model>, String> {
    let mut bound: Vec<String> = skill_bindings_global::table
        .select(skill_bindings_global::dir_name)
        .load(conn)
        .map_err(|e| e.to_string())?;
    if let Some(id) = project_id {
        bound.extend(
            skill_bindings_project::table
                .filter(skill_bindings_project::project_id.eq(id))
                .select(skill_bindings_project::dir_name)
                .load::<String>(conn)
                .map_err(|e| e.to_string())?,
        );
    }
    if let Some(id) = assistant_id {
        bound.extend(
            skill_bindings_assistant::table
                .filter(skill_bindings_assistant::assistant_id.eq(id))
                .select(skill_bindings_assistant::dir_name)
                .load::<String>(conn)
                .map_err(|e| e.to_string())?,
        );
    }
    bound.sort();
    bound.dedup();
    if bound.is_empty() {
        return Ok(Vec::new());
    }

    skills::table
        .filter(skills::dir_name.eq_any(&bound))
        .filter(skills::is_enabled.eq(1))
        .order(skills::dir_name.asc())
        .load::<SkillRow>(conn)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(model_of)
        .collect()
}

/// The row as the SeaORM model, held to the same checks a SeaORM read makes:
/// a source it does not know and a flag that is not 0/1 are errors.
fn model_of(row: SkillRow) -> Result<skill::Model, String> {
    let flag = |value: i32, column: &str| match value {
        0 => Ok(false.into()),
        1 => Ok(true.into()),
        other => Err(format!("skills.{column} holds {other}, not 0 or 1")),
    };
    Ok(skill::Model {
        source: SkillSource::parse(&row.source)?,
        is_enabled: flag(row.is_enabled, "is_enabled")?,
        is_builtin: flag(row.is_builtin, "is_builtin")?,
        dir_name: row.dir_name,
        llm_name: row.llm_name,
        llm_description: row.llm_description,
        display_name: row.display_name,
        display_description: row.display_description,
        mtime_hash: row.mtime_hash,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}
