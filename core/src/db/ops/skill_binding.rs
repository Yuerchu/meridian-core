use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::skill::SkillRow;
use crate::db::models::skill_binding::SkillLayer;
use crate::db::schema::{skill_bindings_assistant, skill_bindings_global, skill_bindings_project, skills};

/// Bindings cost context on every request (each one contributes a name and a
/// description to the tool schema), so the cap is per anchor rather than a
/// global storage quota.
pub const MAX_BINDINGS_PER_ANCHOR: usize = 50;

pub fn bind(
    conn: &mut SqliteConnection,
    layer: SkillLayer,
    anchor_id: Option<&str>,
    dir_name: &str,
) -> QueryResult<()> {
    match layer {
        SkillLayer::Global => {
            diesel::insert_or_ignore_into(skill_bindings_global::table)
                .values(skill_bindings_global::dir_name.eq(dir_name))
                .execute(conn)?;
        }
        SkillLayer::Project => {
            let Some(id) = anchor_id else { return Ok(()) };
            diesel::insert_or_ignore_into(skill_bindings_project::table)
                .values((
                    skill_bindings_project::project_id.eq(id),
                    skill_bindings_project::dir_name.eq(dir_name),
                ))
                .execute(conn)?;
        }
        SkillLayer::Assistant => {
            let Some(id) = anchor_id else { return Ok(()) };
            diesel::insert_or_ignore_into(skill_bindings_assistant::table)
                .values((
                    skill_bindings_assistant::assistant_id.eq(id),
                    skill_bindings_assistant::dir_name.eq(dir_name),
                ))
                .execute(conn)?;
        }
    }
    Ok(())
}

pub fn unbind(
    conn: &mut SqliteConnection,
    layer: SkillLayer,
    anchor_id: Option<&str>,
    dir_name: &str,
) -> QueryResult<()> {
    match layer {
        SkillLayer::Global => {
            diesel::delete(skill_bindings_global::table.find(dir_name)).execute(conn)?;
        }
        SkillLayer::Project => {
            let Some(id) = anchor_id else { return Ok(()) };
            diesel::delete(
                skill_bindings_project::table
                    .filter(skill_bindings_project::project_id.eq(id))
                    .filter(skill_bindings_project::dir_name.eq(dir_name)),
            )
            .execute(conn)?;
        }
        SkillLayer::Assistant => {
            let Some(id) = anchor_id else { return Ok(()) };
            diesel::delete(
                skill_bindings_assistant::table
                    .filter(skill_bindings_assistant::assistant_id.eq(id))
                    .filter(skill_bindings_assistant::dir_name.eq(dir_name)),
            )
            .execute(conn)?;
        }
    }
    Ok(())
}

/// Directory names bound at one specific layer, for the settings UI.
pub fn list_layer(conn: &mut SqliteConnection, layer: SkillLayer, anchor_id: Option<&str>) -> QueryResult<Vec<String>> {
    match layer {
        SkillLayer::Global => skill_bindings_global::table
            .select(skill_bindings_global::dir_name)
            .order(skill_bindings_global::dir_name.asc())
            .load::<String>(conn),
        SkillLayer::Project => {
            let Some(id) = anchor_id else { return Ok(Vec::new()) };
            skill_bindings_project::table
                .filter(skill_bindings_project::project_id.eq(id))
                .select(skill_bindings_project::dir_name)
                .order(skill_bindings_project::dir_name.asc())
                .load::<String>(conn)
        }
        SkillLayer::Assistant => {
            let Some(id) = anchor_id else { return Ok(Vec::new()) };
            skill_bindings_assistant::table
                .filter(skill_bindings_assistant::assistant_id.eq(id))
                .select(skill_bindings_assistant::dir_name)
                .order(skill_bindings_assistant::dir_name.asc())
                .load::<String>(conn)
        }
    }
}

pub fn count_layer(conn: &mut SqliteConnection, layer: SkillLayer, anchor_id: Option<&str>) -> QueryResult<usize> {
    Ok(list_layer(conn, layer, anchor_id)?.len())
}

