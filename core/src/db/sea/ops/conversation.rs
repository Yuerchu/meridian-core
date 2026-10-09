//! Reading and writing `conversations`.
//!
//! Most conversation ops still run on Diesel, inside the queue, turn and
//! plan-review transactions. A function appears here once a SeaORM root needs
//! it (the setters below are the shell's guarded conversation commands), and
//! stays a dual implementation (`docs/dual-impl.md`) until the last Diesel
//! caller has moved.

use std::collections::{HashMap, HashSet};

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait, LikeExpr};
use sea_orm::{
    ColumnTrait, Condition, DbErr, EntityTrait, IntoActiveModel, JoinType, QueryFilter, QueryOrder, QuerySelect,
    RelationTrait,
};

use crate::db::entity::{conversation, message, turn};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, Snapshot, WriteTx};
use crate::db::types::{EpochMs, SqlBool};

/// Every conversation's id, in no particular order.
pub async fn all_ids(db: &impl Read) -> Result<Vec<String>, DbErr> {
    conversation::Entity::find()
        .select_only()
        .column(conversation::Column::Id)
        .into_tuple()
        .all(db.conn()?)
        .await
}

/// Every conversation whose working directory comes from this project, by
/// id. Archived and delegated rows count: changing or deleting the project
/// changes their next turn's directory too.
pub async fn ids_by_project(db: &impl Read, project_id: &str) -> Result<Vec<String>, DbErr> {
    conversation::Entity::find()
        .filter(conversation::Column::ProjectId.eq(project_id))
        .select_only()
        .column(conversation::Column::Id)
        .order_by_asc(conversation::Column::Id)
        .into_tuple()
        .all(db.conn()?)
        .await
}

/// `None` for an id with no row. The Diesel version answers `NotFound`
/// instead, which its callers match on; here the absence is in the type.
pub async fn get_conversation(db: &impl Read, id: &str) -> Result<Option<conversation::Model>, DbErr> {
    conversation::Entity::find_by_id(id).one(db.conn()?).await
}

