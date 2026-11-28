//! PostgreSQL test database: a translated copy of the production schema.
//!
//! Behind the `postgres` feature. With `MERIDIAN_PG_URL` set, `sea_test_db` /
//! `file_test_db` hand out a fresh schema per test. The URL must name a
//! disposable database: a run drops leftover `t_…` / `probe_…` test schemas.
//! Column widths come from the entities, because sqlx on PostgreSQL decodes
//! INT4 and INT8 strictly where SQLite never cared.
//!
//! This is the S0a translator, not the S0c migrator. CHECK expressions and
//! partial-index `WHERE` clauses that PostgreSQL refuses are skipped and
//! logged; S0c replaces them with registered `pg_text`.

use std::collections::HashMap;
use std::str::FromStr;

use sea_orm::sea_query::ColumnType;
use sea_orm::{ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, IdenStatic, Iterable};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::OnceCell;

use super::super::introspect::{Schema, affinity};
use crate::db::entity::*;

pub fn url() -> Option<String> {
    std::env::var("MERIDIAN_PG_URL").ok().filter(|u| !u.is_empty())
}

fn pg_type(column_type: &ColumnType) -> &'static str {
    use ColumnType::*;
    match column_type {
        TinyInteger | SmallInteger | Integer | TinyUnsigned | SmallUnsigned | Unsigned => "INTEGER",
        BigInteger | BigUnsigned => "BIGINT",
        Boolean => "BOOLEAN",
        Float => "REAL",
        Double => "DOUBLE PRECISION",
        Blob | Binary(_) | VarBinary(_) => "BYTEA",
        _ => "TEXT",
    }
}

type Widths = HashMap<(String, String), &'static str>;

fn widths_of<E: EntityTrait>(out: &mut Widths) {
    let table = E::default().table_name().to_owned();
    for column in E::Column::iter() {
        out.insert(
            (table.clone(), column.as_str().to_owned()),
            pg_type(column.def().get_column_type()),
        );
    }
}

fn widths() -> Widths {
    let mut w = Widths::new();
    macro_rules! all { ($($m:ident),* $(,)?) => { $( widths_of::<$m::Entity>(&mut w); )* } }
    all!(
        acp_context_delivery,
        acp_session,
        acp_session_notice,
        assistant,
        assistant_emoji_pack,
        audit_message,
        background_task,
        board_task,
        cached_model,
        composer_draft,
        conversation,
        custom_tool,
        dream_run,
        dream_watermark,
        emoji,
        emoji_embedding,
        emoji_pack,
        journal_blob,
        journal_file,
        journal_version,
        mcp_server,
        media_generation,
        memory,
        memory_proposal,
        memory_subject,
        message,
        message_context_item,
        message_sticker,
        mode_artifact,
        model_config,
        model_profile,
        notification_alert_state,
        notification_webhook,
        plan_comment,
        plan_document,
        plan_materialization,
        plan_review_delivery,
        plan_review_draft,
        plan_review_session,
        plan_revision,
        preference,
        project,
        provider,
        queued_prompt,
        queued_prompt_context_item,
        redaction_rule,
        skill,
        skill_binding_assistant,
        skill_binding_global,
        skill_binding_project,
        todo_item,
        todo_list,
        tool_category,
        tool_preset,
        turn,
        voice_blob,
        voice_clip,
        voice_sender_optout,
    );
    w
}

fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Deleting a subtree leaves this key dangling until ON DELETE SET NULL runs.
/// PostgreSQL checks it on the message-count trigger's conversation update;
/// SQLite does not, because that update does not touch the key. Commit-time
/// checking makes the two agree without weakening the constraint.
const DEFERRED_FOREIGN_KEYS: &[(&str, &str)] = &[("conversations", "head_message_id")];

const TRIGGERS: &str = r#"
CREATE FUNCTION trg_messages_count_insert() RETURNS trigger AS $$ BEGIN
  UPDATE conversations SET message_count = message_count + 1, updated_at = NEW.created_at WHERE id = NEW.conversation_id;
  RETURN NULL; END $$ LANGUAGE plpgsql;
