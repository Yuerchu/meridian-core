//! Reading and writing `messages`, on SeaORM.
//!
//! Messages are a tree read as one path: each row has a `parent_id`, and the
//! conversation's `head_message_id` names the leaf the active path ends at.
//! Write only through [`append_message`] (or [`append_context`] for injected
//! background), which links the row and moves the head in the caller's write;
//! delete only whole subtrees ([`delete_subtree`]). The tree helpers below are
//! pure and shared with the Diesel module, which re-exports them; the Diesel
//! ops stay while Diesel roots still write messages (`docs/dual-impl.md`).

use std::collections::{HashMap, HashSet};

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder, QuerySelect};

use crate::db::entity::{conversation, message};
use crate::db::models::message::MessageUsage;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::sea::ops::audit;
use crate::db::types::{EpochMs, SqlBool};

/// A `context` row about to be written: background a turn reads and nobody
/// said, so it has no audit copy.
pub struct ContextRowInsert<'a> {
    pub id: &'a str,
    pub conversation_id: &'a str,
    pub content: &'a str,
    /// What wrote it, so the transcript and the exports can tell it apart.
    pub source: &'a str,
    pub turn_id: Option<&'a str>,
    pub created_at: EpochMs,
}

/// Write a `context` row under `parent` and make it the conversation's head,
/// in the caller's transaction.
pub async fn append_context(tx: &WriteTx, row: &ContextRowInsert<'_>, parent: Option<&str>) -> Result<(), DbErr> {
    let model = message::Model {
        source: Some(row.source.to_string()),
        turn_id: row.turn_id.map(str::to_string),
        ..new_row(row.id, row.conversation_id, "context", row.content, row.created_at)
    };
    link(tx, model, parent).await.map(|_| ())
}

/// A row of `role` with everything an append fills in left blank — the
/// parent is the append's to set and `sort_order` the trigger's — and
/// nothing billed: what somebody typed and what was injected cost no tokens
/// and came from no upstream. Callers add what their row carries beyond that
/// (a turn, a source, a sender).
pub fn new_row(id: &str, conversation_id: &str, role: &str, content: &str, created_at: EpochMs) -> message::Model {
    message::Model {
        id: id.to_owned(),
        conversation_id: conversation_id.to_owned(),
        role: role.to_owned(),
        content: content.to_owned(),
        provider_id: None,
        model_id: None,
        input_tokens: None,
        output_tokens: None,
        tool_calls: None,
        tool_call_id: None,
        sort_order: 0,
        created_at,
        reasoning_content: None,
        rating: None,
        schema_version: 2,
        is_compact_summary: SqlBool::FALSE,
        sender_id: None,
        parent_id: None,
        compact_anchor_id: None,
        source: None,
        turn_id: None,
        tool_outcome: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        provider_name: None,
        provider_state: None,
        auto_review: None,
        server_tool_calls: None,
        tool_diffs: None,
        response_model_id: None,
    }
}

/// Insert `new` under `parent` and move the conversation's head onto it.
/// `sort_order` 0 is left for `trg_messages_sort_order` to assign.
async fn link(tx: &WriteTx, new: message::Model, parent: Option<&str>) -> Result<message::Model, DbErr> {
    let id = new.id.clone();
    let conversation_id = new.conversation_id.clone();
    let mut row = new.into_active_model();
    row.parent_id = Set(parent.map(str::to_owned));
    message::Entity::insert(row).exec_without_returning(tx.conn()?).await?;
    conversation::Entity::update_many()
        .col_expr(conversation::Column::HeadMessageId, Expr::value(id.clone()))
        .filter(conversation::Column::Id.eq(&conversation_id))
        .exec(tx.conn()?)
        .await?;
    get_message(tx, &id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("message `{id}`")))
}

/// Append a message to the end of a conversation's active path: the single
/// write path for conversation messages. `parent` is the caller's own cursor
/// for the turn rather than a re-read of the head, so a turn that is
/// deliberately branching writes a sibling. Exclusion between turns is the
/// coordinator's job, taken before this is called.
///
/// A user row also gets its audit copy, in a savepoint: a database that
/// cannot take the copy is logged, never allowed to take the message with it.
pub async fn append_message(tx: &WriteTx, new: message::Model, parent: Option<&str>) -> Result<message::Model, DbErr> {
    let row = link(tx, new, parent).await?;
    audit_copy(tx, &row).await;
    Ok(row)
}

/// Only user rows, and never a compaction summary or a local shell record
/// (its command routinely carries tokens; the output is in the context-item
/// table). An assistant reply is recorded once it is complete.
async fn audit_copy(tx: &WriteTx, row: &message::Model) {
    if row.role != "user" || row.is_compact_summary.get() || row.source.as_deref() == Some("shell") {
        return;
    }
    if let Err(e) = tx.nested(async |tx| audit::record(tx, row).await).await {
        tracing::error!(
            error = %e,
            message_id = %row.id,
            "the audit copy of a message could not be written",
        );
    }
}

/// A conversation's rows in insertion order (`sort_order`), every branch.
pub async fn list_messages(db: &impl Read, conversation_id: &str) -> Result<Vec<message::Model>, DbErr> {
    message::Entity::find()
        .filter(message::Column::ConversationId.eq(conversation_id))
        .order_by_asc(message::Column::SortOrder)
        .all(db.conn()?)
        .await
}

/// One row by id; `None` for an id with no row.
pub async fn get_message(db: &impl Read, id: &str) -> Result<Option<message::Model>, DbErr> {
    message::Entity::find_by_id(id).one(db.conn()?).await
}