/// Writes `row`'s set columns to the conversation `id`; how many rows that
/// touched, 0 for an id with no row (as the Diesel setters, which say nothing).
async fn update_columns(tx: &WriteTx, id: &str, row: conversation::ActiveModel) -> Result<u64, DbErr> {
    Ok(conversation::Entity::update_many()
        .set(row)
        .filter(conversation::Column::Id.eq(id))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

pub async fn update_assistant(
    tx: &WriteTx,
    id: &str,
    assistant_id: Option<String>,
    now: EpochMs,
) -> Result<u64, DbErr> {
    let row = conversation::ActiveModel {
        assistant_id: Set(assistant_id),
        updated_at: Set(now),
        ..Default::default()
    };
    update_columns(tx, id, row).await
}

/// The thinking level is stored as written by `StoredThinkingLevel::as_str`.
pub async fn update_reasoning_prefs(
    tx: &WriteTx,
    id: &str,
    thinking_level: Option<String>,
    fast_mode: bool,
    now: EpochMs,
) -> Result<u64, DbErr> {
    let row = conversation::ActiveModel {
        thinking_level: Set(thinking_level),
        fast_mode: Set(SqlBool::from(fast_mode)),
        updated_at: Set(now),
        ..Default::default()
    };
    update_columns(tx, id, row).await
}

/// The standing approval for ordinary edits. Its own setter, apart from the
/// mode: a mode narrows what the assistant can do, this widens what it can do
/// without asking.
pub async fn update_accept_edits(tx: &WriteTx, id: &str, accept_edits: bool, now: EpochMs) -> Result<u64, DbErr> {
    let row = conversation::ActiveModel {
        accept_edits: Set(SqlBool::from(accept_edits)),
        updated_at: Set(now),
        ..Default::default()
    };
    update_columns(tx, id, row).await
}

/// Refile the conversation under another project, or under none. For a native
/// conversation this moves what the next turn resolves its working directory
/// and file access against.
pub async fn update_project(tx: &WriteTx, id: &str, project_id: Option<String>, now: EpochMs) -> Result<u64, DbErr> {
    let row = conversation::ActiveModel {
        project_id: Set(project_id),
        updated_at: Set(now),
        ..Default::default()
    };
    update_columns(tx, id, row).await
}

/// A conversation row with nothing set but its id and clock, as the table's
/// defaults would leave it: for callers that fill in the few columns they
/// care about and hand the row to [`insert`].
pub fn new_row(id: &str, now: EpochMs) -> conversation::Model {
    conversation::Model {
        id: id.to_owned(),
        title: None,
        assistant_id: None,
        is_pinned: SqlBool::FALSE,
        is_archived: SqlBool::FALSE,
        message_count: 0,
        created_at: now,
        updated_at: now,
        project_id: None,
        thinking_level: None,
        fast_mode: SqlBool::FALSE,
        mode: None,
        head_message_id: None,
        accept_edits: SqlBool::FALSE,
        parent_conversation_id: None,
        spawned_by_message_id: None,
        spawned_by_call_id: None,
        spawned_turn_id: None,
        agent_kind: None,
        agent_provider_id: None,
        agent_model_id: None,
    }
}

/// Insert a prepared row and read it back. `message_count` belongs to the
/// triggers; whatever the caller set is written as given (0 for a new row).
pub async fn insert(tx: &WriteTx, new: conversation::Model) -> Result<conversation::Model, DbErr> {
    let id = new.id.clone();
    conversation::Entity::insert(new.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    get_conversation(tx, &id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("conversation `{id}`")))
}

pub async fn create_conversation(
    tx: &WriteTx,
    id: &str,
    title: Option<&str>,
    assistant_id: Option<&str>,
    project_id: Option<&str>,
    now: EpochMs,
) -> Result<conversation::Model, DbErr> {
    insert(
        tx,
        conversation::Model {
            title: title.map(str::to_owned),
            assistant_id: assistant_id.map(str::to_owned),
            project_id: project_id.map(str::to_owned),
            ..new_row(id, now)
        },
    )
    .await
}

/// The user's own conversations — archived or not — pinned first, then
/// newest. Sub-agent transcripts are left out: being spawned is not a
/// decision anyone can reverse, unlike archiving.
pub async fn list_conversations(db: &impl Read, archived: bool) -> Result<Vec<conversation::Model>, DbErr> {
    conversation::Entity::find()
        .filter(conversation::Column::IsArchived.eq(SqlBool::from(archived)))
        .filter(conversation::Column::ParentConversationId.is_null())
        .order_by_desc(conversation::Column::IsPinned)
        .order_by_desc(conversation::Column::UpdatedAt)
        .all(db.conn()?)
        .await
}

pub async fn list_conversations_by_project(
    db: &impl Read,
    project_id: &str,
    archived: bool,
) -> Result<Vec<conversation::Model>, DbErr> {
    conversation::Entity::find()
        .filter(conversation::Column::ProjectId.eq(project_id))
        .filter(conversation::Column::IsArchived.eq(SqlBool::from(archived)))
        .filter(conversation::Column::ParentConversationId.is_null())
        .order_by_desc(conversation::Column::IsPinned)
        .order_by_desc(conversation::Column::UpdatedAt)
        .all(db.conn()?)
        .await
}

pub async fn update_title(tx: &WriteTx, id: &str, title: &str, now: EpochMs) -> Result<u64, DbErr> {
    let row = conversation::ActiveModel {
        title: Set(Some(title.to_owned())),
        updated_at: Set(now),
        ..Default::default()
    };
    update_columns(tx, id, row).await
}

/// Persist the collaboration mode; `None` is the default (work) mode.
pub async fn update_mode(tx: &WriteTx, id: &str, mode: Option<&str>, now: EpochMs) -> Result<u64, DbErr> {
    let row = conversation::ActiveModel {
        mode: Set(mode.map(str::to_owned)),
        updated_at: Set(now),
        ..Default::default()
    };
    update_columns(tx, id, row).await
}

pub async fn archive_conversation(tx: &WriteTx, id: &str, now: EpochMs) -> Result<u64, DbErr> {
    let row = conversation::ActiveModel {
        is_archived: Set(SqlBool::TRUE),
        updated_at: Set(now),
        ..Default::default()
    };
    update_columns(tx, id, row).await
}

/// Flip one flag and hand back the row as it is after the flip. The read and
/// the write share the caller's `BEGIN IMMEDIATE`, so two presses at once
/// are each applied rather than one read seeing the other's stale value.
async fn toggle(
    tx: &WriteTx,
    id: &str,
    now: EpochMs,
    flag: impl Fn(&conversation::Model) -> SqlBool,
    set: impl Fn(SqlBool) -> conversation::ActiveModel,
) -> Result<conversation::Model, DbErr> {
    let current = get_conversation(tx, id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("conversation `{id}`")))?;
    let mut row = set(SqlBool::from(!flag(&current).get()));
    row.updated_at = Set(now);
    update_columns(tx, id, row).await?;
    get_conversation(tx, id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("conversation `{id}`")))
}

