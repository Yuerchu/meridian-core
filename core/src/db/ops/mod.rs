pub mod acp_session;
pub mod acp_session_notice;
pub mod assistant;
pub mod audit;
pub mod cached_model;
pub mod conversation;
pub mod emoji;
pub mod emoji_pack;
pub mod memory;
pub mod message;
pub mod message_context_item;
pub mod model_config;
pub mod plan;
pub mod plan_review;
pub mod preference;
pub mod project;
pub mod provider;
pub mod queue;
pub mod queued_prompt_context_item;
pub mod skill_binding;
pub mod todo;
pub mod tool_preset;
pub mod turn;

/// A first-party contract broken by stored content, as a Diesel read error:
/// the strict row-to-model conversions fail the read with it.
pub(crate) fn contract_violation(message: String) -> diesel::result::Error {
    diesel::result::Error::DeserializationError(message.into())
}
