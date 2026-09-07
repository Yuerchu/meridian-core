pub mod models;
pub mod ops;
pub mod schema;

use diesel::RunQueryDsl;
use diesel::r2d2::{ConnectionManager, Pool, PooledConnection};
use diesel::sqlite::SqliteConnection;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

pub type DbPool = Pool<ConnectionManager<SqliteConnection>>;
pub type PooledConn = PooledConnection<ConnectionManager<SqliteConnection>>;

/// How long a connection waits for a lock before giving up.
///
/// Long enough to sit through any write this app makes — they are single-row
/// inserts and updates — while still failing rather than hanging if something
/// holds the write lock indefinitely.
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// How long `pool.get()` waits for a free connection.
///
/// r2d2 defaults this to 30 seconds, which is long enough that an exhausted
/// pool reads as the app having frozen rather than as an error. Every caller
/// here either reports the failure or falls back within a request, so failing
/// fast is strictly better than waiting.
const POOL_ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// SQLite pragmas are per-connection, so they must run on every connection the
/// pool hands out — running them once on a single connection leaves the other
/// pooled connections without foreign key enforcement.
#[derive(Debug)]
struct ConnectionCustomizer;

impl diesel::r2d2::CustomizeConnection<SqliteConnection, diesel::r2d2::Error> for ConnectionCustomizer {
    fn on_acquire(&self, conn: &mut SqliteConnection) -> Result<(), diesel::r2d2::Error> {
        // First, so that the statement after it is covered too.
        //
        // SQLite defaults this to zero: a connection that meets a held lock
        // fails on the spot with "database is locked" instead of waiting. WAL
        // lets readers run alongside a writer, but two writers still collide,
        // and a single turn writes from several places — the user row, the
        // assistant placeholder, then a row per tool result, all while the
        // frontend is polling context size. That is what put the error in the
        // log, raised from r2d2 handing out a connection rather than from any
        // one query, because even this pragma run below could not get in.
        diesel::sql_query(format!("PRAGMA busy_timeout={BUSY_TIMEOUT_MS}"))
            .execute(conn)
            .map_err(diesel::r2d2::Error::QueryError)?;
        diesel::sql_query("PRAGMA foreign_keys=ON")
            .execute(conn)
            .map_err(diesel::r2d2::Error::QueryError)?;
        Ok(())
    }
}

