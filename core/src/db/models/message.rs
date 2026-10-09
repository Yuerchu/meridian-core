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
