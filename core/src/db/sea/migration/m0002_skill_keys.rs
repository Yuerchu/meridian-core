//! `skills.dir_name` and `skill_bindings_global.dir_name` become `NOT NULL`, and
//! a skill's key has to be a slug.
//!
//! Both columns were declared `text PRIMARY KEY` without `NOT NULL`, which in
//! SQLite (for compatibility with an old bug) lets a key hold NULL — any number
//! of them, since NULLs are distinct. And `dir_name` names a directory: on a
//! case-insensitive file system `pdf-tools` and `PDF-Tools` are one path but two
//! keys. The scanner only ever indexes slugs (`agent::skills::is_valid_slug`:
//! lowercase ASCII letters, digits and inner hyphens, 1–64 bytes), so the
//! `CHECK` below states that rule in the schema: with one spelling per path, the
//! primary key is what makes a path appear once. A test holds the `CHECK` and
//! `is_valid_slug` to the same answers.
//!
//! SQLite cannot add `NOT NULL` or a `CHECK` to a column in place, so both
//! tables are rebuilt the documented way: create the new table, copy, drop the
//! old one, rename. Rows the new rules refuse — a NULL key, a key that is not a
//! slug — are not copied; the scanner could never have produced or reached
//! them, and the next rescan would delete them anyway. The project and assistant
//! bindings that pointed at such a row go too, as `ON DELETE CASCADE` would have
//! taken them.
//!
//! backend: sqlite-only — the whole file. The rebuild is SQLite's way of
//! altering a column; a PostgreSQL schema starts from the constrained shape.

/// The rule `is_valid_slug` enforces, as a `CHECK` on `dir_name`. `GLOB`'s
/// `[^-a-z0-9]` is "anything but a hyphen, a lowercase letter or a digit": a
/// leading `-` in a class is literal.
pub const SLUG_CHECK: &str = "length(dir_name) BETWEEN 1 AND 64 \
     AND dir_name NOT GLOB '*[^-a-z0-9]*' \
     AND dir_name NOT GLOB '-*' \
     AND dir_name NOT GLOB '*-'";

/// The statements SQLite runs, in order. They must run with foreign keys off:
/// with them on, `DROP TABLE skills` deletes every row first and the binding
/// tables' `ON DELETE CASCADE` empties them. `M0002SkillKeys::up` refuses a
/// connection with foreign keys on; `diesel_test_db` runs these on an empty
/// schema, where there is nothing to cascade.
pub fn sqlite_statements() -> Vec<String> {
    vec![
        format!(
            r#"CREATE TABLE "skills_m0002" ( "dir_name" text NOT NULL PRIMARY KEY, "llm_name" text NOT NULL, "llm_description" text NOT NULL, "display_name" text NOT NULL, "display_description" text, "source" text NOT NULL DEFAULT 'user', "is_enabled" integer NOT NULL DEFAULT 1, "is_builtin" integer NOT NULL DEFAULT 0, "mtime_hash" text, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, CHECK ({SLUG_CHECK}) )"#
        ),
        format!(
            r#"INSERT INTO "skills_m0002" ("dir_name", "llm_name", "llm_description", "display_name", "display_description", "source", "is_enabled", "is_builtin", "mtime_hash", "created_at", "updated_at") SELECT "dir_name", "llm_name", "llm_description", "display_name", "display_description", "source", "is_enabled", "is_builtin", "mtime_hash", "created_at", "updated_at" FROM "skills" WHERE "dir_name" IS NOT NULL AND {SLUG_CHECK}"#
        ),
        r#"DROP TABLE "skills""#.to_owned(),
        r#"ALTER TABLE "skills_m0002" RENAME TO "skills""#.to_owned(),
        r#"CREATE INDEX "idx_skills_llm_name" ON "skills" ("llm_name" ASC)"#.to_owned(),
        r#"CREATE TABLE "skill_bindings_global_m0002" ( "dir_name" text NOT NULL PRIMARY KEY, FOREIGN KEY ("dir_name") REFERENCES "skills" ("dir_name") ON DELETE CASCADE )"#.to_owned(),
        r#"INSERT INTO "skill_bindings_global_m0002" ("dir_name") SELECT "dir_name" FROM "skill_bindings_global" WHERE "dir_name" IN (SELECT "dir_name" FROM "skills")"#.to_owned(),
        r#"DROP TABLE "skill_bindings_global""#.to_owned(),
        r#"ALTER TABLE "skill_bindings_global_m0002" RENAME TO "skill_bindings_global""#.to_owned(),
        r#"DELETE FROM "skill_bindings_project" WHERE "dir_name" NOT IN (SELECT "dir_name" FROM "skills")"#.to_owned(),
        r#"DELETE FROM "skill_bindings_assistant" WHERE "dir_name" NOT IN (SELECT "dir_name" FROM "skills")"#.to_owned(),
    ]
}

