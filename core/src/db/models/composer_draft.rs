use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::schema::composer_drafts;

/// Which composer a draft belongs to.
///
/// There is one composer per conversation and one on the welcome screen, and
/// nothing else types into the database. Kept as an enum rather than an
/// `Option<&str>` so the key the row is stored under is derived in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftSlot {
    /// The welcome composer, before its first send creates a conversation.
    NewConversation,
    Conversation(String),
}

impl DraftSlot {
    /// `None` is the welcome composer — the caller's "no conversation yet".
    pub fn for_conversation(conversation_id: Option<String>) -> Self {
        match conversation_id {
            Some(id) => Self::Conversation(id),
            None => Self::NewConversation,
        }
    }

    /// The primary key. The migration's CHECK pins the same two spellings.
    pub fn key(&self) -> String {
        match self {
            Self::NewConversation => "new".to_string(),
            Self::Conversation(id) => format!("conversation:{id}"),
        }
    }

    pub fn conversation_id(&self) -> Option<&str> {
        match self {
            Self::NewConversation => None,
            Self::Conversation(id) => Some(id),
        }
    }
}

/// An attachment that can be opened again by path after a restart.
///
/// The JSON shape of one element of `composer_drafts.attachments`. Strict both
/// ways: an unknown member in a stored row is a decode error, not something to
/// skip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftAttachment {
    pub path: String,
    pub name: String,
}

/// What a composer holds, decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposerDraftContent {
    pub body: String,
    pub attachments: Vec<DraftAttachment>,
    pub conversation_refs: Vec<String>,
    pub sticker_id: Option<String>,
}

impl ComposerDraftContent {
    /// Nothing worth keeping. An empty draft is stored as no row at all, so
    /// "is there a draft" and "does a row exist" are the same question.
    ///
    /// Whitespace counts as content: it is what is in the field, and a draft
    /// that comes back without the blank line somebody was about to type after
    /// would be a draft that changed on its own.
    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
            && self.attachments.is_empty()
            && self.conversation_refs.is_empty()
            && self.sticker_id.is_none()
    }
}

#[derive(Debug, Clone, Queryable, Selectable, Identifiable)]
#[diesel(table_name = composer_drafts)]
#[diesel(primary_key(slot))]
pub struct ComposerDraftRow {
    pub slot: String,
    pub conversation_id: Option<String>,
    pub body: String,
    /// JSON array of [`DraftAttachment`]; decode with [`ComposerDraftRow::content`].
    pub attachments: String,
    /// JSON array of conversation ids.
    pub conversation_refs: String,
    pub sticker_id: Option<String>,
    pub revision: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl ComposerDraftRow {
    /// The typed form of the row. A column that does not decode is an error:
    /// the draft is shown back to the person who typed it, and guessing at a
    /// part of it would be showing them something they did not write.
    pub fn content(&self) -> Result<ComposerDraftContent, String> {
        let attachments = serde_json::from_str(&self.attachments)
            .map_err(|e| format!("composer_drafts.attachments for {} is not valid: {e}", self.slot))?;
        let conversation_refs = serde_json::from_str(&self.conversation_refs)
            .map_err(|e| format!("composer_drafts.conversation_refs for {} is not valid: {e}", self.slot))?;
        Ok(ComposerDraftContent {
            body: self.body.clone(),
            attachments,
            conversation_refs,
            sticker_id: self.sticker_id.clone(),
        })
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = composer_drafts)]
pub struct ComposerDraftInsert<'a> {
    pub slot: &'a str,
    pub conversation_id: Option<&'a str>,
    pub body: &'a str,
    pub attachments: &'a str,
    pub conversation_refs: &'a str,
    pub sticker_id: Option<&'a str>,
    pub revision: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, AsChangeset)]
#[diesel(table_name = composer_drafts)]
#[diesel(treat_none_as_null = true)]
pub struct ComposerDraftChangeset<'a> {
    pub body: &'a str,
    pub attachments: &'a str,
    pub conversation_refs: &'a str,
    pub sticker_id: Option<&'a str>,
    pub revision: i64,
    pub updated_at: i64,
}
