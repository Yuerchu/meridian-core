pub mod auto_review;
mod base_prompt;
pub mod call_identity;
mod compact;
mod context;
pub mod conversation_excerpt;
pub mod denied;
pub(crate) mod diagnostics;
pub mod engine;
pub mod file_access;
mod inline_tag;
pub mod interrupted;
mod loop_guard;
pub(crate) mod manual;
pub(crate) mod memory_context;
pub mod modes;
pub mod pricing;
mod project_instructions;
mod provider_config;
pub mod queue;
pub mod skills;
mod stream;
pub mod sub_agents;
pub(crate) mod tokenizer;
// `pub(crate)` for `acp::session`, which writes the same `messages.tool_calls`
// column a native turn does and must encode it identically — two encoders would
// mean a transcript that renders differently depending on who wrote it.
pub(crate) mod tool_calls;
pub(crate) mod tool_defs;
mod truncate;
pub mod turn_config;
pub mod turn_record;

/// Skills the app ships and rewrites on every launch. They are uneditable and
/// undeletable through the UI, so the check has to be a set rather than a
/// comparison against one name.
pub(crate) const BUILTIN_SKILL_DIRS: &[&str] = &[manual::MANUAL_DIR, diagnostics::DIAGNOSTICS_DIR];

pub(crate) fn is_builtin_skill_dir(dir_name: &str) -> bool {
    BUILTIN_SKILL_DIRS.contains(&dir_name)
}

pub use base_prompt::base_prompt;
pub(crate) use compact::mid_turn_compact;
pub use compact::{CompactCircuitBreaker, CompactCircuitBreakerState, do_compact};
pub(crate) use context::SenderNames;
#[cfg(any(test, feature = "test-support"))]
pub use context::build_messages;
pub use context::{
    build_messages_with_context_items, build_messages_with_senders, microcompact, resolve_file_uris_in_messages,
    resolve_sticker_parts_in_messages, trim_to_context_limit,
};
pub use file_access::{build_file_access, file_access_prompt};
pub(crate) use inline_tag::{InlineHiddenTagParser, InlineTagSpec};
pub(crate) use loop_guard::{LoopVerdict, ToolLoopGuard, loop_abort_message, loop_warning_message};
pub use memory_context::{
    MemoryRequest, memory_budget, persist_injection, plan_injection, plan_injection_async, trailing_with_memory,
};
pub(crate) use memory_context::{MemorySubjectRef, roster_block};
pub use project_instructions::{instruction_budget, load_project_instructions};
pub(crate) use provider_config::without_thinking;
pub use provider_config::{
    ResolvedProvider, TurnParams, TurnParamsResolveRequest, build_tool_secrets, get_provider_api_key,
    provider_secret_name, resolve_provider_config, resolve_turn_params, resolve_with_overrides,
};
pub(crate) use stream::{
    MAX_STREAM_RETRIES, STREAM_RETRY_BASE, StreamResult, is_context_window_error, is_retryable_stream_error,
    parse_retry_after,
};
pub use tokenizer::{TokenBudget, TokenCounter, TokenizerKind};
pub(crate) use tool_calls::serialize_tool_calls_openai;
pub use tool_calls::{extract_tool_calls_from_blocks, parse_stored_tool_calls};
pub(crate) use truncate::{TOOL_OUTPUT_TRUNCATION, formatted_truncate_text};
