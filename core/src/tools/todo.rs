use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Permission, Tool, ToolContext};
use crate::db::models::todo::ItemStatus;
use crate::db::ops::todo::TodoItemSpec;

const MAX_ITEMS: usize = 50;
const MAX_CONTENT_LEN: usize = 200;

fn get_pool_and_conversation(context: &ToolContext) -> Result<(crate::db::DbPool, String), String> {
    let pool = context
        .db_pool
        .as_ref()
        .ok_or("The todo list requires a conversation context")?
        .clone();
    let conversation_id = context
        .conversation_id
        .as_ref()
        .ok_or("The todo list requires a conversation context")?
        .clone();
    Ok((pool, conversation_id))
}

/// Pull one step out of the model's payload, rejecting anything the checklist
/// cannot represent. Errors here go straight back to the model, so they name
/// the offending position and say what a valid entry looks like.
fn parse_item(idx: usize, raw: &Value) -> Result<TodoItemSpec, String> {
    let position = idx + 1;
    let obj = raw
        .as_object()
        .ok_or_else(|| format!("todos[{position}] must be an object"))?;

    let field = |name: &str| -> Result<String, String> {
        let value = obj
            .get(name)
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("todos[{position}] is missing '{name}'"))?
            .trim();
        if value.is_empty() {
            return Err(format!("todos[{position}].{name} must not be empty"));
        }
        if value.chars().count() > MAX_CONTENT_LEN {
            return Err(format!(
                "todos[{position}].{name} exceeds {MAX_CONTENT_LEN} characters; keep steps short"
            ));
        }
        Ok(value.to_string())
    };

    let content = field("content")?;
    let active_form = field("active_form")?;
    let status = obj
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("todos[{position}] is missing 'status'"))?;
    let status = ItemStatus::parse(status).map_err(|e| format!("todos[{position}]: {e}"))?;

    Ok(TodoItemSpec {
        content,
        active_form,
        status,
    })
}

pub struct UpdateTodosTool;

#[async_trait]
impl Tool for UpdateTodosTool {
    fn name(&self) -> &str {
        "update_todos"
    }

