//! The data-mapping tests of the legacy migrations: each one stops the database
//! just before the migration it is about, seeds rows, runs it, and reads the
//! rows back. They replay the embedded SQL through sqlx, each migration in a
//! transaction of its own as Diesel ran it, so a migration that refuses bad data
//! still leaves the rows it refused untouched.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, QueryResult, Statement, TryGetable, Value};

use super::legacy::{LEGACY, index_of, replay_in_transactions};
use super::memory_connection;

fn raw(sql: &str, params: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Sqlite, sql, params)
}

/// An empty database with foreign keys off, as a bare Diesel `:memory:`
/// connection had them.
async fn blank() -> DatabaseConnection {
    let conn = memory_connection().await;
    conn.execute_unprepared("PRAGMA foreign_keys = OFF").await.unwrap();
    conn
}

/// Bring a database up to migration 18 only, so migration 19 can be tested
/// against realistic pre-existing rows rather than against an empty schema.
/// Running the whole migration set (as `sea_test_db` does) would never exercise
/// the data-mapping half of the migration.
async fn conn_at_18() -> DatabaseConnection {
    conn_before("00000000000019").await
}

async fn conn_before(version: &str) -> DatabaseConnection {
    let conn = blank().await;
    replay_in_transactions(&conn, 0..index_of(version)).await.unwrap();
    conn
}

async fn run_migration(conn: &DatabaseConnection, version: &str) {
    let i = index_of(version);
    replay_in_transactions(conn, i..i + 1).await.unwrap();
}

/// Everything from `version` onward, in order.
///
/// A test that stops at the migration it is about still has to read the rows
/// back, and the readers select every column the schema has *today* — so
/// stopping short fails inside the reader rather than in anything the
/// migration did. Catching up to head afterwards keeps such a test about its
/// own migration, and stops the next column added to the same table from
/// breaking it for a reason it has nothing to do with.
async fn run_migrations_from(conn: &DatabaseConnection, version: &str) {
    replay_in_transactions(conn, index_of(version)..LEGACY.len())
        .await
        .unwrap();
}

async fn exec(conn: &DatabaseConnection, sql: &str) {
    conn.execute_unprepared(sql).await.unwrap();
}

async fn row(conn: &DatabaseConnection, sql: &str, params: Vec<Value>) -> QueryResult {
    conn.query_one_raw(raw(sql, params)).await.unwrap().unwrap()
}

async fn rows(conn: &DatabaseConnection, sql: &str) -> Vec<QueryResult> {
    conn.query_all_raw(raw(sql, vec![])).await.unwrap()
}

async fn one<T: TryGetable>(conn: &DatabaseConnection, sql: &str, params: Vec<Value>, column: &str) -> T {
    row(conn, sql, params).await.try_get("", column).unwrap()
}

async fn count(conn: &DatabaseConnection, sql: &str) -> i64 {
    one(conn, sql, vec![], "n").await
}

/// `None` when the key has no row, as `ops::preference::get_preference` answers.
async fn preference(conn: &DatabaseConnection, key: &str) -> Option<String> {
    conn.query_one_raw(raw("SELECT value FROM preferences WHERE key = ?", vec![key.into()]))
        .await
        .unwrap()
        .map(|row| row.try_get::<String>("", "value").unwrap())
}

/// Two migrations sharing a version number is silent: Diesel records the
/// version as applied and the second one never runs, so a column simply
/// never appears and every query against it fails at runtime. Cheap to
/// check, and it catches the case where a branch and its upstream both
/// claim the next number.
#[test]
fn no_two_migrations_claim_the_same_version() {
    let mut versions: Vec<&str> = LEGACY.iter().map(|(name, _)| name.split('_').next().unwrap()).collect();
    let total = versions.len();
    versions.sort_unstable();
    versions.dedup();
    assert_eq!(versions.len(), total, "duplicate migration version among {versions:?}");
}

/// Selecting the model checks every column the schema declares, so this
/// fails if `schema.rs` and the schema the baseline builds have drifted apart
/// — which is otherwise a runtime error rather than a compile one.
#[test]
fn a_conversation_starts_out_asking_about_every_edit() {
    use diesel::connection::SimpleConnection;
    use diesel::prelude::*;

    use crate::db::models::conversation::ConversationRow;
    use crate::db::schema::conversations::dsl::*;

    let pool = crate::db::diesel_test_db();
    let mut conn = pool.get().unwrap();
    conn.batch_execute(
        "INSERT INTO conversations
             (id, is_pinned, is_archived, message_count, created_at, updated_at)
         VALUES ('c1', 0, 0, 0, 1, 1)",
    )
    .unwrap();

    let c: ConversationRow = conversations
        .find("c1")
        .select(ConversationRow::as_select())
        .first(&mut conn)
        .unwrap();
    assert_eq!(
        c.accept_edits, 0,
        "a conversation that predates the column must keep asking"
    );
}

async fn seed_pre19(conn: &DatabaseConnection) {
    exec(
        conn,
        "INSERT INTO projects (id, name, path, source_type, source_id, created_at, updated_at)
         VALUES ('p-desk', 'Desktop', '/tmp', 'local', NULL, 1, 1),
                ('p-priv', 'QQ Alice', NULL, 'onebot_private', '10001', 1, 1),
                ('p-grp',  'QQ Group', NULL, 'onebot_group',   '20002', 1, 1);

         INSERT INTO memories (id, project_id, key, content, memory_type, created_at, updated_at)
         VALUES ('m-desk', 'p-desk', 'style', 'terse', 'preference', 1, 1),
                ('m-priv', 'p-priv', 'tz',    'UTC+8', 'fact',       1, 1),
                ('m-grp',  'p-grp',  'slang', 'in-joke','general',   1, 1);",
    )
    .await;
}

