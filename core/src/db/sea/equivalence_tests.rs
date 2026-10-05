//! The baseline against the 65 migrations it replaces.
//!
//! Three comparisons, because no single one sees everything. The pragmas see
//! columns, keys, foreign keys and indexes, and are compared as structure.
//! `CHECK` clauses, a partial index's filter and trigger bodies exist only as
//! text, and are compared as text — with a count of `CHECK` tokens in the
//! `CREATE TABLE` statements beside it, so an extractor that missed a clause
//! on both sides would still be caught. And a set of rows each constraint
//! must refuse is thrown at both databases, which is the part that reads the
//! schema the way the application does.

use pretty_assertions::assert_eq;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr};
use sea_orm_migration::{MigrationTrait, MigratorTrait};

use super::baseline_gen;
use super::bridge::migrate_with;
use super::introspect::{Schema, strip_line_comments};
use super::legacy::replay_all;
use super::memory_connection;
use super::migration::M0001Baseline;

/// The baseline and nothing after it: this comparison is against the schema
/// as the migrations left it, and later migrations move only the live side.
struct BaselineOnly;

#[async_trait::async_trait]
impl MigratorTrait for BaselineOnly {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(M0001Baseline)]
    }
}

async fn replayed() -> DatabaseConnection {
    let conn = memory_connection().await;
    conn.execute_unprepared("PRAGMA foreign_keys = OFF").await.unwrap();
    replay_all(&conn).await.unwrap();
    conn
}

async fn baseline() -> DatabaseConnection {
    let conn = memory_connection().await;
    conn.execute_unprepared("PRAGMA foreign_keys = OFF").await.unwrap();
    migrate_with::<BaselineOnly>(&conn).await.unwrap();
    conn
}

/// What the migrations build, counted once so the comparison below cannot
/// pass by both sides being empty.
const TABLES: usize = 52;
const INDEXES: usize = 55;
const PARTIAL_INDEXES: usize = 12;
const TRIGGERS: usize = 3;
const CHECKS: usize = 75;

#[tokio::test]
async fn the_baseline_builds_what_the_migrations_built() {
    let expected = Schema::read(&replayed().await).await.unwrap();
    assert_eq!(expected.tables.len(), TABLES);
    assert_eq!(expected.indexes.len(), INDEXES);
    assert_eq!(
        expected.indexes.iter().filter(|i| i.where_clause.is_some()).count(),
        PARTIAL_INDEXES
    );
    assert_eq!(expected.triggers.len(), TRIGGERS);
    assert_eq!(expected.tables.iter().map(|t| t.checks.len()).sum::<usize>(), CHECKS);

    let actual = Schema::read(&baseline().await).await.unwrap();
    assert_eq!(actual.normalized(), expected.normalized());
}

/// The extractor against the raw text: every `CHECK` token in a `CREATE TABLE`
/// is one clause it found, on both sides.
#[tokio::test]
async fn every_check_in_the_create_statements_was_extracted() {
    for conn in [replayed().await, baseline().await] {
        let schema = Schema::read(&conn).await.unwrap();
        let sql: Vec<String> = conn
            .query_all_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
                 AND name NOT IN ('seaql_migrations', '__diesel_schema_migrations')",
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get_by_index::<String>(0).unwrap())
            .collect();
        let tokens: usize = sql
            .iter()
            .map(|sql| {
                strip_line_comments(sql)
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .filter(|token| token.eq_ignore_ascii_case("CHECK"))
                    .count()
            })
            .sum();
        let extracted: usize = schema.tables.iter().map(|t| t.checks.len()).sum();
        assert_eq!(extracted, tokens);
        assert_eq!(extracted, CHECKS);
    }
}

/// The checked-in file is what the generator writes from today's migrations:
/// a change to either shows up here before it shows up as a difference in
/// behaviour.
#[tokio::test]
async fn the_checked_in_baseline_is_what_the_generator_writes_today() {
    let schema = Schema::read(&replayed().await).await.unwrap();
    let generated = baseline_gen::render(&schema);
    let checked_in = include_str!("migration/m0001_baseline.rs").replace("\r\n", "\n");
    assert_eq!(generated, checked_in);
}

