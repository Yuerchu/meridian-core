use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::conversation::{ConversationInsert, ConversationRow, SubAgentRun};
use crate::db::models::turn::TurnRow;
use crate::db::schema::conversations;

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

pub fn list_conversations(conn: &mut SqliteConnection, archived: bool) -> QueryResult<Vec<ConversationRow>> {
    let archived_val = if archived { 1 } else { 0 };
    conversations::table
        .filter(conversations::is_archived.eq(archived_val))
        .filter(conversations::parent_conversation_id.is_null())
        .order((conversations::is_pinned.desc(), conversations::updated_at.desc()))
        .load::<ConversationRow>(conn)
}

pub fn get_conversation(conn: &mut SqliteConnection, id: &str) -> QueryResult<ConversationRow> {
    conversations::table.find(id).first::<ConversationRow>(conn)
}

pub fn create_conversation(
    conn: &mut SqliteConnection,
    id: &str,
    title: Option<&str>,
    assistant_id: Option<&str>,
    project_id: Option<&str>,
    now: i64,
) -> QueryResult<ConversationRow> {
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
pub fn insert(conn: &mut SqliteConnection, new: ConversationInsert<'_>) -> QueryResult<ConversationRow> {
    let id = new.id.to_string();
    diesel::insert_into(conversations::table).values(&new).execute(conn)?;
    conversations::table.find(&id).first::<ConversationRow>(conn)
}

pub fn list_conversations_by_project(
    conn: &mut SqliteConnection,
    project_id: &str,
    archived: bool,
) -> QueryResult<Vec<ConversationRow>> {
    let archived_val = if archived { 1 } else { 0 };
    conversations::table
        .filter(conversations::project_id.eq(project_id))
        .filter(conversations::is_archived.eq(archived_val))
        .filter(conversations::parent_conversation_id.is_null())
        .order((conversations::is_pinned.desc(), conversations::updated_at.desc()))
        .load::<ConversationRow>(conn)
}

/// Every conversation whose runtime workspace is derived from this project.
///
/// Unlike the sidebar query above this includes archived and delegated rows:
/// changing or deleting the project changes their next turn's working
/// directory too, and an active turn/review on either kind must block it.
pub fn ids_by_project(conn: &mut SqliteConnection, project_id: &str) -> QueryResult<Vec<String>> {
    conversations::table
        .filter(conversations::project_id.eq(project_id))
        .select(conversations::id)
        .order(conversations::id.asc())
        .load(conn)
}

pub fn update_title(conn: &mut SqliteConnection, id: &str, title: &str, now: i64) -> QueryResult<()> {
    diesel::update(conversations::table.find(id))
        .set((conversations::title.eq(title), conversations::updated_at.eq(now)))
        .execute(conn)?;
    Ok(())
}

pub fn update_assistant(
    conn: &mut SqliteConnection,
    id: &str,
    assistant_id: Option<&str>,
    now: i64,
) -> QueryResult<()> {
    diesel::update(conversations::table.find(id))
        .set((
            conversations::assistant_id.eq(assistant_id),
            conversations::updated_at.eq(now),
        ))
        .execute(conn)?;
    Ok(())
}

pub fn toggle_pin(conn: &mut SqliteConnection, id: &str, now: i64) -> QueryResult<ConversationRow> {
    let conv = conversations::table.find(id).first::<ConversationRow>(conn)?;
    let new_pinned = if conv.is_pinned == 0 { 1 } else { 0 };
    diesel::update(conversations::table.find(id))
        .set((
            conversations::is_pinned.eq(new_pinned),
            conversations::updated_at.eq(now),
        ))
        .execute(conn)?;
    conversations::table.find(id).first::<ConversationRow>(conn)
}

pub fn archive_conversation(conn: &mut SqliteConnection, id: &str, now: i64) -> QueryResult<()> {
    diesel::update(conversations::table.find(id))
        .set((conversations::is_archived.eq(1), conversations::updated_at.eq(now)))
        .execute(conn)?;
    Ok(())
}

pub fn toggle_archive(conn: &mut SqliteConnection, id: &str, now: i64) -> QueryResult<ConversationRow> {
    let conv = conversations::table.find(id).first::<ConversationRow>(conn)?;
    let new_archived = if conv.is_archived == 0 { 1 } else { 0 };
    diesel::update(conversations::table.find(id))
        .set((
            conversations::is_archived.eq(new_archived),
            conversations::updated_at.eq(now),
        ))
        .execute(conn)?;
    conversations::table.find(id).first::<ConversationRow>(conn)
}

/// Persist the per-conversation reasoning preferences. `thinking_level` of
/// `None` means "inherit the assistant default".
pub fn update_reasoning_prefs(
    conn: &mut SqliteConnection,
    id: &str,
    thinking_level: Option<&str>,
    fast_mode: bool,
    now: i64,
) -> QueryResult<()> {
    diesel::update(conversations::table.find(id))
        .set((
            conversations::thinking_level.eq(thinking_level),
            conversations::fast_mode.eq(i32::from(fast_mode)),
            conversations::updated_at.eq(now),
        ))
        .execute(conn)?;
    Ok(())
}

/// Persist the collaboration mode. `None` means the default (work) mode.
///
/// Deliberately its own setter rather than another parameter on
/// `update_reasoning_prefs`: that one already writes two fields at once, which
/// forces every caller to pass the current value of the other. A third field
/// would make all three callers depend on each other.
pub fn update_mode(conn: &mut SqliteConnection, id: &str, mode: Option<&str>, now: i64) -> QueryResult<()> {
    diesel::update(conversations::table.find(id))
        .set((conversations::mode.eq(mode), conversations::updated_at.eq(now)))
        .execute(conn)?;
    Ok(())
}

/// Refile the conversation under another project, or under none.
///
/// Organisational for a hosted conversation — its working directory lives in
/// `acp_sessions` — but load-bearing for a native one: the project's path is
/// what the next turn resolves its working directory and `FileAccess` against,
/// so moving a conversation changes what its tools may reach from here on.
pub fn update_project(conn: &mut SqliteConnection, id: &str, project_id: Option<&str>, now: i64) -> QueryResult<()> {
    diesel::update(conversations::table.find(id))
        .set((
            conversations::project_id.eq(project_id),
            conversations::updated_at.eq(now),
        ))
        .execute(conn)?;
    Ok(())
}

/// Its own setter for the same reason as `update_mode`, and kept apart from it
/// for a second one: a mode narrows what the assistant can do, this widens what
/// it can do without asking. Writing both through one call would suggest they
/// are two settings of the same kind.
pub fn update_accept_edits(conn: &mut SqliteConnection, id: &str, accept_edits: bool, now: i64) -> QueryResult<()> {
    diesel::update(conversations::table.find(id))
        .set((
            conversations::accept_edits.eq(i32::from(accept_edits)),
            conversations::updated_at.eq(now),
        ))
        .execute(conn)?;
    Ok(())
}

/// The conversations spawned by this one, oldest first.
///
/// Ordered by creation so that a caller comparing two readings of this list can
/// compare them element by element, and so leases are always taken in the same
/// order.
pub fn sub_agent_conversation_ids(conn: &mut SqliteConnection, parent_id: &str) -> QueryResult<Vec<String>> {
    conversations::table
        .filter(conversations::parent_conversation_id.eq(parent_id))
        .order(conversations::created_at.asc())
        .select(conversations::id)
        .load::<String>(conn)
}

/// One conversation that says the query somewhere in its transcript, with a
/// snippet around the newest mention.
#[derive(Debug)]
pub struct TranscriptHit {
    pub conversation_id: String,
    pub title: Option<String>,
    /// Who said the matched line — `user` or `assistant`.
    pub role: String,
    pub snippet: String,
    pub created_at: i64,
}

#[derive(diesel::QueryableByName)]
struct RawTranscriptHitRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    conversation_id: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    title: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Text)]
    role: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    content: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    created_at: i64,
}