async fn run_19(conn: &DatabaseConnection) {
    run_migration(conn, "00000000000019").await;
}

struct MemoryRow {
    scope_type: String,
    scope_id: String,
    subject_scope_id: Option<String>,
    origin: String,
}

async fn fetch(conn: &DatabaseConnection, id: &str) -> MemoryRow {
    let row = row(
        conn,
        "SELECT scope_type, scope_id, subject_scope_id, origin FROM memories WHERE id = ?",
        vec![id.into()],
    )
    .await;
    MemoryRow {
        scope_type: row.try_get("", "scope_type").unwrap(),
        scope_id: row.try_get("", "scope_id").unwrap(),
        subject_scope_id: row.try_get("", "subject_scope_id").unwrap(),
        origin: row.try_get("", "origin").unwrap(),
    }
}

/// The whole point of the data mapping: a private-chat memory becomes a
/// memory about that person, so /memory me and opt-out can reach it.
#[tokio::test]
async fn private_chat_memories_become_per_person() {
    let conn = conn_at_18().await;
    seed_pre19(&conn).await;
    run_19(&conn).await;

    let row = fetch(&conn, "m-priv").await;
    assert_eq!(row.scope_type, "onebot_user");
    assert_eq!(row.scope_id, "onebot:10001");
    assert_eq!(row.subject_scope_id.as_deref(), Some("onebot:10001"));
    assert_eq!(row.origin, "private");
}

/// Old group rows carry no trustworthy sender, so they must not pass as
/// `group` — that would let them act as evidence from the identity pipeline.
#[tokio::test]
async fn group_memories_are_marked_legacy() {
    let conn = conn_at_18().await;
    seed_pre19(&conn).await;
    run_19(&conn).await;

    let row = fetch(&conn, "m-grp").await;
    assert_eq!(row.scope_type, "project");
    assert_eq!(row.scope_id, "p-grp");
    assert_eq!(row.subject_scope_id, None);
    assert_eq!(row.origin, "legacy");
}

#[tokio::test]
async fn desktop_memories_stay_on_their_project() {
    let conn = conn_at_18().await;
    seed_pre19(&conn).await;
    run_19(&conn).await;

    let row = fetch(&conn, "m-desk").await;
    assert_eq!(row.scope_type, "project");
    assert_eq!(row.scope_id, "p-desk");
    assert_eq!(row.origin, "desktop");
}

/// A soft-deleted key must be creatable again; a plain unique index would
/// force upsert to resurrect tombstones and break the trash.
#[tokio::test]
async fn unique_index_only_constrains_live_rows() {
    let conn = conn_at_18().await;
    seed_pre19(&conn).await;
    run_19(&conn).await;

    conn.execute_unprepared(
        "UPDATE memories SET deleted_at = 99, deleted_by = 'self' WHERE id = 'm-desk';
         INSERT INTO memories (id, scope_type, scope_id, key, content, memory_type,
                               origin, visibility, created_at, updated_at)
         VALUES ('m-desk2', 'project', 'p-desk', 'style', 'verbose', 'preference',
                 'desktop', 'normal', 2, 2);",
    )
    .await
    .expect("re-creating a soft-deleted key must be allowed");

    let live = count(
        &conn,
        "SELECT COUNT(*) AS n FROM memories \
         WHERE scope_id='p-desk' AND key='style' AND deleted_at IS NULL",
    )
    .await;
    assert_eq!(live, 1);
}

/// Proposal ids must never be reused: a stale "同意 N" would otherwise
/// approve a completely different proposal.
#[tokio::test]
async fn proposal_ids_are_not_reused() {
    let conn = conn_at_18().await;
    run_19(&conn).await;

    exec(
        &conn,
        "INSERT INTO memory_proposals (key, content, memory_type, status, created_at, expires_at)
         VALUES ('a', 'x', 'general', 'pending', 1, 2);
         DELETE FROM memory_proposals;
         INSERT INTO memory_proposals (key, content, memory_type, status, created_at, expires_at)
         VALUES ('b', 'y', 'general', 'pending', 1, 2);",
    )
    .await;

    let id = count(&conn, "SELECT id AS n FROM memory_proposals WHERE key = 'b'").await;
    assert_eq!(id, 2, "AUTOINCREMENT must not hand out id 1 again");
}

async fn conn_at_20() -> DatabaseConnection {
    conn_before("00000000000021").await
}