pub fn init_db(db_path: &str) -> DbPool {
    let manager = ConnectionManager::<SqliteConnection>::new(db_path);
    // max_size is deliberately left where it was: it is a capacity figure with
    // no measurement behind it, and the acquire timeout above is what turns
    // exhaustion from a hang into a visible, logged failure. Raise it once the
    // logs say how often the pool actually runs dry.
    let pool = Pool::builder()
        .max_size(5)
        .connection_timeout(POOL_ACQUIRE_TIMEOUT)
        .connection_customizer(Box::new(ConnectionCustomizer))
        .build(manager)
        .expect("failed to create db pool");

    let mut conn = pool.get().expect("failed to get db connection");
    // journal_mode is persistent (stored in the db file), one connection suffices.
    diesel::sql_query("PRAGMA journal_mode=WAL").execute(&mut conn).ok();

    // Migrations that rebuild tables via DROP TABLE must not fire ON DELETE
    // actions on referencing rows (migration 11 nulled conversations.project_id
    // this way), so foreign keys are off for the migration run only.
    diesel::sql_query("PRAGMA foreign_keys=OFF").execute(&mut conn).ok();
    conn.run_pending_migrations(MIGRATIONS)
        .expect("failed to run migrations");
    // If this one fails the connection spends the rest of its life without
    // foreign keys, and cascades stop happening — deleting a conversation would
    // leave its messages behind, silently.
    if let Err(e) = diesel::sql_query("PRAGMA foreign_keys=ON").execute(&mut conn) {
        tracing::error!(error = %e, "could not re-enable foreign keys after migrating");
    }

    // Memories no longer hang off projects by foreign key, and migrations run
    // with foreign keys off anyway, so a table rebuild can leave orphans behind.
    let now = crate::util::now_ms();
    match ops::plan_review::backfill_legacy_artifacts(&mut conn, now) {
        Ok(0) => {}
        Ok(n) => tracing::info!(documents = n, "backfilled legacy plan artifacts"),
        // The old rows remain readable through their existing path, so this is
        // diagnosable degradation rather than a reason to make the database
        // unavailable. The next startup retries the idempotent backfill.
        Err(error) => tracing::error!(error = %error, "could not backfill legacy plan artifacts"),
    }
    match ops::plan_review::reconcile_dispatched_deliveries(&mut conn, now) {
        Ok(0) => {}
        Ok(n) => tracing::warn!(deliveries = n, "reconciled plan review deliveries after restart"),
        Err(error) => tracing::error!(error = %error, "could not reconcile plan review deliveries"),
    }
    let orphans = ops::memory::purge_orphan_project_memories(&mut conn).unwrap_or(0);
    let proposals = ops::memory::expire_proposals(&mut conn, now).unwrap_or(0);
    // Bounded-growth housekeeping. Kept off the write path: neither sweep
    // depends on what was just written, and the trash purge has no usable index
    // (both are partial on `deleted_at IS NULL`), so doing it per write meant a
    // full table scan each time.
    let swept = ops::memory::sweep_untracked_subjects(&mut conn, now).unwrap_or(0);
    // Startup housekeeping deletes rows the user may later go looking for. When
    // it removed nothing there is nothing to say, but when it did, this is the
    // only record that it happened.
    if orphans > 0 || proposals > 0 || swept > 0 {
        tracing::info!(
            orphan_memories_deleted = orphans,
            proposals_expired = proposals,
            subjects_swept = swept,
            "startup housekeeping removed rows"
        );
    }

    // Turns only ever run inside the process that recorded them, so anything
    // still marked running was killed rather than finished. This is the only
    // moment that fact is knowable — after this the row would just look like a
    // turn that has been going for a very long time.
    match ops::turn::reconcile_interrupted(&mut conn, now) {
        Ok(0) => {}
        Ok(n) => tracing::info!(turns = n, "turns left running by the previous session"),
        // Not fatal: it costs the diagnosis, not the conversation.
        Err(e) => tracing::error!(error = %e, "could not reconcile interrupted turns"),
    }

    pool
}

#[cfg(test)]
mod pool_tests {
    use super::*;
    use diesel::prelude::*;
    use diesel::sql_types::Integer;

    #[derive(QueryableByName)]
    struct BusyTimeout {
        #[diesel(sql_type = Integer)]
        timeout: i32,
    }

    /// Without this every pooled connection fails the moment it meets a lock,
    /// which is what "database is locked" in the log was.
    #[test]
    fn test_pooled_connections_wait_for_locks() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let rows: Vec<BusyTimeout> = diesel::sql_query("PRAGMA busy_timeout").load(&mut conn).unwrap();
        assert_eq!(rows[0].timeout, BUSY_TIMEOUT_MS as i32);
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use diesel::connection::SimpleConnection;
    use diesel::migration::{Migration, MigrationSource};
    use diesel::prelude::*;
    use diesel::sql_types::{BigInt, Nullable, Text};

    #[derive(QueryableByName)]
    struct MemoryRow {
        #[diesel(sql_type = Text)]
        scope_type: String,
        #[diesel(sql_type = Text)]
        scope_id: String,
        #[diesel(sql_type = Nullable<Text>)]
        subject_scope_id: Option<String>,
        #[diesel(sql_type = Text)]
        origin: String,
    }

