//! The SeaORM ops, one module per table, filled in as transaction roots move
//! over. A module here that shares a name with one in `db/ops` is a dual
//! implementation while Diesel callers remain; `docs/dual-impl.md` lists each
//! such pair with its remaining Diesel callers, and the checker keeps the two
//! in step.

pub mod assistant;
pub mod cached_model;
pub mod composer_draft;
pub mod conversation;
pub mod custom_tool;
pub mod emoji;
pub mod emoji_pack;
pub mod journal;
pub mod mcp_server;
pub mod memory;
pub mod notification;
pub mod plan_review;
pub mod preference;
pub mod provider;
pub mod redaction_rule;
pub mod skill;
pub mod skill_binding;
pub mod tool_category;
pub mod tool_preset;
pub mod voice_corpus;
