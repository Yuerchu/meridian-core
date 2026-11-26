//! Cross-protocol and non-GCD capability mixins.
//!
//! Naming follows Meridian / foxline conventions:
//! - Sampling parameters are protocol-specific capabilities (never merged across
//!   vendors and never placed on base GCD structs where a variant like Codex or
//!   DeepSeek forbids them).
//! - Cross-protocol auth & transport helpers (`VertexServiceAccountAuthMixin`,
//!   `SseStreamMixin`, `ToolImageCarryMixin`) live here so multiple protocol
//!   families can compose them without duplicating code.

pub mod sampling;
pub mod sse;
pub mod tool_image_carry;
pub mod vertex_auth;

pub use sampling::{AnthropicSamplingParamsMixin, GoogleSamplingParamsMixin, OpenAISamplingParamsMixin};
pub use sse::SseStreamMixin;
pub use tool_image_carry::ToolImageCarryMixin;
pub use vertex_auth::{VertexServiceAccountAuthMixin, parse_vertex_service_account};
