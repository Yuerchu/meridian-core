//! Tool-result image carry mixin for chat protocols that do not allow images
//! directly inside `role: "tool"` messages (OpenAI Chat Completions, DeepSeek,
//! etc.).

pub type ToolImageCarryMixin = meridian_provider_sdk::chat::ToolImageCarry;