pub async fn toggle_pin(tx: &WriteTx, id: &str, now: EpochMs) -> Result<conversation::Model, DbErr> {
    toggle(
        tx,
        id,
        now,
        |c| c.is_pinned,
        |pinned| conversation::ActiveModel {
            is_pinned: Set(pinned),
            ..Default::default()
        },
    )
    .await
}

pub async fn toggle_archive(tx: &WriteTx, id: &str, now: EpochMs) -> Result<conversation::Model, DbErr> {
    toggle(
        tx,
        id,
        now,
        |c| c.is_archived,
        |archived| conversation::ActiveModel {
            is_archived: Set(archived),
            ..Default::default()
        },
    )
    .await
}

/// The conversations spawned by this one, oldest first, so two readings
/// compare element by element and leases are always taken in one order.
pub async fn sub_agent_conversation_ids(db: &impl Read, parent_id: &str) -> Result<Vec<String>, DbErr> {
    conversation::Entity::find()
        .filter(conversation::Column::ParentConversationId.eq(parent_id))
        .order_by_asc(conversation::Column::CreatedAt)
        .select_only()
        .column(conversation::Column::Id)
        .into_tuple()
        .all(db.conn()?)
        .await
}

/// Every conversation that hangs off this one, nearest first, walking every
/// level. `parent_conversation_id` carries no foreign key, so nothing
/// cascades down it; `seen` stops a cycle a future bug might write.
pub async fn descendants(db: &impl Snapshot, id: &str) -> Result<Vec<String>, DbErr> {
    let mut seen: HashSet<String> = [id.to_string()].into_iter().collect();
    let mut out = Vec::new();
    let mut frontier = vec![id.to_string()];
    while !frontier.is_empty() {
        let children: Vec<String> = conversation::Entity::find()
            .filter(conversation::Column::ParentConversationId.is_in(frontier.iter().map(String::as_str)))
            .order_by_asc(conversation::Column::CreatedAt)
            .select_only()
            .column(conversation::Column::Id)
            .into_tuple()
            .all(db.conn()?)
            .await?;
        frontier = children.into_iter().filter(|c| seen.insert(c.clone())).collect();
        out.extend_from_slice(&frontier);
    }
    Ok(out)
}

