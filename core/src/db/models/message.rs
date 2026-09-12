use crate::db::schema::messages;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    Context,
}

impl MessageRole {
    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown message role `{value}`"))
    }

    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = messages)]
pub struct MessageRow {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub tool_calls: Option<String>,
    pub tool_call_id: Option<String>,
    /// Insertion order within the conversation, assigned by a trigger. Since
    /// messages became a tree this no longer means "position in the transcript"
    /// — sibling branches interleave their ranges. It still orders siblings for
    /// the version pager, and still identifies the newest row, which is what
    /// `resolve_head` falls back to. To read a conversation in order, walk the
    /// path with `active_context`.
    pub sort_order: i32,
    pub created_at: i64,
    pub reasoning_content: Option<String>,
    pub rating: Option<i32>,
    pub schema_version: i32,
    pub is_compact_summary: i32,
    /// Platform id of whoever sent this, when there is a trustworthy one.
    /// `None` for desktop chats, assistant/tool rows, and anything written
    /// before the identity pipeline existed.
    pub sender_id: Option<i64>,
    /// The message this one answers or follows. `None` marks a root: the first
    /// message of the conversation, or a second root created by editing it.
    /// Siblings under one parent are alternative versions.
    pub parent_id: Option<String>,
    /// Set only on compaction summaries: the first message on the path that this
    /// summary stands in front of. A summary whose anchor is not on the active
    /// path does not apply.
    pub compact_anchor_id: Option<String>,
    /// How this message was produced. `None` means typed; `"voice"` marks
    /// offline speech-to-text, whose transcripts may carry homophone errors.
    pub source: Option<String>,
    /// The run of the turn that wrote this row. `None` on compaction summaries,
    /// which stand in for history rather than being something a turn produced,
    /// and on everything written before turns were recorded.
    pub turn_id: Option<String>,
    /// How the call this row answers ended: `success`, `denied` or `error`.
    /// Only set on `tool` rows. `None` reads as success — that is what every
    /// row written before this column existed means, and it is the answer the
    /// transcript gave for all of them anyway.
    pub tool_outcome: Option<String>,
    /// Prompt tokens the upstream served out of its cache, and wrote into it.
    /// Both are subsets of `input_tokens`, never things to add to it — see
    /// migration 28 for the contract the provider layer normalises to.
    ///
    /// `None` is an upstream that said nothing about caching; `Some(0)` is one
    /// that said nothing was cached. A hit rate that reads the first as the
    /// second reports every reply from a silent provider as a total miss.
    pub cache_read_tokens: Option<i32>,
    pub cache_write_tokens: Option<i32>,
    /// Billable provider-side tool invocations this reply made. Charged per
    /// call on top of the tokens, so it is a cost the four counts above cannot
    /// express.
    pub server_tool_calls: Option<i32>,
    /// What the provider was called when this row was written.
    ///
    /// Stored beside `provider_id` rather than joined to it, because that
    /// column's foreign key is `ON DELETE SET NULL`: deleting a provider
    /// rewrites the attribution of every reply it ever produced. `model_id` has
    /// always been a keyless snapshot for the same reason.
    pub provider_name: Option<String>,
    /// Versioned provider-owned continuation state. This is a persistence
    /// concern, not part of the transcript DTO exposed through Tauri.
    pub provider_state: Option<String>,
    /// What an automatic reviewer decided about this row's tool calls, keyed by
    /// call id. `None` on every row nothing reviewed, which is most of them —
    /// the fast paths in `tools::reach` never reach a reviewer at all. See
    /// migration 33.
    pub auto_review: Option<String>,
    /// The diff a hosted agent reported for each Edit/Write on this row, keyed
    /// by call id: `{ call_id: [ToolCallDiff, ...] }`. `None` on every row the
    /// agent reported nothing for, which is every native row. See migration
    /// 56. Written after the row by `record_tool_diffs`, never at insert.
    pub tool_diffs: Option<String>,
}

/// The four token counts one message row records.
///
/// A value rather than four parameters, because all four are `Option<i32>` and
/// nothing but their order tells them apart. `complete_assistant` already took
/// two of them positionally; a third and a fourth would make transposing output
/// into input — or a cache read into a cache write — something the compiler
/// cannot see. Nor would a test catch it: the numbers still land, the report
/// still draws, and only the column headings are wrong.
///
/// The other three things that function is handed stay positional on purpose.
/// Reasoning and a tool-call payload are the same type and could be swapped too,
/// but swapping them puts thinking text where a JSON array belongs and fails
/// loudly the first time it runs. These four fail silently, which is what earns
/// them a struct.
///
/// `None` and `Some(0)` are kept apart throughout: `None` is an upstream that
/// said nothing, `Some(0)` is one that said nothing was cached. See migration 28
/// for why collapsing them breaks the hit rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageUsage {
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    /// Prompt tokens the upstream served out of its cache. A subset of
    /// `input_tokens`, never something to add to it.
    pub cache_read_tokens: Option<i32>,
    /// Prompt tokens the upstream wrote into its cache, charged at a premium so
    /// later reads are cheap. Also a subset of `input_tokens`.
    pub cache_write_tokens: Option<i32>,
    /// Billable provider-side tool invocations — searches the upstream ran
    /// itself. Not a token count: charged per call, on top of the tokens.
    pub server_tool_calls: Option<i32>,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = messages)]
pub struct MessageInsert<'a> {
    pub id: &'a str,
    pub conversation_id: &'a str,
    pub role: &'a str,
    pub content: &'a str,
    pub provider_id: Option<&'a str>,
    pub model_id: Option<&'a str>,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub tool_calls: Option<&'a str>,
    pub tool_call_id: Option<&'a str>,
    pub sort_order: i32,
    pub created_at: i64,
    pub reasoning_content: Option<&'a str>,
    pub rating: Option<i32>,
    pub schema_version: i32,
    pub is_compact_summary: i32,
    pub sender_id: Option<i64>,
    pub parent_id: Option<&'a str>,
    pub compact_anchor_id: Option<&'a str>,
    pub source: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    pub tool_outcome: Option<&'a str>,
    pub cache_read_tokens: Option<i32>,
    pub cache_write_tokens: Option<i32>,
    pub server_tool_calls: Option<i32>,
    pub provider_name: Option<&'a str>,
}