/// Rows fetched per page while scanning. A bound on memory per round trip,
/// **never on the answer**: the scan pages on until `limit` conversations are
/// found or the candidates run out. Capping the scan itself was the first
/// version's defect — one talkative conversation filled the whole window,
/// newest first, and every quieter conversation behind it vanished from the
/// results before the dedupe ever saw them.
const SEARCH_PAGE: i64 = 400;

/// The candidate rows: what the SQL side may prefilter away, and the shared
/// WHERE/ORDER of both pages of the scan.
///
/// The prefilter must pass a **superset** of what the recheck accepts, and raw
/// LIKE alone is not one: a block-array row stores its words JSON-encoded, so
/// a query containing `"`, `\` or a newline matches the readable text and not
/// the stored bytes, and a phrase can span two `text` blocks that the encoding
/// keeps apart. Those rows are shipped wholesale (`LIKE '[%'` — the same
/// predicate `searchable_text` decodes by, and the two must stay identical)
/// and judged in Rust; plain rows are prefiltered by literal LIKE, which for
/// them is exact.
const SEARCH_CANDIDATES: &str = "FROM messages m JOIN conversations c ON c.id = m.conversation_id \
     WHERE c.parent_conversation_id IS NULL \
       AND m.role IN ('user', 'assistant') \
       AND (m.content LIKE ? ESCAPE '\\' OR m.content LIKE '[%')";

