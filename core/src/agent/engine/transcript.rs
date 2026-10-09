//! The rows a turn writes as it runs, and the phase it records while it works.
//!
//! Free functions over the database handle rather than a trait. Both runners
//! write the same rows to the same tables in the same order — there is no
//! second implementation for a trait to abstract over, and one with a single
//! impl would be ceremony that also has to be argued past the `Send` bound on
//! the loop's future.
//!
//! What is worth stating is that these writes fail in different ways, on
//! purpose, and each signature says which:
//!
//! | write | database says no |
//! |---|---|
//! | the assistant placeholder | the turn ends |
//! | filling that row in | the turn ends |
//! | the tool result row | logged, turn continues |
//!
//! Filling the assistant row is the checkpoint before tools may touch the
//! world. The tool result row is deliberate and load-bearing: by the time it
//! runs the tool has already touched the world, so refusing to carry on would
//! cost more than the row does.

use std::future::Future;

use crate::db::entity::message;
use crate::db::models::message::MessageUsage;
use crate::db::models::turn::TurnPhase;
use crate::db::sea::cap::Db;
use crate::db::sea::ops::{audit, message as message_ops};
use crate::db::types::SqlBool;
use crate::util::now_ms;

/// A row of `role` on this turn, with everything an append fills in left
/// blank: the parent is the append's to set and `sort_order` the trigger's.
fn turn_row(id: &str, conversation_id: &str, turn_id: &str, role: &str, content: &str) -> message::Model {
    message::Model {
        id: id.to_owned(),
        conversation_id: conversation_id.to_owned(),
        role: role.to_owned(),
        content: content.to_owned(),
        provider_id: None,
        model_id: None,
        input_tokens: None,
        output_tokens: None,
        tool_calls: None,
        tool_call_id: None,
        sort_order: 0,
        created_at: now_ms(),
        reasoning_content: None,
        rating: None,
        schema_version: 2,
        is_compact_summary: SqlBool::FALSE,
        sender_id: None,
        parent_id: None,
        compact_anchor_id: None,
        source: None,
        turn_id: Some(turn_id.to_owned()),
        tool_outcome: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        provider_name: None,
        provider_state: None,
        auto_review: None,
        server_tool_calls: None,
        tool_diffs: None,
        response_model_id: None,
    }
}

/// Open the row this iteration will stream into, and return its id.
///
/// Hard error, unlike the tool result. Nothing downstream works without it:
/// the `message_start` event names it, every chunk is addressed to it, and the
/// tool rows hang off it.
pub(crate) async fn begin_assistant(
    db: &Db,
    conversation_id: &str,
    turn_id: &str,
    provider: (Option<&str>, Option<&str>),
    model: &str,
    parent: Option<&str>,
) -> Result<String, String> {
    let message_id = uuid::Uuid::new_v4().to_string();
    let row = message::Model {
        // Written when the row is opened, not when it is filled in. The window
        // between the two is the longest in the turn, and a turn that dies
        // inside it still cost the upstream everything it had served —
        // attributing at the end would leave exactly the expensive, interrupted
        // rows belonging to nobody. The foreign key onto `providers` is
        // enforced: every caller has read that row before reaching here, and a
        // provider deleted in between is a broken configuration worth failing on.
        provider_id: provider.0.map(str::to_owned),
        provider_name: provider.1.map(str::to_owned),
        model_id: Some(model.to_owned()),
        ..turn_row(&message_id, conversation_id, turn_id, "assistant", "")
    };
    db.write(async |tx| message_ops::append_message(tx, row, parent).await)
        .await
        .map_err(|e| e.to_string())?;
    Ok(message_id)
}

/// Fill in the row once the model has finished with it, and file its audit
/// copy: this is the first moment it has content and token counts. This is
/// the durable boundary before any tool side effect; a failed fill-in ends the
/// turn, a failed audit copy is logged.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn complete_assistant(
    db: &Db,
    message_id: &str,
    content: &str,
    reasoning: Option<&str>,
    tool_calls_json: Option<&str>,
    provider_state: Option<&str>,
    usage: MessageUsage,
    response_model_id: Option<&str>,
) -> Result<(), String> {
    db.write(async |tx| {
        message_ops::update_assistant_message(
            tx,
            message_id,
            content,
            reasoning,
            tool_calls_json,
            provider_state,
            &usage,
            response_model_id,
        )
        .await?;
        match message_ops::get_message(tx, message_id).await {
            Ok(Some(row)) => {
                if let Err(e) = tx.nested(async |tx| audit::record(tx, &row).await).await {
                    tracing::error!(
                        error = %e,
                        message_id = %message_id,
                        "the audit copy of a reply could not be written",
                    );
                }
            }
            Ok(None) => tracing::error!(message_id = %message_id, "a completed reply vanished before its audit copy"),
            Err(e) => tracing::error!(
                error = %e,
                message_id = %message_id,
                "a completed reply could not be read back for the audit log",
            ),
        }
        Ok::<_, sea_orm::DbErr>(())
    })
    .await
    .map_err(|e| e.to_string())
}