/// Two conversations, one of them compacted, so the backfill has to keep the
/// chains apart and place the summary anchor.
async fn seed_pre21(conn: &DatabaseConnection) {
    exec(
        conn,
        "INSERT INTO conversations (id, title, is_pinned, is_archived, message_count,
                                    created_at, updated_at, compact_cursor, fast_mode)
         VALUES ('c-a', 'A', 0, 0, 4, 1, 1, 3, 0),
                ('c-b', 'B', 0, 0, 2, 1, 1, NULL, 0);

         INSERT INTO messages (id, conversation_id, role, content, sort_order, created_at,
                               schema_version, is_compact_summary)
         VALUES ('a1', 'c-a', 'user',      'q1', 1, 10, 2, 0),
                ('a2', 'c-a', 'assistant', 'r1', 2, 11, 2, 0),
                ('a3', 'c-a', 'user',      'q2', 3, 12, 2, 0),
                ('a4', 'c-a', 'assistant', 'r2', 4, 13, 2, 0),
                ('asum', 'c-a', 'user', 'summary', -1, 14, 2, 1),
                ('b1', 'c-b', 'user',      'q1', 1, 10, 2, 0),
                ('b2', 'c-b', 'assistant', 'r1', 2, 11, 2, 0);",
    )
    .await;
}

async fn parent_of(conn: &DatabaseConnection, id: &str) -> Option<String> {
    one(
        conn,
        "SELECT parent_id AS id FROM messages WHERE id = ?",
        vec![id.into()],
        "id",
    )
    .await
}

#[tokio::test]
async fn backfill_chains_existing_messages_in_order() {
    let conn = conn_at_20().await;
    seed_pre21(&conn).await;
    run_migration(&conn, "00000000000021").await;

    assert_eq!(parent_of(&conn, "a1").await, None, "the first message is a root");
    assert_eq!(parent_of(&conn, "a2").await.as_deref(), Some("a1"));
    assert_eq!(parent_of(&conn, "a3").await.as_deref(), Some("a2"));
    assert_eq!(parent_of(&conn, "a4").await.as_deref(), Some("a3"));
}

/// The correlated subquery has to filter on conversation_id; without it every
/// conversation would splice onto the globally previous message.
#[tokio::test]
async fn backfill_keeps_conversations_apart() {
    let conn = conn_at_20().await;
    seed_pre21(&conn).await;
    run_migration(&conn, "00000000000021").await;

    assert_eq!(parent_of(&conn, "b1").await, None);
    assert_eq!(parent_of(&conn, "b2").await.as_deref(), Some("b1"));
}

/// A summary sits beside the tree, not in it. Chaining it would make the
/// first real message look like it had a sibling.
#[tokio::test]
async fn backfill_leaves_summaries_off_the_chain() {
    let conn = conn_at_20().await;
    seed_pre21(&conn).await;
    run_migration(&conn, "00000000000021").await;

    assert_eq!(parent_of(&conn, "asum").await, None);
    let children = count(&conn, "SELECT COUNT(*) AS n FROM messages WHERE parent_id = 'asum'").await;
    assert_eq!(children, 0);
}

/// The old cursor names a sort_order; the anchor is the first message at or
/// past it.
#[tokio::test]
async fn backfill_translates_the_compact_cursor_to_an_anchor() {
    let conn = conn_at_20().await;
    seed_pre21(&conn).await;
    run_migration(&conn, "00000000000021").await;

    let anchor: Option<String> = one(
        &conn,
        "SELECT compact_anchor_id AS id FROM messages WHERE id = 'asum'",
        vec![],
        "id",
    )
    .await;
    assert_eq!(
        anchor.as_deref(),
        Some("a3"),
        "cursor 3 maps to the row at sort_order 3"
    );
}

#[tokio::test]
async fn backfill_points_head_at_the_last_message() {
    let conn = conn_at_20().await;
    seed_pre21(&conn).await;
    run_migration(&conn, "00000000000021").await;

    let head: Option<String> = one(
        &conn,
        "SELECT head_message_id AS id FROM conversations WHERE id = 'c-a'",
        vec![],
        "id",
    )
    .await;
    assert_eq!(head.as_deref(), Some("a4"));
}

/// Migration 25 adds two nullable columns to `messages` rather than
/// rebuilding the table, so rows written before it keep working untouched.
/// `tool_outcome` reading as NULL is what makes that safe: NULL means
/// success, which is what the transcript claimed for every one of them
/// anyway.
#[tokio::test]
async fn existing_messages_survive_the_turn_columns() {
    let conn = conn_before("00000000000025").await;
    exec(
        &conn,
        "INSERT INTO conversations (id, title, is_pinned, is_archived, message_count,
                                    created_at, updated_at, fast_mode)
         VALUES ('c1', 'A', 0, 0, 2, 1, 1, 0);
         INSERT INTO messages (id, conversation_id, role, content, sort_order,
                               created_at, schema_version, is_compact_summary)
         VALUES ('m1', 'c1', 'user', 'hi', 1, 1, 2, 0),
                ('m2', 'c1', 'tool', 'refused', 2, 2, 2, 0);",
    )
    .await;

    run_migration(&conn, "00000000000025").await;

    let rows = rows(&conn, "SELECT turn_id, tool_outcome FROM messages ORDER BY sort_order").await;

    assert_eq!(rows.len(), 2, "no row is lost or duplicated");
    assert!(
        rows.iter()
            .all(|r| r.try_get::<Option<String>>("", "turn_id").unwrap().is_none())
    );
    assert!(
        rows.iter()
            .all(|r| r.try_get::<Option<String>>("", "tool_outcome").unwrap().is_none()),
        "nothing is backfilled: NULL already means what these rows meant",
    );
}

