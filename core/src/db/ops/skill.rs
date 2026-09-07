use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::skill::{SkillChangeset, SkillInsert, SkillRow};
use crate::db::schema::skills;

pub fn list_skills(conn: &mut SqliteConnection) -> QueryResult<Vec<SkillRow>> {
    skills::table.order(skills::dir_name.asc()).load::<SkillRow>(conn)
}

pub fn get_skill(conn: &mut SqliteConnection, dir_name: &str) -> QueryResult<SkillRow> {
    skills::table.find(dir_name).first::<SkillRow>(conn)
}

/// Insert or refresh the index row for a scanned skill directory. The filesystem
/// is the source of truth, so a rescan overwrites the parsed fields but leaves
/// user-controlled state (`is_enabled`) alone.
pub fn upsert_skill(conn: &mut SqliteConnection, new: &SkillInsert) -> QueryResult<SkillRow> {
    let existing = skills::table.find(new.dir_name).first::<SkillRow>(conn).optional()?;
    match existing {
        Some(_) => {
            diesel::update(skills::table.find(new.dir_name))
                .set((
                    skills::llm_name.eq(new.llm_name),
                    skills::llm_description.eq(new.llm_description),
                    skills::display_name.eq(new.display_name),
                    skills::display_description.eq(new.display_description),
                    skills::source.eq(new.source),
                    skills::is_builtin.eq(new.is_builtin),
                    skills::mtime_hash.eq(new.mtime_hash),
                    skills::updated_at.eq(new.updated_at),
                ))
                .execute(conn)?;
        }
        None => {
            diesel::insert_into(skills::table).values(new).execute(conn)?;
        }
    }
    skills::table.find(new.dir_name).first::<SkillRow>(conn)
}

pub fn update_skill(conn: &mut SqliteConnection, dir_name: &str, changeset: &SkillChangeset) -> QueryResult<SkillRow> {
    diesel::update(skills::table.find(dir_name))
        .set(changeset)
        .execute(conn)?;
    skills::table.find(dir_name).first::<SkillRow>(conn)
}

pub fn delete_skill(conn: &mut SqliteConnection, dir_name: &str) -> QueryResult<()> {
    diesel::delete(skills::table.find(dir_name)).execute(conn)?;
    Ok(())
}

/// Drop index rows whose directory no longer exists on disk. Bindings clean up
/// through ON DELETE CASCADE.
pub fn delete_missing(conn: &mut SqliteConnection, present: &[String]) -> QueryResult<usize> {
    diesel::delete(skills::table.filter(skills::dir_name.ne_all(present))).execute(conn)
}

/// Skill directories sharing an `llm_name`. A model addresses skills by that
/// name, so a duplicate makes the name ambiguous and it must not be offered.
#[cfg(test)]
pub fn find_name_clashes(conn: &mut SqliteConnection, llm_name: &str) -> QueryResult<Vec<String>> {
    skills::table
        .filter(skills::llm_name.eq(llm_name))
        .select(skills::dir_name)
        .order(skills::dir_name.asc())
        .load::<String>(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn make_skill<'a>(dir_name: &'a str, llm_name: &'a str) -> SkillInsert<'a> {
        SkillInsert {
            dir_name,
            llm_name,
            llm_description: "Does a thing",
            display_name: "A Skill",
            display_description: None,
            source: "user",
            is_enabled: 1,
            is_builtin: 0,
            mtime_hash: Some("abc"),
            created_at: 1000,
            updated_at: 1000,
        }
    }

    #[test]
    fn upsert_inserts_then_updates() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();

        let created = upsert_skill(&mut conn, &make_skill("my-skill", "my-skill")).unwrap();
        assert_eq!(created.llm_description, "Does a thing");

        let mut changed = make_skill("my-skill", "my-skill");
        changed.llm_description = "Does another thing";
        changed.updated_at = 2000;
        let updated = upsert_skill(&mut conn, &changed).unwrap();

        assert_eq!(updated.llm_description, "Does another thing");
        assert_eq!(list_skills(&mut conn).unwrap().len(), 1, "upsert must not duplicate");
    }

    #[test]
    fn upsert_preserves_user_toggled_enabled_state() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        upsert_skill(&mut conn, &make_skill("s", "s")).unwrap();
        update_skill(
            &mut conn,
            "s",
            &SkillChangeset {
                is_enabled: Some(0),
                ..Default::default()
            },
        )
        .unwrap();

        upsert_skill(&mut conn, &make_skill("s", "s")).unwrap();

        assert_eq!(get_skill(&mut conn, "s").unwrap().is_enabled, 0);
    }

    #[test]
    fn list_is_sorted_by_dir_name() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        upsert_skill(&mut conn, &make_skill("zebra", "zebra")).unwrap();
        upsert_skill(&mut conn, &make_skill("alpha", "alpha")).unwrap();
        upsert_skill(&mut conn, &make_skill("middle", "middle")).unwrap();

        let names: Vec<String> = list_skills(&mut conn)
            .unwrap()
            .into_iter()
            .map(|s| s.dir_name)
            .collect();
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn delete_missing_removes_only_absent_dirs() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        upsert_skill(&mut conn, &make_skill("kept", "kept")).unwrap();
        upsert_skill(&mut conn, &make_skill("gone", "gone")).unwrap();

        delete_missing(&mut conn, &["kept".to_string()]).unwrap();

        let names: Vec<String> = list_skills(&mut conn)
            .unwrap()
            .into_iter()
            .map(|s| s.dir_name)
            .collect();
        assert_eq!(names, vec!["kept"]);
    }

    #[test]
    fn find_name_clashes_reports_every_dir_sharing_a_name() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        upsert_skill(&mut conn, &make_skill("mine-pdf", "pdf-tools")).unwrap();
        upsert_skill(&mut conn, &make_skill("theirs-pdf", "pdf-tools")).unwrap();
        upsert_skill(&mut conn, &make_skill("solo", "solo")).unwrap();

        assert_eq!(find_name_clashes(&mut conn, "pdf-tools").unwrap().len(), 2);
        assert_eq!(find_name_clashes(&mut conn, "solo").unwrap().len(), 1);
    }
}