/// Record what a tool returned. `None` means it was not written.
///
/// The caller is expected to leave its parent cursor where it was on `None`,
/// so the next row hangs off the last one that did land and the chain stays
/// intact. The unanswered `tool_call` is stripped from the payload later by
/// `remove_orphan_tool_messages`.
pub(crate) async fn append_tool_result(
    db: &Db,
    conversation_id: &str,
    turn_id: &str,
    call_id: &str,
    content: &str,
    outcome: &'static str,
    parent: Option<&str>,
) -> Option<String> {
    let message_id = uuid::Uuid::new_v4().to_string();
    let row = message::Model {
        tool_call_id: Some(call_id.to_owned()),
        // The same word the event carries, so a reload does not turn a refusal
        // into a green tick with the refusal text sitting in it as the result.
        tool_outcome: Some(outcome.to_owned()),
        ..turn_row(&message_id, conversation_id, turn_id, "tool", content)
    };
    match db
        .write(async |tx| message_ops::append_message(tx, row, parent).await)
        .await
    {
        Ok(_) => Some(message_id),
        Err(e) => {
            tracing::error!("failed to persist tool result: {e}");
            None
        }
    }
}

/// Record something a person said while the turn was already running.
///
/// A user row, not an assistant one, and it belongs to the turn it is steering
/// rather than to the next one — the model reads it in this turn's next request.
/// Fails the same way a tool result does: the message has already been said.
///
/// `role` is what the message *is*: `user` for somebody talking, `context`
/// for a notice this app generated. The two reach the model differently — a
/// notice as `system_context` — and the row has to say the same, because the
/// next turn is built from the rows.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn append_steering(
    db: &Db,
    conversation_id: &str,
    turn_id: &str,
    content: &str,
    sender_id: Option<i64>,
    parent: Option<&str>,
    role: SteeringRole,
    created_at: i64,
) -> Option<String> {
    match write_steering_as(
        db,
        conversation_id,
        turn_id,
        content,
        sender_id,
        parent,
        role,
        created_at,
    )
    .await
    {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::error!("failed to persist steered message: {e}");
            None
        }
    }
}

/// The same write, with the failure handed back instead of logged: for the
/// one caller that has already promised something.
pub async fn write_steering(
    db: &Db,
    conversation_id: &str,
    turn_id: &str,
    content: &str,
    sender_id: Option<i64>,
    parent: Option<&str>,
    created_at: i64,
) -> Result<String, String> {
    write_steering_as(
        db,
        conversation_id,
        turn_id,
        content,
        sender_id,
        parent,
        SteeringRole::User,
        created_at,
    )
    .await
}

/// Who a steered message is from, as far as its row is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteeringRole {
    /// Somebody talking. Stored as `role = 'user'`.
    User,
    /// A notice this app generated. Stored as `role = 'context'`, which is
    /// read back as `system_context` — what it was sent as.
    Context,
}

impl SteeringRole {
    fn as_str(self) -> &'static str {
        match self {
            SteeringRole::User => "user",
            SteeringRole::Context => "context",
        }
    }
}

/// `created_at` is passed in rather than read here because the caller has
/// already built the live `ChatMessage` with it: the row and the message must
/// carry the same instant, or the next turn's replay renders a different
/// `<sent_at>` from the one the provider cached.
#[allow(clippy::too_many_arguments)]
async fn write_steering_as(
    db: &Db,
    conversation_id: &str,
    turn_id: &str,
    content: &str,
    sender_id: Option<i64>,
    parent: Option<&str>,
    role: SteeringRole,
    created_at: i64,
) -> Result<String, String> {
    let message_id = uuid::Uuid::new_v4().to_string();
    let row = message::Model {
        sender_id,
        created_at,
        ..turn_row(&message_id, conversation_id, turn_id, role.as_str(), content)
    };
    db.write(async |tx| message_ops::append_message(tx, row, parent).await)
        .await
        .map_err(|e| e.to_string())?;
    Ok(message_id)
}

