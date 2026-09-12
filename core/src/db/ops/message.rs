use std::collections::HashMap;

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::message::{MessageInsert, MessageRow, MessageUsage};
use crate::db::schema::{conversations, messages};

/// Append a message to the end of a conversation's active path.
///
/// The single write path for conversation messages. It links the new row to
/// `parent` and moves the conversation's head onto it, in one transaction, so a
/// row can never exist without being reachable from the head.
///
/// `parent` is the caller's own cursor for the turn rather than a re-read of the
/// head, so a turn that is deliberately branching — regenerating an answer, or
/// answering an edited question — writes a sibling instead of a continuation.
///
/// It is not a defence against two turns running at once. That reads as though
/// they would come out as two branches, and the rows do; but the head belongs to
/// whichever finishes last, and the other turn's entire output stops being on
/// the active path — which the user sees as their answer vanishing. Exclusion is
/// `turn::TurnCoordinator`'s job, one turn per conversation, and it is taken
/// before anything here is called.
pub fn append_message(
    conn: &mut SqliteConnection,
    new: &MessageInsert,
    parent: Option<&str>,
) -> QueryResult<MessageRow> {
    let row = conn.transaction(|conn| {
        let row = insert_message(
            conn,
            &MessageInsert {
                parent_id: parent,
                ..copy_of(new)
            },
        )?;
        diesel::update(conversations::table.find(new.conversation_id))
            .set(conversations::head_message_id.eq(Some(&row.id)))
            .execute(conn)?;
        Ok::<_, diesel::result::Error>(row)
    })?;
    audit_copy(conn, &row);
    Ok(row)
}

/// Keep a copy of what someone said where deleting the conversation cannot reach
/// it.
///
/// Outside the transaction above, and its failure is logged rather than
/// returned. A database that cannot take the audit copy is worth shouting about,
/// but rolling the message itself back because of it would turn a bookkeeping
/// fault into the user's message disappearing as they watch.
///
/// Only user rows. An assistant reply is recorded by `complete_assistant`, once
/// it has content and token counts — recording the placeholder here would file an
/// empty row and then a full one for every turn. Tool results are left out
/// altogether: they are our own text, and their arguments carry file contents and
/// command output this table has no business holding a second copy of. A
/// compaction summary is not something anyone said.
fn audit_copy(conn: &mut SqliteConnection, row: &MessageRow) {
    // A `shell` row is a local execution record, not training/audit text. Its
    // command routinely contains tokens and passwords, while the paired output
    // already lives in the deliberately private context-item table.
    if row.role != "user" || row.is_compact_summary != 0 || row.source.as_deref() == Some("shell") {
        return;
    }
    if let Err(e) = crate::db::ops::audit::record(conn, row) {
        tracing::error!(
            error = %e,
            message_id = %row.id,
            "the audit copy of a message could not be written",
        );
    }
}

/// Where the active path currently ends.
///
/// Falls back to the highest `sort_order` row when the stored head is missing or
/// dangling. That row is necessarily a leaf: any child of it would have been
/// inserted afterwards and so carry a larger `sort_order`, contradicting it being
/// the maximum. So the fallback always names a legitimate tip, which is what lets
/// a dropped head write cost an alternative branch rather than the transcript.
///
/// `history` is the caller's already-loaded message list for the conversation,
/// ordered by `sort_order`.
pub fn resolve_head(stored_head: Option<&str>, history: &[MessageRow]) -> Option<String> {
    if let Some(head) = stored_head
        && history.iter().any(|m| m.id == head && m.is_compact_summary == 0)
    {
        return Some(head.to_string());
    }
    history
        .iter()
        .rfind(|m| m.is_compact_summary == 0)
        .map(|m| m.id.clone())
}

/// Everything a turn needs to rebuild its context, resolved once.
///
/// Existed as three separate lookups threaded through
/// `build_messages_with_senders`, which meant four call sites each had to
/// remember to pair a message list with the matching cursor. Bundling them makes
/// the pairing impossible to get wrong, and gives the front end the split point
/// without recomputing it from sort_order.
pub struct ActiveContext {
    /// Root to head, in order. Excludes summaries and inactive branches.
    pub path: Vec<MessageRow>,
    /// The summary standing in front of `path`, when one applies.
    pub summary: Option<MessageRow>,
    /// Where `summary` takes over: everything before this index is represented
    /// by it. `None` when no summary applies.
    pub anchor_index: Option<usize>,
    pub head_id: Option<String>,
}

impl ActiveContext {
    /// The messages a request actually carries: the tail from the anchor on,
    /// since anything before it is covered by the summary.
    pub fn live(&self) -> &[MessageRow] {
        match self.anchor_index {
            Some(i) => &self.path[i..],
            None => &self.path,
        }
    }
}