/// Insert a row as given, without linking it or moving the head: for the
/// writers that build a tree by hand (import) or a summary that is not on
/// the path.
pub async fn insert_message(tx: &WriteTx, new: message::Model) -> Result<message::Model, DbErr> {
    let id = new.id.clone();
    message::Entity::insert(new.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    get_message(tx, &id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("message `{id}`")))
}

/// Fill in an assistant row once the model has finished with it.
/// `RecordNotFound` when there is no such row.
#[allow(clippy::too_many_arguments)]
pub async fn update_assistant_message(
    tx: &WriteTx,
    id: &str,
    content: &str,
    reasoning_content: Option<&str>,
    tool_calls: Option<&str>,
    provider_state: Option<&str>,
    usage: &MessageUsage,
    response_model_id: Option<&str>,
) -> Result<(), DbErr> {
    let affected = message::Entity::update_many()
        .set(message::ActiveModel {
            content: Set(content.to_owned()),
            reasoning_content: Set(reasoning_content.map(str::to_owned)),
            tool_calls: Set(tool_calls.map(str::to_owned)),
            provider_state: Set(provider_state.map(str::to_owned)),
            input_tokens: Set(usage.input_tokens),
            output_tokens: Set(usage.output_tokens),
            cache_read_tokens: Set(usage.cache_read_tokens),
            cache_write_tokens: Set(usage.cache_write_tokens),
            server_tool_calls: Set(usage.server_tool_calls),
            response_model_id: Set(response_model_id.map(str::to_owned)),
            ..Default::default()
        })
        .filter(message::Column::Id.eq(id))
        .exec(tx.conn()?)
        .await?
        .rows_affected;
    if affected != 1 {
        return Err(DbErr::RecordNotFound(format!("message `{id}`")));
    }
    Ok(())
}

/// The turn's rows that made tool calls, newest first, with their stored
/// calls decoded.
async fn calls_in_turn(tx: &WriteTx, turn_id: &str) -> Result<Vec<(String, Vec<crate::provider::ToolCall>)>, DbErr> {
    let rows: Vec<(String, Option<String>)> = message::Entity::find()
        .filter(message::Column::TurnId.eq(turn_id))
        .filter(message::Column::ToolCalls.is_not_null())
        .order_by_desc(message::Column::SortOrder)
        .select_only()
        .column(message::Column::Id)
        .column(message::Column::ToolCalls)
        .into_tuple()
        .all(tx.conn()?)
        .await?;
    rows.into_iter()
        .map(|(id, json)| {
            let calls = crate::agent::tool_calls::parse_openai_tool_calls(json.as_deref())
                .map_err(|error| DbErr::Type(format!("message {id} has invalid persisted tool_calls: {error}")))?;
            Ok((id, calls))
        })
        .collect()
}

/// Fill in one tool call inside a row already stored: a hosted adapter can
/// announce a call twice, the first time as a placeholder, after the round
/// holding it has closed. `None` for either field leaves it alone — this only
/// ever adds information. Answers with the row it landed on, or `None` when
/// no row in this turn holds the call.
pub async fn revise_tool_call(
    tx: &WriteTx,
    turn_id: &str,
    call_id: &str,
    tool_name: Option<&str>,
    arguments: Option<&str>,
) -> Result<Option<(String, String, String)>, DbErr> {
    for (id, mut calls) in calls_in_turn(tx, turn_id).await? {
        let Some(call) = calls.iter_mut().find(|c| c.id == call_id) else {
            continue;
        };
        if let Some(name) = tool_name {
            call.name = name.to_string();
        }
        if let Some(args) = arguments {
            call.arguments = args.to_string();
        }
        let found = (call.name.clone(), call.arguments.clone());
        message::Entity::update_many()
            .col_expr(
                message::Column::ToolCalls,
                Expr::value(crate::agent::tool_calls::serialize_tool_calls_openai(&calls)),
            )
            .filter(message::Column::Id.eq(&id))
            .exec(tx.conn()?)
            .await?;
        return Ok(Some((id, found.0, found.1)));
    }
    Ok(None)
}

/// Merge `value` under `call_id` into the JSON map in `column` of the row,
/// keeping every other call's entry. Stored JSON that cannot be read is an
/// error rather than a fresh map, which would discard the other entries. A row
/// that has gone is not an error: nobody can open it any more.
async fn merge_by_call<V>(
    tx: &WriteTx,
    message_id: &str,
    call_id: &str,
    column: message::Column,
    value: V,
) -> Result<(), DbErr>
where
    V: serde::Serialize + serde::de::DeserializeOwned,
{
    if call_id.is_empty() {
        return Err(DbErr::Custom("a call id must not be empty".into()));
    }
    let existing: Option<Option<String>> = message::Entity::find_by_id(message_id)
        .select_only()
        .column(column)
        .into_tuple()
        .one(tx.conn()?)
        .await?;
    let mut all = match existing.flatten() {
        Some(raw) => serde_json::from_str::<std::collections::BTreeMap<String, V>>(&raw)
            .map_err(|error| DbErr::Type(format!("message {message_id} has an unreadable stored map: {error}")))?,
        None => std::collections::BTreeMap::new(),
    };
    all.insert(call_id.to_string(), value);
    let encoded = serde_json::to_string(&all).map_err(|error| DbErr::Custom(error.to_string()))?;
    message::Entity::update_many()
        .col_expr(column, Expr::value(encoded))
        .filter(message::Column::Id.eq(message_id))
        .exec(tx.conn()?)
        .await?;
    Ok(())
}