/// Full-text search over what people and the assistant actually said.
///
/// Two layers with one meaning: SQL prefilters candidates (see
/// [`SEARCH_CANDIDATES`]) and the Rust side decides on the row's *readable*
/// text — so a match inside a base64 data URI is not a mention, and words
/// inside a block array are found however JSON spelled them. `user` and
/// `assistant` rows only: `context` is injected background and tool rows are
/// machine output, and surfacing either as "the conversation said this" is how
/// search results stop being believed.
pub fn search_transcripts(conn: &mut SqliteConnection, query: &str, limit: usize) -> QueryResult<Vec<TranscriptHit>> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    // `%` and `_` are wildcards to LIKE; someone searching for "100%" means
    // the characters. ESCAPE has no default, so the clause names one. This
    // only narrows the *prefilter* — the recheck below already refuses a row
    // whose readable text lacks the literal query, so an unescaped wildcard
    // could widen the scan but never the answer.
    let escaped = trimmed.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let pattern = format!("%{escaped}%");

    let first_page = format!(
        "SELECT m.id, m.conversation_id, c.title, m.role, m.content, m.created_at {SEARCH_CANDIDATES} \
         ORDER BY m.created_at DESC, m.id DESC LIMIT ?"
    );
    // Keyset, not OFFSET: the tie-break on `m.id` is what stops a run of rows
    // sharing one millisecond from being skipped or served twice across pages.
    let next_page = format!(
        "SELECT m.id, m.conversation_id, c.title, m.role, m.content, m.created_at {SEARCH_CANDIDATES} \
           AND (m.created_at < ? OR (m.created_at = ? AND m.id < ?)) \
         ORDER BY m.created_at DESC, m.id DESC LIMIT ?"
    );

    let mut seen = std::collections::HashSet::new();
    let mut hits = Vec::new();
    let mut cursor: Option<(i64, String)> = None;
    loop {
        let raw: Vec<RawTranscriptHitRow> = match &cursor {
            None => diesel::sql_query(&first_page)
                .bind::<diesel::sql_types::Text, _>(&pattern)
                .bind::<diesel::sql_types::BigInt, _>(SEARCH_PAGE)
                .load(conn)?,
            Some((at, id)) => diesel::sql_query(&next_page)
                .bind::<diesel::sql_types::Text, _>(&pattern)
                .bind::<diesel::sql_types::BigInt, _>(*at)
                .bind::<diesel::sql_types::BigInt, _>(*at)
                .bind::<diesel::sql_types::Text, _>(id)
                .bind::<diesel::sql_types::BigInt, _>(SEARCH_PAGE)
                .load(conn)?,
        };
        let page_len = raw.len() as i64;
        for row in raw {
            // Advanced on every row, refused or not — the cursor tracks the
            // scan, and the scan includes what the recheck threw away.
            cursor = Some((row.created_at, row.id));
            if seen.contains(&row.conversation_id) {
                continue;
            }
            let Some(snippet) = snippet_around(&searchable_text(&row.content), trimmed) else {
                continue;
            };
            seen.insert(row.conversation_id.clone());
            hits.push(TranscriptHit {
                conversation_id: row.conversation_id,
                title: row.title,
                role: row.role,
                snippet,
                created_at: row.created_at,
            });
            if hits.len() >= limit {
                return Ok(hits);
            }
        }
        if page_len < SEARCH_PAGE {
            return Ok(hits);
        }
    }
}

/// What a row *reads as*. A block-array row stores JSON; the words are its
/// `text` members and everything else — data URIs, type tags — is transport.
///
/// The leading test is byte-for-byte the SQL prefilter's `LIKE '[%'` branch,
/// on purpose: a row this function decodes but the prefilter does not ship is
/// a row that can never match. The `type` check keeps a *plain* message that
/// happens to be a JSON array — someone pasting `[1, 2, 3]` — matchable as the
/// text it is.
fn searchable_text(content: &str) -> String {
    if content.starts_with('[')
        && let Ok(serde_json::Value::Array(parts)) = serde_json::from_str::<serde_json::Value>(content)
        && parts.iter().all(|p| p.get("type").is_some())
    {
        return parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" ");
    }
    content.to_string()
}

/// How much of the line travels with a match. Chars, not bytes: the transcript
/// is largely CJK, where 30 bytes is ten characters.
const SNIPPET_BEFORE: usize = 24;
const SNIPPET_AFTER: usize = 56;