CREATE TRIGGER trg_messages_count_insert AFTER INSERT ON messages FOR EACH ROW EXECUTE FUNCTION trg_messages_count_insert();
CREATE FUNCTION trg_messages_count_delete() RETURNS trigger AS $$ BEGIN
  UPDATE conversations SET message_count = message_count - 1 WHERE id = OLD.conversation_id;
  RETURN NULL; END $$ LANGUAGE plpgsql;
CREATE TRIGGER trg_messages_count_delete AFTER DELETE ON messages FOR EACH ROW EXECUTE FUNCTION trg_messages_count_delete();
CREATE FUNCTION trg_messages_sort_order() RETURNS trigger AS $$ BEGIN
  IF NEW.sort_order = 0 THEN
    UPDATE messages SET sort_order = (SELECT COALESCE(MAX(sort_order), 0) + 1 FROM messages WHERE conversation_id = NEW.conversation_id) WHERE id = NEW.id;
  END IF; RETURN NULL; END $$ LANGUAGE plpgsql;
CREATE TRIGGER trg_messages_sort_order AFTER INSERT ON messages FOR EACH ROW EXECUTE FUNCTION trg_messages_sort_order();
"#;

pub struct Ddl {
    pub statements: Vec<String>,
    pub refused: Vec<(String, String)>,
}

fn translate(schema: &Schema, widths: &Widths) -> (Vec<String>, Vec<String>) {
    let mut required = Vec::new();
    let mut optional = Vec::new();
    let mut fks = Vec::new();
    for t in &schema.tables {
        let mut cols = Vec::new();
        for c in &t.columns {
            let ty =
                widths
                    .get(&(t.name.clone(), c.name.clone()))
                    .copied()
                    .unwrap_or(match affinity(&c.declared_type) {
                        "INTEGER" => "BIGINT",
                        "REAL" => "DOUBLE PRECISION",
                        "BLOB" => "BYTEA",
                        _ => "TEXT",
                    });
            let rowid =
                t.primary_key.len() == 1 && t.primary_key[0] == c.name && affinity(&c.declared_type) == "INTEGER";
            let mut s = format!("{} {}", q(&c.name), ty);
            if rowid {
                s.push_str(" GENERATED BY DEFAULT AS IDENTITY");
            }
            if c.not_null {
                s.push_str(" NOT NULL");
            }
            if let Some(d) = &c.default {
                s.push_str(&format!(" DEFAULT {d}"));
            }
            cols.push(s);
        }
        if !t.primary_key.is_empty() {
            cols.push(format!(
                "PRIMARY KEY ({})",
                t.primary_key.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
            ));
        }
        for u in &t.uniques {
            cols.push(format!(
                "UNIQUE ({})",
                u.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
            ));
        }
        required.push(format!("CREATE TABLE {} ({})", q(&t.name), cols.join(", ")));
        for (i, check) in t.checks.iter().enumerate() {
            optional.push(format!(
                "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({check})",
                q(&t.name),
                q(&format!("ck_{}_{i}", t.name))
            ));
        }
        for fk in &t.foreign_keys {
            let from = fk.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
            let to = fk
                .references_columns
                .iter()
                .map(|c| q(c))
                .collect::<Vec<_>>()
                .join(", ");
            let deferred = DEFERRED_FOREIGN_KEYS
                .iter()
                .any(|(table, column)| *table == t.name && fk.columns == [*column]);
            fks.push(format!(
                "ALTER TABLE {} ADD FOREIGN KEY ({from}) REFERENCES {} ({to}) ON DELETE {} ON UPDATE {}{}",
                q(&t.name),
                q(&fk.references_table),
                fk.on_delete,
                fk.on_update,
                if deferred { " DEFERRABLE INITIALLY DEFERRED" } else { "" }
            ));
        }
    }
    required.extend(fks);
    for i in &schema.indexes {
        let cols = i
            .columns
            .iter()
            .map(|c| format!("{}{}", q(&c.name), if c.descending { " DESC" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ");
        let s = format!(
            "CREATE {}INDEX {} ON {} ({cols})",
            if i.unique { "UNIQUE " } else { "" },
            q(&i.name),
            q(&i.table)
        );
        match &i.where_clause {
            Some(w) => optional.push(format!("{s} WHERE {w}")),
            None => required.push(s),
        }
    }
    for t in &schema.triggers {
        assert!(TRIGGERS.contains(&t.name), "unhandled trigger {}", t.name);
    }
    required.push(TRIGGERS.to_owned());
    (required, optional)
}

async fn admin() -> DatabaseConnection {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url().unwrap())
        .await
        .expect("connect admin");
    sea_orm::SqlxPostgresConnector::from_sqlx_postgres_pool(pool)
}

static DDL: OnceCell<Ddl> = OnceCell::const_new();

async fn ddl() -> &'static Ddl {
    DDL.get_or_init(|| async {
        drop_leftover_test_schemas().await;
        let sqlite = super::super::memory_connection().await;
        sqlite.execute_unprepared("PRAGMA foreign_keys = OFF").await.unwrap();
        super::super::bridge::migrate(&sqlite).await.unwrap();
        let schema = Schema::read(&sqlite).await.unwrap();
        let (required, optional) = translate(&schema, &widths());
        let conn = admin().await;
        let probe = format!("probe_{}", uuid::Uuid::new_v4().simple());
        conn.execute_unprepared(&format!("CREATE SCHEMA {probe}; SET search_path TO {probe}"))
            .await
            .unwrap();
        let mut statements = Vec::new();
        for s in required {
            conn.execute_unprepared(&s)
                .await
                .unwrap_or_else(|e| panic!("required DDL refused: {e}\n{s}"));
            statements.push(s);
        }
        let mut refused = Vec::new();
        for s in optional {
            match conn.execute_unprepared(&s).await {
                Ok(_) => statements.push(s),
                Err(e) => refused.push((s, e.to_string())),
            }
        }
        conn.execute_unprepared(&format!("DROP SCHEMA {probe} CASCADE"))
            .await
            .unwrap();
        for (s, e) in &refused {
            eprintln!("postgres translator refused: {e} :: {s}");
        }
        Ddl { statements, refused }
    })
    .await
}

/// A test schema is never dropped from Drop, which cannot await. Sweep at the
/// start of a run instead of accumulating whole databases until the disk fills.
async fn drop_leftover_test_schemas() {
    let conn = admin().await;
    let names = conn
        .query_all_raw(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Postgres,
            "SELECT nspname FROM pg_namespace WHERE nspname LIKE 't\\_%' OR nspname LIKE 'probe\\_%'",
        ))
        .await
        .expect("list test schemas")
        .iter()
        .map(|row| row.try_get::<String>("", "nspname").expect("schema name"))
        .collect::<Vec<_>>();
    for name in &names {
        conn.execute_unprepared(&format!("DROP SCHEMA {name} CASCADE"))
            .await
            .expect("drop a leftover test schema");
    }
    if !names.is_empty() {
        eprintln!("dropped {} leftover test schema(s)", names.len());
    }
    conn.close().await.ok();
}