/// File one automatic-review verdict against the call it judged, merged into
/// whatever verdicts the row already carries.
pub async fn record_auto_review(
    tx: &WriteTx,
    message_id: &str,
    call_id: &str,
    verdict: &crate::events::AutoReviewVerdict,
) -> Result<(), DbErr> {
    merge_by_call(tx, message_id, call_id, message::Column::AutoReview, verdict.clone()).await
}

/// Keep the diff a hosted agent reported for one call, replaced per call: a
/// later update for the same call is a correction, not an addition.
pub async fn record_tool_diffs(
    tx: &WriteTx,
    message_id: &str,
    call_id: &str,
    diffs: &[crate::events::ToolCallDiff],
) -> Result<(), DbErr> {
    merge_by_call(tx, message_id, call_id, message::Column::ToolDiffs, diffs.to_vec()).await
}

/// [`record_tool_diffs`] for a call whose row is not known: found among the
/// turn's stored rows. `None` when no row of this turn made the call.
pub async fn record_tool_diffs_for_call(
    tx: &WriteTx,
    turn_id: &str,
    call_id: &str,
    diffs: &[crate::events::ToolCallDiff],
) -> Result<Option<String>, DbErr> {
    for (id, calls) in calls_in_turn(tx, turn_id).await? {
        if calls.iter().any(|c| c.id == call_id) {
            record_tool_diffs(tx, &id, call_id, diffs).await?;
            return Ok(Some(id));
        }
    }
    Ok(None)
}

pub async fn update_rating(tx: &WriteTx, id: &str, rating: Option<i32>) -> Result<(), DbErr> {
    message::Entity::update_many()
        .col_expr(message::Column::Rating, Expr::value(rating))
        .filter(message::Column::Id.eq(id))
        .exec(tx.conn()?)
        .await?;
    Ok(())
}

/// Drop the summaries belonging to one path, leaving other branches' alone.
pub async fn delete_summaries_anchored_in(
    tx: &WriteTx,
    conversation_id: &str,
    path_ids: &[String],
) -> Result<(), DbErr> {
    if path_ids.is_empty() {
        return Ok(());
    }
    message::Entity::delete_many()
        .filter(message::Column::ConversationId.eq(conversation_id))
        .filter(message::Column::IsCompactSummary.eq(SqlBool::TRUE))
        .filter(message::Column::CompactAnchorId.is_in(path_ids))
        .exec(tx.conn()?)
        .await?;
    Ok(())
}

/// Delete a message and everything descended from it, sibling branches under
/// it included, and repair the head: it may have pointed into the subtree.
///
/// The subtree is collected in Rust from the conversation's parent links
/// rather than by a recursive query, and deleted in chunks: leaning on
/// ON DELETE CASCADE would recurse once per level and exhaust SQLite's
/// trigger depth on a long conversation.
pub async fn delete_subtree(tx: &WriteTx, conversation_id: &str, message_id: &str) -> Result<Option<String>, DbErr> {
    let links: Vec<(String, Option<String>)> = message::Entity::find()
        .filter(message::Column::ConversationId.eq(conversation_id))
        .select_only()
        .column(message::Column::Id)
        .column(message::Column::ParentId)
        .into_tuple()
        .all(tx.conn()?)
        .await?;
    let Some(parent) = links.iter().find(|(id, _)| id == message_id).map(|(_, p)| p.clone()) else {
        return Ok(None);
    };
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, parent) in &links {
        if let Some(parent) = parent {
            children.entry(parent.as_str()).or_default().push(id.as_str());
        }
    }
    let mut doomed: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![message_id];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        doomed.push(id.to_owned());
        stack.extend(children.get(id).into_iter().flatten().copied());
    }
    for chunk in doomed.chunks(500) {
        message::Entity::delete_many()
            .filter(message::Column::Id.is_in(chunk))
            .exec(tx.conn()?)
            .await?;
    }

    let history = list_messages(tx, conversation_id).await?;
    let new_head = parent
        .filter(|p| history.iter().any(|m| &m.id == p))
        .map(|p| deepest_descendant(&history, &p))
        .or_else(|| resolve_head(None, &history));
    conversation::Entity::update_many()
        .col_expr(conversation::Column::HeadMessageId, Expr::value(new_head.clone()))
        .filter(conversation::Column::Id.eq(conversation_id))
        .exec(tx.conn()?)
        .await?;
    Ok(new_head)
}

/// Move the head onto `message_id`'s branch, at the point that branch was
/// last written. `RecordNotFound` for a message that is not on this
/// conversation's tree.
pub async fn switch_branch(tx: &WriteTx, conversation_id: &str, message_id: &str) -> Result<Option<String>, DbErr> {
    let history = list_messages(tx, conversation_id).await?;
    if !history
        .iter()
        .any(|m| m.id == message_id && !m.is_compact_summary.get())
    {
        return Err(DbErr::RecordNotFound(format!("message `{message_id}`")));
    }
    let head = deepest_descendant(&history, message_id);
    conversation::Entity::update_many()
        .col_expr(conversation::Column::HeadMessageId, Expr::value(head.clone()))
        .filter(conversation::Column::Id.eq(conversation_id))
        .exec(tx.conn()?)
        .await?;
    Ok(Some(head))
}

// ---------------------------------------------------------------------------
// The tree, read in Rust over a conversation's already-loaded rows.
// ---------------------------------------------------------------------------

/// Where the active path currently ends: the stored head when it names a row
/// that is not a summary, else the highest `sort_order` row, which is
/// necessarily a leaf. `history` is ordered by `sort_order`.
pub fn resolve_head(stored_head: Option<&str>, history: &[message::Model]) -> Option<String> {
    if let Some(head) = stored_head
        && history.iter().any(|m| m.id == head && !m.is_compact_summary.get())
    {
        return Some(head.to_string());
    }
    history
        .iter()
        .rfind(|m| !m.is_compact_summary.get())
        .map(|m| m.id.clone())
}