/// A window of text around the first occurrence of `query`, or `None` when the
/// readable text never says it.
///
/// ASCII case folding, deliberately the same fold SQLite's LIKE applies: plain
/// rows only reach here through the LIKE prefilter, so a broader Unicode fold
/// would accept matches ("Ä" for "ä") on exactly the rows the prefilter never
/// ships — a promise the pipeline as a whole cannot keep. Folding ASCII is
/// also byte-preserving, so the offset found in the folded copy needs no
/// translation back.
fn snippet_around(text: &str, query: &str) -> Option<String> {
    let anchor = text.to_ascii_lowercase().find(&query.to_ascii_lowercase())?;

    let start = text[..anchor]
        .char_indices()
        .rev()
        .take(SNIPPET_BEFORE)
        .last()
        .map_or(anchor, |(i, _)| i);
    let end = text[anchor..]
        .char_indices()
        .nth(query.chars().count() + SNIPPET_AFTER)
        .map_or(text.len(), |(i, _)| anchor + i);

    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    // Newlines flatten to spaces: the snippet is one line under a title, and a
    // line break inside it would push the match out of the row.
    snippet.extend(
        text[start..end]
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c }),
    );
    if end < text.len() {
        snippet.push('…');
    }
    Some(snippet)
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn snippet_centres_the_match_and_marks_the_cuts() {
        let text = format!("{}目标词{}", "前".repeat(50), "后".repeat(100));
        let s = snippet_around(&text, "目标词").unwrap();
        assert!(s.starts_with('…') && s.ends_with('…'), "{s}");
        assert!(s.contains("目标词"));
    }

    #[test]
    fn snippet_is_case_insensitive_and_none_when_absent() {
        assert!(snippet_around("Hello Meridian", "meridian").is_some());
        assert!(snippet_around("Hello Meridian", "absent").is_none());
    }

    #[test]
    fn multimodal_rows_match_on_their_words_not_their_bytes() {
        let content = r#"[{"type":"text","text":"看看这张图"},{"type":"image_url","image_url":{"url":"data:image/png;base64,xyzzy"}}]"#;
        assert_eq!(searchable_text(content), "看看这张图");
        // A match that only exists inside the data URI is not a mention.
        assert!(snippet_around(&searchable_text(content), "xyzzy").is_none());
    }

    #[test]
    fn newlines_do_not_break_the_row() {
        let s = snippet_around("first line\nsecond target line\r\nthird", "target").unwrap();
        assert!(!s.contains('\n') && !s.contains('\r'), "{s}");
    }

    /// A pasted JSON array is somebody's text, not a block array — the `type`
    /// gate is what tells them apart.
    #[test]
    fn a_pasted_json_array_stays_text() {
        assert_eq!(searchable_text("[1, 2, 3]"), "[1, 2, 3]");
        assert!(snippet_around(&searchable_text("[1, 2, 3]"), "2, 3").is_some());
    }

    /// The fold is ASCII on purpose — the same one LIKE applies — so both
    /// layers of the pipeline promise the same matches. See `snippet_around`.
    #[test]
    fn case_folding_is_ascii_like_the_prefilter() {
        assert!(snippet_around("ÄPFEL kaufen", "äpfel").is_none());
    }

    fn say(conn: &mut SqliteConnection, id: &str, conv: &str, role: &str, content: &str, at: i64) {
        use crate::db::models::message::MessageInsert;
        crate::db::ops::message::append_message(
            conn,
            &MessageInsert {
                id,
                conversation_id: conv,
                role,
                content,
                provider_id: None,
                model_id: None,
                input_tokens: None,
                output_tokens: None,
                tool_calls: None,
                tool_call_id: None,
                sort_order: 0,
                created_at: at,
                reasoning_content: None,
                rating: None,
                schema_version: 2,
                is_compact_summary: 0,
                sender_id: None,
                parent_id: None,
                compact_anchor_id: None,
                source: None,
                turn_id: None,
                tool_outcome: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                server_tool_calls: None,
                provider_name: None,
            },
            None,
        )
        .unwrap();
    }

    #[test]
    fn search_speaks_once_per_conversation_and_only_for_speech() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", Some("消息树聊天"), None, None, 1).unwrap();
        create_conversation(&mut conn, "c2", Some("别的"), None, None, 2).unwrap();
        say(&mut conn, "m1", "c1", "user", "我们聊聊消息树的设计", 10);
        say(&mut conn, "m2", "c1", "assistant", "消息树以 parent_id 相连", 20);
        // Injected background is not speech, and must not surface as it.
        say(&mut conn, "m3", "c2", "context", "消息树的背景资料", 30);

        let hits = search_transcripts(&mut conn, "消息树", 20).unwrap();
        assert_eq!(hits.len(), 1, "two mentions in c1 collapse; c2's context row is out");
        assert_eq!(hits[0].conversation_id, "c1");
        assert_eq!(hits[0].created_at, 20, "the newest mention is the one shown");
        assert!(hits[0].snippet.contains("消息树"), "{}", hits[0].snippet);
    }

    #[test]
    fn a_zero_search_limit_returns_no_rows() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", Some("match"), None, None, 1).unwrap();
        say(&mut conn, "m1", "c1", "user", "needle", 10);

        assert!(search_transcripts(&mut conn, "needle", 0).unwrap().is_empty());
    }

    #[test]
    fn search_skips_delegated_transcripts() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "parent", None, None, None, 1).unwrap();
        insert(
            &mut conn,
            ConversationInsert {
                id: "sub",
                parent_conversation_id: Some("parent"),
                created_at: 1,
                updated_at: 1,
                ..Default::default()
            },
        )
        .unwrap();
        say(&mut conn, "m1", "sub", "assistant", "errand 的中间产物", 10);

        assert!(search_transcripts(&mut conn, "errand", 20).unwrap().is_empty());
    }

    #[test]
    fn search_treats_like_wildcards_as_characters() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        say(&mut conn, "m1", "c1", "user", "进度到 50% 了", 10);
        say(&mut conn, "m2", "c1", "assistant", "编号是 50X", 20);

        let hits = search_transcripts(&mut conn, "50%", 20).unwrap();
        assert_eq!(hits.len(), 1);
        // What holds this is the readable-text recheck, not the LIKE escaping:
        // an unescaped `%` widens the prefilter to "50X", and the recheck then
        // refuses it for lacking the literal query. The escape is scan hygiene;
        // this pins the answer.
        assert_eq!(hits[0].created_at, 10);
    }

    /// JSON encodes `"` as `\"`, so a query containing a quote exists in the
    /// readable text and *not* in the stored bytes. Raw LIKE alone silently
    /// loses these rows; the `LIKE '[%'` branch is what ships them to the
    /// decoder. This test goes red if that branch is dropped.
    #[test]
    fn search_survives_json_escaping_in_block_arrays() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        say(
            &mut conn,
            "m1",
            "c1",
            "user",
            r#"[{"type":"text","text":"他说：\"消息树\"，很妙"}]"#,
            10,
        );

        // The quoted phrase, quotes included — the bytes in the column spell
        // it `\"消息树\"`, which raw LIKE cannot see.
        let hits = search_transcripts(&mut conn, r#""消息树""#, 20).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("\"消息树\""), "{}", hits[0].snippet);
    }

    /// One conversation saying the query more times than a whole scan page
    /// must not push quieter conversations out of the answer. This is the test
    /// that goes red if the scan is capped instead of paged.
    #[test]
    fn search_scans_past_a_talkative_conversation() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "chatty", None, None, None, 1).unwrap();
        create_conversation(&mut conn, "quiet", None, None, None, 2).unwrap();
        // Older than everything the talkative conversation says.
        say(&mut conn, "mq", "quiet", "user", "关键词只提了一次", 5);
        for i in 0..(SEARCH_PAGE + 5) {
            say(
                &mut conn,
                &format!("mc{i}"),
                "chatty",
                "assistant",
                "关键词又出现了",
                1_000 + i,
            );
        }

        let hits = search_transcripts(&mut conn, "关键词", 20).unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.conversation_id.as_str()).collect();
        assert_eq!(ids, ["chatty", "quiet"], "newest first, and nobody crowded out");
    }

    /// The one case the LIKE prefilter and the readable-text recheck disagree
    /// on, which is what makes the recheck observable at all: raw content that
    /// contains the query only inside transport (a data URI), never in words.
    /// This is the test that goes red if the recheck is dropped.
    #[test]
    fn search_never_matches_inside_a_data_uri() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        say(
            &mut conn,
            "m1",
            "c1",
            "user",
            r#"[{"type":"text","text":"看看这张图"},{"type":"image_url","image_url":{"url":"data:image/png;base64,xyzzyAAAA"}}]"#,
            10,
        );

        assert!(search_transcripts(&mut conn, "xyzzy", 20).unwrap().is_empty());
        // And the words beside the image are still found, as words.
        let hits = search_transcripts(&mut conn, "这张图", 20).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].snippet.contains("base64"), "{}", hits[0].snippet);
    }

    #[test]
    fn update_project_refiles_and_unfiles() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        crate::db::ops::project::create_project(
            &mut conn,
            &crate::db::models::project::ProjectInsert {
                id: "p1",
                name: "P",
                path: None,
                source_type: "local",
                source_id: None,
                assistant_id: None,
                description: None,
                created_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();

        update_project(&mut conn, "c1", Some("p1"), 5).unwrap();
        let conv = get_conversation(&mut conn, "c1").unwrap();
        assert_eq!(conv.project_id.as_deref(), Some("p1"));
        assert_eq!(conv.updated_at, 5, "a move is a change the sidebar sorts by");

        // `None` is a destination — back out to no project at all.
        update_project(&mut conn, "c1", None, 6).unwrap();
        assert_eq!(get_conversation(&mut conn, "c1").unwrap().project_id, None);
    }
}