/// Run something with the turn recorded as being in a phase, and back to
/// streaming when it returns.
///
/// The phase is written *before* the work, which is the entire point: what is
/// stored when the process dies is where it died. Restoring `Streaming`
/// afterwards matters nearly as much: leaving the phase behind would have a
/// crash a minute later report a tool that finished long ago.
pub async fn in_phase<T>(
    db: &Db,
    turn_id: &str,
    phase: TurnPhase,
    tool: Option<&str>,
    work: impl Future<Output = T>,
) -> T {
    crate::agent::turn_record::note_phase(db, turn_id, phase, tool).await;
    let out = work.await;
    crate::agent::turn_record::note_phase(db, turn_id, TurnPhase::Streaming, None).await;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::turn::TurnStatus;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::turn::TurnOrigin;

    async fn conversation() -> Db {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, title, created_at, updated_at) VALUES ('c1', 't', 1, 1)",
        )
        .await
        .unwrap();
        db.write(async |tx| crate::db::sea::ops::turn::begin(tx, "t1", "c1", TurnOrigin::Desktop, None, 1000).await)
            .await
            .unwrap();
        db
    }

    async fn rows(db: &Db) -> Vec<message::Model> {
        message_ops::list_messages(db, "c1").await.unwrap()
    }

    async fn head(db: &Db) -> Option<String> {
        crate::db::sea::ops::conversation::get_conversation(db, "c1")
            .await
            .unwrap()
            .unwrap()
            .head_message_id
    }

    async fn phase(db: &Db) -> (Option<TurnPhase>, Option<String>) {
        let t = crate::db::sea::ops::turn::get(db, "t1").await.unwrap().unwrap();
        (t.phase, t.phase_tool)
    }

    fn usage() -> MessageUsage {
        MessageUsage {
            input_tokens: Some(7),
            output_tokens: Some(11),
            cache_read_tokens: Some(41),
            cache_write_tokens: Some(43),
            server_tool_calls: Some(47),
        }
    }

    #[tokio::test]
    async fn an_iteration_opens_a_row_and_then_fills_it_in() {
        let db = conversation().await;
        let id = begin_assistant(&db, "c1", "t1", (None, None), "gpt-4.1-mini", None)
            .await
            .unwrap();
        // Empty until the model has finished, which is what makes a `done` turn
        // above an empty row diagnosable as a lost write.
        assert_eq!(rows(&db).await[0].content, "");
        assert_eq!(head(&db).await.as_deref(), Some(id.as_str()));

        complete_assistant(
            &db,
            &id,
            "the answer",
            Some("thinking"),
            None,
            Some(r#"{"version":1,"producer":{"vendor":"anthropic","protocol":"messages","model":"m"},"kind":"anthropic_thinking_signature","payload":{"signature":"sig"}}"#),
            usage(),
            None,
        )
        .await
        .unwrap();

        let row = &rows(&db).await[0];
        assert_eq!(
            (row.content.as_str(), row.reasoning_content.as_deref()),
            ("the answer", Some("thinking"))
        );
        assert!(row.provider_state.as_deref().is_some_and(|s| s.contains("sig")));
        // Distinct values, so a transposed pair cannot pass.
        assert_eq!(
            (
                row.input_tokens,
                row.output_tokens,
                row.cache_read_tokens,
                row.cache_write_tokens,
                row.server_tool_calls
            ),
            (Some(7), Some(11), Some(41), Some(43), Some(47))
        );
        assert_eq!(row.turn_id.as_deref(), Some("t1"));
    }

    /// A reply reaches the audit log when it is finished, not when its row is
    /// opened — the placeholder has neither content nor token counts.
    #[tokio::test]
    async fn a_completed_reply_is_copied_into_the_audit_log() {
        let db = conversation().await;
        let id = begin_assistant(&db, "c1", "t1", (None, None), "m", None).await.unwrap();
        assert!(
            audit::list_recent(&db, 10).await.unwrap().is_empty(),
            "an empty placeholder is not worth recording"
        );
        complete_assistant(&db, &id, "the answer", None, None, None, usage(), None)
            .await
            .unwrap();

        let logged = audit::list_recent(&db, 10).await.unwrap();
        assert_eq!(logged.len(), 1);
        assert_eq!(
            (
                logged[0].message_id.as_str(),
                logged[0].content.as_str(),
                logged[0].role.as_str()
            ),
            (id.as_str(), "the answer", "assistant")
        );
        assert_eq!(
            (logged[0].input_tokens, logged[0].cache_read_tokens),
            (Some(7), Some(41))
        );
        assert_eq!(
            logged[0].turn_origin.as_deref(),
            Some("desktop"),
            "snapshotted off the turn"
        );
    }

    /// The one write that takes the turn down with it.
    #[tokio::test]
    async fn a_turn_that_cannot_open_a_row_stops() {
        let db = conversation().await;
        // No such conversation, so the foreign key refuses it.
        assert!(
            begin_assistant(&db, "nope", "t1", (None, None), "m", None)
                .await
                .is_err()
        );
    }

    /// Filling the row is the checkpoint before tools may run, so a refused
    /// write must stop the turn. The test database is one connection, so
    /// `query_only` set on it holds for the write that follows.
    #[tokio::test]
    async fn filling_a_row_in_propagates_a_database_write_error() {
        let db = conversation().await;
        let id = begin_assistant(&db, "c1", "t1", (None, None), "m", None).await.unwrap();
        execute_for_tests(&db, "PRAGMA query_only=ON").await.unwrap();
        assert!(
            complete_assistant(&db, &id, "the answer", None, None, None, MessageUsage::default(), None)
                .await
                .is_err()
        );
        execute_for_tests(&db, "PRAGMA query_only=OFF").await.unwrap();
        assert_eq!(rows(&db).await[0].content, "", "and it really did not land");
    }

    /// The opposite trade: by the time this runs the tool has already touched
    /// the world, so a lost row is logged and the turn carries on.
    #[tokio::test]
    async fn a_tool_result_that_cannot_be_written_does_not_stop_the_turn() {
        let db = conversation().await;
        let assistant = begin_assistant(&db, "c1", "t1", (None, None), "m", None).await.unwrap();
        let landed = append_tool_result(&db, "c1", "t1", "call-1", "done", "success", Some(&assistant)).await;
        assert!(landed.is_some());
        assert_eq!(head(&db).await, landed, "the cursor moves onto it");

        let lost = append_tool_result(&db, "nope", "t1", "call-2", "done", "success", Some(&assistant)).await;
        assert_eq!(lost, None, "None is how the caller knows to leave the cursor alone");
    }

    /// A refusal has to survive a reload, or the card comes back as a green
    /// tick with the refusal text displayed as the tool's output.
    #[tokio::test]
    async fn a_tool_row_records_how_the_call_went() {
        let db = conversation().await;
        for (call, outcome) in [("a", "success"), ("b", "denied"), ("c", "error")] {
            append_tool_result(&db, "c1", "t1", call, "x", outcome, None)
                .await
                .unwrap();
        }
        let stored: Vec<Option<String>> = rows(&db)
            .await
            .into_iter()
            .filter(|m| m.role == "tool")
            .map(|m| m.tool_outcome)
            .collect();
        assert_eq!(
            stored,
            [Some("success".into()), Some("denied".into()), Some("error".into())]
        );
    }

    /// A steered message is a user row of this turn, at the instant the
    /// caller built its live message with; a notice is a context row.
    #[tokio::test]
    async fn steering_is_written_as_what_it_is() {
        let db = conversation().await;
        let said = write_steering(&db, "c1", "t1", "actually, stop", Some(42), None, 1234)
            .await
            .unwrap();
        let notice = append_steering(
            &db,
            "c1",
            "t1",
            "they left",
            None,
            Some(&said),
            SteeringRole::Context,
            1235,
        )
        .await
        .unwrap();
        let rows = rows(&db).await;
        let user = rows.iter().find(|r| r.id == said).unwrap();
        assert_eq!(
            (
                user.role.as_str(),
                user.sender_id,
                user.created_at,
                user.turn_id.as_deref()
            ),
            ("user", Some(42), 1234, Some("t1"))
        );
        let context = rows.iter().find(|r| r.id == notice).unwrap();
        assert_eq!(
            (context.role.as_str(), context.parent_id.as_deref()),
            ("context", Some(said.as_str()))
        );
    }

    /// Written before the work, not after — whatever is stored when the process
    /// dies is where it died — and given back after.
    #[tokio::test]
    async fn the_phase_is_recorded_while_the_work_runs_and_given_back_after() {
        let db = conversation().await;
        let seen = in_phase(&db, "t1", TurnPhase::RunningTool, Some("edit_file"), phase(&db)).await;
        assert_eq!(seen, (Some(TurnPhase::RunningTool), Some("edit_file".into())));
        assert_eq!(
            phase(&db).await,
            (Some(TurnPhase::Streaming), None),
            "and it does not linger"
        );

        let waiting = in_phase(&db, "t1", TurnPhase::AwaitingApproval, Some("run_command"), phase(&db)).await;
        assert_eq!(waiting, (Some(TurnPhase::AwaitingApproval), Some("run_command".into())));
    }

    /// A turn that has already ended does not move, so a bracket that outlives
    /// it cannot rewrite how it finished.
    #[tokio::test]
    async fn a_bracket_on_a_finished_turn_changes_nothing() {
        let db = conversation().await;
        crate::agent::turn_record::finish(&db, "t1", TurnStatus::Done, None).await;
        in_phase(&db, "t1", TurnPhase::RunningTool, Some("edit_file"), async {}).await;
        assert_eq!(
            phase(&db).await,
            (Some(TurnPhase::Streaming), None),
            "as `begin` left it"
        );
    }
}