/// Everything a turn needs to rebuild its context, resolved once.
pub struct ActiveContext {
    /// Root to head, in order. Excludes summaries and inactive branches.
    pub path: Vec<message::Model>,
    /// The summary standing in front of `path`, when one applies.
    pub summary: Option<message::Model>,
    /// Where `summary` takes over: everything before this index is
    /// represented by it.
    pub anchor_index: Option<usize>,
    pub head_id: Option<String>,
}

impl ActiveContext {
    /// The messages a request actually carries: the tail from the anchor on.
    pub fn live(&self) -> &[message::Model] {
        match self.anchor_index {
            Some(i) => &self.path[i..],
            None => &self.path,
        }
    }
}

/// Walk the tree from `head` back to a root, then reverse. The visited set
/// guards against a cycle, which no writer can produce but corrupted data
/// could.
fn path_to_head(history: &[message::Model], head: &str) -> Vec<message::Model> {
    let by_id: HashMap<&str, &message::Model> = history
        .iter()
        .filter(|m| !m.is_compact_summary.get())
        .map(|m| (m.id.as_str(), m))
        .collect();
    let mut seen = HashSet::new();
    let mut reversed = Vec::new();
    let mut cursor = Some(head);
    while let Some(id) = cursor {
        if !seen.insert(id) {
            tracing::error!("cycle in message tree at {id}; truncating the path here");
            break;
        }
        let Some(m) = by_id.get(id) else { break };
        reversed.push((*m).clone());
        cursor = m.parent_id.as_deref();
    }
    reversed.reverse();
    reversed
}

/// The active path plus whichever summary applies to it: one whose anchor is
/// on this path (so one branch is never handed another's summary), the
/// deepest anchor winning. `history` is the whole conversation by
/// `sort_order`.
pub fn active_context(history: &[message::Model], stored_head: Option<&str>) -> ActiveContext {
    let head_id = resolve_head(stored_head, history);
    let path = match head_id.as_deref() {
        Some(head) => path_to_head(history, head),
        None => Vec::new(),
    };
    let mut best: Option<(usize, &message::Model)> = None;
    for s in history.iter().filter(|m| m.is_compact_summary.get()) {
        let Some(anchor) = s.compact_anchor_id.as_deref() else {
            continue;
        };
        if let Some(idx) = path.iter().position(|m| m.id == anchor)
            && best.is_none_or(|(prev, _)| idx > prev)
        {
            best = Some((idx, s));
        }
    }
    ActiveContext {
        path,
        summary: best.map(|(_, s)| s.clone()),
        anchor_index: best.map(|(i, _)| i),
        head_id,
    }
}

/// A point on the active path where the conversation was answered more than
/// once.
#[derive(Debug, Clone)]
pub struct BranchPoint {
    /// The version of this step currently on the path.
    pub message_id: String,
    /// 0-based position of `message_id` among its siblings.
    pub index: usize,
    pub total: usize,
    /// All versions, oldest first, so paging is stable across reloads.
    pub sibling_ids: Vec<String>,
}

/// The parent a version comparison is made against: injected background
/// (`role = "context"`) is walked past, so a message written after one is
/// still a version of whatever preceded it.
fn effective_parent(by_id: &HashMap<&str, &message::Model>, m: &message::Model) -> Option<String> {
    let mut cursor = m.parent_id.clone();
    while let Some(id) = cursor {
        let Some(parent) = by_id.get(id.as_str()) else {
            return Some(id);
        };
        if parent.role != "context" {
            return Some(id);
        }
        cursor = parent.parent_id.clone();
    }
    None
}

/// Every step on the path that has more than one version, with its
/// siblings. Grouped once rather than searched per row: per-row search was n³
/// on an unbranched conversation, 10s at 2000 rows.
pub fn branch_points(history: &[message::Model], path: &[message::Model]) -> Vec<BranchPoint> {
    let by_id: HashMap<&str, &message::Model> = history.iter().map(|m| (m.id.as_str(), m)).collect();
    let mut families: HashMap<Option<String>, Vec<&message::Model>> = HashMap::new();
    for m in history
        .iter()
        .filter(|s| !s.is_compact_summary.get() && s.role != "context")
    {
        families.entry(effective_parent(&by_id, m)).or_default().push(m);
    }
    for siblings in families.values_mut() {
        siblings.sort_by_key(|s| s.sort_order);
    }
    let mut out = Vec::new();
    for m in path {
        if m.role == "context" {
            continue;
        }
        let Some(siblings) = families.get(&effective_parent(&by_id, m)) else {
            continue;
        };
        if siblings.len() < 2 {
            continue;
        }
        let Some(index) = siblings.iter().position(|s| s.id == m.id) else {
            continue;
        };
        out.push(BranchPoint {
            message_id: m.id.clone(),
            index,
            total: siblings.len(),
            sibling_ids: siblings.iter().map(|s| s.id.clone()).collect(),
        });
    }
    out
}