/// Every delegated run this conversation started, for the cards that report on
/// them.
///
/// The turn each one carries is the one named by `spawned_turn_id`, never the
/// conversation's latest. A sub-agent's transcript stays writable after the run
/// ends, so "latest" would let a follow-up chat decide what the parent's card
/// says about a run that finished long ago.
/// One spawned conversation as selected: id, spawning message/call, title,
/// last turn's status and error.
type SubAgentRunRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub fn sub_agent_runs(conn: &mut SqliteConnection, parent_id: &str) -> QueryResult<Vec<SubAgentRun>> {
    use crate::db::schema::{messages, turns};

    let rows: Vec<SubAgentRunRow> = conversations::table
        .filter(conversations::parent_conversation_id.eq(parent_id))
        .order(conversations::created_at.asc())
        .select((
            conversations::id,
            conversations::spawned_by_message_id,
            conversations::spawned_by_call_id,
            conversations::spawned_turn_id,
            conversations::agent_kind,
            conversations::title,
        ))
        .load(conn)?;

    let turn_ids: Vec<String> = rows.iter().filter_map(|r| r.3.clone()).collect();

    // AssistantRow rows, not every row and not tool calls: the loop writes one
    // assistant row per iteration, so this counts how many times the model was
    // asked. Tool rows and steering rows would inflate it, and a round that
    // called three tools is still one step.
    let counts: Vec<(Option<String>, i64)> = messages::table
        .filter(messages::turn_id.eq_any(&turn_ids))
        .filter(messages::role.eq("assistant"))
        .group_by(messages::turn_id)
        .select((messages::turn_id, diesel::dsl::count_star()))
        .load(conn)?;

    let turns: Vec<TurnRow> = turns::table
        .filter(turns::id.eq_any(&turn_ids))
        .select(TurnRow::as_select())
        .load(conn)?;

    Ok(rows
        .into_iter()
        .map(|(conversation_id, message_id, call_id, turn_id, agent_kind, title)| {
            let steps = turn_id
                .as_ref()
                .and_then(|id| {
                    counts
                        .iter()
                        .find(|(t, _)| t.as_deref() == Some(id.as_str()))
                        .map(|(_, n)| *n)
                })
                .unwrap_or(0);
            let turn = turn_id
                .as_ref()
                .and_then(|id| turns.iter().find(|t| &t.id == id).cloned());
            SubAgentRun {
                conversation_id,
                spawned_by_message_id: message_id,
                spawned_by_call_id: call_id,
                spawned_turn_id: turn_id,
                agent_kind,
                title,
                steps,
                turn,
            }
        })
        .collect())
}