/// Rows the schema must refuse, each for one constraint, and the kind of
/// constraint that must refuse it: a case that failed for another reason —
/// a column that does not exist, say — would not count.
const REFUSED: &[(&str, &str, &str)] = &[
    (
        "money with a trailing dot",
        "INSERT INTO model_profiles (id, name, context_window, compact_threshold, input_price, created_at, updated_at) \
         VALUES ('r1', 'n', 1, 1, '1.', 1, 1)",
        "CHECK",
    ),
    (
        "money with a leading zero",
        "INSERT INTO model_profiles (id, name, context_window, compact_threshold, output_price, created_at, updated_at) \
         VALUES ('r2', 'n', 1, 1, '01', 1, 1)",
        "CHECK",
    ),
    (
        "money with a trailing zero",
        "INSERT INTO model_profiles (id, name, context_window, compact_threshold, cache_read_price, created_at, updated_at) \
         VALUES ('r3', 'n', 1, 1, '1.0', 1, 1)",
        "CHECK",
    ),
    (
        "money with a sign",
        "INSERT INTO model_profiles (id, name, context_window, compact_threshold, cache_write_price, created_at, updated_at) \
         VALUES ('r4', 'n', 1, 1, '-1', 1, 1)",
        "CHECK",
    ),
    (
        "pricing tiers that are not an array",
        "INSERT INTO model_profiles (id, name, context_window, compact_threshold, pricing_tiers, created_at, updated_at) \
         VALUES ('r5', 'n', 1, 1, '{}', 1, 1)",
        "CHECK",
    ),
    (
        "a draft at revision zero",
        "INSERT INTO composer_drafts (slot, conversation_id, body, attachments, conversation_refs, revision, created_at, updated_at) \
         VALUES ('new', NULL, '', '[]', '[]', 0, 1, 1)",
        "CHECK",
    ),
    (
        "a draft whose slot names another conversation",
        "INSERT INTO composer_drafts (slot, conversation_id, body, attachments, conversation_refs, revision, created_at, updated_at) \
         VALUES ('conversation:a', 'b', '', '[]', '[]', 1, 1, 1)",
        "CHECK",
    ),
    (
        "a transcript without a source",
        "INSERT INTO voice_clips (id, blob_id, bot_self_id, source_type, source_id, sender_id, transcript, transcript_source, created_at, updated_at) \
         VALUES ('r6', 'b', 1, 'private', 's', 'u', 'hi', NULL, 1, 1)",
        "CHECK",
    ),
    (
        "an external change claiming a conversation",
        "INSERT INTO journal_versions (id, file_id, seq, op, source, conversation_id, created_at) \
         VALUES ('r7', 'f', 1, 'external', 'external', 'c', 1)",
        "CHECK",
    ),
    (
        "a journal op nobody defined",
        "INSERT INTO journal_versions (id, file_id, seq, op, source, created_at) \
         VALUES ('r8', 'f', 1, 'sync', 'native', 1)",
        "CHECK",
    ),
    (
        "a turn with an unknown trigger",
        "INSERT INTO turns (id, conversation_id, origin, status, started_at, updated_at, trigger) \
         VALUES ('r9', 'c', 'desktop', 'done', 1, 1, 'cron')",
        "CHECK",
    ),
    (
        "a plan document in an unknown state",
        "INSERT INTO plan_documents (id, conversation_id, state, file_rel_path, created_at, updated_at) \
         VALUES ('r10', 'c', 'lost', 'p.md', 1, 1)",
        "CHECK",
    ),
    (
        "a preference without a value",
        "INSERT INTO preferences (key, updated_at) VALUES ('k', 1)",
        "NOT NULL",
    ),
    (
        "two configs for one model",
        "INSERT INTO model_configs (id, provider_id, model_id, profile_id, created_at, updated_at) VALUES ('u1', 'p', 'm', 'pr', 1, 1); \
         INSERT INTO model_configs (id, provider_id, model_id, profile_id, created_at, updated_at) VALUES ('u2', 'p', 'm', 'pr', 1, 1)",
        "UNIQUE",
    ),
    (
        "two live memories with one key in one scope",
        "INSERT INTO memories (id, scope_type, scope_id, key, content, memory_type, origin, visibility, created_at, updated_at) \
         VALUES ('u3', 'project', 'p', 'k', 'a', 'general', 'desktop', 'normal', 1, 1); \
         INSERT INTO memories (id, scope_type, scope_id, key, content, memory_type, origin, visibility, created_at, updated_at) \
         VALUES ('u4', 'project', 'p', 'k', 'b', 'general', 'desktop', 'normal', 1, 1)",
        "UNIQUE",
    ),
];