/// Walk the tree from `head` back to a root, then reverse.
///
/// Done in Rust rather than a recursive CTE because the whole conversation is
/// already loaded: reading a path through SQL would mean giving `MessageRow` a
/// `QueryableByName` impl and hand-writing every column's type, which is pure
/// upkeep. The visited set guards against a cycle, which no writer can produce
/// but corrupted data could.
fn path_to_head(history: &[MessageRow], head: &str) -> Vec<MessageRow> {
    let by_id: std::collections::HashMap<&str, &MessageRow> = history
        .iter()
        .filter(|m| m.is_compact_summary == 0)
        .map(|m| (m.id.as_str(), m))
        .collect();

    let mut seen = std::collections::HashSet::new();
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

/// Load the active path plus whichever summary applies to it.
///
/// `history` is the full conversation, ordered by `sort_order`.
pub fn active_context(history: &[MessageRow], stored_head: Option<&str>) -> ActiveContext {
    let head_id = resolve_head(stored_head, history);
    // No sort_order fallback for an unlinked history. The backfill runs inside
    // the migration transaction and a failure there aborts startup, so a
    // conversation cannot quietly end up without parent links — while several
    // parentless rows *are* expected once editing the opening message starts
    // producing sibling roots, and flattening those would splice two versions of
    // the conversation into one.
    let path = match head_id.as_deref() {
        Some(head) => path_to_head(history, head),
        None => Vec::new(),
    };

    // A summary applies only if its anchor is on this path — that is what stops
    // one branch from being handed another branch's summary. With several, the
    // deepest anchor wins, being the most recent compaction of this path.
    let mut best: Option<(usize, &MessageRow)> = None;
    for s in history.iter().filter(|m| m.is_compact_summary == 1) {
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

/// `MessageInsert` holds borrows, so it cannot derive Clone without tying the copy
/// to the original's lifetime. Rebuilding it field by field keeps `append_message`
/// able to override `parent_id` without forcing every caller to pass it.
fn copy_of<'a>(n: &MessageInsert<'a>) -> MessageInsert<'a> {
    MessageInsert {
        id: n.id,
        conversation_id: n.conversation_id,
        role: n.role,
        content: n.content,
        provider_id: n.provider_id,
        model_id: n.model_id,
        input_tokens: n.input_tokens,
        output_tokens: n.output_tokens,
        tool_calls: n.tool_calls,
        tool_call_id: n.tool_call_id,
        sort_order: n.sort_order,
        created_at: n.created_at,
        reasoning_content: n.reasoning_content,
        rating: n.rating,
        schema_version: n.schema_version,
        is_compact_summary: n.is_compact_summary,
        sender_id: n.sender_id,
        parent_id: n.parent_id,
        source: n.source,
        compact_anchor_id: n.compact_anchor_id,
        turn_id: n.turn_id,
        tool_outcome: n.tool_outcome,
        cache_read_tokens: n.cache_read_tokens,
        cache_write_tokens: n.cache_write_tokens,
        server_tool_calls: n.server_tool_calls,
        provider_name: n.provider_name,
    }
}

/// Selected by name rather than by position.
///
/// `load::<MessageRow>` maps columns to fields in declaration order, so two
/// adjacent columns of the same type are held apart by nothing but the order of
/// two files agreeing. `messages` now has `cache_read_tokens` and
/// `cache_write_tokens` side by side, both `Nullable<Integer>`: transposing them
/// in either `schema.rs` or the struct would compile, pass every test, and
/// quietly report each cache write as a read for the rest of the table's life.
/// `as_select()` makes that a compile error instead.
pub fn list_messages(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Vec<MessageRow>> {
    messages::table
        .filter(messages::conversation_id.eq(conversation_id))
        .order(messages::sort_order.asc())
        .select(MessageRow::as_select())
        .load(conn)
}

pub fn insert_message(conn: &mut SqliteConnection, new: &MessageInsert) -> QueryResult<MessageRow> {
    diesel::insert_into(messages::table).values(new).execute(conn)?;
    messages::table.find(new.id).first::<MessageRow>(conn)
}

// `update_content` was here, and went with the `update_message_content`
// command that was its only caller. Rewriting one row's text in place has no
// safe entry point: it names a message, not a conversation, so it cannot take
// the lease that keeps a running turn from having the ground moved under it.

/// One row by id. Selected by name, for the reason `list_messages` is.
pub fn get_message(conn: &mut SqliteConnection, id: &str) -> QueryResult<MessageRow> {
    messages::table.find(id).select(MessageRow::as_select()).first(conn)
}

pub fn update_assistant_message(
    conn: &mut SqliteConnection,
    id: &str,
    content: &str,
    reasoning_content: Option<&str>,
    tool_calls: Option<&str>,
    provider_state: Option<&str>,
    usage: &MessageUsage,
) -> QueryResult<()> {
    let affected = diesel::update(messages::table.find(id))
        .set((
            messages::content.eq(content),
            messages::reasoning_content.eq(reasoning_content),
            messages::tool_calls.eq(tool_calls),
            messages::provider_state.eq(provider_state),
            messages::input_tokens.eq(usage.input_tokens),
            messages::output_tokens.eq(usage.output_tokens),
            messages::cache_read_tokens.eq(usage.cache_read_tokens),
            messages::cache_write_tokens.eq(usage.cache_write_tokens),
            messages::server_tool_calls.eq(usage.server_tool_calls),
        ))
        .execute(conn)?;
    if affected != 1 {
        return Err(diesel::result::Error::NotFound);
    }
    Ok(())
}

/// Fill in one tool call inside a row that has already been stored.
///
/// For the case an in-memory patch cannot reach: a hosted adapter announces a
/// call twice, from two sources that can arrive in either order, and the first
/// announcement is routinely a placeholder with no arguments. If the round
/// holding it closes before the second one lands — a result and the next call
/// are enough to do that — the row is already in the database and the real
/// arguments have nowhere to go. Left there, the transcript and the audit copy
/// keep `{}` for ever, which is worse than a card that reads "Terminal": a
/// reader cannot tell a call whose arguments were never recorded from one that
/// genuinely took none.
///
/// Scoped by turn because that is what the caller has and it bounds the scan;
/// within an ACP session a `toolCallId` is unique anyway. Rows are few and each
/// carries at most a handful of calls, so this is a scan rather than a JSON
/// query — SQLite's `json_each` would tie the storage format to the query.
///
/// `None` for either field leaves it alone. This only ever *adds* information:
/// the adapter sends plain progress beats on the same shape, and taking them at
/// face value would blank arguments already recorded.
///
/// Answers with the row it landed on, or `None` when no row in this turn holds
/// the call.
pub fn revise_tool_call(
    conn: &mut SqliteConnection,
    turn_id: &str,
    call_id: &str,
    tool_name: Option<&str>,
    arguments: Option<&str>,
) -> QueryResult<Option<(String, String, String)>> {
    let rows: Vec<(String, Option<String>)> = messages::table
        .filter(messages::turn_id.eq(turn_id))
        .filter(messages::tool_calls.is_not_null())
        // Newest first: a late revision belongs to a round that closed a moment
        // ago far more often than to one at the top of the turn.
        .order(messages::sort_order.desc())
        .select((messages::id, messages::tool_calls))
        .load(conn)?;

    for (id, json) in rows {
        let mut calls = crate::agent::tool_calls::parse_openai_tool_calls(json.as_deref()).map_err(|error| {
            diesel::result::Error::DeserializationError(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("message {id} has invalid persisted tool_calls: {error}"),
            )))
        })?;
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
        diesel::update(messages::table.find(&id))
            .set(messages::tool_calls.eq(Some(crate::agent::tool_calls::serialize_tool_calls_openai(&calls))))
            .execute(conn)?;
        return Ok(Some((id, found.0, found.1)));
    }
    Ok(None)
}

// `update_tokens` was here, and had no callers. It wrote the same two columns
// `update_assistant_message` writes, from nowhere, which meant a second answer
// to "how does a row get its token counts" that could drift from the first. The
// cache columns would have doubled that surface for nothing.

