use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::entity::conversation;
use crate::db::models::conversation::{ConversationInsert, ConversationRow};
use crate::db::schema::conversations;
// One transcript reading, whichever ORM ran the query.

/// A Diesel row as the entity model; a stored value this build cannot read
/// fails the read, as it does on the SeaORM side.
fn model(row: ConversationRow) -> QueryResult<conversation::Model> {
    conversation::Model::try_from(row).map_err(|error| {
        diesel::result::Error::DeserializationError(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error,
        )))
    })
}

/// The user's own conversations, newest first.
///
/// Sub-agent transcripts are excluded here rather than by archiving them: the
/// archive flag is a user's decision and can be undone, and the project view
/// deliberately concatenates archived rows onto active ones, which would spill
/// them back into the list. Being spawned is not a decision anyone can reverse.
/// Every conversation id — archived and delegated runs included, unlike the
/// sidebar queries below. For reconciling external resources keyed by id: a
/// container judged an orphan against a *filtered* list is one an archived
/// conversation was still counting on.
pub fn all_ids(conn: &mut SqliteConnection) -> QueryResult<Vec<String>> {
    conversations::table.select(conversations::id).load(conn)
}

pub fn get_conversation(conn: &mut SqliteConnection, id: &str) -> QueryResult<conversation::Model> {
    conversations::table
        .find(id)
        .first::<ConversationRow>(conn)
        .and_then(model)
}

pub fn create_conversation(
    conn: &mut SqliteConnection,
    id: &str,
    title: Option<&str>,
    assistant_id: Option<&str>,
    project_id: Option<&str>,
    now: i64,
) -> QueryResult<conversation::Model> {
    let new = ConversationInsert {
        id,
        title,
        assistant_id,
        is_pinned: 0,
        is_archived: 0,
        created_at: now,
        updated_at: now,
        project_id,
        ..Default::default()
    };
    insert(conn, new)
}

/// Insert a prepared row. Split out so a sub-agent can fill the spawned-by
/// columns without `create_conversation` growing seven more parameters that
/// every ordinary caller would pass `None` to.
pub fn insert(conn: &mut SqliteConnection, new: ConversationInsert<'_>) -> QueryResult<conversation::Model> {
    let id = new.id.to_string();
    diesel::insert_into(conversations::table).values(&new).execute(conn)?;
    conversations::table
        .find(&id)
        .first::<ConversationRow>(conn)
        .and_then(model)
}

pub fn update_title(conn: &mut SqliteConnection, id: &str, title: &str, now: i64) -> QueryResult<()> {
    diesel::update(conversations::table.find(id))
        .set((conversations::title.eq(title), conversations::updated_at.eq(now)))
        .execute(conn)?;
    Ok(())
}

/// Flip the pin and hand back the row as it is after the flip.
///
/// `immediate_transaction`, because a read followed by a write on autocommit
/// is two connections' worth of race: both read `0`, both write `1`, and one
/// of two presses is lost. `BEGIN IMMEDIATE` takes the write lock *before* the
/// read, so the second caller waits and then reads the first one's result. The
/// same lock is what makes the row read back at the end this call's own
/// outcome rather than whatever a later caller has since written.
/// `concurrent_toggles_are_each_applied` is the test that goes red without it.
pub fn toggle_pin(conn: &mut SqliteConnection, id: &str, now: i64) -> QueryResult<conversation::Model> {
    conn.immediate_transaction(|conn| {
        let conv = conversations::table.find(id).first::<ConversationRow>(conn)?;
        let new_pinned = if conv.is_pinned == 0 { 1 } else { 0 };
        diesel::update(conversations::table.find(id))
            .set((
                conversations::is_pinned.eq(new_pinned),
                conversations::updated_at.eq(now),
            ))
            .execute(conn)?;
        conversations::table
            .find(id)
            .first::<ConversationRow>(conn)
            .and_then(model)
    })
}