/// The tables whose foreign keys the rebuild touches, checked before the
/// transaction commits.
pub const CHECKED_TABLES: &[&str] = &[
    "skills",
    "skill_bindings_global",
    "skill_bindings_project",
    "skill_bindings_assistant",
];

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
    use sea_orm_migration::{MigrationTrait, MigratorTrait};

    use super::super::{M0001Baseline, Migrator};
    use crate::db::sea::{bridge, execute_for_tests, memory_connection, sea_test_db};

    const BINDINGS: [&str; 3] = [
        "skill_bindings_global",
        "skill_bindings_project",
        "skill_bindings_assistant",
    ];

    /// The schema as it was before this migration.
    struct BaselineOnly;

    #[async_trait::async_trait]
    impl MigratorTrait for BaselineOnly {
        fn migrations() -> Vec<Box<dyn MigrationTrait>> {
            vec![Box::new(M0001Baseline)]
        }
    }

    async fn column(conn: &DatabaseConnection, sql: &str) -> Vec<Option<String>> {
        conn.query_all_raw(Statement::from_string(DbBackend::Sqlite, sql))
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.try_get_by_index::<Option<String>>(0).unwrap())
            .collect()
    }

    async fn applied(conn: &DatabaseConnection) -> bool {
        column(conn, "SELECT version FROM seaql_migrations")
            .await
            .contains(&Some("m0002_skill_keys".into()))
    }

    /// A database at the baseline holding what the old schema allowed: a
    /// skill with a NULL key, one whose key is not a slug, and bindings to
    /// each of them beside a good one.
    async fn before() -> DatabaseConnection {
        let conn = memory_connection().await;
        conn.execute_unprepared("PRAGMA foreign_keys = OFF").await.unwrap();
        bridge::migrate_with::<BaselineOnly>(&conn).await.unwrap();
        conn.execute_unprepared(
            "INSERT INTO projects (id, name, source_type, created_at, updated_at) VALUES ('p1', 'P', 'local', 1, 1);
             INSERT INTO assistants (id, name, created_at, updated_at) VALUES ('a1', 'A', 1, 1);
             INSERT INTO skills (dir_name, llm_name, llm_description, display_name, display_description,
                                 source, is_enabled, is_builtin, mtime_hash, created_at, updated_at)
                 VALUES ('ok', 'ok', 'kept', 'shown name', 'shown', 'official', 0, 1, 'h', 10, 20),
                        (NULL, 'null', 'd', 'n', NULL, 'user', 1, 0, NULL, 1, 1),
                        ('Bad', 'bad', 'd', 'b', NULL, 'user', 1, 0, NULL, 1, 1);
             INSERT INTO skill_bindings_global (dir_name) VALUES ('ok'), (NULL), ('Bad');
             INSERT INTO skill_bindings_project (project_id, dir_name) VALUES ('p1', 'ok'), ('p1', 'Bad');
             INSERT INTO skill_bindings_assistant (assistant_id, dir_name) VALUES ('a1', 'ok'), ('a1', 'Bad');",
        )
        .await
        .unwrap();
        conn
    }

    async fn only_ok_is_bound(conn: &DatabaseConnection) {
        for table in BINDINGS {
            assert_eq!(
                column(conn, &format!("SELECT dir_name FROM {table}")).await,
                [Some("ok".into())],
                "{table}"
            );
        }
    }

    #[tokio::test]
    async fn rows_the_new_rules_refuse_are_dropped_and_the_rest_kept_exactly() {
        let conn = before().await;
        bridge::migrate(&conn).await.unwrap();

        assert!(applied(&conn).await);
        let kept = column(
            &conn,
            "SELECT dir_name || '|' || llm_name || '|' || llm_description || '|' || display_name || '|' ||
                    display_description || '|' || source || '|' || is_enabled || '|' || is_builtin || '|' ||
                    mtime_hash || '|' || created_at || '|' || updated_at FROM skills",
        )
        .await;
        assert_eq!(kept, [Some("ok|ok|kept|shown name|shown|official|0|1|h|10|20".into())]);
        only_ok_is_bound(&conn).await;
        assert!(
            column(&conn, "SELECT name FROM sqlite_master WHERE name LIKE '%m0002%'")
                .await
                .is_empty(),
            "the temporary tables are gone"
        );

        // The foreign keys still point at the rebuilt table and still hold.
        conn.execute_unprepared("PRAGMA foreign_keys = ON").await.unwrap();
        assert!(
            conn.execute_unprepared("INSERT INTO skill_bindings_global VALUES ('ghost')")
                .await
                .is_err()
        );
        conn.execute_unprepared("DELETE FROM skills WHERE dir_name = 'ok'")
            .await
            .unwrap();
        for table in BINDINGS {
            let left = column(&conn, &format!("SELECT dir_name FROM {table}")).await;
            assert!(left.is_empty(), "{table}: {left:?}");
        }
    }

    /// With foreign keys on, dropping `skills` would cascade into every
    /// binding; the migration refuses before touching anything.
    #[tokio::test]
    async fn with_foreign_keys_on_it_refuses_and_changes_nothing() {
        let conn = before().await;
        conn.execute_unprepared(
            "DELETE FROM skills WHERE dir_name IS NULL OR dir_name = 'Bad';
             DELETE FROM skill_bindings_global WHERE dir_name IS NULL OR dir_name = 'Bad';
             DELETE FROM skill_bindings_project WHERE dir_name = 'Bad';
             DELETE FROM skill_bindings_assistant WHERE dir_name = 'Bad';",
        )
        .await
        .unwrap();
        conn.execute_unprepared("PRAGMA foreign_keys = ON").await.unwrap();
        let schema = "SELECT sql FROM sqlite_master WHERE name IN ('skills', 'skill_bindings_global') ORDER BY name";
        let schema_before = column(&conn, schema).await;

        let error = Migrator::up(&conn, None).await.unwrap_err();
        assert!(error.to_string().contains("foreign keys off"), "{error}");

        assert_eq!(column(&conn, schema).await, schema_before);
        assert!(!applied(&conn).await);
        only_ok_is_bound(&conn).await;
    }

    /// The `CHECK` and `agent::skills::is_valid_slug` answer every name the
    /// same way: the scanner indexes exactly what the schema accepts.
    #[tokio::test]
    async fn the_check_and_is_valid_slug_agree() {
        let db = sea_test_db().await;
        let long = "a".repeat(64);
        let too_long = "a".repeat(65);
        let names = [
            "pdf-tools",
            "a1",
            "0",
            "a--b",
            long.as_str(),
            "PDF-tools",
            "-x",
            "x-",
            "",
            "a b",
            "a.b",
            "a_b",
            "\u{fc}",
            "a]b",
            "a^b",
            "a/b",
            too_long.as_str(),
        ];
        let insert = |name: &str| {
            format!(
                "INSERT INTO skills (dir_name, llm_name, llm_description, display_name, created_at, updated_at)
                 VALUES ('{name}', 'n', 'd', 'x', 1, 1)"
            )
        };
        for name in names {
            let inserted = execute_for_tests(&db, &insert(name)).await;
            assert_eq!(
                inserted.is_ok(),
                crate::agent::skills::is_valid_slug(name),
                "{name:?}: the CHECK said {inserted:?}"
            );
        }
        assert!(
            execute_for_tests(&db, &insert("pdf-tools")).await.is_err(),
            "the same path twice"
        );
    }

    /// A binding whose assistant was deleted while foreign keys were off
    /// violates another key, not one this migration touches. Real databases
    /// have them (the rehearsal on a user database found two); the migration
    /// neither stops on them nor deletes them.
    #[tokio::test]
    async fn an_older_violation_of_another_key_neither_blocks_nor_is_removed() {
        let conn = before().await;
        conn.execute_unprepared("INSERT INTO skill_bindings_assistant (assistant_id, dir_name) VALUES ('gone', 'ok')")
            .await
            .unwrap();
        bridge::migrate(&conn).await.unwrap();

        assert!(applied(&conn).await);
        assert_eq!(
            column(
                &conn,
                "SELECT assistant_id FROM skill_bindings_assistant ORDER BY assistant_id"
            )
            .await,
            [Some("a1".into()), Some("gone".into())]
        );
    }
}
