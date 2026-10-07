use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Permission, Tool, ToolContext};
use crate::db::entity::memory;
use crate::db::entity::memory::{DeletedBy, GLOBAL_SCOPE_ID, MemoryScope, MemoryType, Origin, Visibility};
use crate::db::sea::cap::Db;
use crate::db::sea::ops::memory as mem_ops;

/// Where a memory tool call reads and writes.
///
/// A conversation attached to a project uses that project's scope. One without a
/// project falls back to the client-wide scope rather than failing: memories used
/// to be refused outright there, so anything the model learned in an unattached
/// conversation was lost the moment the turn ended.
///
/// Never `OnebotGlobal` — that layer belongs to the bot side and is not injected
/// into client conversations, so writing there would store rows nobody reads.
fn get_db_and_scope(context: &ToolContext) -> Result<(&Db, MemoryScope, String), String> {
    let db = context
        .sea
        .as_ref()
        .ok_or("Memory tools are unavailable: no database handle")?;
    match context.project_id.as_ref() {
        Some(pid) => Ok((db, MemoryScope::Project, pid.clone())),
        None => Ok((db, MemoryScope::ClientGlobal, GLOBAL_SCOPE_ID.to_string())),
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
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::AppState,
            loop_handled: false,
            plan_mode: false,
            explore: false,
            reviewer: false,
            parallel: false,
        }
    }

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
        let (db, scope, scope_id) = get_db_and_scope(context)?;
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
        let memory_type = MemoryType::parse(args.get("memory_type").and_then(|v| v.as_str()).unwrap_or("general"))?;

        let now = crate::util::now_ms();
        let row = memory::Model {
            id: uuid::Uuid::new_v4().to_string(),
            scope_type: scope,
            scope_id: scope_id.clone(),
            key: key.clone(),
            content,
            memory_type,
            subject_scope_id: None,
            origin: Origin::Desktop,
            visibility: Visibility::Normal,
            source_session_id: None,
            deleted_at: None,
            deleted_by: None,
            created_at: now,
            updated_at: now,
        };
        // Length and quota live in ops so this path and the IPC path cannot
        // disagree, and so neither can bypass the other. The lookup that picks
        // the wording runs in the same write, so it describes what the write did.
        let existed = db
            .write(async |tx| {
                let existed = mem_ops::get_memory_by_key(tx, scope, &scope_id, &key).await?.is_some();
                Ok::<_, crate::db::sea::DbErr>(mem_ops::remember(tx, row).await?.map(|_| existed))
            })
            .await
            .map_err(|e| e.to_string())??;

        let where_ = scope_label(scope);
        if existed {
            Ok(format!("Updated memory '{key}' for {where_}."))
        } else {
            Ok(format!("Saved memory '{key}' for {where_}."))
        }
    }
}

pub struct RecallMemoryTool;

#[async_trait]
impl Tool for RecallMemoryTool {
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::Read,
            loop_handled: false,
            plan_mode: true,
            explore: true,
            reviewer: false,
            parallel: true,
        }
    }

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
        let (db, scope, scope_id) = get_db_and_scope(context)?;
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: key")?
            .to_string();

        match mem_ops::get_memory_by_key(db, scope, &scope_id, &key)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(m) => Ok(format!("[{}] {}: {}", m.memory_type.as_str(), m.key, m.content)),
            None => Ok(format!("No memory found for key '{key}'.")),
        }
    }
}

pub struct ListMemoriesTool;

#[async_trait]
impl Tool for ListMemoriesTool {
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::Read,
            loop_handled: false,
            plan_mode: true,
            explore: true,
            reviewer: false,
            parallel: true,
        }
    }

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
        let (db, scope, scope_id) = get_db_and_scope(context)?;

        let memories = mem_ops::list_by_scope(db, scope, &scope_id)
            .await
            .map_err(|e| e.to_string())?;

        if memories.is_empty() {
            return Ok("No memories stored.".to_string());
        }

        let mut out = format!("{} memories:\n", memories.len());
        for m in &memories {
            let preview: String = m.content.chars().take(80).collect();
            let ellipsis = if m.content.len() > 80 { "..." } else { "" };
            out.push_str(&format!(
                "- [{}] {}: {}{}\n",
                m.memory_type.as_str(),
                m.key,
                preview,
                ellipsis
            ));
        }
        Ok(out)
    }
}

pub struct DeleteMemoryTool;

#[async_trait]
impl Tool for DeleteMemoryTool {
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::AppState,
            loop_handled: false,
            plan_mode: false,
            explore: false,
            reviewer: false,
            parallel: false,
        }
    }

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
        let (db, scope, scope_id) = get_db_and_scope(context)?;
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: key")?
            .to_string();

        // Soft delete, like every other delete path, so the row stays
        // recoverable from the trash. The lookup and the delete are one write.
        let deleted = db
            .write(async |tx| {
                let Some(existing) = mem_ops::get_memory_by_key(tx, scope, &scope_id, &key).await? else {
                    return Ok(false);
                };
                mem_ops::soft_delete_memories(tx, &[existing.id], DeletedBy::Admin, crate::util::now_ms())
                    .await
                    .map(|n| n > 0)
            })
            .await
            .map_err(|e| e.to_string())?;
        if deleted {
            Ok(format!("Deleted memory '{key}'."))
        } else {
            Ok(format!("No memory found for key '{key}'."))
        }
    }
}