/// The archive flag's `toggle_pin`, under the same lock for the same reason.
pub fn toggle_archive(conn: &mut SqliteConnection, id: &str, now: i64) -> QueryResult<conversation::Model> {
    conn.immediate_transaction(|conn| {
        let conv = conversations::table.find(id).first::<ConversationRow>(conn)?;
        let new_archived = if conv.is_archived == 0 { 1 } else { 0 };
        diesel::update(conversations::table.find(id))
            .set((
                conversations::is_archived.eq(new_archived),
                conversations::updated_at.eq(now),
            ))
            .execute(conn)?;
        conversations::table
            .find(id)
            .first::<ConversationRow>(conn)
            .and_then(model)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::diesel_test_db;

    /// A new conversation inherits the assistant's reasoning preferences.
    /// Setting and clearing them is `db::sea::ops::conversation`'s test.
    #[test]
    fn a_new_conversation_inherits_its_reasoning_prefs() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();

        let conv = create_conversation(&mut conn, "c1", Some("t"), None, None, 1).unwrap();
        assert_eq!(conv.thinking_level, None, "defaults to inheriting the assistant");
        assert!(!conv.fast_mode.get());
    }

    /// Everything a delegated run needs in the database, written the way
    /// `commands::sub_agent` will write it.
    fn spawn(conn: &mut SqliteConnection, id: &str, parent: &str, message_id: &str, call_id: &str, turn_id: &str) {
        // The project is inherited, the way `commands::sub_agent` will inherit
        // it: tools resolve their paths through it.
        let project_id = conversations::table
            .find(parent)
            .select(conversations::project_id)
            .first::<Option<String>>(conn)
            .unwrap();
        insert(
            conn,
            ConversationInsert {
                id,
                title: Some("look something up"),
                is_pinned: 0,
                is_archived: 0,
                created_at: 10,
                updated_at: 10,
                project_id: project_id.as_deref(),
                parent_conversation_id: Some(parent),
                spawned_by_message_id: Some(message_id),
                spawned_by_call_id: Some(call_id),
                spawned_turn_id: Some(turn_id),
                agent_kind: Some("explore"),
                ..Default::default()
            },
        )
        .unwrap();
        crate::db::ops::turn::begin(conn, turn_id, id, crate::turn::TurnOrigin::SubAgent, None, 10).unwrap();
    }

    /// Two connections toggling one row at the same time must each land their
    /// toggle. Read-then-write on autocommit does not: both read the same
    /// value, both write the same inverse, and one of the two presses is gone.
    /// Not `diesel_test_db()`, whose pool holds one connection to a private in-memory
    /// database — the race needs two connections to one file, which is what
    /// the desktop's pool is. This is the test that goes red without the
    /// `immediate_transaction`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_toggles_are_each_applied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.sqlite");
        crate::db::sea::bridge::migrate_file(&path).await.unwrap();
        let pool = crate::db::init_db(path.to_str().unwrap());
        create_conversation(&mut pool.get().unwrap(), "c1", None, None, None, 1).unwrap();

        /// A writer starved past `busy_timeout` answers "database is locked".
        /// SQLite's busy handler sleeps and retries with no fairness, and four
        /// threads writing back to back can keep one of them asleep for the
        /// whole five seconds on a slow CI runner — measured there, not here.
        /// Availability under that load is not what this test is about; a
        /// toggle that was refused changed nothing and is simply asked again.
        fn until_applied(mut op: impl FnMut() -> QueryResult<conversation::Model>) {
            loop {
                match op() {
                    Ok(_) => return,
                    Err(diesel::result::Error::DatabaseError(_, info)) if info.message() == "database is locked" => {}
                    Err(e) => panic!("{e}"),
                }
            }
        }

        const ROUNDS: i64 = 2000;
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let pool = pool.clone();
                std::thread::spawn(move || {
                    let mut conn = pool.get().unwrap();
                    // Per connection. Durability is not what is under test, and
                    // an fsync per write would spend the whole run waiting on
                    // the disk rather than on each other.
                    diesel::sql_query("PRAGMA synchronous=OFF").execute(&mut conn).unwrap();
                    for i in 0..ROUNDS {
                        until_applied(|| toggle_archive(&mut conn, "c1", i));
                        until_applied(|| toggle_pin(&mut conn, "c1", i));
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }

        // Every thread applied an even number of each toggle, so both flags
        // are back where they started — unless a press was lost.
        let conv = get_conversation(&mut pool.get().unwrap(), "c1").unwrap();
        assert_eq!(
            (conv.is_archived.get(), conv.is_pinned.get()),
            (false, false),
            "an even number of toggles lands back where it started"
        );
    }

    /// Rows written before this migration have every new column empty, and go on
    /// behaving exactly as they did.
    #[test]
    fn conversations_that_predate_delegation_are_ordinary() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        let conv = create_conversation(&mut conn, "c1", Some("t"), None, None, 1).unwrap();

        assert!(conv.parent_conversation_id.is_none());
        assert!(conv.spawned_by_message_id.is_none());
        assert!(conv.spawned_by_call_id.is_none());
        assert!(conv.spawned_turn_id.is_none());
        assert!(conv.agent_kind.is_none());
        assert!(conv.agent_provider_id.is_none());
        assert!(conv.agent_model_id.is_none(), "it goes on resolving from the assistant");
    }

    /// Three paths ask what model a conversation runs on — the next turn, the
    /// context indicator, and manual compaction — and a delegated run has to
    /// give all three the model its transcript was written by. Getting this
    /// wrong is not visible as an error: a run on a 64K model reports how full
    /// a 200K window is, and compaction waits for a threshold no request will
    /// ever reach.
    #[test]
    fn a_delegated_run_pins_the_model_its_transcript_was_written_by() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "parent", Some("t"), None, None, 1).unwrap();
        spawn(&mut conn, "child", "parent", "m1", "0", "t-a");
        diesel::update(conversations::table.find("child"))
            .set((
                conversations::agent_provider_id.eq("deepseek"),
                conversations::agent_model_id.eq("deepseek-chat"),
            ))
            .execute(&mut conn)
            .unwrap();

        let big = crate::db::entity::assistant::Model {
            provider_id: Some("anthropic".into()),
            model_id: Some("mythos".into()),
            context_limit: 200_000,
            ..assistant()
        };

        let parent = get_conversation(&mut conn, "parent").unwrap();
        let unchanged = parent.pin_model(Some(big.clone())).unwrap();
        assert_eq!(unchanged.model_id.as_deref(), Some("mythos"));
        assert_eq!(
            unchanged.context_limit, 200_000,
            "an ordinary conversation keeps its own"
        );

        let child = get_conversation(&mut conn, "child").unwrap();
        let pinned = child.pin_model(Some(big)).unwrap();
        assert_eq!(pinned.provider_id.as_deref(), Some("deepseek"));
        assert_eq!(pinned.model_id.as_deref(), Some("deepseek-chat"));
        // The part that is easy to miss: a non-zero limit here outranks
        // everything the model says, so leaving it would make the swap look
        // done while changing nothing that matters.
        assert_eq!(pinned.context_limit, 0, "the window comes from the model now");
    }

    fn assistant() -> crate::db::entity::assistant::Model {
        crate::db::entity::assistant::Model {
            id: "a1".into(),
            name: "A".into(),
            description: None,
            avatar: None,
            system_prompt: String::new(),
            provider_id: None,
            model_id: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            is_default: crate::db::types::SqlBool::FALSE,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
            context_limit: 0,
            compact_keep_recent: 10,
            enabled_tools: None,
            thinking_enabled: crate::db::types::SqlBool::FALSE,
            thinking_budget: None,
            tool_preset_id: None,
            auto_compact_enabled: crate::db::types::SqlBool::FALSE,
        }
    }

    /// The Diesel reads hand out the entity model and hold it to the same
    /// rules as the SeaORM read: a flag that is not 0/1, or a turn status this
    /// build does not know, fails the read rather than reaching a response.
    #[tokio::test]
    async fn a_stored_value_the_model_cannot_hold_fails_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let (pool, sea) = crate::db::sea::shared_test_db(dir.path()).await;
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        crate::db::ops::turn::begin(&mut conn, "t1", "c1", crate::turn::TurnOrigin::Desktop, None, 1).unwrap();
        crate::db::sea::execute_for_tests(
            &sea,
            "UPDATE conversations SET is_pinned = 2 WHERE id = 'c1';
             UPDATE turns SET status = 'from_the_future' WHERE id = 't1'",
        )
        .await
        .unwrap();

        let error = get_conversation(&mut conn, "c1").unwrap_err().to_string();
        assert!(error.contains("conversation c1 has an invalid is_pinned"), "{error}");
    }
}