/// The `turns` table is new, so upgrading finds it empty — and startup
/// reconciliation over an empty table must not report anything.
#[tokio::test]
async fn upgrading_starts_with_no_turn_history() {
    let conn = conn_before("00000000000025").await;
    run_migration(&conn, "00000000000025").await;

    // What `ops::turn::reconcile_interrupted` does to the table: every
    // `running` row becomes `interrupted`, and it reports how many there were.
    let reconciled = conn
        .execute_unprepared(
            "UPDATE turns SET status = 'interrupted', ended_at = 1000, updated_at = 1000
             WHERE status = 'running'",
        )
        .await
        .unwrap();
    assert_eq!(reconciled.rows_affected(), 0);
}

/// Migrations 26 and 27 each add a nullable column to a table 25 had
/// already written rows into, so there is real data to preserve. NULL is the
/// right value in both: nothing had been told to any model, because there
/// was no mechanism to tell it. Backfilling a timestamp would silence
/// exactly the warnings this whole record exists to keep.
///
/// Everything from 26 is applied, not just those two: `Turn` selects every
/// column the table has today, so stopping short fails in the reader rather
/// than in anything a migration did.
#[tokio::test]
async fn existing_turns_survive_the_reported_columns_still_owing_their_explanation() {
    let conn = conn_before("00000000000026").await;
    exec(
        &conn,
        "INSERT INTO conversations (id, title, is_pinned, is_archived, message_count,
                                    created_at, updated_at, fast_mode)
         VALUES ('c1', 'A', 0, 0, 0, 1, 1, 0);
         INSERT INTO turns (id, conversation_id, origin, status, phase, phase_tool,
                            started_at, updated_at, ended_at)
         VALUES ('t-cut', 'c1', 'desktop', 'interrupted', 'running_tool', 'edit_file',
                 1000, 1001, 1500),
                ('t-done', 'c1', 'desktop', 'done', 'streaming', NULL, 2000, 2001, 2500);",
    )
    .await;

    run_migrations_from(&conn, "00000000000026").await;

    // As `ops::turn::list_for_conversation` reads them: oldest first, rowid
    // breaking ties.
    let turns = rows(
        &conn,
        "SELECT id, phase_tool, ended_at, reported_at, parent_reported_at FROM turns
         WHERE conversation_id = 'c1' ORDER BY started_at ASC, rowid ASC",
    )
    .await;
    assert_eq!(turns.len(), 2, "no row is lost or duplicated");
    assert!(
        turns
            .iter()
            .all(|t| t.try_get::<Option<i64>>("", "reported_at").unwrap().is_none()),
        "nothing is backfilled"
    );
    assert!(
        turns
            .iter()
            .all(|t| t.try_get::<Option<i64>>("", "parent_reported_at").unwrap().is_none()),
        "and neither ledger starts out settled",
    );
    // The one that was cut off is still owed its explanation, and the one
    // that finished never was.
    let cut = &turns[0];
    assert_eq!(cut.try_get::<String>("", "id").unwrap(), "t-cut");
    assert_eq!(
        cut.try_get::<Option<String>>("", "phase_tool").unwrap().as_deref(),
        Some("edit_file")
    );
    assert_eq!(cut.try_get::<Option<i64>>("", "ended_at").unwrap(), Some(1500));
    // What `ops::turn::unreported_for_conversation` selects for this
    // conversation, which has delegated nothing.
    let unreported: Vec<String> = rows(
        &conn,
        "SELECT id FROM turns
         WHERE conversation_id = 'c1' AND id <> '' AND reported_at IS NULL
           AND status IN ('running', 'interrupted')
         ORDER BY started_at DESC, rowid DESC LIMIT 10",
    )
    .await
    .iter()
    .map(|t| t.try_get::<String>("", "id").unwrap())
    .collect();
    assert_eq!(unreported, ["t-cut"]);
}

/// Migration 27 hides a conversation from the sidebar by giving it a parent.
/// Every row that predates it has none, so the lists it appears in must not
/// change — a conversation the user started years ago cannot become a
/// sub-agent's transcript because a column arrived.
#[tokio::test]
async fn conversations_that_predate_delegation_stay_in_the_lists() {
    let conn = conn_before("00000000000027").await;
    exec(
        &conn,
        "INSERT INTO conversations (id, title, is_pinned, is_archived, message_count,
                                    created_at, updated_at, fast_mode)
         VALUES ('c1', 'A', 0, 0, 0, 1, 1, 0);",
    )
    .await;

    run_migrations_from(&conn, "00000000000027").await;

    // The sidebar's query, `ops::conversation::list_conversations(conn, false)`.
    let listed = rows(
        &conn,
        "SELECT id, parent_conversation_id, agent_model_id FROM conversations
         WHERE is_archived = 0 AND parent_conversation_id IS NULL
         ORDER BY is_pinned DESC, updated_at DESC",
    )
    .await;
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0]
            .try_get::<Option<String>>("", "parent_conversation_id")
            .unwrap()
            .is_none()
    );
    assert!(
        listed[0]
            .try_get::<Option<String>>("", "agent_model_id")
            .unwrap()
            .is_none(),
        "and it goes on resolving from the assistant"
    );
    // And `ops::conversation::sub_agent_runs(conn, "c1")` finds nothing.
    let runs = count(
        &conn,
        "SELECT COUNT(*) AS n FROM conversations WHERE parent_conversation_id = 'c1'",
    )
    .await;
    assert_eq!(runs, 0);
}