/// Every conversation that hangs off this one, nearest first.
///
/// `parent_conversation_id` carries no foreign key, so SQLite will not cascade
/// down it — see migration 27, and `messages.parent_id` before it. Deleting
/// without this leaves conversations nothing can reach: the sidebar filters them
/// out by design, and the card that could have opened them went with its parent.
///
/// Walks rather than assuming one level. Depth is one today because a delegated
/// run is handed no way to delegate, but that is a property of the tool set, not
/// of this column, and a query that quietly depended on it is what would be left
/// behind if the tool set ever changed. `seen` is not only for efficiency: a
/// cycle written by some future bug would otherwise loop here for as long as the
/// process lives.
pub fn descendants(conn: &mut SqliteConnection, id: &str) -> QueryResult<Vec<String>> {
    let mut seen: std::collections::HashSet<String> = [id.to_string()].into_iter().collect();
    let mut out: Vec<String> = Vec::new();
    let mut frontier = vec![id.to_string()];
    while !frontier.is_empty() {
        let children: Vec<String> = conversations::table
            .filter(conversations::parent_conversation_id.eq_any(&frontier))
            .order(conversations::created_at.asc())
            .select(conversations::id)
            .load(conn)?;
        frontier = children.into_iter().filter(|c| seen.insert(c.clone())).collect();
        out.extend_from_slice(&frontier);
    }
    Ok(out)
}

