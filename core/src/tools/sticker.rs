use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Permission, Tool, ToolContext};

fn context_parts(context: &ToolContext) -> Result<(crate::db::DbPool, String), String> {
    let pool = context
        .db_pool
        .clone()
        .ok_or("Sticker tools require a database context")?;
    let assistant_id = context
        .assistant_id
        .clone()
        .ok_or("Sticker tools require an active assistant")?;
    Ok((pool, assistant_id))
}

fn assigned_sticker(
    conn: &mut diesel::SqliteConnection,
    assistant_id: &str,
    sticker_id: &str,
) -> Result<crate::db::models::emoji::EmojiRow, String> {
    let sticker = crate::db::ops::emoji::get_emoji(conn, sticker_id).map_err(|_| "Unknown sticker id".to_string())?;
    let packs = crate::db::ops::emoji_pack::list_assigned_pack_ids(conn, assistant_id).map_err(|e| e.to_string())?;
    if sticker.semantic_status != "confirmed" || !packs.contains(&sticker.pack_id) {
        return Err("That sticker is not in this assistant's confirmed roster".into());
    }
    Ok(sticker)
}

pub struct ListStickersTool;

#[async_trait]
impl Tool for ListStickersTool {
    fn name(&self) -> &str {
        "list_stickers"
    }

    fn description(&self) -> &str {
        "List the confirmed stickers this assistant may send. The roster is dynamic; use the returned id with send_sticker."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Optional name/tag filter" }
            }
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let (pool, assistant_id) = context_parts(context)?;
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_lowercase();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            let pack_ids = crate::db::ops::emoji_pack::list_assigned_pack_ids(&mut conn, &assistant_id)
                .map_err(|e| e.to_string())?;
            let stickers =
                crate::db::ops::emoji::list_confirmed_for_packs(&mut conn, &pack_ids).map_err(|e| e.to_string())?;
            let items: Vec<Value> = stickers
                .into_iter()
                .filter(|sticker| {
                    query.is_empty()
                        || format!("{} {}", sticker.name, sticker.tags.as_deref().unwrap_or(""))
                            .to_lowercase()
                            .contains(&query)
                })
                .take(100)
                .map(|sticker| {
                    json!({
                        "sticker_id": sticker.id,
                        "name": sticker.name,
                        "tags": sticker.tags,
                    })
                })
                .collect();
            serde_json::to_string(&items).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

pub struct SendStickerTool {
    sent_turns: Mutex<HashSet<String>>,
}

impl SendStickerTool {
    pub fn new() -> Self {
        Self {
            sent_turns: Mutex::new(HashSet::new()),
        }
    }
}

#[async_trait]
impl Tool for SendStickerTool {
    fn name(&self) -> &str {
        "send_sticker"
    }

    fn description(&self) -> &str {
        "Send one confirmed sticker as a separate visual message part. At most one call may succeed per assistant turn; text may accompany it."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sticker_id": { "type": "string", "description": "Exact id returned by list_stickers" },
                "description": super::description_property(),
            },
            "required": ["sticker_id"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let sticker_id = args
            .get("sticker_id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or("Missing required parameter: sticker_id")?
            .to_string();
        let turn_id = context.turn_id.clone().ok_or("Sticker send requires a turn context")?;
        let (pool, assistant_id) = context_parts(context)?;
        let sticker = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            assigned_sticker(&mut conn, &assistant_id, &sticker_id)
        })
        .await
        .map_err(|e| e.to_string())??;

        let mut sent = self.sent_turns.lock().unwrap_or_else(|e| e.into_inner());
        if sent.contains(&turn_id) {
            return Err("A sticker has already been sent in this turn".into());
        }
        if sent.len() >= 4096 {
            sent.clear();
        }
        sent.insert(turn_id);
        serde_json::to_string(&json!({ "sticker_id": sticker.id, "name": sticker.name })).map_err(|e| e.to_string())
    }
}