/// Migration 48 removes only schema that had no runtime owner. Existing
/// conversations and messages must survive even when an attachment row and
/// the legacy compaction cursor are present in the old database.
#[tokio::test]
async fn dead_schema_is_removed_without_touching_live_rows() {
    let conn = conn_before("00000000000048").await;
    exec(
        &conn,
        "INSERT INTO conversations (id, title, is_pinned, is_archived, message_count,
                                    created_at, updated_at, compact_cursor, fast_mode)
         VALUES ('c1', 'A', 0, 0, 0, 1, 1, 1, 0);
         INSERT INTO messages (id, conversation_id, role, content, sort_order,
                               created_at, schema_version, is_compact_summary)
         VALUES ('m1', 'c1', 'user', 'hi', 1, 1, 2, 0);
         INSERT INTO attachments (id, message_id, file_name, file_path, mime_type,
                                  file_size, created_at)
         VALUES ('a1', 'm1', 'old.txt', '/tmp/old.txt', 'text/plain', 3, 1);",
    )
    .await;

    run_migration(&conn, "00000000000048").await;

    let dead_tables = count(
        &conn,
        "SELECT COUNT(*) AS n FROM sqlite_master
         WHERE type = 'table' AND name IN ('attachments', 'tool_permissions')",
    )
    .await;
    assert_eq!(dead_tables, 0);

    let dead_columns = count(
        &conn,
        "SELECT COUNT(*) AS n FROM pragma_table_info('conversations')
         WHERE name = 'compact_cursor'",
    )
    .await;
    assert_eq!(dead_columns, 0);

    let live_rows = count(
        &conn,
        "SELECT COUNT(*) AS n FROM conversations c
         JOIN messages m ON m.conversation_id = c.id
         WHERE c.id = 'c1' AND m.id = 'm1'",
    )
    .await;
    assert_eq!(live_rows, 1);
}

struct DecimalMigrationRow {
    input_price: Option<String>,
    output_price: Option<String>,
    cache_read_price: Option<String>,
    pricing_tiers: Option<String>,
}

async fn decimal_row(conn: &DatabaseConnection, sql: &str) -> DecimalMigrationRow {
    let row = row(conn, sql, vec![]).await;
    DecimalMigrationRow {
        input_price: row.try_get("", "input_price").unwrap(),
        output_price: row.try_get("", "output_price").unwrap(),
        cache_read_price: row.try_get("", "cache_read_price").unwrap(),
        pricing_tiers: row.try_get("", "pricing_tiers").unwrap(),
    }
}