    #[derive(QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = BigInt)]
        n: i64,
    }

    #[derive(QueryableByName)]
    struct DecimalMigrationRow {
        #[diesel(sql_type = Nullable<Text>)]
        input_price: Option<String>,
        #[diesel(sql_type = Nullable<Text>)]
        output_price: Option<String>,
        #[diesel(sql_type = Nullable<Text>)]
        cache_read_price: Option<String>,
        #[diesel(sql_type = Nullable<Text>)]
        pricing_tiers: Option<String>,
    }

    #[derive(QueryableByName)]
    struct AutoReviewMigrationRow {
        #[diesel(sql_type = Nullable<Text>)]
        auto_review: Option<String>,
    }

    /// Two migrations sharing a version number is silent: Diesel records the
    /// version as applied and the second one never runs, so a column simply
    /// never appears and every query against it fails at runtime. Cheap to
    /// check, and it catches the case where a branch and its upstream both
    /// claim the next number.
    #[test]
    fn no_two_migrations_claim_the_same_version() {
        let all = MigrationSource::<diesel::sqlite::Sqlite>::migrations(&MIGRATIONS).unwrap();
        let mut versions: Vec<String> = all.iter().map(|m| m.name().version().as_owned().to_string()).collect();
        let total = versions.len();
        versions.sort();
        versions.dedup();
        assert_eq!(versions.len(), total, "duplicate migration version among {versions:?}");
    }

    /// Selecting the model checks every column the schema declares, so this
    /// fails if `schema.rs` and the migrations have drifted apart — which is
    /// otherwise a runtime error rather than a compile one.
    #[test]
    fn a_conversation_starts_out_asking_about_every_edit() {
        use crate::db::models::conversation::ConversationRow;
        use crate::db::schema::conversations::dsl::*;

        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        conn.run_pending_migrations(MIGRATIONS).unwrap();
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

    /// Bring a database up to migration 18 only, so migration 19 can be tested
    /// against realistic pre-existing rows rather than against an empty schema.
    /// Running the whole migration set (as `test_db` does) would never exercise
    /// the data-mapping half of the migration.
    fn conn_at_18() -> SqliteConnection {
        conn_before("00000000000019")
    }

    fn conn_before(version: &str) -> SqliteConnection {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        let all = MigrationSource::<diesel::sqlite::Sqlite>::migrations(&MIGRATIONS).unwrap();
        for m in all {
            if m.name().version().as_owned() >= version.into() {
                break;
            }
            m.run(&mut conn).unwrap();
        }
        conn
    }

    fn run_migration(conn: &mut SqliteConnection, version: &str) {
        let all = MigrationSource::<diesel::sqlite::Sqlite>::migrations(&MIGRATIONS).unwrap();
        for m in all {
            if m.name().version().as_owned() == version.into() {
                m.run(conn).unwrap();
                return;
            }
        }
        panic!("migration {version} not found");
    }

    /// Everything from `version` onward, in order.
    ///
    /// A test that stops at the migration it is about still has to read the rows
    /// back, and the readers select every column the schema has *today* — so
    /// stopping short fails inside the reader rather than in anything the
    /// migration did. Catching up to head afterwards keeps such a test about its
    /// own migration, and stops the next column added to the same table from
    /// breaking it for a reason it has nothing to do with.
    fn run_migrations_from(conn: &mut SqliteConnection, version: &str) {
        let all = MigrationSource::<diesel::sqlite::Sqlite>::migrations(&MIGRATIONS).unwrap();
        for m in all {
            if m.name().version().as_owned() >= version.into() {
                m.run(conn).unwrap();
            }
        }
    }

    fn seed_pre19(conn: &mut SqliteConnection) {
        conn.batch_execute(
            "INSERT INTO projects (id, name, path, source_type, source_id, created_at, updated_at)
             VALUES ('p-desk', 'Desktop', '/tmp', 'local', NULL, 1, 1),
                    ('p-priv', 'QQ Alice', NULL, 'onebot_private', '10001', 1, 1),
                    ('p-grp',  'QQ Group', NULL, 'onebot_group',   '20002', 1, 1);

             INSERT INTO memories (id, project_id, key, content, memory_type, created_at, updated_at)
             VALUES ('m-desk', 'p-desk', 'style', 'terse', 'preference', 1, 1),
                    ('m-priv', 'p-priv', 'tz',    'UTC+8', 'fact',       1, 1),
                    ('m-grp',  'p-grp',  'slang', 'in-joke','general',   1, 1);",
        )
        .unwrap();
    }

    fn run_19(conn: &mut SqliteConnection) {
        let all = MigrationSource::<diesel::sqlite::Sqlite>::migrations(&MIGRATIONS).unwrap();
        for m in all {
            if m.name().version().as_owned() == "00000000000019".into() {
                m.run(conn).unwrap();
                return;
            }
        }
        panic!("migration 19 not found");
    }

    fn fetch(conn: &mut SqliteConnection, id: &str) -> MemoryRow {
        diesel::sql_query("SELECT scope_type, scope_id, subject_scope_id, origin FROM memories WHERE id = ?")
            .bind::<Text, _>(id)
            .get_result(conn)
            .unwrap()
    }

    /// The whole point of the data mapping: a private-chat memory becomes a
    /// memory about that person, so /memory me and opt-out can reach it.
    #[test]
    fn private_chat_memories_become_per_person() {
        let mut conn = conn_at_18();
        seed_pre19(&mut conn);
        run_19(&mut conn);

        let row = fetch(&mut conn, "m-priv");
        assert_eq!(row.scope_type, "onebot_user");
        assert_eq!(row.scope_id, "onebot:10001");
        assert_eq!(row.subject_scope_id.as_deref(), Some("onebot:10001"));
        assert_eq!(row.origin, "private");
    }

    /// Old group rows carry no trustworthy sender, so they must not pass as
    /// `group` — that would let them act as evidence from the identity pipeline.
    #[test]
    fn group_memories_are_marked_legacy() {
        let mut conn = conn_at_18();
        seed_pre19(&mut conn);
        run_19(&mut conn);

        let row = fetch(&mut conn, "m-grp");
        assert_eq!(row.scope_type, "project");
        assert_eq!(row.scope_id, "p-grp");
        assert_eq!(row.subject_scope_id, None);
        assert_eq!(row.origin, "legacy");
    }

    #[test]
    fn desktop_memories_stay_on_their_project() {
        let mut conn = conn_at_18();
        seed_pre19(&mut conn);
        run_19(&mut conn);

        let row = fetch(&mut conn, "m-desk");
        assert_eq!(row.scope_type, "project");
        assert_eq!(row.scope_id, "p-desk");
        assert_eq!(row.origin, "desktop");
    }

    /// A soft-deleted key must be creatable again; a plain unique index would
    /// force upsert to resurrect tombstones and break the trash.
    #[test]
    fn unique_index_only_constrains_live_rows() {
        let mut conn = conn_at_18();
        seed_pre19(&mut conn);
        run_19(&mut conn);

        conn.batch_execute(
            "UPDATE memories SET deleted_at = 99, deleted_by = 'self' WHERE id = 'm-desk';
             INSERT INTO memories (id, scope_type, scope_id, key, content, memory_type,
                                   origin, visibility, created_at, updated_at)
             VALUES ('m-desk2', 'project', 'p-desk', 'style', 'verbose', 'preference',
                     'desktop', 'normal', 2, 2);",
        )
        .expect("re-creating a soft-deleted key must be allowed");

        let live: CountRow = diesel::sql_query(
            "SELECT COUNT(*) AS n FROM memories \
             WHERE scope_id='p-desk' AND key='style' AND deleted_at IS NULL",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(live.n, 1);
    }

    /// Proposal ids must never be reused: a stale "同意 N" would otherwise
    /// approve a completely different proposal.
    #[test]
    fn proposal_ids_are_not_reused() {
        let mut conn = conn_at_18();
        run_19(&mut conn);

        conn.batch_execute(
            "INSERT INTO memory_proposals (key, content, memory_type, status, created_at, expires_at)
             VALUES ('a', 'x', 'general', 'pending', 1, 2);
             DELETE FROM memory_proposals;
             INSERT INTO memory_proposals (key, content, memory_type, status, created_at, expires_at)
             VALUES ('b', 'y', 'general', 'pending', 1, 2);",
        )
        .unwrap();

        let row: CountRow = diesel::sql_query("SELECT id AS n FROM memory_proposals WHERE key = 'b'")
            .get_result(&mut conn)
            .unwrap();
        assert_eq!(row.n, 2, "AUTOINCREMENT must not hand out id 1 again");
    }

    #[derive(QueryableByName)]
    struct IdRow {
        #[diesel(sql_type = Nullable<Text>)]
        id: Option<String>,
    }

    fn conn_at_20() -> SqliteConnection {
        conn_before("00000000000021")
    }

    /// Two conversations, one of them compacted, so the backfill has to keep the
    /// chains apart and place the summary anchor.
    fn seed_pre21(conn: &mut SqliteConnection) {
        conn.batch_execute(
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
        .unwrap();
    }

    fn parent_of(conn: &mut SqliteConnection, id: &str) -> Option<String> {
        diesel::sql_query("SELECT parent_id AS id FROM messages WHERE id = ?")
            .bind::<Text, _>(id)
            .get_result::<IdRow>(conn)
            .unwrap()
            .id
    }

    #[test]
    fn backfill_chains_existing_messages_in_order() {
        let mut conn = conn_at_20();
        seed_pre21(&mut conn);
        run_migration(&mut conn, "00000000000021");

        assert_eq!(parent_of(&mut conn, "a1"), None, "the first message is a root");
        assert_eq!(parent_of(&mut conn, "a2").as_deref(), Some("a1"));
        assert_eq!(parent_of(&mut conn, "a3").as_deref(), Some("a2"));
        assert_eq!(parent_of(&mut conn, "a4").as_deref(), Some("a3"));
    }

    /// The correlated subquery has to filter on conversation_id; without it every
    /// conversation would splice onto the globally previous message.
    #[test]
    fn backfill_keeps_conversations_apart() {
        let mut conn = conn_at_20();
        seed_pre21(&mut conn);
        run_migration(&mut conn, "00000000000021");

        assert_eq!(parent_of(&mut conn, "b1"), None);
        assert_eq!(parent_of(&mut conn, "b2").as_deref(), Some("b1"));
    }

    /// A summary sits beside the tree, not in it. Chaining it would make the
    /// first real message look like it had a sibling.
    #[test]
    fn backfill_leaves_summaries_off_the_chain() {
        let mut conn = conn_at_20();
        seed_pre21(&mut conn);
        run_migration(&mut conn, "00000000000021");

        assert_eq!(parent_of(&mut conn, "asum"), None);
        let children: CountRow = diesel::sql_query("SELECT COUNT(*) AS n FROM messages WHERE parent_id = 'asum'")
            .get_result(&mut conn)
            .unwrap();
        assert_eq!(children.n, 0);
    }

    /// The old cursor names a sort_order; the anchor is the first message at or
    /// past it.
    #[test]
    fn backfill_translates_the_compact_cursor_to_an_anchor() {
        let mut conn = conn_at_20();
        seed_pre21(&mut conn);
        run_migration(&mut conn, "00000000000021");

        let anchor = diesel::sql_query("SELECT compact_anchor_id AS id FROM messages WHERE id = 'asum'")
            .get_result::<IdRow>(&mut conn)
            .unwrap()
            .id;
        assert_eq!(
            anchor.as_deref(),
            Some("a3"),
            "cursor 3 maps to the row at sort_order 3"
        );
    }

    #[test]
    fn backfill_points_head_at_the_last_message() {
        let mut conn = conn_at_20();
        seed_pre21(&mut conn);
        run_migration(&mut conn, "00000000000021");

        let head = diesel::sql_query("SELECT head_message_id AS id FROM conversations WHERE id = 'c-a'")
            .get_result::<IdRow>(&mut conn)
            .unwrap()
            .id;
        assert_eq!(head.as_deref(), Some("a4"));
    }

    /// Migration 25 adds two nullable columns to `messages` rather than
    /// rebuilding the table, so rows written before it keep working untouched.
    /// `tool_outcome` reading as NULL is what makes that safe: NULL means
    /// success, which is what the transcript claimed for every one of them
    /// anyway.
    #[test]
    fn existing_messages_survive_the_turn_columns() {
        let mut conn = conn_before("00000000000025");
        conn.batch_execute(
            "INSERT INTO conversations (id, title, is_pinned, is_archived, message_count,
                                        created_at, updated_at, fast_mode)
             VALUES ('c1', 'A', 0, 0, 2, 1, 1, 0);
             INSERT INTO messages (id, conversation_id, role, content, sort_order,
                                   created_at, schema_version, is_compact_summary)
             VALUES ('m1', 'c1', 'user', 'hi', 1, 1, 2, 0),
                    ('m2', 'c1', 'tool', 'refused', 2, 2, 2, 0);",
        )
        .unwrap();

        run_migration(&mut conn, "00000000000025");

        #[derive(QueryableByName)]
        struct Cols {
            #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
            turn_id: Option<String>,
            #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
            tool_outcome: Option<String>,
        }
        let rows = diesel::sql_query("SELECT turn_id, tool_outcome FROM messages ORDER BY sort_order")
            .get_results::<Cols>(&mut conn)
            .unwrap();

        assert_eq!(rows.len(), 2, "no row is lost or duplicated");
        assert!(rows.iter().all(|r| r.turn_id.is_none()));
        assert!(
            rows.iter().all(|r| r.tool_outcome.is_none()),
            "nothing is backfilled: NULL already means what these rows meant",
        );
    }

    /// The `turns` table is new, so upgrading finds it empty — and startup
    /// reconciliation over an empty table must not report anything.
    #[test]
    fn upgrading_starts_with_no_turn_history() {
        let mut conn = conn_before("00000000000025");
        run_migration(&mut conn, "00000000000025");

        assert_eq!(ops::turn::reconcile_interrupted(&mut conn, 1000).unwrap(), 0);
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
    #[test]
    fn existing_turns_survive_the_reported_columns_still_owing_their_explanation() {
        let mut conn = conn_before("00000000000026");
        conn.batch_execute(
            "INSERT INTO conversations (id, title, is_pinned, is_archived, message_count,
                                        created_at, updated_at, fast_mode)
             VALUES ('c1', 'A', 0, 0, 0, 1, 1, 0);
             INSERT INTO turns (id, conversation_id, origin, status, phase, phase_tool,
                                started_at, updated_at, ended_at)
             VALUES ('t-cut', 'c1', 'desktop', 'interrupted', 'running_tool', 'edit_file',
                     1000, 1001, 1500),
                    ('t-done', 'c1', 'desktop', 'done', 'streaming', NULL, 2000, 2001, 2500);",
        )
        .unwrap();

        run_migrations_from(&mut conn, "00000000000026");

        let turns = ops::turn::list_for_conversation(&mut conn, "c1").unwrap();
        assert_eq!(turns.len(), 2, "no row is lost or duplicated");
        assert!(turns.iter().all(|t| t.reported_at.is_none()), "nothing is backfilled");
        assert!(
            turns.iter().all(|t| t.parent_reported_at.is_none()),
            "and neither ledger starts out settled",
        );
        // The one that was cut off is still owed its explanation, and the one
        // that finished never was.
        let cut = &turns[0];
        assert_eq!(cut.id, "t-cut");
        assert_eq!(cut.phase_tool.as_deref(), Some("edit_file"));
        assert_eq!(cut.ended_at, Some(1500));
        assert_eq!(
            ids_of(ops::turn::unreported_for_conversation(&mut conn, "c1", None, 10).unwrap()),
            ["t-cut"],
        );
    }

    /// Migration 27 hides a conversation from the sidebar by giving it a parent.
    /// Every row that predates it has none, so the lists it appears in must not
    /// change — a conversation the user started years ago cannot become a
    /// sub-agent's transcript because a column arrived.
    #[test]
    fn conversations_that_predate_delegation_stay_in_the_lists() {
        let mut conn = conn_before("00000000000027");
        conn.batch_execute(
            "INSERT INTO conversations (id, title, is_pinned, is_archived, message_count,
                                        created_at, updated_at, fast_mode)
             VALUES ('c1', 'A', 0, 0, 0, 1, 1, 0);",
        )
        .unwrap();

        run_migrations_from(&mut conn, "00000000000027");

        let listed = ops::conversation::list_conversations(&mut conn, false).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].parent_conversation_id.is_none());
        assert!(
            listed[0].agent_model_id.is_none(),
            "and it goes on resolving from the assistant"
        );
        assert!(ops::conversation::sub_agent_runs(&mut conn, "c1").unwrap().is_empty());
    }

    /// Migration 48 removes only schema that had no runtime owner. Existing
    /// conversations and messages must survive even when an attachment row and
    /// the legacy compaction cursor are present in the old database.
    #[test]
    fn dead_schema_is_removed_without_touching_live_rows() {
        let mut conn = conn_before("00000000000048");
        conn.batch_execute(
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
        .unwrap();

        run_migration(&mut conn, "00000000000048");

        let dead_tables: CountRow = diesel::sql_query(
            "SELECT COUNT(*) AS n FROM sqlite_master
             WHERE type = 'table' AND name IN ('attachments', 'tool_permissions')",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(dead_tables.n, 0);

        let dead_columns: CountRow = diesel::sql_query(
            "SELECT COUNT(*) AS n FROM pragma_table_info('conversations')
             WHERE name = 'compact_cursor'",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(dead_columns.n, 0);

        let live_rows: CountRow = diesel::sql_query(
            "SELECT COUNT(*) AS n FROM conversations c
             JOIN messages m ON m.conversation_id = c.id
             WHERE c.id = 'c1' AND m.id = 'm1'",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(live_rows.n, 1);
    }

    /// Migration 49 is a data migration, not merely a column-type change. It
    /// must turn legacy REAL values and numeric tier JSON into the one canonical
    /// string representation accepted by the Decimal protocol. The old 0/0
    /// sentinel becomes NULL only when both token prices are zero, so a model
    /// with free input and paid output keeps that deliberate zero.
    #[test]
    fn exact_decimal_migration_canonicalizes_existing_money() {
        let mut conn = conn_before("00000000000049");
        conn.batch_execute(
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
        .unwrap();

        run_migration(&mut conn, "00000000000049");

        let model: DecimalMigrationRow = diesel::sql_query(
            "SELECT input_price, output_price, cache_read_price, pricing_tiers
             FROM model_configs WHERE id = 'mc1'",
        )
        .get_result(&mut conn)
        .unwrap();
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

        let future: DecimalMigrationRow = diesel::sql_query(
            "SELECT input_price, output_price, cache_read_price, pricing_tiers
             FROM model_configs WHERE id = 'mc_future'",
        )
        .get_result(&mut conn)
        .unwrap();
        let future_tiers: serde_json::Value = serde_json::from_str(future.pricing_tiers.as_deref().unwrap()).unwrap();
        assert_eq!(future_tiers[0]["future"], "keep");
        assert!(future_tiers[0].get("cache_read_price").is_some());
        assert!(future_tiers[0]["cache_read_price"].is_null());
        assert!(crate::agent::pricing::parse_tiers(future.pricing_tiers.as_deref()).is_err());

        let free: DecimalMigrationRow = diesel::sql_query(
            "SELECT input_price, output_price, cache_read_price, pricing_tiers
             FROM model_configs WHERE id = 'mc_free'",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(free.input_price, None);
        assert_eq!(free.output_price, None);

        let audit: DecimalMigrationRow = diesel::sql_query(
            "SELECT input_price, output_price, cache_read_price AS cache_read_price,
                    NULL AS pricing_tiers
             FROM audit_messages WHERE id = 'a1'",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(audit.input_price.as_deref(), Some("1.25"));
        assert_eq!(audit.output_price.as_deref(), Some("2.5"));
        assert_eq!(audit.cache_read_price.as_deref(), Some("0.125"));

        conn.batch_execute(
            "UPDATE audit_messages
             SET input_price = '12.34', output_price = '23.45',
                 cache_read_price = '0.12', cache_write_price = '0.34',
                 server_tool_price = '4.56'
             WHERE id = 'a1'",
        )
        .unwrap();
        let canonical_audit_money: CountRow = diesel::sql_query(
            "SELECT COUNT(*) AS n FROM audit_messages
             WHERE id = 'a1'
               AND typeof(input_price) = 'text'
               AND typeof(output_price) = 'text'
               AND typeof(cache_read_price) = 'text'
               AND typeof(cache_write_price) = 'text'
               AND typeof(server_tool_price) = 'text'",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(canonical_audit_money.n, 1);

        for column in [
            "input_price",
            "output_price",
            "cache_read_price",
            "cache_write_price",
            "server_tool_price",
        ] {
            assert!(
                conn.batch_execute(&format!("UPDATE audit_messages SET {column} = x'3132' WHERE id = 'a1'"))
                    .is_err(),
                "audit_messages.{column} must reject BLOB money"
            );
        }

        assert!(
            conn.batch_execute("UPDATE model_configs SET output_price = '01.25' WHERE id = 'mc1'")
                .is_err()
        );
        assert!(
            conn.batch_execute("UPDATE model_configs SET output_price = '' WHERE id = 'mc1'")
                .is_err()
        );
        assert!(
            conn.batch_execute("UPDATE audit_messages SET billing_mode = 'future' WHERE id = 'a1'")
                .is_err()
        );

        let empty_threshold: CountRow = diesel::sql_query(
            "SELECT COUNT(*) AS n FROM preferences
             WHERE key = 'onebot.balance_alert_threshold'",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(empty_threshold.n, 0);
    }

    fn seed_auto_review_before_50(conn: &mut SqliteConnection, raw: &str) {
        conn.batch_execute(
            "INSERT INTO conversations
                 (id, is_pinned, is_archived, message_count, created_at, updated_at)
             VALUES ('c1', 0, 0, 1, 1, 1);
             INSERT INTO messages
                 (id, conversation_id, role, content, sort_order, created_at,
                  schema_version, is_compact_summary)
             VALUES ('m1', 'c1', 'assistant', '', 1, 1, 2, 0);",
        )
        .unwrap();
        diesel::sql_query("UPDATE messages SET auto_review = ? WHERE id = 'm1'")
            .bind::<Text, _>(raw)
            .execute(conn)
            .unwrap();
    }

    #[test]
    fn auto_review_migration_rewrites_legacy_rows_to_the_only_public_shape() {
        let mut conn = conn_before("00000000000050");
        seed_auto_review_before_50(
            &mut conn,
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
        );

        run_migration(&mut conn, "00000000000050");

        let stored: AutoReviewMigrationRow = diesel::sql_query("SELECT auto_review FROM messages WHERE id = 'm1'")
            .get_result(&mut conn)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(stored.auto_review.as_deref().unwrap()).unwrap();
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

    #[test]
    fn auto_review_migration_rejects_json_it_cannot_canonicalize() {
        for (label, raw) in [
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
            let mut conn = conn_before("00000000000050");
            seed_auto_review_before_50(&mut conn, raw);
            let all = MigrationSource::<diesel::sqlite::Sqlite>::migrations(&MIGRATIONS).unwrap();
            let migration = all
                .into_iter()
                .find(|migration| migration.name().version().as_owned() == "00000000000050".into())
                .unwrap();
            assert!(migration.run(&mut conn).is_err(), "{label} must fail migration 50");

            let stored: AutoReviewMigrationRow = diesel::sql_query("SELECT auto_review FROM messages WHERE id = 'm1'")
                .get_result(&mut conn)
                .unwrap();
            assert_eq!(
                stored.auto_review.as_deref(),
                Some(raw),
                "{label} must not be rewritten"
            );
        }
    }

    #[test]
    fn sandbox_mode_migration_preserves_legacy_and_canonical_choices() {
        for (stored, expected) in [
            ("true", "auto"),
            ("false", "off"),
            ("auto", "auto"),
            ("off", "off"),
            ("container", "container"),
            ("FALSE", "FALSE"),
        ] {
            let mut conn = conn_before("00000000000052");
            conn.batch_execute(&format!(
                "INSERT INTO preferences (key, value, updated_at)
                 VALUES ('sandbox.enabled', '{stored}', 17),
                        ('unrelated.enabled', 'false', 23);"
            ))
            .unwrap();

            run_migration(&mut conn, "00000000000052");

            use crate::db::schema::preferences::dsl::{preferences, updated_at, value};
            let migrated: (String, i64) = preferences
                .find("sandbox.enabled")
                .select((value, updated_at))
                .first(&mut conn)
                .unwrap();
            assert_eq!(migrated, (expected.into(), 17), "legacy value {stored:?}");
            assert_eq!(
                ops::preference::get_preference(&mut conn, "unrelated.enabled")
                    .unwrap()
                    .as_deref(),
                Some("false")
            );
        }
    }

    fn ids_of(candidates: Vec<ops::turn::InterruptedCandidate>) -> Vec<String> {
        candidates.into_iter().map(|c| c.turn.id).collect()
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_db() -> DbPool {
    let manager = ConnectionManager::<SqliteConnection>::new(":memory:");
    let pool = Pool::builder()
        .max_size(1)
        .connection_customizer(Box::new(ConnectionCustomizer))
        .build(manager)
        .expect("failed to create test db pool");

    let mut conn = pool.get().expect("failed to get test db connection");
    conn.run_pending_migrations(MIGRATIONS)
        .expect("failed to run test migrations");

    pool
}