    fn description(&self) -> &str {
        "Record the checklist for the work you are doing, and keep it current as you go. \
         Send the entire list on every call — it replaces the stored one, so omitting a step \
         deletes it. Give each step a 'content' (imperative: \"Add the migration\") and an \
         'active_form' (present continuous: \"Adding the migration\"); the interface shows the \
         latter while that step runs. Keep exactly one step 'in_progress'. Passing a different \
         'title' retires the current checklist and opens a new one, so reuse the same title for \
         the whole of a piece of work."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Short name for this piece of work, e.g. 'Refactor auth module'. Reuse it across updates; a new title starts a new checklist."
                },
                "todos": {
                    "type": "array",
                    "description": "The complete list of steps, in the order they will be done.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "The step in imperative form, e.g. 'Run the tests'"
                            },
                            "active_form": {
                                "type": "string",
                                "description": "The same step in present continuous form, e.g. 'Running the tests'"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Step state. Exactly one step should be in_progress until the work is done."
                            }
                        },
                        "required": ["content", "active_form", "status"]
                    }
                }
            },
            "required": ["title", "todos"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let (pool, conversation_id) = get_pool_and_conversation(context)?;

        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: title")?
            .trim()
            .to_string();
        if title.is_empty() {
            return Err("title must not be empty".to_string());
        }
        if title.chars().count() > MAX_CONTENT_LEN {
            return Err(format!("title exceeds {MAX_CONTENT_LEN} characters"));
        }

        let raw_todos = args
            .get("todos")
            .and_then(|v| v.as_array())
            .ok_or("Missing required parameter: todos")?;
        if raw_todos.is_empty() {
            return Err("todos must contain at least one step".to_string());
        }
        if raw_todos.len() > MAX_ITEMS {
            return Err(format!(
                "todos contains {} steps; at most {MAX_ITEMS} are allowed. Group the work more coarsely.",
                raw_todos.len()
            ));
        }

        let items = raw_todos
            .iter()
            .enumerate()
            .map(|(idx, raw)| parse_item(idx, raw))
            .collect::<Result<Vec<_>, _>>()?;

        // The one invariant worth enforcing in code: a checklist with two
        // things "in progress" tells the user nothing about what is happening.
        let in_progress: Vec<&str> = items
            .iter()
            .filter(|i| i.status == ItemStatus::InProgress)
            .map(|i| i.content.as_str())
            .collect();
        if in_progress.len() > 1 {
            return Err(format!(
                "Only one step may be in_progress at a time, but {} are: {}. \
                 Mark the others pending or completed.",
                in_progress.len(),
                in_progress.join(", ")
            ));
        }

        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            let now = crate::util::now_ms();
            let (view, retired_plans) = crate::db::ops::todo::replace_active_list_with_plan_completion(
                &mut conn,
                &conversation_id,
                &title,
                &items,
                now,
            )
            .map_err(|e| e.to_string())?;

            let total = view.items.len();
            let done = view
                .items
                .iter()
                .filter(|i| i.status == ItemStatus::Completed.as_str())
                .count();

            if done == total {
                let plan_note = if retired_plans > 0 {
                    " The approved plan is complete and no longer in force."
                } else {
                    ""
                };
                return Ok(format!(
                    "Checklist \"{title}\" finished ({done}/{total}). The next update starts a new one.{plan_note}"
                ));
            }
            match view.items.iter().find(|i| i.status == ItemStatus::InProgress.as_str()) {
                Some(current) => Ok(format!(
                    "Checklist \"{title}\" updated ({done}/{total} done). Now: {}",
                    current.content
                )),
                None => Ok(format!(
                    "Checklist \"{title}\" updated ({done}/{total} done). No step is marked in_progress."
                )),
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbPool;
    use crate::tools::{FileAccess, ShellType};

    fn ctx(pool: DbPool, conversation_id: &str) -> ToolContext {
        ToolContext {
            working_directory: None,
            shell: ShellType::Bash,
            file_access: FileAccess::Unrestricted,
            project_id: None,
            conversation_id: Some(conversation_id.to_string()),
            turn_id: Some("t1".into()),
            assistant_id: None,
            db_pool: Some(pool),
            #[cfg(not(target_os = "android"))]
            sandbox_policy: None,
            tool_secrets: std::collections::HashMap::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
            journal: None,
        }
    }

    fn seed_conversation(pool: &DbPool, id: &str) {
        use crate::db::schema::conversations;
        use diesel::prelude::*;
        let mut conn = pool.get().unwrap();
        diesel::insert_into(conversations::table)
            .values((
                conversations::id.eq(id),
                conversations::created_at.eq(1),
                conversations::updated_at.eq(1),
            ))
            .execute(&mut conn)
            .unwrap();
    }

    fn step(content: &str, status: &str) -> Value {
        json!({
            "content": content,
            "active_form": format!("Doing {content}"),
            "status": status,
        })
    }

    #[tokio::test]
    async fn writes_the_checklist_and_reports_the_current_step() {
        let pool = crate::db::test_db();
        seed_conversation(&pool, "c1");
        let ctx = ctx(pool.clone(), "c1");

        let out = UpdateTodosTool
            .execute(
                json!({
                    "title": "Refactor auth",
                    "todos": [step("Extract token check", "in_progress"), step("Add tests", "pending")],
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(out.contains("0/2 done"), "{out}");
        assert!(out.contains("Now: Extract token check"), "{out}");

        let mut conn = pool.get().unwrap();
        let view = crate::db::ops::todo::get_active_view(&mut conn, "c1").unwrap().unwrap();
        assert_eq!(view.items.len(), 2);
    }

    #[tokio::test]
    async fn rejects_two_steps_in_progress() {
        let pool = crate::db::test_db();
        seed_conversation(&pool, "c1");
        let ctx = ctx(pool.clone(), "c1");

        let err = UpdateTodosTool
            .execute(
                json!({
                    "title": "Refactor auth",
                    "todos": [step("a", "in_progress"), step("b", "in_progress")],
                }),
                &ctx,
            )
            .await
            .unwrap_err();

        assert!(err.contains("Only one step may be in_progress"), "{err}");
        // Nothing was written, so the model can retry from a clean slate.
        let mut conn = pool.get().unwrap();
        assert!(
            crate::db::ops::todo::get_active_view(&mut conn, "c1")
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_blank_and_unknown_fields() {
        let pool = crate::db::test_db();
        seed_conversation(&pool, "c1");
        let ctx = ctx(pool.clone(), "c1");

        let blank = UpdateTodosTool
            .execute(json!({ "title": "T", "todos": [step("   ", "pending")] }), &ctx)
            .await
            .unwrap_err();
        assert!(blank.contains("todos[1].content"), "{blank}");

        let bad_status = UpdateTodosTool
            .execute(json!({ "title": "T", "todos": [step("a", "doing")] }), &ctx)
            .await
            .unwrap_err();
        assert!(bad_status.contains("unknown todo status"), "{bad_status}");

        let empty = UpdateTodosTool
            .execute(json!({ "title": "T", "todos": [] }), &ctx)
            .await
            .unwrap_err();
        assert!(empty.contains("at least one step"), "{empty}");
    }

    /// The only signal that a plan's work is over. Without it the approved plan
    /// stays in the system prompt for the rest of the conversation.
    #[tokio::test]
    async fn finishing_the_checklist_retires_the_approved_plan() {
        let pool = crate::db::test_db();
        seed_conversation(&pool, "c1");
        let ctx = ctx(pool.clone(), "c1");
        {
            let mut conn = pool.get().unwrap();
            let plan = crate::db::ops::plan::record_plan(&mut conn, "c1", "the plan", 1).unwrap();
            crate::db::ops::plan::approve(&mut conn, &plan.id, 2).unwrap();
        }

        UpdateTodosTool
            .execute(json!({ "title": "Ship it", "todos": [step("a", "in_progress")] }), &ctx)
            .await
            .unwrap();
        {
            let mut conn = pool.get().unwrap();
            assert!(
                crate::db::ops::plan::get_active(&mut conn, "c1").unwrap().is_some(),
                "still in force while work is outstanding"
            );
        }

        let out = UpdateTodosTool
            .execute(json!({ "title": "Ship it", "todos": [step("a", "completed")] }), &ctx)
            .await
            .unwrap();

        let mut conn = pool.get().unwrap();
        assert!(crate::db::ops::plan::get_active(&mut conn, "c1").unwrap().is_none());
        assert!(out.contains("no longer in force"), "{out}");
    }

    #[tokio::test]
    async fn reports_completion_when_every_step_is_done() {
        let pool = crate::db::test_db();
        seed_conversation(&pool, "c1");
        let ctx = ctx(pool.clone(), "c1");

        let out = UpdateTodosTool
            .execute(
                json!({ "title": "Refactor auth", "todos": [step("a", "completed")] }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(out.contains("finished (1/1)"), "{out}");
    }

    /// The loop re-derives the prompt block from the database every turn, which
    /// is what carries the checklist across a compaction. This walks the same
    /// path the chat command takes: call the tool, then read the block back.
    #[tokio::test]
    async fn the_prompt_block_follows_the_tool_across_calls() {
        let pool = crate::db::test_db();
        seed_conversation(&pool, "c1");
        let ctx = ctx(pool.clone(), "c1");

        let block_now = || {
            let mut conn = pool.get().unwrap();
            crate::db::ops::todo::get_active_view(&mut conn, "c1")
                .unwrap()
                .as_ref()
                .and_then(crate::db::ops::todo::format_todo_block)
        };

        assert!(block_now().is_none(), "nothing to inject before the first call");

        UpdateTodosTool
            .execute(
                json!({
                    "title": "Refactor auth",
                    "todos": [step("Extract token check", "in_progress"), step("Add tests", "pending")],
                }),
                &ctx,
            )
            .await
            .unwrap();
        let first = block_now().unwrap();
        assert!(first.contains("Title: Refactor auth"));
        assert!(first.contains("1. [in_progress] Extract token check"));
        assert!(first.contains("2. [pending] Add tests"));

        UpdateTodosTool
            .execute(
                json!({
                    "title": "Refactor auth",
                    "todos": [step("Extract token check", "completed"), step("Add tests", "in_progress")],
                }),
                &ctx,
            )
            .await
            .unwrap();
        let second = block_now().unwrap();
        assert!(second.contains("1. [completed] Extract token check"));
        assert!(second.contains("2. [in_progress] Add tests"));

        UpdateTodosTool
            .execute(
                json!({
                    "title": "Refactor auth",
                    "todos": [step("Extract token check", "completed"), step("Add tests", "completed")],
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(block_now().is_none(), "a finished checklist stops being injected");
    }

    #[tokio::test]
    async fn without_a_conversation_it_says_so_instead_of_panicking() {
        let pool = crate::db::test_db();
        let mut ctx = ctx(pool, "c1");
        ctx.conversation_id = None;

        let err = UpdateTodosTool
            .execute(json!({ "title": "T", "todos": [step("a", "pending")] }), &ctx)
            .await
            .unwrap_err();

        assert!(err.contains("conversation context"), "{err}");
    }
}