/// Migration 49 is a data migration, not merely a column-type change. It
/// must turn legacy REAL values and numeric tier JSON into the one canonical
/// string representation accepted by the Decimal protocol. The old 0/0
/// sentinel becomes NULL only when both token prices are zero, so a model
/// with free input and paid output keeps that deliberate zero.
#[tokio::test]
async fn exact_decimal_migration_canonicalizes_existing_money() {
    let conn = conn_before("00000000000049").await;
    exec(
        &conn,
        r#"
        INSERT INTO providers (id, name, base_url, created_at, updated_at)
        VALUES ('p1', 'Provider', 'https://example.invalid', 1, 1);

        INSERT INTO model_configs VALUES (
            'mc1', 'p1', 'm1', NULL, 128000, 100000, NULL,
            0, 1.25, 0.125, 1, 1, NULL, 0.5,
            '[{"min_prompt_tokens":1000,"input":2.5,"output":3,"cache_read":0.25}]',
            NULL, 4.75
        );

        INSERT INTO model_configs VALUES (
            'mc_future', 'p1', 'm-future', NULL, 128000, 100000, NULL,
            0, 1.25, 0.125, 1, 1, NULL, 0.5,
            '[{"min_prompt_tokens":1000,"input":2.5,"output":3,"future":"keep"}]',
            NULL, 4.75
        );

        INSERT INTO model_configs VALUES (
            'mc_free', 'p1', 'm-free', NULL, 128000, 100000, NULL,
            0, 0, NULL, 1, 1, NULL, NULL, NULL, NULL, NULL
        );

        INSERT INTO audit_messages (
            id, recorded_at, message_id, conversation_id, role, content,
            created_at, input_price, output_price, cache_read_price,
            cache_write_price, server_tool_price, billing_mode
        ) VALUES (
            'a1', 1, 'm1', 'c1', 'assistant', 'ok', 1,
            1.25, 2.5, 0.125, 0.5, 4.75, 'metered'
        );

        INSERT INTO preferences (key, value, updated_at)
        VALUES ('onebot.balance_alert_threshold', '', 1);
        "#,
    )
    .await;

    run_migration(&conn, "00000000000049").await;

    let model = decimal_row(
        &conn,
        "SELECT input_price, output_price, cache_read_price, pricing_tiers
         FROM model_configs WHERE id = 'mc1'",
    )
    .await;
    assert_eq!(model.input_price.as_deref(), Some("0"));
    assert_eq!(model.output_price.as_deref(), Some("1.25"));
    assert_eq!(model.cache_read_price.as_deref(), Some("0.125"));

    let tiers: serde_json::Value = serde_json::from_str(model.pricing_tiers.as_deref().unwrap()).unwrap();
    assert_eq!(tiers[0]["input_price"], "2.5");
    assert_eq!(tiers[0]["output_price"], "3");
    assert_eq!(tiers[0]["cache_read_price"], "0.25");
    assert!(tiers[0].get("cache_write_price").is_some());
    assert!(tiers[0]["cache_write_price"].is_null());
    assert!(tiers[0].get("input").is_none());
    assert!(tiers[0].get("output").is_none());
    assert!(tiers[0].get("cache_read").is_none());
    assert!(crate::agent::pricing::parse_tiers(model.pricing_tiers.as_deref()).is_ok());

    let future = decimal_row(
        &conn,
        "SELECT input_price, output_price, cache_read_price, pricing_tiers
         FROM model_configs WHERE id = 'mc_future'",
    )
    .await;
    let future_tiers: serde_json::Value = serde_json::from_str(future.pricing_tiers.as_deref().unwrap()).unwrap();
    assert_eq!(future_tiers[0]["future"], "keep");
    assert!(future_tiers[0].get("cache_read_price").is_some());
    assert!(future_tiers[0]["cache_read_price"].is_null());
    assert!(crate::agent::pricing::parse_tiers(future.pricing_tiers.as_deref()).is_err());

    let free = decimal_row(
        &conn,
        "SELECT input_price, output_price, cache_read_price, pricing_tiers
         FROM model_configs WHERE id = 'mc_free'",
    )
    .await;
    assert_eq!(free.input_price, None);
    assert_eq!(free.output_price, None);

    let audit = decimal_row(
        &conn,
        "SELECT input_price, output_price, cache_read_price AS cache_read_price,
                NULL AS pricing_tiers
         FROM audit_messages WHERE id = 'a1'",
    )
    .await;
    assert_eq!(audit.input_price.as_deref(), Some("1.25"));
    assert_eq!(audit.output_price.as_deref(), Some("2.5"));
    assert_eq!(audit.cache_read_price.as_deref(), Some("0.125"));

    exec(
        &conn,
        "UPDATE audit_messages
         SET input_price = '12.34', output_price = '23.45',
             cache_read_price = '0.12', cache_write_price = '0.34',
             server_tool_price = '4.56'
         WHERE id = 'a1'",
    )
    .await;
    let canonical_audit_money = count(
        &conn,
        "SELECT COUNT(*) AS n FROM audit_messages
         WHERE id = 'a1'
           AND typeof(input_price) = 'text'
           AND typeof(output_price) = 'text'
           AND typeof(cache_read_price) = 'text'
           AND typeof(cache_write_price) = 'text'
           AND typeof(server_tool_price) = 'text'",
    )
    .await;
    assert_eq!(canonical_audit_money, 1);

    for column in [
        "input_price",
        "output_price",
        "cache_read_price",
        "cache_write_price",
        "server_tool_price",
    ] {
        assert!(
            conn.execute_unprepared(&format!("UPDATE audit_messages SET {column} = x'3132' WHERE id = 'a1'"))
                .await
                .is_err(),
            "audit_messages.{column} must reject BLOB money"
        );
    }

    assert!(
        conn.execute_unprepared("UPDATE model_configs SET output_price = '01.25' WHERE id = 'mc1'")
            .await
            .is_err()
    );
    assert!(
        conn.execute_unprepared("UPDATE model_configs SET output_price = '' WHERE id = 'mc1'")
            .await
            .is_err()
    );
    assert!(
        conn.execute_unprepared("UPDATE audit_messages SET billing_mode = 'future' WHERE id = 'a1'")
            .await
            .is_err()
    );

    let empty_threshold = count(
        &conn,
        "SELECT COUNT(*) AS n FROM preferences
         WHERE key = 'onebot.balance_alert_threshold'",
    )
    .await;
    assert_eq!(empty_threshold, 0);
}

async fn seed_auto_review_before_50(conn: &DatabaseConnection, raw_review: &str) {
    exec(
        conn,
        "INSERT INTO conversations
             (id, is_pinned, is_archived, message_count, created_at, updated_at)
         VALUES ('c1', 0, 0, 1, 1, 1);
         INSERT INTO messages
             (id, conversation_id, role, content, sort_order, created_at,
              schema_version, is_compact_summary)
         VALUES ('m1', 'c1', 'assistant', '', 1, 1, 2, 0);",
    )
    .await;
    conn.execute_raw(raw(
        "UPDATE messages SET auto_review = ? WHERE id = 'm1'",
        vec![raw_review.into()],
    ))
    .await
    .unwrap();
}

async fn stored_auto_review(conn: &DatabaseConnection) -> Option<String> {
    one(
        conn,
        "SELECT auto_review FROM messages WHERE id = 'm1'",
        vec![],
        "auto_review",
    )
    .await
}

