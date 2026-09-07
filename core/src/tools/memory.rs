use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Permission, Tool, ToolContext};
use crate::db::models::memory::{GLOBAL_SCOPE_ID, MemoryInsert, MemoryScope, Origin, Visibility};

/// Where a memory tool call reads and writes.
///
/// A conversation attached to a project uses that project's scope. One without a
/// project falls back to the client-wide scope rather than failing: memories used
/// to be refused outright there, so anything the model learned in an unattached
/// conversation was lost the moment the turn ended.
///
/// Never `OnebotGlobal` — that layer belongs to the bot side and is not injected
/// into client conversations, so writing there would store rows nobody reads.
fn get_pool_and_scope(context: &ToolContext) -> Result<(crate::db::DbPool, MemoryScope, String), String> {
    let pool = context
        .db_pool
        .as_ref()
        .ok_or("Memory tools are unavailable: no database handle")?
        .clone();
    match context.project_id.as_ref() {
        Some(pid) => Ok((pool, MemoryScope::Project, pid.clone())),
        None => Ok((pool, MemoryScope::ClientGlobal, GLOBAL_SCOPE_ID.to_string())),
    }
}

/// Names the scope in tool output so the model — and the user reading the tool
/// card — can tell a project memory from a client-wide one.
fn scope_label(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Project => "this project",
        _ => "all conversations in this app",
    }
}

pub struct SaveMemoryTool;

#[async_trait]
impl Tool for SaveMemoryTool {
    fn name(&self) -> &str {
        "save_memory"
    }

    fn description(&self) -> &str {
        "Save or update a persistent memory. Memories persist across conversations and are automatically injected into your context. Scope is implicit: in a conversation belonging to a project the memory is stored for that project, otherwise it is stored app-wide."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "A short, descriptive key for this memory (e.g. 'user_preference_language', 'important_dates')"
                },
                "content": {
                    "type": "string",
                    "description": "The memory content to store"
                },
                "memory_type": {
                    "type": "string",
                    "enum": ["general", "preference", "fact", "instruction"],
                    "description": "Type of memory. Defaults to 'general'"
                }
            },
            "required": ["key", "content"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let (pool, scope, scope_id) = get_pool_and_scope(context)?;
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: key")?
            .to_string();
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: content")?
            .to_string();
        let memory_type = args
            .get("memory_type")
            .and_then(|v| v.as_str())
            .unwrap_or("general")
            .to_string();

        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;

            // Length and quota live in ops so this path and the IPC path cannot
            // disagree, and so neither can bypass the other.
            crate::db::ops::memory::validate_memory(&mut conn, scope, &scope_id, &key, &content)?;
            let existing = crate::db::ops::memory::get_memory_by_key(&mut conn, scope, &scope_id, &key)
                .map_err(|e| e.to_string())?;

            let id = uuid::Uuid::new_v4().to_string();
            let now = crate::util::now_ms();
            crate::db::ops::memory::upsert_memory(
                &mut conn,
                &MemoryInsert {
                    id: &id,
                    scope_type: scope.as_str(),
                    scope_id: &scope_id,
                    key: &key,
                    content: &content,
                    memory_type: &memory_type,
                    subject_scope_id: None,
                    origin: Origin::Desktop.as_str(),
                    visibility: Visibility::Normal.as_str(),
                    source_session_id: None,
                    created_at: now,
                    updated_at: now,
                },
            )
            .map_err(|e| e.to_string())?;

            let where_ = scope_label(scope);
            if existing.is_some() {
                Ok(format!("Updated memory '{key}' for {where_}."))
            } else {
                Ok(format!("Saved memory '{key}' for {where_}."))
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

pub struct RecallMemoryTool;

#[async_trait]
impl Tool for RecallMemoryTool {
    fn name(&self) -> &str {
        "recall_memory"
    }

    fn description(&self) -> &str {
        "Recall a specific memory by key. Looks in the current project's memories, or the app-wide ones when the conversation has no project."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key of the memory to recall"
                }
            },
            "required": ["key"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let (pool, scope, scope_id) = get_pool_and_scope(context)?;
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: key")?
            .to_string();

        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            match crate::db::ops::memory::get_memory_by_key(&mut conn, scope, &scope_id, &key)
                .map_err(|e| e.to_string())?
            {
                Some(m) => Ok(format!("[{}] {}: {}", m.memory_type, m.key, m.content)),
                None => Ok(format!("No memory found for key '{key}'.")),
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

pub struct ListMemoriesTool;

#[async_trait]
impl Tool for ListMemoriesTool {
    fn name(&self) -> &str {
        "list_memories"
    }

    fn description(&self) -> &str {
        "List stored memories for the current project, or the app-wide ones when the conversation has no project."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, _args: Value, context: &ToolContext) -> Result<String, String> {
        let (pool, scope, scope_id) = get_pool_and_scope(context)?;

        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            let memories =
                crate::db::ops::memory::list_by_scope(&mut conn, scope, &scope_id).map_err(|e| e.to_string())?;

            if memories.is_empty() {
                return Ok("No memories stored.".to_string());
            }

            let mut out = format!("{} memories:\n", memories.len());
            for m in &memories {
                let preview: String = m.content.chars().take(80).collect();
                let ellipsis = if m.content.len() > 80 { "..." } else { "" };
                out.push_str(&format!("- [{}] {}: {}{}\n", m.memory_type, m.key, preview, ellipsis));
            }
            Ok(out)
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

pub struct DeleteMemoryTool;

#[async_trait]
impl Tool for DeleteMemoryTool {
    fn name(&self) -> &str {
        "delete_memory"
    }

    fn description(&self) -> &str {
        "Delete a memory by key from the current project, or from the app-wide ones when the conversation has no project."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key of the memory to delete"
                }
            },
            "required": ["key"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let (pool, scope, scope_id) = get_pool_and_scope(context)?;
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: key")?
            .to_string();

        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            let existing = crate::db::ops::memory::get_memory_by_key(&mut conn, scope, &scope_id, &key)
                .map_err(|e| e.to_string())?;
            let Some(existing) = existing else {
                return Ok(format!("No memory found for key '{key}'."));
            };
            // Soft delete, like every other delete path, so the row stays
            // recoverable from the trash.
            crate::db::ops::memory::soft_delete_memories(
                &mut conn,
                &[existing.id],
                crate::db::models::memory::DeletedBy::Admin,
                crate::util::now_ms(),
            )
            .map_err(|e| e.to_string())?;
            Ok(format!("Deleted memory '{key}'."))
        })
        .await
        .map_err(|e| e.to_string())?
    }
}