/// The same rows with the one fault removed, so a refusal above is known to
/// be about the constraint and not about the row.
const ACCEPTED: &[&str] = &[
    "INSERT INTO model_profiles (id, name, context_window, compact_threshold, input_price, pricing_tiers, created_at, updated_at) \
     VALUES ('a1', 'n', 1, 1, '0.5', '[]', 1, 1)",
    "INSERT INTO composer_drafts (slot, conversation_id, body, attachments, conversation_refs, revision, created_at, updated_at) \
     VALUES ('new', NULL, '', '[]', '[]', 1, 1, 1)",
    "INSERT INTO voice_clips (id, blob_id, bot_self_id, source_type, source_id, sender_id, transcript, transcript_source, created_at, updated_at) \
     VALUES ('a2', 'b', 1, 'private', 's', 'u', 'hi', 'adapter', 1, 1)",
    "INSERT INTO journal_versions (id, file_id, seq, op, source, conversation_id, created_at) \
     VALUES ('a3', 'f', 1, 'write', 'native', 'c', 1)",
    "INSERT INTO turns (id, conversation_id, origin, status, started_at, updated_at, trigger) \
     VALUES ('a4', 'c', 'desktop', 'done', 1, 1, 'user')",
    "INSERT INTO plan_documents (id, conversation_id, state, file_rel_path, created_at, updated_at) \
     VALUES ('a5', 'c', 'drafting', 'p.md', 1, 1)",
    "INSERT INTO memories (id, scope_type, scope_id, key, content, memory_type, origin, visibility, created_at, updated_at, deleted_at) \
     VALUES ('a6', 'project', 'p', 'k', 'a', 'general', 'desktop', 'normal', 1, 1, 2); \
     INSERT INTO memories (id, scope_type, scope_id, key, content, memory_type, origin, visibility, created_at, updated_at) \
     VALUES ('a7', 'project', 'p', 'k', 'b', 'general', 'desktop', 'normal', 1, 1)",
];

/// Rows a foreign key must refuse, with the keys on.
const REFUSED_BY_A_KEY: &[(&str, &str)] = &[
    (
        "a message in a conversation that does not exist",
        "INSERT INTO messages (id, conversation_id, role, created_at) VALUES ('k1', 'nope', 'user', 1)",
    ),
    (
        "a draft for a conversation that does not exist",
        "INSERT INTO composer_drafts (slot, conversation_id, body, attachments, conversation_refs, revision, created_at, updated_at) \
         VALUES ('conversation:x', 'x', '', '[]', '[]', 1, 1, 1)",
    ),
];

async fn outcome(conn: &DatabaseConnection, sql: &str) -> Result<(), DbErr> {
    conn.execute_unprepared(sql).await.map(|_| ())
}

#[tokio::test]
async fn both_schemas_refuse_the_same_rows_for_the_same_reasons() {
    for (side, conn) in [("replayed", replayed().await), ("baseline", baseline().await)] {
        for sql in ACCEPTED {
            outcome(&conn, sql)
                .await
                .unwrap_or_else(|error| panic!("{side} refused a valid row: {error}\n{sql}"));
        }
        for (label, sql, kind) in REFUSED {
            let error = outcome(&conn, sql)
                .await
                .expect_err(&format!("{side} accepted {label}"))
                .to_string();
            let expected = format!("{kind} constraint failed");
            assert!(error.contains(&expected), "{side}, {label}: {error}");
        }
        conn.execute_unprepared("PRAGMA foreign_keys = ON").await.unwrap();
        for (label, sql) in REFUSED_BY_A_KEY {
            let error = outcome(&conn, sql)
                .await
                .expect_err(&format!("{side} accepted {label}"))
                .to_string();
            assert!(
                error.contains("FOREIGN KEY constraint failed"),
                "{side}, {label}: {error}"
            );
        }
    }
}