/// Migration 61 splits one row into two, and what it must *not* do is leave
/// the prices in both: a number kept in two places is a number that comes
/// to disagree, and the resolver would then have to guess which is current.
/// Every existing row starts unoverridden, priced by the profile it just
/// created, with the provider-side tools left where they belong.
#[tokio::test]
async fn model_profile_migration_gives_every_row_its_own_profile() {
    let conn = conn_before("00000000000061").await;
    exec(
        &conn,
        r#"
        INSERT INTO providers (id, name, base_url, created_at, updated_at)
        VALUES ('p1', 'Provider', 'https://example.invalid', 1, 1),
               ('p2', 'Relay', 'https://relay.invalid', 1, 1);

        INSERT INTO model_configs (
            id, provider_id, model_id, display_name, context_window,
            compact_threshold, max_output_tokens, input_price, output_price,
            cache_read_price, created_at, updated_at, capability_overrides,
            cache_write_price, pricing_tiers, server_tools, server_tool_price
        ) VALUES (
            'mc1', 'p1', 'claude-sonnet-5', 'Claude Sonnet 5', 200000,
            150000, 64000, '3', '15', '0.3', 1, 1, '{"supports_fast":true}',
            '3.75', NULL, '["web_search"]', '5'
        ), (
            'mc2', 'p2', 'anthropic/claude-sonnet-5', NULL, 200000,
            150000, NULL, '4', '20', NULL, 1, 1, NULL, NULL, NULL, NULL, NULL
        );"#,
    )
    .await;
    run_migration(&conn, "00000000000061").await;

    // A row that named itself keeps that name; one that did not is named by
    // the only other thing it has, its wire id.
    let named = row(
        &conn,
        "SELECT name, context_window, input_price, capability_overrides FROM model_profiles WHERE id = 'mc1'",
        vec![],
    )
    .await;
    assert_eq!(named.try_get::<String>("", "name").unwrap(), "Claude Sonnet 5");
    assert_eq!(named.try_get::<i64>("", "context_window").unwrap(), 200000);
    assert_eq!(
        named.try_get::<Option<String>>("", "input_price").unwrap().as_deref(),
        Some("3")
    );
    assert_eq!(
        named
            .try_get::<Option<String>>("", "capability_overrides")
            .unwrap()
            .as_deref(),
        Some(r#"{"supports_fast":true}"#)
    );

    let unnamed = row(
        &conn,
        "SELECT name, context_window, input_price, capability_overrides FROM model_profiles WHERE id = 'mc2'",
        vec![],
    )
    .await;
    assert_eq!(
        unnamed.try_get::<String>("", "name").unwrap(),
        "anthropic/claude-sonnet-5"
    );
    assert_eq!(
        unnamed.try_get::<Option<String>>("", "input_price").unwrap().as_deref(),
        Some("4")
    );

    // The config keeps the provider's own facts and nothing else.
    let config = row(
        &conn,
        "SELECT profile_id, overrides_pricing, input_price, server_tools FROM model_configs WHERE id = 'mc1'",
        vec![],
    )
    .await;
    assert_eq!(config.try_get::<String>("", "profile_id").unwrap(), "mc1");
    assert_eq!(config.try_get::<i64>("", "overrides_pricing").unwrap(), 0);
    assert_eq!(config.try_get::<Option<String>>("", "input_price").unwrap(), None);
    assert_eq!(
        config.try_get::<Option<String>>("", "server_tools").unwrap().as_deref(),
        Some(r#"["web_search"]"#)
    );

    // One profile per row, not one shared by name: two providers may serve
    // genuinely different deployments, and merging them is the user's call.
    let profiles = count(&conn, "SELECT COUNT(*) AS n FROM model_profiles").await;
    assert_eq!(profiles, 2);

    // The override columns keep migration 49's contract.
    assert!(
        conn.execute_unprepared("UPDATE model_configs SET input_price = '01.25' WHERE id = 'mc1'")
            .await
            .is_err()
    );
    assert!(
        conn.execute_unprepared("UPDATE model_profiles SET output_price = '1.250' WHERE id = 'mc1'")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn auto_review_migration_rewrites_legacy_rows_to_the_only_public_shape() {
    let conn = conn_before("00000000000050").await;
    seed_auto_review_before_50(
        &conn,
        r#"{
            "call-allow": {
                "outcome": "allow",
                "risk": "low",
                "authorization": "high",
                "rationale": "requested",
                "stage": "quick",
                "model": "reviewer",
                "evidence": [{"tool": "read_file", "arguments": "{\"path\":\"x\"}"}],
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 2,
                    "cache_read_tokens": null,
                    "cache_write_tokens": 0
                }
            },
            "call-unreadable": {
                "outcome": "unreadable",
                "rationale": "bad response",
                "stage": "investigate",
                "model": "reviewer",
                "usage": {
                    "input_tokens": null,
                    "output_tokens": null,
                    "cache_read_tokens": null,
                    "cache_write_tokens": null
                }
            }
        }"#,
    )
    .await;

    run_migration(&conn, "00000000000050").await;

    let stored = stored_auto_review(&conn).await;
    let value: serde_json::Value = serde_json::from_str(stored.as_deref().unwrap()).unwrap();
    let expected_keys = [
        "authorization",
        "evidence",
        "model",
        "outcome",
        "rationale",
        "risk",
        "stage",
    ];
    for call_id in ["call-allow", "call-unreadable"] {
        let verdict = value[call_id].as_object().unwrap();
        let mut keys = verdict.keys().map(String::as_str).collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(keys, expected_keys);
        serde_json::from_value::<crate::events::AutoReviewVerdict>(value[call_id].clone()).unwrap();
    }
    assert_eq!(
        value["call-allow"]["evidence"],
        serde_json::json!([{"tool": "read_file", "arguments": "{\"path\":\"x\"}"}])
    );
    assert_eq!(value["call-unreadable"]["evidence"], serde_json::json!([]));
    assert!(value["call-unreadable"]["risk"].is_null());
    assert!(value["call-unreadable"]["authorization"].is_null());
}

#[tokio::test]
async fn auto_review_migration_rejects_json_it_cannot_canonicalize() {
    for (label, raw_review) in [
        ("invalid JSON", "not json"),
        ("wrong outer type", "[]"),
        (
            "unknown verdict field",
            r#"{"call-1":{"outcome":"allow","future":true}}"#,
        ),
        (
            "missing evidence arguments",
            r#"{"call-1":{"outcome":"deny","evidence":[{"tool":"read"}]}}"#,
        ),
        ("missing outcome", r#"{"call-1":{"risk":"low"}}"#),
        (
            "missing evidence tool",
            r#"{"call-1":{"outcome":"deny","evidence":[{"arguments":"{}"}]}}"#,
        ),
        (
            "duplicate usage key",
            r#"{"call-1":{"outcome":"allow","usage":{"input_tokens":1,"input_tokens":2,"output_tokens":1,"cache_read_tokens":0}}}"#,
        ),
        (
            "duplicate evidence key",
            r#"{"call-1":{"outcome":"deny","evidence":[{"tool":"read","tool":"write"}]}}"#,
        ),
    ] {
        let conn = conn_before("00000000000050").await;
        seed_auto_review_before_50(&conn, raw_review).await;
        let i = index_of("00000000000050");
        assert!(
            replay_in_transactions(&conn, i..i + 1).await.is_err(),
            "{label} must fail migration 50"
        );

        let stored = stored_auto_review(&conn).await;
        assert_eq!(stored.as_deref(), Some(raw_review), "{label} must not be rewritten");
    }
}

#[tokio::test]
async fn sandbox_mode_migration_preserves_legacy_and_canonical_choices() {
    for (stored, expected) in [
        ("true", "auto"),
        ("false", "off"),
        ("auto", "auto"),
        ("off", "off"),
        ("container", "container"),
        ("FALSE", "FALSE"),
    ] {
        let conn = conn_before("00000000000052").await;
        exec(
            &conn,
            &format!(
                "INSERT INTO preferences (key, value, updated_at)
                 VALUES ('sandbox.enabled', '{stored}', 17),
                        ('unrelated.enabled', 'false', 23);"
            ),
        )
        .await;

        run_migration(&conn, "00000000000052").await;

        let migrated = row(
            &conn,
            "SELECT value, updated_at FROM preferences WHERE key = 'sandbox.enabled'",
            vec![],
        )
        .await;
        let migrated: (String, i64) = (
            migrated.try_get("", "value").unwrap(),
            migrated.try_get("", "updated_at").unwrap(),
        );
        assert_eq!(migrated, (expected.into(), 17), "legacy value {stored:?}");
        assert_eq!(preference(&conn, "unrelated.enabled").await.as_deref(), Some("false"));
    }
}

/// The balance threshold moves out of OneBot, and the move has to carry the
/// switch with it.
///
/// Setting `onebot.balance_alert_threshold` is what turned the old watcher
/// on — there was no separate enable — so migrating the number alone would
/// silently stop warning somebody who had asked to be warned, and they
/// would find out from the balance rather than from the app.
#[tokio::test]
async fn the_balance_threshold_moves_out_of_onebot_and_brings_its_switch() {
    let conn = conn_before("00000000000057").await;
    exec(
        &conn,
        "INSERT INTO preferences (key, value, updated_at)
         VALUES ('onebot.balance_alert_threshold', '12.5', 17),
                ('onebot.enabled', 'true', 23);",
    )
    .await;

    run_migration(&conn, "00000000000057").await;

    assert_eq!(
        preference(&conn, "notify.balance.threshold").await.as_deref(),
        Some("12.5")
    );
    assert_eq!(
        preference(&conn, "notify.enabled").await.as_deref(),
        Some("true"),
        "the old key was its own on switch"
    );
    assert_eq!(
        preference(&conn, "onebot.balance_alert_threshold").await,
        None,
        "two keys meaning one thing is where they start to disagree"
    );
    assert_eq!(
        preference(&conn, "onebot.enabled").await.as_deref(),
        Some("true"),
        "nothing else about OneBot is touched"
    );
}

/// An install that never set the old key must not come out of the migration
/// with a notification watcher switched on that nobody asked for — it makes
/// periodic requests with the user's API keys.
#[tokio::test]
async fn an_install_without_the_old_key_gets_no_notifications_switched_on() {
    let conn = conn_before("00000000000057").await;
    run_migration(&conn, "00000000000057").await;
    assert_eq!(preference(&conn, "notify.enabled").await, None);
    assert_eq!(preference(&conn, "notify.balance.threshold").await, None);
}

/// The old loader treated an empty string as absent, so an install that had
/// the field cleared must not come out of this with a threshold that will
/// not parse — and `notify::load_config` refuses an empty decimal.
#[tokio::test]
async fn an_emptied_threshold_migrates_to_nothing_at_all() {
    let conn = conn_before("00000000000057").await;
    exec(
        &conn,
        "INSERT INTO preferences (key, value, updated_at)
         VALUES ('onebot.balance_alert_threshold', '', 17);",
    )
    .await;

    run_migration(&conn, "00000000000057").await;

    assert_eq!(preference(&conn, "notify.balance.threshold").await, None);
    assert_eq!(preference(&conn, "notify.enabled").await, None);
    assert_eq!(preference(&conn, "onebot.balance_alert_threshold").await, None);
}