fn lock_key_for(schema: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    schema.hash(&mut hasher);
    hasher.finish() as i64
}

/// Server CI injects its own extension DDL through this hook.
pub async fn from_ddl(statements: Vec<String>, max: u32) -> super::super::cap::Db {
    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());
    let conn = admin().await;
    conn.execute_unprepared(&format!(
        "CREATE SCHEMA {schema}; SET search_path TO {schema}; {}",
        statements.join(";\n")
    ))
    .await
    .unwrap_or_else(|e| panic!("building {schema}: {e}"));
    conn.close().await.ok();
    let options = PgConnectOptions::from_str(&url().unwrap())
        .unwrap()
        .options([("search_path", schema.as_str())]);
    let pool = PgPoolOptions::new()
        .max_connections(max)
        .connect_with(options)
        .await
        .expect("connect test pool");
    super::super::cap::Db::new_postgres(
        sea_orm::SqlxPostgresConnector::from_sqlx_postgres_pool(pool),
        lock_key_for(&schema),
    )
}

pub async fn fresh(max: u32) -> super::super::cap::Db {
    let ddl = ddl().await;
    from_ddl(ddl.statements.clone(), max).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lock is a constructor invariant, not a convention. Without this,
    /// `Db::new(pg_connection)` is a quiet way back to READ COMMITTED writes.
    #[tokio::test]
    async fn postgres_pool_cannot_be_constructed_without_a_writer_lock_key() {
        if url().is_none() {
            return;
        }
        let conn = admin().await;
        let bypass = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::db::sea::cap::Db::new(conn)));
        assert!(bypass.is_err(), "Db::new accepted PostgreSQL without a lock key");
    }
}