/// Follow the newest child at each step: where a branch was last written.
pub fn deepest_descendant(history: &[message::Model], from: &str) -> String {
    let mut current = from.to_string();
    loop {
        let next = history
            .iter()
            .filter(|m| !m.is_compact_summary.get())
            .filter(|m| m.parent_id.as_deref() == Some(current.as_str()))
            .max_by_key(|m| m.sort_order);
        match next {
            Some(child) => current = child.id.clone(),
            None => return current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    async fn with_conversations(ids: &[&str]) -> Db {
        let db = sea_test_db().await;
        for id in ids {
            execute_for_tests(
                &db,
                &format!("INSERT INTO conversations (id, created_at, updated_at) VALUES ('{id}', 1, 1)"),
            )
            .await
            .unwrap();
        }
        db
    }

    fn row(id: &str, conv: &str, role: &str) -> message::Model {
        message::Model {
            id: id.into(),
            conversation_id: conv.into(),
            role: role.into(),
            content: String::new(),
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
            is_compact_summary: SqlBool::FALSE,
            sender_id: None,
            parent_id: None,
            compact_anchor_id: None,
            source: None,
            turn_id: None,
            tool_outcome: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            provider_name: None,
            provider_state: None,
            auto_review: None,
            server_tool_calls: None,
            tool_diffs: None,
            response_model_id: None,
        }
    }

    async fn append(db: &Db, new: message::Model, parent: Option<&str>) -> message::Model {
        db.write(async |tx| append_message(tx, new, parent).await)
            .await
            .unwrap()
    }

    async fn stored(db: &Db, id: &str) -> message::Model {
        get_message(db, id).await.unwrap().unwrap()
    }

    async fn head(db: &Db, conversation: &str) -> Option<String> {
        conversation::Entity::find_by_id(conversation)
            .one(db.conn().unwrap())
            .await
            .unwrap()
            .unwrap()
            .head_message_id
    }

    async fn ids(db: &Db, conversation: &str) -> Vec<String> {
        list_messages(db, conversation)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect()
    }

    /// Insert rows linked as given, without moving the head.
    async fn tree(db: &Db, edges: &[(&str, Option<&str>)]) {
        for (id, parent) in edges {
            let new = message::Model {
                parent_id: parent.map(str::to_owned),
                ..row(id, "c1", "user")
            };
            db.write(async |tx| insert_message(tx, new).await).await.unwrap();
        }
    }

    fn verdict() -> crate::events::AutoReviewVerdict {
        crate::events::AutoReviewVerdict {
            outcome: crate::events::AutoReviewOutcome::Allow,
            risk: None,
            authorization: None,
            rationale: None,
            stage: None,
            model: None,
            evidence: Vec::new(),
        }
    }

    fn hunk(old_text: Option<&str>, new_text: &str, line: Option<u32>) -> crate::events::ToolCallDiff {
        crate::events::ToolCallDiff {
            path: "src/lib.rs".into(),
            old_text: old_text.map(str::to_string),
            new_text: new_text.into(),
            line,
        }
    }

    /// Every field handed to `append_message` comes back out of it, but the
    /// parent, which it sets. Destructured with no `..`, so a column added to
    /// the table fails to compile here rather than silently going uncovered;
    /// same-typed columns hold distinct values so a transposition shows.
    #[tokio::test]
    async fn append_message_keeps_every_field_it_was_given() {
        let db = with_conversations(&["c1"]).await;
        let root = append(&db, row("root", "c1", "user"), None).await;
        let full = message::Model {
            id: "m1".into(),
            conversation_id: "c1".into(),
            role: "assistant".into(),
            content: "the answer".into(),
            provider_id: None,
            model_id: Some("deepseek-chat".into()),
            input_tokens: Some(7),
            output_tokens: Some(11),
            tool_calls: Some("[]".into()),
            tool_call_id: Some("call-1".into()),
            sort_order: 0,
            created_at: 1234,
            reasoning_content: Some("thinking".into()),
            rating: Some(1),
            schema_version: 2,
            is_compact_summary: SqlBool::FALSE,
            sender_id: Some(99),
            parent_id: None,
            compact_anchor_id: Some(root.id.clone()),
            source: Some("voice".into()),
            turn_id: Some("t1".into()),
            tool_outcome: Some("success".into()),
            cache_read_tokens: Some(41),
            cache_write_tokens: Some(43),
            provider_name: Some("DeepSeek".into()),
            provider_state: Some("state".into()),
            auto_review: None,
            server_tool_calls: Some(47),
            tool_diffs: None,
            response_model_id: Some("deepseek-chat-0324".into()),
        };
        append(&db, full, Some(&root.id)).await;

        let message::Model {
            id,
            conversation_id,
            role,
            content,
            provider_id,
            model_id,
            input_tokens,
            output_tokens,
            tool_calls,
            tool_call_id,
            sort_order,
            created_at,
            reasoning_content,
            rating,
            schema_version,
            is_compact_summary,
            sender_id,
            parent_id,
            compact_anchor_id,
            source,
            turn_id,
            tool_outcome,
            cache_read_tokens,
            cache_write_tokens,
            provider_name,
            provider_state,
            auto_review,
            server_tool_calls,
            tool_diffs,
            response_model_id,
        } = stored(&db, "m1").await;
        assert_eq!(
            (id.as_str(), conversation_id.as_str(), role.as_str()),
            ("m1", "c1", "assistant")
        );
        assert_eq!((content.as_str(), provider_id), ("the answer", None));
        assert_eq!(model_id.as_deref(), Some("deepseek-chat"));
        assert_eq!((input_tokens, output_tokens), (Some(7), Some(11)));
        assert_eq!((cache_read_tokens, cache_write_tokens), (Some(41), Some(43)));
        assert_eq!(server_tool_calls, Some(47));
        assert_eq!(
            (tool_calls.as_deref(), tool_call_id.as_deref()),
            (Some("[]"), Some("call-1"))
        );
        assert!(sort_order > 0, "assigned by the trigger");
        assert_eq!(
            (created_at, reasoning_content.as_deref(), rating),
            (1234, Some("thinking"), Some(1))
        );
        assert_eq!(
            (schema_version, is_compact_summary.get(), sender_id),
            (2, false, Some(99))
        );
        assert_eq!(parent_id.as_deref(), Some("root"), "the one field append sets");
        assert_eq!(compact_anchor_id.as_deref(), Some("root"));
        assert_eq!((source.as_deref(), turn_id.as_deref()), (Some("voice"), Some("t1")));
        assert_eq!(tool_outcome.as_deref(), Some("success"));
        assert_eq!(
            (provider_name.as_deref(), provider_state.as_deref()),
            (Some("DeepSeek"), Some("state"))
        );
        assert_eq!((auto_review, tool_diffs), (None, None));
        assert_eq!(response_model_id.as_deref(), Some("deepseek-chat-0324"));
    }

    #[tokio::test]
    async fn append_links_each_row_to_the_last_and_moves_the_head() {
        let db = with_conversations(&["c1"]).await;
        let a = append(&db, row("a", "c1", "user"), None).await;
        let b = append(&db, row("b", "c1", "assistant"), Some(&a.id)).await;
        let c = append(&db, row("c", "c1", "tool"), Some(&b.id)).await;
        assert_eq!(
            (a.parent_id, b.parent_id.as_deref(), c.parent_id.as_deref()),
            (None, Some("a"), Some("b"))
        );
        assert_eq!(head(&db, "c1").await.as_deref(), Some("c"));
    }

    /// Two answers to the same question are siblings, and the head follows
    /// whichever was written last.
    #[tokio::test]
    async fn a_second_child_forks_the_branch() {
        let db = with_conversations(&["c1"]).await;
        let q = append(&db, row("q", "c1", "user"), None).await;
        append(&db, row("a1", "c1", "assistant"), Some(&q.id)).await;
        let second = append(&db, row("a2", "c1", "assistant"), Some(&q.id)).await;
        assert_eq!(second.parent_id.as_deref(), Some("q"));
        assert_eq!(head(&db, "c1").await.as_deref(), Some("a2"));
    }

    /// A user row is copied into the audit table; injected background and an
    /// assistant placeholder are not.
    #[tokio::test]
    async fn only_what_a_person_said_is_audited() {
        let db = with_conversations(&["c1"]).await;
        let context = ContextRowInsert {
            id: "m1",
            conversation_id: "c1",
            content: "<owner_notes>\n- [general] x\n</owner_notes>",
            source: "memory|full|100.abc|-|",
            turn_id: None,
            created_at: 1,
        };
        db.write(async |tx| append_context(tx, &context, None).await)
            .await
            .unwrap();
        let said = message::Model {
            content: "hi".into(),
            ..row("m2", "c1", "user")
        };
        append(&db, said, Some("m1")).await;
        append(&db, row("m3", "c1", "assistant"), Some("m2")).await;
        let shell = message::Model {
            source: Some("shell".into()),
            ..row("m4", "c1", "user")
        };
        append(&db, shell, Some("m3")).await;

        let audited = audit::list_recent(&db, 10).await.unwrap();
        let contents: Vec<_> = audited
            .iter()
            .map(|a| (a.message_id.as_str(), a.content.as_str()))
            .collect();
        assert_eq!(contents, [("m2", "hi")], "only the user row belongs in the audit table");
        assert_eq!(head(&db, "c1").await.as_deref(), Some("m4"));
    }

    #[tokio::test]
    async fn an_assistant_row_is_filled_in_once_and_a_missing_one_is_an_error() {
        let db = with_conversations(&["c1"]).await;
        append(&db, row("m1", "c1", "assistant"), None).await;
        let usage = MessageUsage {
            input_tokens: Some(5),
            output_tokens: Some(6),
            cache_read_tokens: Some(1),
            cache_write_tokens: Some(2),
            server_tool_calls: Some(3),
        };
        db.write(async |tx| {
            update_assistant_message(tx, "m1", "done", Some("why"), Some("[]"), Some("s"), &usage, Some("r")).await
        })
        .await
        .unwrap();
        let m = stored(&db, "m1").await;
        assert_eq!(
            (
                m.content.as_str(),
                m.reasoning_content.as_deref(),
                m.tool_calls.as_deref(),
                m.provider_state.as_deref(),
                m.response_model_id.as_deref()
            ),
            ("done", Some("why"), Some("[]"), Some("s"), Some("r"))
        );
        assert_eq!(
            (
                m.input_tokens,
                m.output_tokens,
                m.cache_read_tokens,
                m.cache_write_tokens,
                m.server_tool_calls
            ),
            (Some(5), Some(6), Some(1), Some(2), Some(3))
        );
        let missing = db
            .write(async |tx| update_assistant_message(tx, "nope", "", None, None, None, &usage, None).await)
            .await;
        assert!(matches!(missing, Err(DbErr::RecordNotFound(_))));
    }

    /// A late announcement fills in a call already stored, within its turn
    /// only, and `None` leaves a field alone.
    #[tokio::test]
    async fn a_stored_tool_call_is_revised_in_place() {
        let db = with_conversations(&["c1"]).await;
        let with_call = message::Model {
            turn_id: Some("t1".into()),
            tool_calls: Some(
                r#"[{"id":"call-a","type":"function","function":{"name":"Terminal","arguments":"{}"}}]"#.into(),
            ),
            ..row("m1", "c1", "assistant")
        };
        append(&db, with_call, None).await;

        let revised = db
            .write(async |tx| revise_tool_call(tx, "t1", "call-a", None, Some(r#"{"cmd":"ls"}"#)).await)
            .await
            .unwrap();
        assert_eq!(
            revised,
            Some(("m1".to_string(), "Terminal".to_string(), r#"{"cmd":"ls"}"#.to_string()))
        );
        let calls =
            crate::agent::tool_calls::parse_openai_tool_calls(stored(&db, "m1").await.tool_calls.as_deref()).unwrap();
        assert_eq!(
            (calls[0].name.as_str(), calls[0].arguments.as_str()),
            ("Terminal", r#"{"cmd":"ls"}"#)
        );

        let elsewhere = db
            .write(async |tx| revise_tool_call(tx, "t2", "call-a", Some("x"), None).await)
            .await
            .unwrap();
        assert_eq!(elsewhere, None);
    }

    /// Per call the hunk list is replaced; other calls' entries are kept;
    /// nullable keys are written out.
    #[tokio::test]
    async fn record_tool_diffs_replaces_one_call_and_keeps_the_others() {
        let db = with_conversations(&["c1"]).await;
        append(&db, row("m1", "c1", "assistant"), None).await;
        db.write(async |tx| {
            record_tool_diffs(tx, "m1", "call-a", &[hunk(None, "created", None)]).await?;
            record_tool_diffs(tx, "m1", "call-b", &[hunk(Some("old"), "new", Some(3))]).await?;
            record_tool_diffs(tx, "m1", "call-a", &[hunk(Some("was there"), "created", Some(1))]).await
        })
        .await
        .unwrap();
        let raw = stored(&db, "m1").await.tool_diffs.unwrap();
        let map: std::collections::BTreeMap<String, Vec<crate::events::ToolCallDiff>> =
            serde_json::from_str(&raw).unwrap();
        assert_eq!(map["call-a"], vec![hunk(Some("was there"), "created", Some(1))]);
        assert_eq!(map["call-b"], vec![hunk(Some("old"), "new", Some(3))]);
        assert!(raw.contains(r#""line":3"#) && raw.contains(r#""old_text""#), "{raw}");
    }

    /// Stored maps this build cannot read are an error, not an empty map to
    /// write over; an empty call id is refused; neither writes anything.
    #[tokio::test]
    async fn unreadable_stored_maps_and_empty_call_ids_are_refused_without_writing() {
        let db = with_conversations(&["c1"]).await;
        append(&db, row("m1", "c1", "assistant"), None).await;
        execute_for_tests(
            &db,
            "UPDATE messages SET tool_diffs = 'not json', auto_review = 'not json' WHERE id = 'm1'",
        )
        .await
        .unwrap();
        let diffs = db
            .write(async |tx| record_tool_diffs(tx, "m1", "call-a", &[hunk(None, "x", None)]).await)
            .await;
        let review = db
            .write(async |tx| record_auto_review(tx, "m1", "call-1", &verdict()).await)
            .await;
        assert!(diffs.is_err() && review.is_err());
        let m = stored(&db, "m1").await;
        assert_eq!(
            (m.tool_diffs.as_deref(), m.auto_review.as_deref()),
            (Some("not json"), Some("not json"))
        );

        for raw in [
            r#"{"old":{"outcome":"allow"}}"#,
            r#"{"old":{"outcome":"allow","risk":null,"authorization":null,"rationale":null,"stage":null,"model":null,"evidence":[],"future":true}}"#,
        ] {
            execute_for_tests(
                &db,
                &format!("UPDATE messages SET auto_review = '{raw}' WHERE id = 'm1'"),
            )
            .await
            .unwrap();
            let refused = db
                .write(async |tx| record_auto_review(tx, "m1", "call-1", &verdict()).await)
                .await;
            assert!(refused.is_err(), "{raw}");
            assert_eq!(stored(&db, "m1").await.auto_review.as_deref(), Some(raw));
        }

        append(&db, row("m2", "c1", "assistant"), None).await;
        let empty = db
            .write(async |tx| record_auto_review(tx, "m2", "", &verdict()).await)
            .await;
        assert!(empty.is_err());
        assert_eq!(stored(&db, "m2").await.auto_review, None);
    }

    #[tokio::test]
    async fn record_auto_review_writes_the_typed_required_null_shape() {
        let db = with_conversations(&["c1"]).await;
        append(&db, row("m1", "c1", "assistant"), None).await;
        db.write(async |tx| record_auto_review(tx, "m1", "call-1", &verdict()).await)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&stored(&db, "m1").await.auto_review.unwrap()).unwrap();
        assert_eq!(
            value["call-1"],
            serde_json::json!({
                "outcome": "allow", "risk": null, "authorization": null, "rationale": null,
                "stage": null, "model": null, "evidence": [],
            })
        );
    }

    /// Finding the row by the call it made, bounded by the turn.
    #[tokio::test]
    async fn record_tool_diffs_for_call_finds_the_row_within_the_turn() {
        let db = with_conversations(&["c1"]).await;
        let with_call = message::Model {
            turn_id: Some("t1".into()),
            tool_calls: Some(
                r#"[{"id":"call-a","type":"function","function":{"name":"Write","arguments":"{}"}}]"#.into(),
            ),
            ..row("m1", "c1", "assistant")
        };
        append(&db, with_call, None).await;
        let found = |turn: &'static str| {
            let db = db.clone();
            async move {
                db.write(async |tx| record_tool_diffs_for_call(tx, turn, "call-a", &[hunk(None, "x", None)]).await)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(found("t1").await.as_deref(), Some("m1"));
        assert!(stored(&db, "m1").await.tool_diffs.is_some());
        assert_eq!(found("t2").await, None);
    }

    #[tokio::test]
    async fn switching_lands_on_the_branch_tip_and_an_unknown_message_is_refused() {
        let db = with_conversations(&["c1"]).await;
        tree(
            &db,
            &[("q", None), ("a1", Some("q")), ("a1x", Some("a1")), ("a2", Some("q"))],
        )
        .await;
        let switched = db.write(async |tx| switch_branch(tx, "c1", "a1").await).await.unwrap();
        assert_eq!(switched.as_deref(), Some("a1x"));
        assert_eq!(head(&db, "c1").await.as_deref(), Some("a1x"));
        let unknown = db.write(async |tx| switch_branch(tx, "c1", "nope").await).await;
        assert!(matches!(unknown, Err(DbErr::RecordNotFound(_))));
    }

    #[tokio::test]
    async fn deleting_a_subtree_takes_the_descendants_and_moves_the_head() {
        let db = with_conversations(&["c1", "c2"]).await;
        tree(
            &db,
            &[
                ("q", None),
                ("a1", Some("q")),
                ("a1x", Some("a1")),
                ("a2", Some("q")),
                ("a2x", Some("a2")),
            ],
        )
        .await;
        let other = message::Model {
            parent_id: None,
            ..row("q2", "c2", "user")
        };
        db.write(async |tx| insert_message(tx, other).await).await.unwrap();
        execute_for_tests(&db, "UPDATE conversations SET head_message_id = 'a1x' WHERE id = 'c1'")
            .await
            .unwrap();

        let new_head = db.write(async |tx| delete_subtree(tx, "c1", "a1").await).await.unwrap();
        assert_eq!(ids(&db, "c1").await, ["q", "a2", "a2x"], "the siblings are spared");
        assert_eq!(
            new_head.as_deref(),
            Some("a2x"),
            "the head follows the surviving branch to its tip"
        );
        assert_eq!(head(&db, "c1").await.as_deref(), Some("a2x"));
        assert_eq!(ids(&db, "c2").await, ["q2"], "another conversation is untouched");

        let emptied = db.write(async |tx| delete_subtree(tx, "c1", "q").await).await.unwrap();
        assert_eq!((emptied, head(&db, "c1").await), (None, None));
        assert!(ids(&db, "c1").await.is_empty());
    }

    /// With no sibling to follow, the head falls back to the deleted branch's
    /// parent rather than to nothing.
    #[tokio::test]
    async fn deleting_the_only_branch_moves_the_head_to_the_parent() {
        let db = with_conversations(&["c1"]).await;
        tree(&db, &[("q", None), ("a", Some("q"))]).await;
        execute_for_tests(&db, "UPDATE conversations SET head_message_id = 'a' WHERE id = 'c1'")
            .await
            .unwrap();

        let new_head = db.write(async |tx| delete_subtree(tx, "c1", "a").await).await.unwrap();
        assert_eq!(new_head.as_deref(), Some("q"));
        assert_eq!(head(&db, "c1").await.as_deref(), Some("q"));
        assert_eq!(ids(&db, "c1").await, ["q"]);
    }

    /// A 2000-deep chain deletes: the parent link carries no foreign key, and
    /// the subtree is collected in Rust rather than by recursion in SQL.
    #[tokio::test]
    async fn deleting_a_deep_chain_does_not_hit_a_recursion_limit() {
        let db = with_conversations(&["c1"]).await;
        db.write(async |tx| {
            let mut parent: Option<String> = None;
            for i in 0..2000 {
                let id = format!("m{i}");
                insert_message(
                    tx,
                    message::Model {
                        parent_id: parent.clone(),
                        ..row(&id, "c1", "user")
                    },
                )
                .await?;
                parent = Some(id);
            }
            Ok::<_, DbErr>(())
        })
        .await
        .unwrap();
        db.write(async |tx| delete_subtree(tx, "c1", "m0").await)
            .await
            .expect("a 2000-deep subtree must delete");
        assert!(ids(&db, "c1").await.is_empty());
    }

    /// Compacting one branch drops only the summaries anchored on its path.
    #[tokio::test]
    async fn summaries_are_dropped_by_the_path_they_belong_to() {
        let db = with_conversations(&["c1"]).await;
        tree(&db, &[("q", None), ("a1", Some("q")), ("a2", Some("q"))]).await;
        for (id, anchor) in [("s1", "a1"), ("s2", "a2")] {
            let summary = message::Model {
                is_compact_summary: SqlBool::TRUE,
                compact_anchor_id: Some(anchor.into()),
                ..row(id, "c1", "user")
            };
            db.write(async |tx| insert_message(tx, summary).await).await.unwrap();
        }
        db.write(async |tx| delete_summaries_anchored_in(tx, "c1", &["q".into(), "a1".into()]).await)
            .await
            .unwrap();
        assert_eq!(ids(&db, "c1").await, ["q", "a1", "a2", "s2"]);
    }

    #[tokio::test]
    async fn a_rating_is_set_and_cleared() {
        let db = with_conversations(&["c1"]).await;
        append(&db, row("m1", "c1", "assistant"), None).await;
        db.write(async |tx| update_rating(tx, "m1", Some(-1)).await)
            .await
            .unwrap();
        assert_eq!(stored(&db, "m1").await.rating, Some(-1));
        db.write(async |tx| update_rating(tx, "m1", None).await).await.unwrap();
        assert_eq!(stored(&db, "m1").await.rating, None);
    }

    /// The flag is held to 0/1 at the read.
    #[tokio::test]
    async fn a_compact_flag_that_is_not_zero_or_one_fails_the_read() {
        let db = with_conversations(&["c1"]).await;
        append(&db, row("m1", "c1", "assistant"), None).await;
        execute_for_tests(&db, "UPDATE messages SET is_compact_summary = 2 WHERE id = 'm1'")
            .await
            .unwrap();
        assert!(list_messages(&db, "c1").await.is_err());
        assert!(get_message(&db, "m1").await.is_err());
    }
}