/// Delete a conversation and every delegated run under it, in the caller's
/// write: half a tree is worse than either outcome. Everything inside each
/// one is reached by the foreign keys those rows do have.
pub async fn delete_conversation(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    let mut doomed = descendants(tx, id).await?;
    doomed.push(id.to_string());
    Ok(conversation::Entity::delete_many()
        .filter(conversation::Column::Id.is_in(doomed))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

pub use crate::db::models::conversation::SubAgentRun;

/// The runs this conversation delegated, oldest first, each with how many
/// times its model was asked (assistant rows of its turn) and the turn row
/// when it is still there. Several statements, so it takes a snapshot.
pub async fn sub_agent_runs(db: &impl Snapshot, parent_id: &str) -> Result<Vec<SubAgentRun>, DbErr> {
    let rows = conversation::Entity::find()
        .filter(conversation::Column::ParentConversationId.eq(parent_id))
        .order_by_asc(conversation::Column::CreatedAt)
        .all(db.conn()?)
        .await?;
    let turn_ids: Vec<String> = rows.iter().filter_map(|r| r.spawned_turn_id.clone()).collect();
    let counts: HashMap<Option<String>, i64> = message::Entity::find()
        .filter(message::Column::TurnId.is_in(turn_ids.iter().map(String::as_str)))
        .filter(message::Column::Role.eq("assistant"))
        .select_only()
        .column(message::Column::TurnId)
        .column_as(message::Column::Id.count(), "steps")
        .group_by(message::Column::TurnId)
        .into_tuple::<(Option<String>, i64)>()
        .all(db.conn()?)
        .await?
        .into_iter()
        .collect();
    let turns: HashMap<String, turn::Model> = turn::Entity::find()
        .filter(turn::Column::Id.is_in(turn_ids.iter().map(String::as_str)))
        .all(db.conn()?)
        .await?
        .into_iter()
        .map(|t| (t.id.clone(), t))
        .collect();
    Ok(rows
        .into_iter()
        .map(|row| {
            let steps = counts.get(&row.spawned_turn_id).copied().unwrap_or_default();
            let turn = row.spawned_turn_id.as_ref().and_then(|id| turns.get(id).cloned());
            SubAgentRun {
                conversation_id: row.id,
                spawned_by_message_id: row.spawned_by_message_id,
                spawned_by_call_id: row.spawned_by_call_id,
                spawned_turn_id: row.spawned_turn_id,
                agent_kind: row.agent_kind,
                title: row.title,
                steps,
                turn,
            }
        })
        .collect())
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
    pub created_at: EpochMs,
}

/// Rows fetched per page while scanning: a bound on memory per round trip,
/// never on the answer — the scan pages on until `limit` conversations are
/// found or the candidates run out.
const SEARCH_PAGE: u64 = 400;

/// Full-text search over what people and the assistant actually said.
///
/// Two layers with one meaning: SQL prefilters candidates and Rust decides on
/// the row's *readable* text. The prefilter passes a superset of what the
/// recheck accepts: plain rows by a literal LIKE, and every block-array row
/// (`LIKE '[%'`, the same test `searchable_text` decodes by), since JSON
/// encoding hides a quote or a newline from LIKE. Keyset paging on
/// (created_at, id) so a run of rows in one millisecond is neither skipped nor
/// served twice. Several statements, so it takes a snapshot.
// backend: sqlite-only — LIKE folds ASCII case on SQLite, which the recheck's
// fold matches; PostgreSQL's LIKE is case-sensitive and would want ILIKE.
pub async fn search_transcripts(db: &impl Snapshot, query: &str, limit: usize) -> Result<Vec<TranscriptHit>, DbErr> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    // `%` and `_` are wildcards to LIKE; someone searching for "100%" means
    // the characters.
    let escaped = trimmed.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let pattern = LikeExpr::new(format!("%{escaped}%")).escape('\\');

    let mut seen = HashSet::new();
    let mut hits = Vec::new();
    let mut cursor: Option<(EpochMs, String)> = None;
    loop {
        let mut page = message::Entity::find()
            .join(JoinType::InnerJoin, message::Relation::Conversation.def())
            .filter(conversation::Column::ParentConversationId.is_null())
            .filter(message::Column::Role.is_in(["user", "assistant"]))
            .filter(
                Condition::any()
                    .add(Expr::col((message::Entity, message::Column::Content)).like(pattern.clone()))
                    .add(Expr::col((message::Entity, message::Column::Content)).like("[%")),
            );
        if let Some((at, id)) = &cursor {
            page = page.filter(
                Condition::any().add(message::Column::CreatedAt.lt(*at)).add(
                    Condition::all()
                        .add(message::Column::CreatedAt.eq(*at))
                        .add(message::Column::Id.lt(id.as_str())),
                ),
            );
        }
        let raw: Vec<(String, String, Option<String>, String, String, EpochMs)> = page
            .order_by_desc(message::Column::CreatedAt)
            .order_by_desc(message::Column::Id)
            .limit(SEARCH_PAGE)
            .select_only()
            .column(message::Column::Id)
            .column(message::Column::ConversationId)
            .column(conversation::Column::Title)
            .column(message::Column::Role)
            .column(message::Column::Content)
            .column(message::Column::CreatedAt)
            .into_tuple()
            .all(db.conn()?)
            .await?;
        let page_len = raw.len() as u64;
        for (id, conversation_id, title, role, content, created_at) in raw {
            // Advanced on every row, refused or not: the cursor tracks the scan.
            cursor = Some((created_at, id));
            if seen.contains(&conversation_id) {
                continue;
            }
            let Some(snippet) = snippet_around(&searchable_text(&content), trimmed) else {
                continue;
            };
            seen.insert(conversation_id.clone());
            hits.push(TranscriptHit {
                conversation_id,
                title,
                role,
                snippet,
                created_at,
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

/// What a row *reads as*: a block array's `text` members, or the content as
/// it is. The `type` check keeps a pasted JSON array matchable as text.
pub fn searchable_text(content: &str) -> String {
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

/// How much of the line travels with a match, in chars: the transcript is
/// largely CJK.
const SNIPPET_BEFORE: usize = 24;
const SNIPPET_AFTER: usize = 56;

/// A window of text around the first occurrence of `query`, or `None` when
/// the readable text never says it. ASCII case folding, the same fold LIKE
/// applies, so both layers promise the same matches.
pub fn snippet_around(text: &str, query: &str) -> Option<String> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    #[tokio::test]
    async fn a_conversation_reads_back_and_a_missing_one_is_none() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, title, is_pinned, created_at, updated_at) VALUES ('c1', 'Hi', 1, 5, 6)",
        )
        .await
        .unwrap();
        let row = get_conversation(&db, "c1").await.unwrap().unwrap();
        assert_eq!(
            (row.title.as_deref(), row.is_pinned.get(), row.updated_at),
            (Some("Hi"), true, 6)
        );
        assert_eq!(get_conversation(&db, "c2").await.unwrap(), None);
        assert_eq!(all_ids(&db).await.unwrap(), ["c1"]);
    }

    /// Each setter writes its own columns and the clock, and nothing else; a
    /// missing id touches nothing.
    #[tokio::test]
    async fn each_setter_writes_only_its_columns() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO assistants (id, name, created_at, updated_at) VALUES ('a1', 'A', 1, 1);
             INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p1', 'P', 1, 1);
             INSERT INTO conversations (id, title, assistant_id, project_id, thinking_level, fast_mode,
                     accept_edits, created_at, updated_at)
                 VALUES ('c1', 'Hi', 'a1', 'p1', 'high', 1, 0, 1, 1)",
        )
        .await
        .unwrap();
        let before = get_conversation(&db, "c1").await.unwrap().unwrap();

        db.write(async |tx| {
            update_assistant(tx, "c1", None, 2).await?;
            update_reasoning_prefs(tx, "c1", None, false, 3).await?;
            update_accept_edits(tx, "c1", true, 4).await?;
            update_project(tx, "c1", None, 5).await
        })
        .await
        .unwrap();
        let after = get_conversation(&db, "c1").await.unwrap().unwrap();
        assert_eq!(
            after,
            conversation::Model {
                assistant_id: None,
                thinking_level: None,
                fast_mode: SqlBool::FALSE,
                accept_edits: SqlBool::TRUE,
                project_id: None,
                updated_at: 5,
                ..before
            }
        );

        let touched = db
            .write(async |tx| update_project(tx, "nope", Some("p1".into()), 6).await)
            .await
            .unwrap();
        assert_eq!(touched, 0);
    }

    async fn conversations(db: &Db, rows: Vec<conversation::Model>) {
        db.write(async |tx| {
            for row in rows {
                insert(tx, row).await?;
            }
            Ok::<_, DbErr>(())
        })
        .await
        .unwrap();
    }

    async fn say(db: &Db, id: &str, conv: &str, role: &str, content: &str, at: EpochMs) {
        execute_for_tests(
            db,
            &format!(
                "INSERT INTO messages (id, conversation_id, role, content, created_at) VALUES ('{id}', '{conv}', '{role}', '{}', {at})",
                content.replace('\'', "''")
            ),
        )
        .await
        .unwrap();
    }

    async fn search(db: &Db, query: &str, limit: usize) -> Vec<TranscriptHit> {
        db.read(async |tx| search_transcripts(tx, query, limit).await)
            .await
            .unwrap()
    }

    fn titled(id: &str, title: &str, now: EpochMs) -> conversation::Model {
        conversation::Model {
            title: Some(title.into()),
            ..new_row(id, now)
        }
    }

    fn delegated(id: &str, parent: &str, message_id: &str, call_id: &str, turn_id: &str) -> conversation::Model {
        conversation::Model {
            title: Some("look something up".into()),
            parent_conversation_id: Some(parent.into()),
            spawned_by_message_id: Some(message_id.into()),
            spawned_by_call_id: Some(call_id.into()),
            spawned_turn_id: Some(turn_id.into()),
            agent_kind: Some("explore".into()),
            ..new_row(id, 10)
        }
    }

    /// Two mentions in one conversation collapse to the newest; injected
    /// background is not speech; delegated transcripts are not the user's.
    #[tokio::test]
    async fn search_speaks_once_per_conversation_and_only_for_speech() {
        let db = sea_test_db().await;
        conversations(
            &db,
            vec![
                titled("c1", "消息树聊天", 1),
                titled("c2", "别的", 2),
                delegated("sub", "c1", "m0", "0", "t0"),
            ],
        )
        .await;
        say(&db, "m1", "c1", "user", "我们聊聊消息树的设计", 10).await;
        say(&db, "m2", "c1", "assistant", "消息树以 parent_id 相连", 20).await;
        say(&db, "m3", "c2", "context", "消息树的背景资料", 30).await;
        say(&db, "m4", "sub", "assistant", "消息树的中间产物", 40).await;

        let hits = search(&db, "消息树", 20).await;
        assert_eq!(
            hits.len(),
            1,
            "c1 collapses; c2's context row and the sub-agent are out"
        );
        assert_eq!((hits[0].conversation_id.as_str(), hits[0].created_at), ("c1", 20));
        assert_eq!(hits[0].title.as_deref(), Some("消息树聊天"));
        assert!(hits[0].snippet.contains("消息树"), "{}", hits[0].snippet);
        assert!(search(&db, "消息树", 0).await.is_empty());
        assert!(search(&db, "   ", 20).await.is_empty());
    }

    /// LIKE's wildcards are characters to the person searching; a quote that
    /// JSON escaped is still found; a match only inside a data URI is not a
    /// mention.
    #[tokio::test]
    async fn search_reads_what_was_said_not_how_it_was_stored() {
        let db = sea_test_db().await;
        conversations(&db, vec![new_row("c1", 1), new_row("c2", 1), new_row("c3", 1)]).await;
        say(&db, "m1", "c1", "user", "进度到 50% 了", 10).await;
        say(&db, "m2", "c1", "assistant", "编号是 50X", 20).await;
        say(
            &db,
            "m3",
            "c2",
            "user",
            r#"[{"type":"text","text":"他说：\"消息树\"，很妙"}]"#,
            10,
        )
        .await;
        say(
            &db,
            "m4",
            "c3",
            "user",
            r#"[{"type":"text","text":"看看这张图"},{"type":"image_url","image_url":{"url":"data:image/png;base64,xyzzyAAAA"}}]"#,
            10,
        )
        .await;

        let percent = search(&db, "50%", 20).await;
        assert_eq!((percent.len(), percent[0].created_at), (1, 10));
        let quoted = search(&db, r#""消息树""#, 20).await;
        assert_eq!(quoted.len(), 1);
        assert!(quoted[0].snippet.contains("\"消息树\""), "{}", quoted[0].snippet);
        assert!(search(&db, "xyzzy", 20).await.is_empty());
        let words = search(&db, "这张图", 20).await;
        assert_eq!(words.len(), 1);
        assert!(!words[0].snippet.contains("base64"));
    }

    /// One conversation saying the query more times than a scan page does
    /// not push a quieter one out of the answer: the scan pages, it is not
    /// capped.
    #[tokio::test]
    async fn search_scans_past_a_talkative_conversation() {
        let db = sea_test_db().await;
        conversations(&db, vec![new_row("chatty", 1), new_row("quiet", 2)]).await;
        say(&db, "mq", "quiet", "user", "关键词只提了一次", 5).await;
        let values: Vec<String> = (0..(SEARCH_PAGE + 5))
            .map(|i| format!("('mc{i}', 'chatty', 'assistant', '关键词又出现了', {})", 1_000 + i))
            .collect();
        execute_for_tests(
            &db,
            &format!(
                "INSERT INTO messages (id, conversation_id, role, content, created_at) VALUES {}",
                values.join(", ")
            ),
        )
        .await
        .unwrap();
        let ids: Vec<String> = search(&db, "关键词", 20)
            .await
            .into_iter()
            .map(|h| h.conversation_id)
            .collect();
        assert_eq!(ids, ["chatty", "quiet"], "newest first, and nobody crowded out");
    }

    /// A delegated run stays out of the user's lists and shows up as a run of
    /// its parent: the delegated turn (not a later one), its assistant
    /// iterations, and two runs sharing a call id kept apart.
    #[tokio::test]
    async fn delegated_runs_live_on_their_parent() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p1', 'P', 1, 1)",
        )
        .await
        .unwrap();
        conversations(
            &db,
            vec![
                conversation::Model {
                    project_id: Some("p1".into()),
                    ..titled("parent", "t", 1)
                },
                delegated("first", "parent", "m1", "0", "t-run"),
                delegated("second", "parent", "m2", "0", "t-second"),
            ],
        )
        .await;
        db.write(async |tx| {
            use crate::db::sea::ops::turn;
            turn::begin(tx, "t-run", "first", crate::turn::TurnOrigin::SubAgent, None, 10).await?;
            turn::finish(tx, "t-run", crate::db::entity::turn::TurnStatus::Done, None, 20).await?;
            turn::begin(tx, "t-followup", "first", crate::turn::TurnOrigin::Desktop, None, 30).await
        })
        .await
        .unwrap();
        for (id, turn) in [("a1", "t-run"), ("a2", "t-run"), ("a3", "t-followup")] {
            execute_for_tests(
                &db,
                &format!(
                    "INSERT INTO messages (id, conversation_id, role, content, turn_id, created_at) VALUES ('{id}', 'first', 'assistant', '', '{turn}', 1)"
                ),
            )
            .await
            .unwrap();
        }

        let listed: Vec<_> = list_conversations(&db, false)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(listed, ["parent"]);
        let by_project: Vec<_> = list_conversations_by_project(&db, "p1", false)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(by_project, ["parent"]);

        let runs = db.read(async |tx| sub_agent_runs(tx, "parent").await).await.unwrap();
        let shape: Vec<_> = runs
            .iter()
            .map(|r| {
                (
                    r.spawned_by_message_id.as_deref().unwrap(),
                    r.spawned_by_call_id.as_deref().unwrap(),
                    r.conversation_id.as_str(),
                    r.steps,
                )
            })
            .collect();
        assert_eq!(shape, [("m1", "0", "first", 2), ("m2", "0", "second", 0)]);
        assert_eq!(
            runs[0].turn.as_ref().map(|t| t.status),
            Some(crate::db::entity::turn::TurnStatus::Done),
            "the card reads the delegated turn, not the follow-up"
        );
        assert_eq!(runs[1].turn, None, "a run whose turn row is gone");
        assert_eq!(
            sub_agent_conversation_ids(&db, "parent").await.unwrap(),
            ["first", "second"]
        );
    }

    /// Deleting a conversation takes every delegated run under it, at every
    /// depth, and a cycle written by a future bug cannot trap the walk.
    #[tokio::test]
    async fn deleting_takes_the_whole_tree_and_a_cycle_cannot_trap_it() {
        let db = sea_test_db().await;
        conversations(
            &db,
            vec![
                new_row("root", 1),
                delegated("child", "root", "m1", "0", "t1"),
                delegated("grandchild", "child", "m2", "0", "t2"),
                new_row("bystander", 1),
            ],
        )
        .await;
        assert_eq!(
            db.read(async |tx| descendants(tx, "root").await).await.unwrap(),
            ["child", "grandchild"]
        );
        execute_for_tests(
            &db,
            "UPDATE conversations SET parent_conversation_id = 'grandchild' WHERE id = 'root'",
        )
        .await
        .unwrap();
        assert_eq!(
            db.read(async |tx| descendants(tx, "root").await).await.unwrap(),
            ["child", "grandchild"],
            "the cycle back to root is not followed"
        );
        assert_eq!(
            db.write(async |tx| delete_conversation(tx, "root").await)
                .await
                .unwrap(),
            3
        );
        assert_eq!(all_ids(&db).await.unwrap(), ["bystander"]);
    }

    /// Two writers toggling one row each land their toggle: the read and the
    /// write share one `BEGIN IMMEDIATE`, so neither reads the other's stale
    /// value. An even number of presses per writer lands back where it began.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_toggles_are_each_applied() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::sea::file_test_db(dir.path()).await;
        conversations(&db, vec![new_row("c1", 1)]).await;
        const ROUNDS: i64 = 200;
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let db = db.clone();
                tokio::spawn(async move {
                    // A writer starved past busy_timeout under a loaded test
                    // run answers "database is locked". A refused toggle changed
                    // nothing and is simply asked again: availability is not
                    // what this test is about, losing a press is.
                    async fn until_applied(db: &Db, pin: bool, now: EpochMs) {
                        loop {
                            let outcome = if pin {
                                db.write(async |tx| toggle_pin(tx, "c1", now).await).await
                            } else {
                                db.write(async |tx| toggle_archive(tx, "c1", now).await).await
                            };
                            match outcome {
                                Ok(_) => return,
                                Err(e) if e.to_string().contains("database is locked") => {}
                                Err(e) => panic!("{e}"),
                            }
                        }
                    }
                    for i in 0..ROUNDS {
                        until_applied(&db, false, i).await;
                        until_applied(&db, true, i).await;
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.await.unwrap();
        }
        let conv = get_conversation(&db, "c1").await.unwrap().unwrap();
        assert_eq!((conv.is_archived.get(), conv.is_pinned.get()), (false, false));
    }

    /// Title, mode and archive each write their column and the clock.
    #[tokio::test]
    async fn the_simple_setters_write_their_columns() {
        let db = sea_test_db().await;
        conversations(&db, vec![new_row("c1", 1)]).await;
        db.write(async |tx| {
            update_title(tx, "c1", "Named", 2).await?;
            update_mode(tx, "c1", Some("plan"), 3).await?;
            archive_conversation(tx, "c1", 4).await
        })
        .await
        .unwrap();
        let c = get_conversation(&db, "c1").await.unwrap().unwrap();
        assert_eq!(
            (c.title.as_deref(), c.mode.as_deref(), c.is_archived.get(), c.updated_at),
            (Some("Named"), Some("plan"), true, 4)
        );
        assert!(list_conversations(&db, false).await.unwrap().is_empty());
        assert_eq!(list_conversations(&db, true).await.unwrap().len(), 1);
    }

    /// A new conversation inherits the assistant's reasoning preferences and
    /// is nobody's delegated run: every delegation column starts empty.
    #[tokio::test]
    async fn a_new_conversation_is_ordinary_and_inherits_its_reasoning_prefs() {
        let db = sea_test_db().await;
        let conv = db
            .write(async |tx| create_conversation(tx, "c1", Some("t"), None, None, 1).await)
            .await
            .unwrap();
        assert_eq!(conv.thinking_level, None, "defaults to inheriting the assistant");
        assert!(!conv.fast_mode.get());
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
    #[tokio::test]
    async fn a_delegated_run_pins_the_model_its_transcript_was_written_by() {
        let db = sea_test_db().await;
        conversations(
            &db,
            vec![
                titled("parent", "t", 1),
                conversation::Model {
                    agent_provider_id: Some("deepseek".into()),
                    agent_model_id: Some("deepseek-chat".into()),
                    ..delegated("child", "parent", "m1", "0", "t-a")
                },
            ],
        )
        .await;
        let big = crate::db::entity::assistant::Model {
            provider_id: Some("anthropic".into()),
            model_id: Some("mythos".into()),
            context_limit: 200_000,
            ..crate::db::sea::ops::assistant::tests::assistant_row("a1", 0)
        };

        let parent = get_conversation(&db, "parent").await.unwrap().unwrap();
        let unchanged = parent.pin_model(Some(big.clone())).unwrap();
        assert_eq!(unchanged.model_id.as_deref(), Some("mythos"));
        assert_eq!(
            unchanged.context_limit, 200_000,
            "an ordinary conversation keeps its own"
        );

        let child = get_conversation(&db, "child").await.unwrap().unwrap();
        let pinned = child.pin_model(Some(big)).unwrap();
        assert_eq!(pinned.provider_id.as_deref(), Some("deepseek"));
        assert_eq!(pinned.model_id.as_deref(), Some("deepseek-chat"));
        // The part that is easy to miss: a non-zero limit here outranks
        // everything the model says, so leaving it would make the swap look
        // done while changing nothing that matters.
        assert_eq!(pinned.context_limit, 0, "the window comes from the model now");
    }

    /// A flag that is not 0/1 fails the read rather than reaching a response.
    #[tokio::test]
    async fn a_stored_flag_the_model_cannot_hold_fails_the_read() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, is_pinned, created_at, updated_at) VALUES ('c1', 2, 1, 1)",
        )
        .await
        .unwrap();
        assert!(get_conversation(&db, "c1").await.is_err());
        assert!(list_conversations(&db, false).await.is_err());
    }
}