/// File one automatic-review verdict against the call it judged.
///
/// Merged into whatever is already there rather than overwriting it: one
/// assistant message can carry several tool calls, each reviewed separately and
/// at a different moment, and the last one to finish must not erase the rest.
///
/// A row that has gone (the conversation was deleted while the reviewer ran) is
/// not an error. The verdict was about a message nobody can open any more, and
/// failing here would take down a turn over bookkeeping.
pub fn record_auto_review(
    conn: &mut SqliteConnection,
    message_id: &str,
    call_id: &str,
    verdict: &crate::events::AutoReviewVerdict,
) -> QueryResult<()> {
    if call_id.is_empty() {
        return Err(diesel::result::Error::SerializationError(Box::new(
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "auto-review call id must not be empty",
            ),
        )));
    }
    let existing: Option<String> = messages::table
        .find(message_id)
        .select(messages::auto_review)
        .first::<Option<String>>(conn)
        .optional()?
        .flatten();

    let mut all = match existing.as_deref() {
        Some(raw) => serde_json::from_str::<std::collections::BTreeMap<String, crate::events::AutoReviewVerdict>>(raw)
            .map_err(|error| diesel::result::Error::DeserializationError(Box::new(error)))?,
        None => std::collections::BTreeMap::new(),
    };
    all.insert(call_id.to_string(), verdict.clone());

    let encoded =
        serde_json::to_string(&all).map_err(|error| diesel::result::Error::SerializationError(Box::new(error)))?;
    diesel::update(messages::table.find(message_id))
        .set(messages::auto_review.eq(Some(encoded)))
        .execute(conn)?;
    Ok(())
}

/// Keep the diff a hosted agent reported for one call, beside the row that
/// made the call.
///
/// Merged by call id and **replaced** per call: the adapter sends a call's
/// whole hunk list in one update, so a later one for the same call is a
/// correction, not an addition. Other calls' entries on the row are kept.
/// Stored JSON that cannot be read is an error rather than a fresh map —
/// overwriting it would quietly discard another call's diff.
pub fn record_tool_diffs(
    conn: &mut SqliteConnection,
    message_id: &str,
    call_id: &str,
    diffs: &[crate::events::ToolCallDiff],
) -> QueryResult<()> {
    if call_id.is_empty() {
        return Err(diesel::result::Error::SerializationError(Box::new(
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "tool diff call id must not be empty"),
        )));
    }
    let existing: Option<String> = messages::table
        .find(message_id)
        .select(messages::tool_diffs)
        .first::<Option<String>>(conn)
        .optional()?
        .flatten();

    let mut all = match existing.as_deref() {
        Some(raw) => serde_json::from_str::<std::collections::BTreeMap<String, Vec<crate::events::ToolCallDiff>>>(raw)
            .map_err(|error| diesel::result::Error::DeserializationError(Box::new(error)))?,
        None => std::collections::BTreeMap::new(),
    };
    all.insert(call_id.to_string(), diffs.to_vec());

    let encoded =
        serde_json::to_string(&all).map_err(|error| diesel::result::Error::SerializationError(Box::new(error)))?;
    diesel::update(messages::table.find(message_id))
        .set(messages::tool_diffs.eq(Some(encoded)))
        .execute(conn)?;
    Ok(())
}

/// [`record_tool_diffs`] for a call whose row is not known: find it among the
/// turn's stored rows the way [`revise_tool_call`] does, and say which row it
/// landed on. `None` when no row of this turn made the call.
pub fn record_tool_diffs_for_call(
    conn: &mut SqliteConnection,
    turn_id: &str,
    call_id: &str,
    diffs: &[crate::events::ToolCallDiff],
) -> QueryResult<Option<String>> {
    let rows: Vec<(String, Option<String>)> = messages::table
        .filter(messages::turn_id.eq(turn_id))
        .filter(messages::tool_calls.is_not_null())
        .order(messages::sort_order.desc())
        .select((messages::id, messages::tool_calls))
        .load(conn)?;

    for (id, json) in rows {
        let calls = crate::agent::tool_calls::parse_openai_tool_calls(json.as_deref()).map_err(|error| {
            diesel::result::Error::DeserializationError(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("message {id} has invalid persisted tool_calls: {error}"),
            )))
        })?;
        if !calls.iter().any(|c| c.id == call_id) {
            continue;
        }
        record_tool_diffs(conn, &id, call_id, diffs)?;
        return Ok(Some(id));
    }
    Ok(None)
}

pub fn update_rating(conn: &mut SqliteConnection, id: &str, rating: Option<i32>) -> QueryResult<()> {
    diesel::update(messages::table.find(id))
        .set(messages::rating.eq(rating))
        .execute(conn)?;
    Ok(())
}

/// Drop the summaries belonging to one path, leaving other branches' alone.
///
/// Compacting used to clear every summary in the conversation, which is right
/// while a conversation is a single line and wrong the moment it is not: the
/// branch being compacted would take the other branches' summaries with it, and
/// switching back would re-summarise from scratch.
pub fn delete_summaries_anchored_in(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    path_ids: &[String],
) -> QueryResult<()> {
    if path_ids.is_empty() {
        return Ok(());
    }
    diesel::delete(
        messages::table
            .filter(messages::conversation_id.eq(conversation_id))
            .filter(messages::is_compact_summary.eq(1))
            .filter(messages::compact_anchor_id.eq_any(path_ids)),
    )
    .execute(conn)?;
    Ok(())
}

/// Delete a message and everything descended from it.
///
/// The unit of deletion, because a message only makes sense with its answer:
/// removing a question but keeping the reply leaves the model reading an answer
/// to nothing, and removing an assistant row on its own strands the tool results
/// it called for. Sibling branches under the same parent go too — they are
/// alternative versions of the same deleted step.
///
/// Collected with a recursive CTE and deleted in one pass rather than leaning on
/// ON DELETE CASCADE, which recurses once per level and would exhaust
/// SQLITE_MAX_TRIGGER_DEPTH on a long conversation. The head is repaired
/// afterwards: it may have pointed into the subtree, and ON DELETE SET NULL
/// would have already blanked it by then.
pub fn delete_subtree(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    message_id: &str,
) -> QueryResult<Option<String>> {
    #[derive(QueryableByName)]
    struct IdRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        id: String,
    }

    conn.transaction(|conn| {
        let parent: Option<String> = messages::table
            .find(message_id)
            .select(messages::parent_id)
            .first::<Option<String>>(conn)
            .optional()?
            .flatten();

        let doomed: Vec<String> = diesel::sql_query(
            "WITH RECURSIVE subtree(id) AS (
               SELECT id FROM messages WHERE id = ? AND conversation_id = ?
               UNION ALL
               SELECT m.id FROM messages m JOIN subtree s ON m.parent_id = s.id
             )
             SELECT id FROM subtree",
        )
        .bind::<diesel::sql_types::Text, _>(message_id)
        .bind::<diesel::sql_types::Text, _>(conversation_id)
        .load::<IdRow>(conn)?
        .into_iter()
        .map(|r| r.id)
        .collect();

        // Chunked to stay under SQLITE_MAX_VARIABLE_NUMBER, which a long
        // conversation would otherwise blow past.
        for chunk in doomed.chunks(500) {
            diesel::delete(messages::table.filter(messages::id.eq_any(chunk))).execute(conn)?;
        }

        // Order matters: the deletes above may have nulled the head via
        // ON DELETE SET NULL, so it is rewritten last.
        let history = messages::table
            .filter(messages::conversation_id.eq(conversation_id))
            .order(messages::sort_order.asc())
            .load::<MessageRow>(conn)?;
        let new_head = parent
            .filter(|p| history.iter().any(|m| &m.id == p))
            .map(|p| deepest_descendant(&history, &p))
            .or_else(|| resolve_head(None, &history));

        diesel::update(conversations::table.find(conversation_id))
            .set(conversations::head_message_id.eq(new_head.as_ref()))
            .execute(conn)?;

        Ok(new_head)
    })
}

