use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Permission, Tool, ToolContext};
use crate::db::entity::todo_item::ItemStatus;
use crate::db::sea::cap::Db;
use crate::db::sea::ops::todo::TodoItemSpec;

const MAX_ITEMS: usize = 50;
const MAX_CONTENT_LEN: usize = 200;

fn get_db_and_conversation(context: &ToolContext) -> Result<(Db, String), String> {
    let db = context
        .db
        .as_ref()
        .ok_or("The todo list requires a conversation context")?
        .clone();
    let conversation_id = context
        .conversation_id
        .as_ref()
        .ok_or("The todo list requires a conversation context")?
        .clone();
    Ok((db, conversation_id))
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
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::AppState,
            loop_handled: false,
            plan_mode: true,
            explore: false,
            reviewer: false,
            parallel: false,
        }
    }

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
        let (db, conversation_id) = get_db_and_conversation(context)?;

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

        let now = crate::util::now_ms();
        let (view, retired_plans) = db
            .write(async |tx| {
                crate::db::sea::ops::todo::replace_active_list_with_plan_completion(
                    tx,
                    &conversation_id,
                    &title,
                    &items,
                    now,
                )
                .await
            })
            .await
            .map_err(|e| e.to_string())?;

        let total = view.items.len();
        let done = view.items.iter().filter(|i| i.status == ItemStatus::Completed).count();

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
        match view.items.iter().find(|i| i.status == ItemStatus::InProgress) {
            Some(current) => Ok(format!(
                "Checklist \"{title}\" updated ({done}/{total} done). Now: {}",
                current.content
            )),
            None => Ok(format!(
                "Checklist \"{title}\" updated ({done}/{total} done). No step is marked in_progress."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{FileAccess, ShellType};

    fn ctx(sea: Db, conversation_id: &str) -> ToolContext {
        ToolContext {
            working_directory: None,
            shell: ShellType::Bash,
            file_access: FileAccess::Unrestricted,
            project_id: None,
            conversation_id: Some(conversation_id.to_string()),
            turn_id: Some("t1".into()),
            assistant_id: None,
            db: Some(sea),
            #[cfg(not(target_os = "android"))]
            sandbox_policy: crate::sandbox::CommandSandbox::UNCONFINED,
            #[cfg(not(target_os = "android"))]
            background: None,
            tool_secrets: std::collections::HashMap::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
            journal: None,
        }
    }

    /// A database holding conversation `c1`.
    async fn shared() -> Db {
        let sea = crate::db::sea::sea_test_db().await;
        crate::db::sea::execute_for_tests(
            &sea,
            "INSERT INTO conversations (id, created_at, updated_at) VALUES ('c1', 1, 1)",
        )
        .await
        .unwrap();
        sea
    }

    async fn active(sea: &Db) -> Option<crate::db::sea::ops::todo::TodoListView> {
        sea.read(async |tx| crate::db::sea::ops::todo::get_active_view(tx, "c1").await)
            .await
            .unwrap()
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
        let sea = shared().await;
        let ctx = ctx(sea.clone(), "c1");

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

        assert_eq!(active(&sea).await.unwrap().items.len(), 2);
    }

    #[tokio::test]
    async fn rejects_two_steps_in_progress() {
        let sea = shared().await;
        let ctx = ctx(sea.clone(), "c1");

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
        assert_eq!(active(&sea).await, None);
    }

    #[tokio::test]
    async fn rejects_blank_and_unknown_fields() {
        let sea = shared().await;
        let ctx = ctx(sea.clone(), "c1");

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
        let sea = shared().await;
        let ctx = ctx(sea.clone(), "c1");
        crate::db::sea::execute_for_tests(
            &sea,
            "INSERT INTO mode_artifacts (id, conversation_id, kind, content, status, created_at, updated_at)
                 VALUES ('p1', 'c1', 'plan', 'the plan', 'approved', 1, 2)",
        )
        .await
        .unwrap();

        UpdateTodosTool
            .execute(json!({ "title": "Ship it", "todos": [step("a", "in_progress")] }), &ctx)
            .await
            .unwrap();
        assert!(
            crate::db::sea::ops::plan::get_active(&sea, "c1")
                .await
                .unwrap()
                .is_some(),
            "still in force while work is outstanding"
        );

        let out = UpdateTodosTool
            .execute(json!({ "title": "Ship it", "todos": [step("a", "completed")] }), &ctx)
            .await
            .unwrap();

        assert_eq!(crate::db::sea::ops::plan::get_active(&sea, "c1").await.unwrap(), None);
        assert!(out.contains("no longer in force"), "{out}");
    }

    #[tokio::test]
    async fn reports_completion_when_every_step_is_done() {
        let sea = shared().await;
        let ctx = ctx(sea.clone(), "c1");

        let out = UpdateTodosTool
            .execute(
                json!({ "title": "Refactor auth", "todos": [step("a", "completed")] }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(out.contains("finished (1/1)"), "{out}");
    }

    /// The loop re-derives the block from the database at the start of every
    /// turn and freezes it into the history when it changed
    /// (`agent::todo_context`), which is what carries the checklist across a
    /// compaction. This walks the same path: call the tool, then read the block
    /// back.
    #[tokio::test]
    async fn the_prompt_block_follows_the_tool_across_calls() {
        let sea = shared().await;
        let ctx = ctx(sea.clone(), "c1");

        let block_now = async || {
            active(&sea)
                .await
                .as_ref()
                .and_then(crate::db::sea::ops::todo::format_todo_block)
        };

        assert!(block_now().await.is_none(), "nothing to inject before the first call");

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
        let first = block_now().await.unwrap();
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
        let second = block_now().await.unwrap();
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
        assert!(block_now().await.is_none(), "a finished checklist stops being injected");
    }

    #[tokio::test]
    async fn without_a_conversation_it_says_so_instead_of_panicking() {
        let mut ctx = ctx(crate::db::sea::sea_test_db().await, "c1");
        ctx.conversation_id = None;

        let err = UpdateTodosTool
            .execute(json!({ "title": "T", "todos": [step("a", "pending")] }), &ctx)
            .await
            .unwrap_err();

        assert!(err.contains("conversation context"), "{err}");
    }
}
