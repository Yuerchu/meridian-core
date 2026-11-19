//! Legacy templates mixed data with shell source. Equivalence cannot be
//! inferred from quoting, so preserve every original byte but disable all
//! legacy rows until their author supplies a structured execution definition.

pub fn sqlite_statements() -> Vec<String> {
    vec![
        "ALTER TABLE custom_tools ADD COLUMN execution TEXT".into(),
        "UPDATE custom_tools SET is_enabled = 0".into(),
    ]
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};

    #[tokio::test]
    async fn normal_and_quoted_legacy_tools_are_disabled_but_preserved() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        for sql in super::super::m0001_baseline::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }
        let cases = [
            ("echo", Some("{{text}}")),
            ("printf '%s'", Some("\"{{text}}\"")),
            ("git status", Some("")),
            ("python tool.py", None),
            ("echo 'fixed' | cat", Some("'{{text}}'")),
        ];
        for (i, (command, template)) in cases.iter().enumerate() {
            db.execute_raw(Statement::from_sql_and_values(DbBackend::Sqlite,
                "INSERT INTO custom_tools (id,name,description,command,args_template,is_enabled,created_at,updated_at) VALUES (?,?, 'd',?,?,1,1,2)",
                [i.to_string().into(), i.to_string().into(), (*command).into(), (*template).into()],
            )).await.unwrap();
        }
        for sql in super::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }
        let rows = db
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT command,args_template,is_enabled,execution,updated_at FROM custom_tools ORDER BY id",
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), cases.len());
        for (row, (command, template)) in rows.iter().zip(cases) {
            assert_eq!(row.try_get::<String>("", "command").unwrap(), command);
            assert_eq!(
                row.try_get::<Option<String>>("", "args_template").unwrap().as_deref(),
                template
            );
            assert_eq!(row.try_get::<i32>("", "is_enabled").unwrap(), 0);
            assert!(row.try_get::<Option<String>>("", "execution").unwrap().is_none());
            assert_eq!(row.try_get::<i64>("", "updated_at").unwrap(), 2);
        }
    }
}