/// A point on the active path where the conversation was answered more than once.
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

/// Where the active path passes through a step that has alternatives.
///
/// Only points with more than one version are reported, so a conversation that
/// has never been regenerated yields an empty list and the front end renders no
/// pagers at all.
/// The parent a version comparison should be made against.
///
/// Injected background (`role = "context"`) is written into the path like any
/// other row, but it is not a step anybody took — so a message written after one
/// is still a version of whatever preceded it, not a child of somewhere else.
/// Walking past those rows is what keeps that true: without it, editing a
/// message on a turn that also froze a memory block leaves the new version
/// hanging off the context row while the old one hangs off its parent, the two
/// stop being siblings, and the version pager silently disappears from a
/// message that certainly has more than one version.
fn effective_parent(by_id: &HashMap<&str, &MessageRow>, m: &MessageRow) -> Option<String> {
    let mut cursor = m.parent_id.clone();
    while let Some(id) = cursor {
        // A parent that is not in `history` is as far as this can go.
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

/// Every step on the path that has more than one version, with its siblings.
///
/// **Grouped once rather than searched per row.** This used to be three nested
/// linear scans — a row on the path, every row in the history, and a lookup by
/// id inside `effective_parent` — which is n³ on a conversation with no
/// branches at all, the exact shape an imported session has. Measured: 0.3s at
/// 622 rows, 10s at 2000, on every mount of the transcript and up to four times
/// per snapshot attempt. Importing a terminal session is what made a
/// conversation that size reachable in one click.
pub fn branch_points(history: &[MessageRow], path: &[MessageRow]) -> Vec<BranchPoint> {
    let by_id: HashMap<&str, &MessageRow> = history.iter().map(|m| (m.id.as_str(), m)).collect();

    // Keyed on the effective parent, `None` for the roots — editing the opening
    // message produces a second one, which is a version of the same step.
    let mut families: HashMap<Option<String>, Vec<&MessageRow>> = HashMap::new();
    for m in history
        .iter()
        .filter(|s| s.is_compact_summary == 0 && s.role != "context")
    {
        families.entry(effective_parent(&by_id, m)).or_default().push(m);
    }
    for siblings in families.values_mut() {
        siblings.sort_by_key(|s| s.sort_order);
    }

    let mut out = Vec::new();
    for m in path {
        // Injected background has no versions to page through.
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

/// Move the head onto `message_id`'s branch, at the point that branch was last
/// written.
pub fn switch_branch(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    message_id: &str,
) -> QueryResult<Option<String>> {
    conn.transaction(|conn| {
        let history = messages::table
            .filter(messages::conversation_id.eq(conversation_id))
            .order(messages::sort_order.asc())
            .load::<MessageRow>(conn)?;
        if !history.iter().any(|m| m.id == message_id && m.is_compact_summary == 0) {
            return Err(diesel::result::Error::NotFound);
        }
        let head = deepest_descendant(&history, message_id);
        diesel::update(conversations::table.find(conversation_id))
            .set(conversations::head_message_id.eq(Some(&head)))
            .execute(conn)?;
        Ok(Some(head))
    })
}

/// Follow the newest child at each step. Used when the head has to move onto a
/// branch: "where that branch was last written" is the position a reader expects
/// to land on.
pub fn deepest_descendant(history: &[MessageRow], from: &str) -> String {
    let mut current = from.to_string();
    loop {
        let next = history
            .iter()
            .filter(|m| m.is_compact_summary == 0)
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
    use crate::db::ops::conversation::{create_conversation, get_conversation};
    use crate::db::test_db;

    fn row<'a>(id: &'a str, conv: &'a str, role: &'a str) -> MessageInsert<'a> {
        MessageInsert {
            id,
            conversation_id: conv,
            role,
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
            turn_id: None,
            tool_outcome: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            server_tool_calls: None,
            provider_name: None,
        }
    }

    fn auto_review_verdict() -> crate::events::AutoReviewVerdict {
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

    /// Per call, the list is replaced — the adapter sends the whole hunk list
    /// each time — while the other calls' entries on the row are kept.
    #[test]
    fn record_tool_diffs_replaces_one_call_and_keeps_the_others() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        append_message(&mut conn, &row("m1", "c1", "assistant"), None).unwrap();

        record_tool_diffs(&mut conn, "m1", "call-a", &[hunk(None, "created", None)]).unwrap();
        record_tool_diffs(&mut conn, "m1", "call-b", &[hunk(Some("old"), "new", Some(3))]).unwrap();
        record_tool_diffs(
            &mut conn,
            "m1",
            "call-a",
            &[hunk(Some("was there"), "created", Some(1))],
        )
        .unwrap();

        let stored: std::collections::BTreeMap<String, Vec<crate::events::ToolCallDiff>> =
            serde_json::from_str(get_message(&mut conn, "m1").unwrap().tool_diffs.as_deref().unwrap()).unwrap();
        assert_eq!(stored["call-a"], vec![hunk(Some("was there"), "created", Some(1))]);
        assert_eq!(stored["call-b"], vec![hunk(Some("old"), "new", Some(3))]);
        // The nullable keys are written out, never omitted.
        let raw = get_message(&mut conn, "m1").unwrap().tool_diffs.unwrap();
        assert!(raw.contains(r#""line":3"#) && raw.contains(r#""old_text""#), "{raw}");
    }

    /// Stored JSON this build cannot read is an error, not an empty map to
    /// write over — overwriting would discard another call's diff.
    #[test]
    fn record_tool_diffs_rejects_corrupt_stored_json_without_overwriting_it() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        append_message(&mut conn, &row("m1", "c1", "assistant"), None).unwrap();
        diesel::update(messages::table.find("m1"))
            .set(messages::tool_diffs.eq(Some("not json")))
            .execute(&mut conn)
            .unwrap();

        assert!(record_tool_diffs(&mut conn, "m1", "call-a", &[hunk(None, "x", None)]).is_err());
        assert!(record_tool_diffs(&mut conn, "m1", "", &[hunk(None, "x", None)]).is_err());
        assert_eq!(
            get_message(&mut conn, "m1").unwrap().tool_diffs.as_deref(),
            Some("not json")
        );
    }

    /// Finding the row by the call it made, bounded by the turn: a refinement
    /// for a call in another turn is not this turn's to file.
    #[test]
    fn record_tool_diffs_for_call_finds_the_row_within_the_turn() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        let mut with_call = row("m1", "c1", "assistant");
        with_call.turn_id = Some("t1");
        with_call.tool_calls =
            Some(r#"[{"id":"call-a","type":"function","function":{"name":"Write","arguments":"{}"}}]"#);
        append_message(&mut conn, &with_call, None).unwrap();

        let landed = record_tool_diffs_for_call(&mut conn, "t1", "call-a", &[hunk(None, "x", None)]).unwrap();
        assert_eq!(landed.as_deref(), Some("m1"));
        assert!(get_message(&mut conn, "m1").unwrap().tool_diffs.is_some());

        let elsewhere = record_tool_diffs_for_call(&mut conn, "t2", "call-a", &[hunk(None, "x", None)]).unwrap();
        assert_eq!(elsewhere, None);
    }

    #[test]
    fn record_auto_review_rejects_corrupt_stored_json_without_overwriting_it() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        append_message(&mut conn, &row("m1", "c1", "assistant"), None).unwrap();
        diesel::update(messages::table.find("m1"))
            .set(messages::auto_review.eq(Some("not json")))
            .execute(&mut conn)
            .unwrap();

        let result = record_auto_review(&mut conn, "m1", "call-1", &auto_review_verdict());

        assert!(result.is_err());
        let stored = get_message(&mut conn, "m1").unwrap();
        assert_eq!(stored.auto_review.as_deref(), Some("not json"));
    }

    #[test]
    fn record_auto_review_rejects_noncanonical_nested_verdicts_without_overwriting_them() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        append_message(&mut conn, &row("m1", "c1", "assistant"), None).unwrap();

        for raw in [
            r#"{"old":{"outcome":"allow"}}"#,
            r#"{"old":{"outcome":"allow","risk":null,"authorization":null,"rationale":null,"stage":null,"model":null,"evidence":[],"future":true}}"#,
        ] {
            diesel::update(messages::table.find("m1"))
                .set(messages::auto_review.eq(Some(raw)))
                .execute(&mut conn)
                .unwrap();

            let result = record_auto_review(&mut conn, "m1", "call-1", &auto_review_verdict());

            assert!(result.is_err());
            let stored = get_message(&mut conn, "m1").unwrap();
            assert_eq!(stored.auto_review.as_deref(), Some(raw));
        }
    }

    #[test]
    fn record_auto_review_writes_the_typed_required_null_shape() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        append_message(&mut conn, &row("m1", "c1", "assistant"), None).unwrap();

        record_auto_review(&mut conn, "m1", "call-1", &auto_review_verdict()).unwrap();

        let stored = get_message(&mut conn, "m1").unwrap().auto_review.unwrap();
        let value: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(
            value["call-1"],
            serde_json::json!({
                "outcome": "allow",
                "risk": null,
                "authorization": null,
                "rationale": null,
                "stage": null,
                "model": null,
                "evidence": [],
            })
        );
    }

    #[test]
    fn record_auto_review_rejects_an_empty_call_id() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        append_message(&mut conn, &row("m1", "c1", "assistant"), None).unwrap();

        let result = record_auto_review(&mut conn, "m1", "", &auto_review_verdict());

        assert!(result.is_err());
        assert!(get_message(&mut conn, "m1").unwrap().auto_review.is_none());
    }

    /// Editing a message on a turn that also froze a memory block must still
    /// leave two versions of that message, not one.
    ///
    /// The frozen row lands between the branch point and the new version, so by
    /// `parent_id` alone the two versions have different parents and neither
    /// looks like it has a sibling. What the user sees is the version pager
    /// vanishing from a message they just created a second version of — and
    /// their earlier text is still there, just unreachable.
    #[test]
    fn a_frozen_memory_row_does_not_hide_the_other_version() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 0).unwrap();

        // A question and its answer.
        append_message(&mut conn, &row("q", "c1", "user"), None).unwrap();
        append_message(&mut conn, &row("a", "c1", "assistant"), Some("q")).unwrap();

        // The question is edited on a turn that also froze a memory block, so
        // the new version hangs off the context row rather than off nothing.
        let mut ctx = row("mem", "c1", "context");
        ctx.source = Some("memory|delta|100.x|-|");
        append_message(&mut conn, &ctx, None).unwrap();
        append_message(&mut conn, &row("q2", "c1", "user"), Some("mem")).unwrap();

        let history = list_messages(&mut conn, "c1").unwrap();
        let path = active_context(&history, Some("q2")).path;
        let points = branch_points(&history, &path);

        let q2 = points.iter().find(|p| p.message_id == "q2").expect("q2 has a sibling");
        assert_eq!(q2.total, 2, "both versions of the question must be reachable");
        assert_eq!(q2.sibling_ids, vec!["q".to_string(), "q2".to_string()]);
        // And the frozen row itself is not a version of anything.
        assert!(points.iter().all(|p| p.message_id != "mem"));
    }

    /// The audit table holds what people said, for billing and for keeping a
    /// copy where deleting a conversation cannot reach it. Injected background
    /// is neither: nobody said it, nobody was billed for it, and it carries
    /// `<owner_notes>`, which exist on the understanding that they stay where
    /// they were put.
    #[test]
    fn injected_background_is_not_audited() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 0).unwrap();

        let mut ctx = row("m1", "c1", "context");
        ctx.content = "<owner_notes>\n- [general] x\n</owner_notes>";
        ctx.source = Some("memory|full|100.abc|-|");
        append_message(&mut conn, &ctx, None).unwrap();

        let mut said = row("m2", "c1", "user");
        said.content = "hi";
        append_message(&mut conn, &said, Some("m1")).unwrap();

        let audited = crate::db::ops::audit::list_recent(&mut conn, 10).unwrap();
        assert_eq!(audited.len(), 1, "only the user row belongs in the audit table");
        assert_eq!(audited[0].content, "hi");
    }

    /// Every field handed to `append_message` comes back out of it.
    ///
    /// `copy_of` is written by hand, field by field, with no `..` spread to
    /// carry anything along — that is what lets `append_message` override
    /// `parent_id` without every caller having to pass one. The cost is that a
    /// column added to `MessageInsert` produces a missing-field error inside
    /// `copy_of`, and the shortest way to silence that error is to write
    /// `None`. Which compiles, and drops the value on every insert, and breaks
    /// no other test in the suite.
    ///
    /// The `MessageRow` below is destructured rather than read field by field on
    /// purpose. An exhaustive pattern with no `..` fails to compile when a
    /// column is added, so this test cannot silently stop covering the table —
    /// which is the only property that makes it worth having.
    ///
    /// Distinct values everywhere, and deliberately distinct *within* each pair
    /// of same-typed columns: input 7 against output 11, cache read 41 against
    /// cache write 43. Two `Option<i32>` columns holding the same number would
    /// pass while transposed, and transposition is exactly what the positional
    /// `Queryable` mapping makes possible.
    #[test]
    fn append_message_keeps_every_field_it_was_given() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        let root = append_message(&mut conn, &row("root", "c1", "user"), None).unwrap();

        let full = MessageInsert {
            id: "m1",
            conversation_id: "c1",
            role: "assistant",
            content: "the answer",
            // No providers row in this fixture, and the foreign key is enforced
            // — the name travels beside it precisely so a report does not need
            // that row to still exist.
            provider_id: None,
            provider_name: Some("DeepSeek"),
            model_id: Some("deepseek-chat"),
            input_tokens: Some(7),
            output_tokens: Some(11),
            cache_read_tokens: Some(41),
            cache_write_tokens: Some(43),
            server_tool_calls: Some(47),
            tool_calls: Some("[]"),
            tool_call_id: Some("call-1"),
            sort_order: 0,
            created_at: 1234,
            reasoning_content: Some("thinking"),
            rating: Some(1),
            schema_version: 2,
            is_compact_summary: 0,
            sender_id: Some(99),
            // Overridden by `append_message` — that is the whole reason
            // `copy_of` exists, so it is asserted below rather than round-tripped.
            parent_id: None,
            // A real row: migration 21 put a foreign key on this one, unlike
            // `parent_id` beside it.
            compact_anchor_id: Some(root.id.as_str()),
            source: Some("voice"),
            turn_id: Some("t1"),
            tool_outcome: Some("success"),
        };
        append_message(&mut conn, &full, Some(&root.id)).unwrap();

        let stored = list_messages(&mut conn, "c1")
            .unwrap()
            .into_iter()
            .find(|m| m.id == "m1")
            .expect("the row that was just written");

        let MessageRow {
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
            server_tool_calls,
            provider_name,
            provider_state,
            auto_review,
            tool_diffs,
        } = stored;

        assert_eq!(id, "m1");
        assert_eq!(tool_diffs, None, "a native row carries no agent-reported diff");
        assert_eq!(conversation_id, "c1");
        assert_eq!(role, "assistant");
        assert_eq!(content, "the answer");
        assert_eq!(provider_id, None);
        assert_eq!(provider_name.as_deref(), Some("DeepSeek"));
        assert_eq!(provider_state, None);
        // Written by the reviewer afterwards, never by the write that creates
        // the row — see `record_auto_review`.
        assert_eq!(auto_review, None);
        assert_eq!(model_id.as_deref(), Some("deepseek-chat"));
        assert_eq!(input_tokens, Some(7));
        assert_eq!(output_tokens, Some(11));
        assert_eq!(cache_read_tokens, Some(41), "a read must not land in the write column");
        assert_eq!(cache_write_tokens, Some(43));
        assert_eq!(server_tool_calls, Some(47));
        assert_eq!(tool_calls.as_deref(), Some("[]"));
        assert_eq!(tool_call_id.as_deref(), Some("call-1"));
        assert!(sort_order > 0, "assigned by the trigger");
        assert_eq!(created_at, 1234);
        assert_eq!(reasoning_content.as_deref(), Some("thinking"));
        assert_eq!(rating, Some(1));
        assert_eq!(schema_version, 2);
        assert_eq!(is_compact_summary, 0);
        assert_eq!(sender_id, Some(99));
        assert_eq!(parent_id.as_deref(), Some("root"), "the one field the copy overrides");
        assert_eq!(compact_anchor_id.as_deref(), Some("root"));
        assert_eq!(source.as_deref(), Some("voice"));
        assert_eq!(turn_id.as_deref(), Some("t1"));
        assert_eq!(tool_outcome.as_deref(), Some("success"));
    }

    #[test]
    fn append_links_each_row_to_the_last_and_moves_the_head() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();

        let a = append_message(&mut conn, &row("a", "c1", "user"), None).unwrap();
        let b = append_message(&mut conn, &row("b", "c1", "assistant"), Some(&a.id)).unwrap();
        let c = append_message(&mut conn, &row("c", "c1", "tool"), Some(&b.id)).unwrap();

        assert_eq!(a.parent_id, None, "the first message is a root");
        assert_eq!(b.parent_id.as_deref(), Some("a"));
        assert_eq!(c.parent_id.as_deref(), Some("b"));

        let conv = get_conversation(&mut conn, "c1").unwrap();
        assert_eq!(conv.head_message_id.as_deref(), Some("c"));
    }

    /// Two answers to the same question are siblings, and the head follows
    /// whichever was written last.
    #[test]
    fn a_second_child_forks_the_branch() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();

        let q = append_message(&mut conn, &row("q", "c1", "user"), None).unwrap();
        append_message(&mut conn, &row("a1", "c1", "assistant"), Some(&q.id)).unwrap();
        let second = append_message(&mut conn, &row("a2", "c1", "assistant"), Some(&q.id)).unwrap();

        assert_eq!(second.parent_id.as_deref(), Some("q"));
        let conv = get_conversation(&mut conn, "c1").unwrap();
        assert_eq!(conv.head_message_id.as_deref(), Some("a2"));
    }

    #[test]
    fn resolve_head_prefers_the_stored_head() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        let q = append_message(&mut conn, &row("q", "c1", "user"), None).unwrap();
        append_message(&mut conn, &row("a1", "c1", "assistant"), Some(&q.id)).unwrap();
        append_message(&mut conn, &row("a2", "c1", "assistant"), Some(&q.id)).unwrap();

        let history = list_messages(&mut conn, "c1").unwrap();
        // a1 is not the newest row, so only the stored head can name it.
        assert_eq!(resolve_head(Some("a1"), &history).as_deref(), Some("a1"));
    }

    /// A dangling head must not strand the transcript. The newest row is always
    /// a leaf, since a child of it would have been inserted later still.
    #[test]
    fn resolve_head_falls_back_to_the_newest_row() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        let a = append_message(&mut conn, &row("a", "c1", "user"), None).unwrap();
        append_message(&mut conn, &row("b", "c1", "assistant"), Some(&a.id)).unwrap();

        let history = list_messages(&mut conn, "c1").unwrap();
        assert_eq!(resolve_head(None, &history).as_deref(), Some("b"));
        assert_eq!(resolve_head(Some("gone"), &history).as_deref(), Some("b"));

        let leaf_children = history.iter().filter(|m| m.parent_id.as_deref() == Some("b")).count();
        assert_eq!(leaf_children, 0, "the fallback must name a leaf");
    }

    /// A summary sits beside the tree. Letting it answer "where does the path
    /// end" would hang the next turn off something that is not conversation.
    #[test]
    fn resolve_head_ignores_compaction_summaries() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        let a = append_message(&mut conn, &row("a", "c1", "user"), None).unwrap();
        append_message(&mut conn, &row("b", "c1", "assistant"), Some(&a.id)).unwrap();

        let mut summary = row("s", "c1", "user");
        summary.is_compact_summary = 1;
        summary.sort_order = -1;
        insert_message(&mut conn, &summary).unwrap();

        let history = list_messages(&mut conn, "c1").unwrap();
        assert_eq!(resolve_head(Some("s"), &history).as_deref(), Some("b"));
        assert_eq!(resolve_head(None, &history).as_deref(), Some("b"));
    }

    #[test]
    fn resolve_head_is_none_for_an_empty_conversation() {
        assert_eq!(resolve_head(None, &[]), None);
        assert_eq!(resolve_head(Some("ghost"), &[]), None);
    }

    /// Builds a tree directly, bypassing append_message, so a shape the writers
    /// cannot currently produce can still be tested.
    fn tree(conn: &mut SqliteConnection, edges: &[(&str, Option<&str>)]) {
        for (id, parent) in edges {
            let mut n = row(id, "c1", "user");
            n.parent_id = *parent;
            insert_message(conn, &n).unwrap();
        }
    }

    fn ids(ctx: &ActiveContext) -> Vec<&str> {
        ctx.path.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn active_context_follows_one_branch_and_ignores_the_other() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(
            &mut conn,
            &[
                ("q", None),
                ("a1", Some("q")),
                ("a1x", Some("a1")),
                ("a2", Some("q")),
                ("a2x", Some("a2")),
            ],
        );
        let history = list_messages(&mut conn, "c1").unwrap();

        assert_eq!(ids(&active_context(&history, Some("a1x"))), ["q", "a1", "a1x"]);
        assert_eq!(ids(&active_context(&history, Some("a2x"))), ["q", "a2", "a2x"]);
    }

    #[test]
    fn active_context_is_empty_for_a_conversation_with_no_messages() {
        let ctx = active_context(&[], None);
        assert!(ctx.path.is_empty());
        assert_eq!(ctx.head_id, None);
        assert_eq!(ctx.anchor_index, None);
    }

    /// A corrupt parent link must not spin forever.
    #[test]
    fn active_context_stops_on_a_cycle() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(&mut conn, &[("a", None), ("b", Some("a"))]);
        diesel::update(messages::table.find("a"))
            .set(messages::parent_id.eq(Some("b")))
            .execute(&mut conn)
            .unwrap();
        let history = list_messages(&mut conn, "c1").unwrap();

        let ctx = active_context(&history, Some("b"));
        assert!(ctx.path.len() <= 2, "a cycle must terminate, got {:?}", ids(&ctx));
    }

    #[test]
    fn a_summary_anchored_on_the_path_takes_effect() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(&mut conn, &[("m1", None), ("m2", Some("m1")), ("m3", Some("m2"))]);

        let mut s = row("s", "c1", "user");
        s.is_compact_summary = 1;
        s.sort_order = -1;
        s.compact_anchor_id = Some("m2");
        insert_message(&mut conn, &s).unwrap();
        let history = list_messages(&mut conn, "c1").unwrap();

        let ctx = active_context(&history, Some("m3"));
        assert_eq!(ctx.anchor_index, Some(1));
        assert_eq!(ctx.summary.as_ref().map(|m| m.id.as_str()), Some("s"));
        // m1 is represented by the summary, so it is not sent again.
        assert_eq!(
            ctx.live().iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["m2", "m3"]
        );
    }

    /// The whole point of anchoring: switching branches must not hand this one
    /// a summary of a history it never had.
    #[test]
    fn a_summary_anchored_off_the_path_is_ignored() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(&mut conn, &[("q", None), ("a1", Some("q")), ("a2", Some("q"))]);

        let mut s = row("s", "c1", "user");
        s.is_compact_summary = 1;
        s.sort_order = -1;
        s.compact_anchor_id = Some("a1");
        insert_message(&mut conn, &s).unwrap();
        let history = list_messages(&mut conn, "c1").unwrap();

        let ctx = active_context(&history, Some("a2"));
        assert!(ctx.summary.is_none());
        assert_eq!(ctx.anchor_index, None);
        assert_eq!(ctx.live().len(), 2);
    }

    #[test]
    fn the_deepest_anchored_summary_wins() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(&mut conn, &[("m1", None), ("m2", Some("m1")), ("m3", Some("m2"))]);

        for (id, anchor) in [("s1", "m1"), ("s2", "m3")] {
            let mut s = row(id, "c1", "user");
            s.is_compact_summary = 1;
            s.sort_order = -1;
            s.compact_anchor_id = Some(anchor);
            insert_message(&mut conn, &s).unwrap();
        }
        let history = list_messages(&mut conn, "c1").unwrap();

        let ctx = active_context(&history, Some("m3"));
        assert_eq!(ctx.summary.as_ref().map(|m| m.id.as_str()), Some("s2"));
        assert_eq!(ctx.anchor_index, Some(2));
    }

    /// Parentless rows are never bridged by sort_order. A gap is what deleting a
    /// message leaves behind, and spanning it would invent a conversation that
    /// never happened — and once editing the opening message is possible, two
    /// roots are two versions of it, not one sequence.
    #[test]
    fn parentless_rows_are_not_stitched_together() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(&mut conn, &[("m1", None), ("m2", None), ("m3", Some("m2"))]);
        let history = list_messages(&mut conn, "c1").unwrap();

        assert_eq!(ids(&active_context(&history, Some("m3"))), ["m2", "m3"]);
    }

    /// The guarantee this whole change rests on: for a conversation that has
    /// never branched — every conversation that exists today — reading the tree
    /// gives back exactly what ordering by sort_order gave.
    #[test]
    fn a_linear_conversation_reads_back_identically() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();

        let mut parent: Option<String> = None;
        for i in 0..12 {
            let id = format!("m{i}");
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            let written = append_message(&mut conn, &row(&id, "c1", role), parent.as_deref()).unwrap();
            parent = Some(written.id);
        }

        let history = list_messages(&mut conn, "c1").unwrap();
        let by_sort_order: Vec<&str> = history.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids(&active_context(&history, None)), by_sort_order);
    }

    #[test]
    fn a_conversation_that_never_branched_has_no_branch_points() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(&mut conn, &[("q", None), ("a", Some("q"))]);
        let history = list_messages(&mut conn, "c1").unwrap();
        let ctx = active_context(&history, None);

        assert!(branch_points(&history, &ctx.path).is_empty());
    }

    #[test]
    fn a_branch_point_reports_the_active_version_and_its_siblings() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(
            &mut conn,
            &[("q", None), ("a1", Some("q")), ("a2", Some("q")), ("a3", Some("q"))],
        );
        let history = list_messages(&mut conn, "c1").unwrap();
        let ctx = active_context(&history, Some("a2"));

        let points = branch_points(&history, &ctx.path);
        assert_eq!(points.len(), 1, "only the answer forked, not the question");
        assert_eq!(points[0].message_id, "a2");
        assert_eq!(points[0].index, 1);
        assert_eq!(points[0].total, 3);
        assert_eq!(points[0].sibling_ids, ["a1", "a2", "a3"]);
    }

    /// Editing the opening message leaves two roots, which are versions of the
    /// same step and must page against each other.
    #[test]
    fn sibling_roots_are_a_branch_point() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(&mut conn, &[("q1", None), ("q2", None)]);
        let history = list_messages(&mut conn, "c1").unwrap();
        let ctx = active_context(&history, Some("q2"));

        let points = branch_points(&history, &ctx.path);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].index, 1);
        assert_eq!(points[0].sibling_ids, ["q1", "q2"]);
    }

    #[test]
    fn switching_lands_on_the_branch_tip() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(
            &mut conn,
            &[("q", None), ("a1", Some("q")), ("a1x", Some("a1")), ("a2", Some("q"))],
        );

        let head = switch_branch(&mut conn, "c1", "a1").unwrap();
        assert_eq!(head.as_deref(), Some("a1x"), "lands where that branch was last written");

        let history = list_messages(&mut conn, "c1").unwrap();
        let ctx = active_context(&history, head.as_deref());
        assert_eq!(ids(&ctx), ["q", "a1", "a1x"]);
    }

    #[test]
    fn switching_to_an_unknown_message_is_rejected() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(&mut conn, &[("q", None)]);

        assert!(switch_branch(&mut conn, "c1", "ghost").is_err());
        // The head must not have moved on a rejected switch.
        assert_eq!(get_conversation(&mut conn, "c1").unwrap().head_message_id, None);
    }

    #[test]
    fn deleting_a_subtree_takes_the_descendants_and_spares_the_siblings() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(
            &mut conn,
            &[("q", None), ("a1", Some("q")), ("a1x", Some("a1")), ("a2", Some("q"))],
        );

        delete_subtree(&mut conn, "c1", "a1").unwrap();

        let left: Vec<String> = list_messages(&mut conn, "c1")
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(left, ["q", "a2"]);
    }

    #[test]
    fn deleting_the_active_branch_moves_the_head_to_the_parent() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(&mut conn, &[("q", None), ("a", Some("q"))]);
        diesel::update(conversations::table.find("c1"))
            .set(conversations::head_message_id.eq(Some("a")))
            .execute(&mut conn)
            .unwrap();

        let head = delete_subtree(&mut conn, "c1", "a").unwrap();
        assert_eq!(head.as_deref(), Some("q"));
        assert_eq!(
            get_conversation(&mut conn, "c1").unwrap().head_message_id.as_deref(),
            Some("q")
        );
    }

    /// With the deleted branch gone the head lands on the surviving one, at the
    /// point it was last written.
    #[test]
    fn the_head_follows_a_surviving_sibling_to_its_tip() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(
            &mut conn,
            &[("q", None), ("a1", Some("q")), ("a2", Some("q")), ("a2x", Some("a2"))],
        );

        delete_subtree(&mut conn, "c1", "a1").unwrap();

        let history = list_messages(&mut conn, "c1").unwrap();
        assert_eq!(deepest_descendant(&history, "q"), "a2x");
    }

    #[test]
    fn deleting_the_root_empties_the_conversation() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(&mut conn, &[("q", None), ("a", Some("q"))]);

        let head = delete_subtree(&mut conn, "c1", "q").unwrap();
        assert_eq!(head, None);
        assert!(list_messages(&mut conn, "c1").unwrap().is_empty());
        assert_eq!(get_conversation(&mut conn, "c1").unwrap().head_message_id, None);
    }

    #[test]
    fn deleting_a_subtree_leaves_other_conversations_alone() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        create_conversation(&mut conn, "c2", None, None, None, 1).unwrap();
        tree(&mut conn, &[("q", None)]);
        let mut other = row("q2", "c2", "user");
        other.parent_id = None;
        insert_message(&mut conn, &other).unwrap();

        delete_subtree(&mut conn, "c1", "q").unwrap();
        assert_eq!(list_messages(&mut conn, "c2").unwrap().len(), 1);
    }

    /// The guard on parent_id carrying no foreign key. ON DELETE CASCADE recurses
    /// once per level, and the chain runs one node per message, so a conversation
    /// this long would hit SQLITE_MAX_TRIGGER_DEPTH. If someone adds the FK back,
    /// this fails.
    #[test]
    fn deleting_a_deep_chain_does_not_hit_the_recursion_limit() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();

        let mut parent: Option<String> = None;
        for i in 0..2000 {
            let id = format!("m{i}");
            let mut n = row(&id, "c1", "user");
            n.parent_id = parent.as_deref();
            insert_message(&mut conn, &n).unwrap();
            parent = Some(id);
        }

        delete_subtree(&mut conn, "c1", "m0").expect("a 2000-deep subtree must delete");
        assert!(list_messages(&mut conn, "c1").unwrap().is_empty());
    }

    /// Flattening a real fork would splice two branches into one transcript.
    #[test]
    fn a_forked_history_is_never_flattened() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        tree(&mut conn, &[("q", None), ("a1", Some("q")), ("a2", Some("q"))]);
        let history = list_messages(&mut conn, "c1").unwrap();

        assert_eq!(ids(&active_context(&history, Some("a2"))), ["q", "a2"]);
    }
}