/// Delete a conversation and every delegated run under it.
///
/// One transaction, because half a tree is worse than either outcome: what
/// survives is unreachable, and what went was the only record of what the
/// survivors were for.
pub fn delete_conversation(conn: &mut SqliteConnection, id: &str) -> QueryResult<()> {
    conn.transaction(|conn| {
        let mut doomed = descendants(conn, id)?;
        doomed.push(id.to_string());
        // Everything inside each one — messages, turns, todo lists, plans — is
        // reached by the foreign keys those do have.
        diesel::delete(conversations::table.filter(conversations::id.eq_any(&doomed))).execute(conn)?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    /// Migrations are plain SQL and Diesel does not check them at compile time,
    /// so this is the only place a broken ALTER TABLE surfaces before runtime.
    #[test]
    fn migrations_apply_and_reasoning_prefs_round_trip() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();

        let conv = create_conversation(&mut conn, "c1", Some("t"), None, None, 1).unwrap();
        assert_eq!(conv.thinking_level, None, "defaults to inheriting the assistant");
        assert_eq!(conv.fast_mode, 0);

        update_reasoning_prefs(&mut conn, "c1", Some("xhigh"), true, 2).unwrap();
        let conv = get_conversation(&mut conn, "c1").unwrap();
        assert_eq!(conv.thinking_level.as_deref(), Some("xhigh"));
        assert_eq!(conv.fast_mode, 1);

        // Clearing back to the assistant default must be expressible.
        update_reasoning_prefs(&mut conn, "c1", None, false, 3).unwrap();
        let conv = get_conversation(&mut conn, "c1").unwrap();
        assert_eq!(conv.thinking_level, None);
        assert_eq!(conv.fast_mode, 0);
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

    fn assistant_row(conn: &mut SqliteConnection, id: &str, conv: &str, turn_id: &str) {
        use crate::db::models::message::MessageInsert;
        crate::db::ops::message::append_message(
            conn,
            &MessageInsert {
                id,
                conversation_id: conv,
                role: "assistant",
                content: "",
                provider_id: None,
                model_id: None,
                input_tokens: None,
                output_tokens: None,
                tool_calls: None,
                tool_call_id: None,
                sort_order: 0,
                created_at: 0,
                reasoning_content: None,
                rating: None,
                schema_version: 2,
                is_compact_summary: 0,
                sender_id: None,
                parent_id: None,
                compact_anchor_id: None,
                source: None,
                turn_id: Some(turn_id),
                tool_outcome: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                server_tool_calls: None,
                provider_name: None,
            },
            None,
        )
        .unwrap();
    }

    /// A sub-agent's transcript is reachable only through the card on the turn
    /// that spawned it. Listing it beside the user's own conversations would
    /// turn one delegated errand into a second entry they never started.
    #[test]
    fn a_delegated_conversation_stays_out_of_the_lists() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        crate::db::ops::project::create_project(
            &mut conn,
            &crate::db::models::project::ProjectInsert {
                id: "p1",
                name: "P",
                path: None,
                source_type: "local",
                source_id: None,
                assistant_id: None,
                description: None,
                created_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();
        create_conversation(&mut conn, "parent", Some("t"), None, Some("p1"), 1).unwrap();
        spawn(&mut conn, "child", "parent", "m1", "0", "t-child");

        let all = list_conversations(&mut conn, false).unwrap();
        assert_eq!(all.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["parent"]);

        // The project view concatenates archived onto active, so it needs the
        // same filter rather than relying on the archive flag.
        let by_project = list_conversations_by_project(&mut conn, "p1", false).unwrap();
        assert_eq!(by_project.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["parent"]);

        let runs = sub_agent_runs(&mut conn, "parent").unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].conversation_id, "child");
        assert_eq!(runs[0].agent_kind.as_deref(), Some("explore"));
    }

    /// The reason the message id is stored alongside the call id. A gateway that
    /// numbers per request answers `"0"` for the first tool call of every
    /// response, so delegating twice in one conversation produces two runs whose
    /// call ids are equal and whose cards are not.
    #[test]
    fn two_runs_with_the_same_call_id_stay_apart() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "parent", Some("t"), None, None, 1).unwrap();
        spawn(&mut conn, "first", "parent", "m1", "0", "t-first");
        spawn(&mut conn, "second", "parent", "m2", "0", "t-second");

        let runs = sub_agent_runs(&mut conn, "parent").unwrap();
        let ids: Vec<_> = runs
            .iter()
            .map(|r| {
                (
                    r.spawned_by_message_id.as_deref().unwrap(),
                    r.spawned_by_call_id.as_deref().unwrap(),
                    r.conversation_id.as_str(),
                )
            })
            .collect();
        assert_eq!(ids, [("m1", "0", "first"), ("m2", "0", "second")]);
    }

    /// The card reports on the run, not on whatever the user did in that
    /// transcript afterwards. Reading the conversation's latest turn instead
    /// would let a follow-up question days later decide what a finished run
    /// looks like.
    #[test]
    fn the_card_reads_the_delegated_turn_and_not_the_latest_one() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "parent", Some("t"), None, None, 1).unwrap();
        spawn(&mut conn, "child", "parent", "m1", "0", "t-run");
        crate::db::ops::turn::finish(&mut conn, "t-run", crate::db::models::turn::TurnStatus::Done, None, 20).unwrap();

        // The user opens the sub-agent's transcript and keeps talking. That is a
        // desktop turn in the same conversation, and it is still running.
        crate::db::ops::turn::begin(
            &mut conn,
            "t-followup",
            "child",
            crate::turn::TurnOrigin::Desktop,
            None,
            30,
        )
        .unwrap();

        let runs = sub_agent_runs(&mut conn, "parent").unwrap();
        assert_eq!(runs[0].spawned_turn_id.as_deref(), Some("t-run"));
        assert_eq!(runs[0].turn.as_ref().unwrap().status, "done");
    }

    /// Steps are assistant iterations: how many times the model was asked. Tool
    /// rows answer calls rather than making them, and a round that called three
    /// tools is still one step.
    #[test]
    fn steps_count_assistant_iterations_of_the_delegated_turn() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "parent", Some("t"), None, None, 1).unwrap();
        spawn(&mut conn, "child", "parent", "m1", "0", "t-run");

        assistant_row(&mut conn, "a1", "child", "t-run");
        assistant_row(&mut conn, "a2", "child", "t-run");
        // A follow-up chat in the same conversation, under a different turn.
        assistant_row(&mut conn, "a3", "child", "t-followup");

        assert_eq!(sub_agent_runs(&mut conn, "parent").unwrap()[0].steps, 2);
    }

    /// Rows written before this migration have every new column empty, and go on
    /// behaving exactly as they did.
    #[test]
    fn conversations_that_predate_delegation_are_ordinary() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let conv = create_conversation(&mut conn, "c1", Some("t"), None, None, 1).unwrap();

        assert!(conv.parent_conversation_id.is_none());
        assert!(conv.spawned_by_message_id.is_none());
        assert!(conv.spawned_by_call_id.is_none());
        assert!(conv.spawned_turn_id.is_none());
        assert!(conv.agent_kind.is_none());
        assert!(conv.agent_provider_id.is_none());
        assert!(conv.agent_model_id.is_none(), "it goes on resolving from the assistant");
        assert_eq!(list_conversations(&mut conn, false).unwrap().len(), 1);
    }

    /// Three paths ask what model a conversation runs on — the next turn, the
    /// context indicator, and manual compaction — and a delegated run has to
    /// give all three the model its transcript was written by. Getting this
    /// wrong is not visible as an error: a run on a 64K model reports how full
    /// a 200K window is, and compaction waits for a threshold no request will
    /// ever reach.
    #[test]
    fn a_delegated_run_pins_the_model_its_transcript_was_written_by() {
        let pool = test_db();
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

        let big = crate::db::models::assistant::AssistantRow {
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

    fn assistant() -> crate::db::models::assistant::AssistantRow {
        crate::db::models::assistant::AssistantRow {
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
            is_default: 0,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
            context_limit: 0,
            compact_keep_recent: 10,
            enabled_tools: None,
            thinking_enabled: 0,
            thinking_budget: None,
            tool_preset_id: None,
            auto_compact_enabled: 0,
        }
    }

    /// A delegated run has no independent existence. Left behind it is a
    /// conversation nothing can reach — the sidebar filters it out, and the card
    /// that could have opened it went with its parent.
    #[test]
    fn deleting_a_conversation_takes_its_delegated_runs_with_it() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "parent", Some("t"), None, None, 1).unwrap();
        create_conversation(&mut conn, "bystander", Some("t"), None, None, 1).unwrap();
        spawn(&mut conn, "child-a", "parent", "m1", "0", "t-a");
        spawn(&mut conn, "child-b", "parent", "m2", "0", "t-b");
        spawn(&mut conn, "theirs", "bystander", "m1", "0", "t-c");

        delete_conversation(&mut conn, "parent").unwrap();

        let left: Vec<String> = conversations::table
            .order(conversations::id.asc())
            .select(conversations::id)
            .load(&mut conn)
            .unwrap();
        assert_eq!(left, ["bystander", "theirs"], "and nobody else's run went with it");
    }

    /// The column has no foreign key, so nothing below it cascades on its own —
    /// which is the whole reason this walk exists. Written as a walk rather than
    /// one query because depth is a property of the tool set, not of the schema.
    #[test]
    fn descendants_walks_and_cannot_be_trapped_by_a_cycle() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "parent", Some("t"), None, None, 1).unwrap();
        spawn(&mut conn, "child", "parent", "m1", "0", "t-a");
        spawn(&mut conn, "grandchild", "child", "m1", "0", "t-b");

        assert_eq!(descendants(&mut conn, "parent").unwrap(), ["child", "grandchild"]);
        assert_eq!(descendants(&mut conn, "child").unwrap(), ["grandchild"]);
        assert!(descendants(&mut conn, "grandchild").unwrap().is_empty());

        // Nothing writes this today. If something ever does, this must return
        // rather than walk for as long as the process lives.
        diesel::update(conversations::table.find("parent"))
            .set(conversations::parent_conversation_id.eq("grandchild"))
            .execute(&mut conn)
            .unwrap();
        assert_eq!(descendants(&mut conn, "parent").unwrap(), ["child", "grandchild"]);
    }
}