/// Every skill reachable from the three anchors, deduplicated and ordered by
/// `dir_name`. The ordering is load-bearing: this list feeds the tool schema,
/// which sits at the front of the prompt cache prefix for every provider — a
/// non-deterministic order would invalidate that cache on each request.
pub fn resolve_available(
    conn: &mut SqliteConnection,
    project_id: Option<&str>,
    assistant_id: Option<&str>,
) -> QueryResult<Vec<SkillRow>> {
    let mut bound: Vec<String> = list_layer(conn, SkillLayer::Global, None)?;
    bound.extend(list_layer(conn, SkillLayer::Project, project_id)?);
    bound.extend(list_layer(conn, SkillLayer::Assistant, assistant_id)?);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::skill::SkillInsert;
    use crate::db::ops::skill::upsert_skill;
    use crate::db::test_db;

    fn seed_skill(conn: &mut SqliteConnection, dir_name: &str) {
        upsert_skill(
            conn,
            &SkillInsert {
                dir_name,
                llm_name: dir_name,
                llm_description: "desc",
                display_name: dir_name,
                display_description: None,
                source: "user",
                is_enabled: 1,
                is_builtin: 0,
                mtime_hash: None,
                created_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();
    }

    /// Anchor rows have to exist for real: foreign keys are enforced on every
    /// pooled connection, which is what keeps bindings from outliving their anchor.
    fn seed_project(conn: &mut SqliteConnection, id: &str) {
        use crate::db::schema::projects;
        diesel::insert_into(projects::table)
            .values((
                projects::id.eq(id),
                projects::name.eq(id),
                projects::source_type.eq("local"),
                projects::created_at.eq(1),
                projects::updated_at.eq(1),
            ))
            .execute(conn)
            .unwrap();
    }

    fn seed_assistant(conn: &mut SqliteConnection, id: &str) {
        use crate::db::schema::assistants;
        diesel::insert_into(assistants::table)
            .values((
                assistants::id.eq(id),
                assistants::name.eq(id),
                assistants::system_prompt.eq(""),
                assistants::is_default.eq(0),
                assistants::sort_order.eq(0),
                assistants::created_at.eq(1),
                assistants::updated_at.eq(1),
                assistants::context_limit.eq(128_000),
                assistants::compact_keep_recent.eq(10),
                assistants::thinking_enabled.eq(0),
                assistants::auto_compact_enabled.eq(0),
            ))
            .execute(conn)
            .unwrap();
    }

    #[test]
    fn global_binding_resolves_without_any_anchor() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_skill(&mut conn, "a");
        bind(&mut conn, SkillLayer::Global, None, "a").unwrap();

        let available = resolve_available(&mut conn, None, None).unwrap();
        assert_eq!(available.len(), 1);
    }

    #[test]
    fn layers_union_and_dedupe() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        for name in ["a", "b", "c"] {
            seed_skill(&mut conn, name);
        }
        seed_project(&mut conn, "p1");
        seed_assistant(&mut conn, "as1");
        // "a" is bound at all three layers and must still appear exactly once.
        bind(&mut conn, SkillLayer::Global, None, "a").unwrap();
        bind(&mut conn, SkillLayer::Project, Some("p1"), "a").unwrap();
        bind(&mut conn, SkillLayer::Project, Some("p1"), "b").unwrap();
        bind(&mut conn, SkillLayer::Assistant, Some("as1"), "a").unwrap();
        bind(&mut conn, SkillLayer::Assistant, Some("as1"), "c").unwrap();

        let names: Vec<String> = resolve_available(&mut conn, Some("p1"), Some("as1"))
            .unwrap()
            .into_iter()
            .map(|s| s.dir_name)
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn other_anchors_do_not_leak() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_skill(&mut conn, "mine");
        seed_skill(&mut conn, "theirs");
        seed_project(&mut conn, "p1");
        seed_project(&mut conn, "p2");
        bind(&mut conn, SkillLayer::Project, Some("p1"), "mine").unwrap();
        bind(&mut conn, SkillLayer::Project, Some("p2"), "theirs").unwrap();

        let names: Vec<String> = resolve_available(&mut conn, Some("p1"), None)
            .unwrap()
            .into_iter()
            .map(|s| s.dir_name)
            .collect();
        assert_eq!(names, vec!["mine"]);
    }

    #[test]
    fn disabled_skills_are_excluded_even_when_bound() {
        use crate::db::models::skill::SkillChangeset;
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_skill(&mut conn, "off");
        bind(&mut conn, SkillLayer::Global, None, "off").unwrap();
        crate::db::ops::skill::update_skill(
            &mut conn,
            "off",
            &SkillChangeset {
                is_enabled: Some(0),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(resolve_available(&mut conn, None, None).unwrap().is_empty());
    }

    #[test]
    fn resolve_is_deterministically_ordered() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        for name in ["zulu", "alpha", "mike"] {
            seed_skill(&mut conn, name);
            bind(&mut conn, SkillLayer::Global, None, name).unwrap();
        }

        let first: Vec<String> = resolve_available(&mut conn, None, None)
            .unwrap()
            .into_iter()
            .map(|s| s.dir_name)
            .collect();
        let second: Vec<String> = resolve_available(&mut conn, None, None)
            .unwrap()
            .into_iter()
            .map(|s| s.dir_name)
            .collect();

        assert_eq!(first, vec!["alpha", "mike", "zulu"]);
        assert_eq!(first, second);
    }

    #[test]
    fn bind_is_idempotent_and_unbind_removes() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_skill(&mut conn, "a");
        seed_assistant(&mut conn, "as1");

        bind(&mut conn, SkillLayer::Assistant, Some("as1"), "a").unwrap();
        bind(&mut conn, SkillLayer::Assistant, Some("as1"), "a").unwrap();
        assert_eq!(count_layer(&mut conn, SkillLayer::Assistant, Some("as1")).unwrap(), 1);

        unbind(&mut conn, SkillLayer::Assistant, Some("as1"), "a").unwrap();
        assert_eq!(count_layer(&mut conn, SkillLayer::Assistant, Some("as1")).unwrap(), 0);
    }

    #[test]
    fn deleting_a_skill_cascades_to_bindings() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_skill(&mut conn, "doomed");
        seed_assistant(&mut conn, "as1");
        bind(&mut conn, SkillLayer::Global, None, "doomed").unwrap();
        bind(&mut conn, SkillLayer::Assistant, Some("as1"), "doomed").unwrap();

        crate::db::ops::skill::delete_skill(&mut conn, "doomed").unwrap();

        assert_eq!(count_layer(&mut conn, SkillLayer::Global, None).unwrap(), 0);
        assert_eq!(count_layer(&mut conn, SkillLayer::Assistant, Some("as1")).unwrap(), 0);
    }
}
