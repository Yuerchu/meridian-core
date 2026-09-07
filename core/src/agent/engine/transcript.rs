//! The rows a turn writes as it runs, and the phase it records while it works.
//!
//! Free functions over a `DbPool` rather than a trait. Both runners write the
//! same rows to the same tables in the same order — there is no second
//! implementation for a trait to abstract over, and one with a single impl
//! would be ceremony that also has to be argued past the `Send` bound on the
//! loop's future. If a sub-agent later needs to persist somewhere else, that is
//! the point at which there are two of something.
//!
//! What is worth stating is that these three writes fail in three different
//! ways, on purpose, and each signature says which:
//!
//! | write | database says no | the worker never came back |
//! |---|---|---|
//! | the assistant placeholder | the turn ends | the turn ends |
//! | filling that row in | the turn ends | the turn ends |
//! | the tool result row | logged, turn continues | logged, turn continues |
//!
//! The two columns are not the same failure. A refused write is the database
//! having an opinion; a `spawn_blocking` join error means the worker panicked or
//! the runtime is going down, and carrying on from that is not a considered
//! trade — it is not knowing what happened. Filling the assistant row is also
//! the checkpoint before tools may touch the world. The third row is deliberate
//! and load-bearing in both columns: by the
//! time it runs the tool has already touched the world, so refusing to carry on
//! would cost more than the row does.

use std::future::Future;

use crate::db::DbPool;
use crate::db::models::message::{MessageInsert, MessageUsage};
use crate::db::models::turn::TurnPhase;
use crate::util::{get_conn, now_ms};

/// Open the row this iteration will stream into, and return its id.
///
/// Hard error, unlike the two below. Nothing downstream works without it: the
/// `message_start` event names it, every chunk is addressed to it, and the tool
/// rows hang off it. A turn that cannot write this one has nowhere to put its
/// answer.
pub(crate) async fn begin_assistant(
    pool: &DbPool,
    conversation_id: &str,
    turn_id: &str,
    provider: (Option<&str>, Option<&str>),
    model: &str,
    parent: Option<&str>,
) -> Result<String, String> {
    let message_id = uuid::Uuid::new_v4().to_string();
    let pool = pool.clone();
    let conv_id = conversation_id.to_string();
    let msg_id = message_id.clone();
    // Owned for the same reason `parent` is: the closure outlives the borrow.
    let (provider_id, provider_name) = (provider.0.map(str::to_string), provider.1.map(str::to_string));
    let model = model.to_string();
    let turn = turn_id.to_string();
    let parent = parent.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        crate::db::ops::message::append_message(
            &mut conn,
            &MessageInsert {
                id: &msg_id,
                conversation_id: &conv_id,
                role: "assistant",
                content: "",
                // Written when the row is opened, not when it is filled in. The
                // window between the two is the longest in the turn, and a turn
                // that dies inside it still cost the upstream everything it had
                // already served — including anything served out of cache.
                // Attributing at the end would leave exactly the expensive,
                // interrupted rows belonging to nobody.
                //
                // A foreign key onto `providers` is enforced on this insert, so an
                // id that does not exist takes the turn down. Every caller has
                // already read that row through `get_provider` before reaching
                // here, and a provider deleted in the window between is a broken
                // configuration worth failing on rather than papering over.
                provider_id: provider_id.as_deref(),
                model_id: Some(&model),
                input_tokens: None,
                output_tokens: None,
                tool_calls: None,
                tool_call_id: None,
                sort_order: 0,
                created_at: now_ms(),
                reasoning_content: None,
                rating: None,
                schema_version: 2,
                is_compact_summary: 0,
                sender_id: None,
                parent_id: None,
                compact_anchor_id: None,
                source: None,
                turn_id: Some(&turn),
                tool_outcome: None,
                // Nothing is known about the reply yet; the update that fills
                // this row in is what supplies them.
                cache_read_tokens: None,
                cache_write_tokens: None,
                server_tool_calls: None,
                provider_name: provider_name.as_deref(),
            },
            parent.as_deref(),
        )
        .map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(message_id)
}

/// Fill in the row once the model has finished with it.
///
/// This is the durable boundary before any tool side effect. Both database and
/// worker failures are returned to the turn.
pub(crate) async fn complete_assistant(
    pool: &DbPool,
    message_id: &str,
    content: &str,
    reasoning: Option<&str>,
    tool_calls_json: Option<&str>,
    provider_state: Option<&str>,
    usage: MessageUsage,
) -> Result<(), String> {
    let pool = pool.clone();
    let msg_id = message_id.to_string();
    let content = content.to_string();
    let reasoning = reasoning.map(str::to_string);
    let tool_calls_json = tool_calls_json.map(str::to_string);
    let provider_state = provider_state.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|e| e.to_string())?;
        crate::db::ops::message::update_assistant_message(
            &mut conn,
            &msg_id,
            &content,
            reasoning.as_deref(),
            tool_calls_json.as_deref(),
            provider_state.as_deref(),
            &usage,
        )
        .map_err(|e| e.to_string())?;
        // Recorded here rather than when the row was opened: this is the
        // first moment it has content and token counts, and an audit copy of
        // an empty placeholder would answer nothing. A turn that dies before
        // reaching this point leaves no record of its reply — the reply does
        // not exist either, and what it cost is still on the `messages` row
        // until that conversation is deleted.
        //
        match crate::db::ops::message::get_message(&mut conn, &msg_id) {
            Ok(row) => {
                if let Err(e) = crate::db::ops::audit::record(&mut conn, &row) {
                    tracing::error!(
                        error = %e,
                        message_id = %msg_id,
                        "the audit copy of a reply could not be written",
                    );
                }
            }
            Err(e) => tracing::error!(
                error = %e,
                message_id = %msg_id,
                "a completed reply could not be read back for the audit log",
            ),
        }
        Ok::<_, String>(())
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(())
}

/// Record what a tool returned. `None` means it was not written.
///
/// The caller is expected to leave its parent cursor where it was on `None`,
/// so the next row hangs off the last one that did land and the chain stays
/// intact. Aborting instead would be worse than losing the row: the tool has
/// already run by the time this is called, so the world has changed whether or
/// not the transcript says so. The unanswered `tool_call` is stripped from the
/// payload later by `remove_orphan_tool_messages`.
pub(crate) async fn append_tool_result(
    pool: &DbPool,
    conversation_id: &str,
    turn_id: &str,
    call_id: &str,
    content: &str,
    outcome: &'static str,
    parent: Option<&str>,
) -> Option<String> {
    let pool = pool.clone();
    let conv_id = conversation_id.to_string();
    let message_id = uuid::Uuid::new_v4().to_string();
    let msg_id = message_id.clone();
    let call_id = call_id.to_string();
    let content = content.to_string();
    let turn = turn_id.to_string();
    let parent = parent.map(str::to_string);
    let written = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|e| e.to_string())?;
        crate::db::ops::message::append_message(
            &mut conn,
            &MessageInsert {
                id: &msg_id,
                conversation_id: &conv_id,
                role: "tool",
                content: &content,
                provider_id: None,
                model_id: None,
                input_tokens: None,
                output_tokens: None,
                tool_calls: None,
                tool_call_id: Some(&call_id),
                sort_order: 0,
                created_at: now_ms(),
                reasoning_content: None,
                rating: None,
                schema_version: 2,
                is_compact_summary: 0,
                sender_id: None,
                parent_id: None,
                compact_anchor_id: None,
                source: None,
                turn_id: Some(&turn),
                // The same word the event carries. Stored so a reload does not
                // turn a refusal into a green tick with the refusal text
                // sitting in it as the result.
                tool_outcome: Some(outcome),
                // A tool result is our own text, not something an upstream was
                // paid to produce.
                cache_read_tokens: None,
                cache_write_tokens: None,
                server_tool_calls: None,
                provider_name: None,
            },
            parent.as_deref(),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    })
    .await;

    match written {
        Ok(Ok(())) => Some(message_id),
        Ok(Err(e)) => {
            tracing::error!("failed to persist tool result: {e}");
            None
        }
        Err(e) => {
            tracing::error!("tool result write panicked: {e}");
            None
        }
    }
}

/// Record something a person said while the turn was already running.
///
/// A user row, not an assistant one, and it belongs to the turn it is steering
/// rather than to the next one — the model reads it in this turn's next request,
/// so a reader who saw it filed elsewhere would be looking at a different
/// conversation than the model was.
///
/// Fails the same way a tool result does, and for a weaker version of the same
/// reason: the message has already been said, and refusing to carry on would
/// throw away the turn it was said to.
pub(crate) async fn append_steering(
    pool: &DbPool,
    conversation_id: &str,
    turn_id: &str,
    content: &str,
    sender_id: Option<i64>,
    parent: Option<&str>,
) -> Option<String> {
    match write_steering(pool, conversation_id, turn_id, content, sender_id, parent).await {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::error!("failed to persist steered message: {e}");
            None
        }
    }
}

/// The same write, with the failure handed back instead of logged.
///
/// For the one caller that has already promised something. A message accepted
/// into an inbox and never delivered has to be either written down or accounted
/// for out loud, and it can be neither if the failure was swallowed on the way
/// past.
pub async fn write_steering(
    pool: &DbPool,
    conversation_id: &str,
    turn_id: &str,
    content: &str,
    sender_id: Option<i64>,
    parent: Option<&str>,
) -> Result<String, String> {
    let pool = pool.clone();
    let conv_id = conversation_id.to_string();
    let message_id = uuid::Uuid::new_v4().to_string();
    let msg_id = message_id.clone();
    let content = content.to_string();
    let turn = turn_id.to_string();
    let parent = parent.map(str::to_string);
    let written = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|e| e.to_string())?;
        crate::db::ops::message::append_message(
            &mut conn,
            &MessageInsert {
                id: &msg_id,
                conversation_id: &conv_id,
                role: "user",
                content: &content,
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
                is_compact_summary: 0,
                sender_id,
                parent_id: None,
                compact_anchor_id: None,
                source: None,
                turn_id: Some(&turn),
                tool_outcome: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                server_tool_calls: None,
                provider_name: None,
            },
            parent.as_deref(),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    })
    .await;

    match written {
        Ok(Ok(())) => Ok(message_id),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("the write panicked: {e}")),
    }
}

/// Run something with the turn recorded as being in a phase, and back to
/// streaming when it returns.
///
/// The phase is written *before* the work, which is the entire point: what is
/// stored when the process dies is where it died. `RunningTool` is the one that
/// matters — it means a call had started and the world outside the database may
/// already have changed.
///
/// Restoring `Streaming` afterwards matters nearly as much. Leaving the phase
/// behind would have a crash a minute later report a tool that finished long
/// ago, or an approval card that is no longer on screen.
pub async fn in_phase<T>(
    pool: &DbPool,
    turn_id: &str,
    phase: TurnPhase,
    tool: Option<&str>,
    work: impl Future<Output = T>,
) -> T {
    crate::agent::turn_record::note_phase(pool, turn_id, phase, tool).await;
    let out = work.await;
    crate::agent::turn_record::note_phase(pool, turn_id, TurnPhase::Streaming, None).await;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::turn::TurnStatus;
    use crate::db::test_db;
    use crate::turn::TurnOrigin;
    use diesel::prelude::*;

    fn conversation(pool: &DbPool) {
        let mut conn = pool.get().unwrap();
        crate::db::ops::conversation::create_conversation(&mut conn, "c1", Some("t"), None, None, 1).unwrap();
        crate::db::ops::turn::begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
    }

    fn rows(pool: &DbPool) -> Vec<crate::db::models::message::MessageRow> {
        let mut conn = pool.get().unwrap();
        crate::db::ops::message::list_messages(&mut conn, "c1").unwrap()
    }

    fn head(pool: &DbPool) -> Option<String> {
        let mut conn = pool.get().unwrap();
        crate::db::ops::conversation::get_conversation(&mut conn, "c1")
            .unwrap()
            .head_message_id
    }

    fn phase(pool: &DbPool) -> (Option<TurnPhase>, Option<String>) {
        let mut conn = pool.get().unwrap();
        let t: crate::db::models::turn::TurnRow = crate::db::schema::turns::table.find("t1").first(&mut conn).unwrap();
        (t.phase().unwrap(), t.phase_tool)
    }

    #[tokio::test]
    async fn an_iteration_opens_a_row_and_then_fills_it_in() {
        let pool = test_db();
        conversation(&pool);

        let id = begin_assistant(&pool, "c1", "t1", (None, None), "gpt-4.1-mini", None)
            .await
            .unwrap();
        // Empty until the model has finished, which is what makes a `done` turn
        // above an empty row diagnosable as a lost write.
        assert_eq!(rows(&pool)[0].content, "");
        assert_eq!(head(&pool).as_deref(), Some(id.as_str()));

        complete_assistant(
            &pool,
            &id,
            "the answer",
            Some("thinking"),
            None,
            Some(r#"{"version":1,"producer":{"vendor":"anthropic","protocol":"messages","model":"m"},"kind":"anthropic_thinking_signature","payload":{"signature":"sig"}}"#),
            MessageUsage {
                input_tokens: Some(7),
                output_tokens: Some(11),
                cache_read_tokens: Some(41),
                cache_write_tokens: Some(43),
                server_tool_calls: Some(47),
            },
        )
        .await
        .unwrap();

        let row = &rows(&pool)[0];
        assert_eq!(row.content, "the answer");
        assert_eq!(row.reasoning_content.as_deref(), Some("thinking"));
        assert!(row.provider_state.as_deref().is_some_and(|s| s.contains("sig")));
        assert_eq!(row.input_tokens, Some(7));
        assert_eq!(row.output_tokens, Some(11));
        // Distinct values, so a transposed pair cannot pass.
        assert_eq!(row.cache_read_tokens, Some(41));
        assert_eq!(row.cache_write_tokens, Some(43));
        assert_eq!(row.server_tool_calls, Some(47));
        assert_eq!(row.turn_id.as_deref(), Some("t1"));
    }

    /// A reply reaches the audit log when it is finished, not when its row is
    /// opened — the placeholder has neither content nor token counts, and a copy
    /// of that answers nothing.
    #[tokio::test]
    async fn a_completed_reply_is_copied_into_the_audit_log() {
        let pool = test_db();
        conversation(&pool);

        // The pool hands out one connection, so every borrow here is scoped:
        // holding one across an await that needs its own is a deadlock, not a
        // failure of what is being tested.
        let id = begin_assistant(&pool, "c1", "t1", (None, None), "m", None)
            .await
            .unwrap();
        {
            let mut conn = pool.get().unwrap();
            assert!(
                crate::db::ops::audit::list_recent(&mut conn, 10).unwrap().is_empty(),
                "an empty placeholder is not worth recording",
            );
        }

        complete_assistant(
            &pool,
            &id,
            "the answer",
            None,
            None,
            None,
            MessageUsage {
                input_tokens: Some(200),
                output_tokens: Some(20),
                cache_read_tokens: Some(180),
                cache_write_tokens: None,
                server_tool_calls: Some(2),
            },
        )
        .await
        .unwrap();

        let mut conn = pool.get().unwrap();
        let logged = crate::db::ops::audit::list_recent(&mut conn, 10).unwrap();
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].message_id, id);
        assert_eq!(logged[0].content, "the answer");
        assert_eq!(logged[0].role, "assistant");
        assert_eq!(logged[0].input_tokens, Some(200));
        assert_eq!(logged[0].cache_read_tokens, Some(180));
        assert_eq!(logged[0].server_tool_calls, Some(2));
        assert_eq!(
            logged[0].turn_origin.as_deref(),
            Some("desktop"),
            "snapshotted off the turn"
        );
    }

    /// The one write that takes the turn down with it. Everything downstream is
    /// addressed to the row it returns, so there is nothing useful to carry on
    /// with.
    #[tokio::test]
    async fn a_turn_that_cannot_open_a_row_stops() {
        let pool = test_db();
        // No conversation, so the foreign key refuses it.
        assert!(
            begin_assistant(&pool, "nope", "t1", (None, None), "m", None)
                .await
                .is_err()
        );
    }

    /// Filling the row is the checkpoint before tools may run, so a refused
    /// write must stop the turn.
    ///
    /// A refusal has to be real to prove anything, so the database is put into
    /// `query_only` and asked to write an existing row.
    ///
    /// The pool hands out one connection (`max_size(1)`) and the customizer's
    /// `on_acquire` runs once when it is established, so the pragma survives
    /// being handed back and picked up again inside `complete_assistant`.
    ///
    /// The other half of this function's contract — a `spawn_blocking` join
    /// error — has no test. Inducing one means
    /// panicking a worker, and the only way to reach that from here would be a
    /// hook in production code. It is held by the return type and by the `?` at
    /// both call sites.
    #[tokio::test]
    async fn filling_a_row_in_propagates_a_database_write_error() {
        let pool = test_db();
        conversation(&pool);
        let id = begin_assistant(&pool, "c1", "t1", (None, None), "m", None)
            .await
            .unwrap();

        {
            use diesel::connection::SimpleConnection;
            let mut conn = pool.get().unwrap();
            conn.batch_execute("PRAGMA query_only=ON").unwrap();
            // The write really is refused, so what follows is testing something.
            assert!(
                crate::db::ops::message::update_assistant_message(
                    &mut conn,
                    &id,
                    "the answer",
                    None,
                    None,
                    None,
                    &MessageUsage::default(),
                )
                .is_err(),
                "query_only must make this a real failure",
            );
        }

        assert!(
            complete_assistant(&pool, &id, "the answer", None, None, None, MessageUsage::default())
                .await
                .is_err()
        );

        {
            use diesel::connection::SimpleConnection;
            let mut conn = pool.get().unwrap();
            conn.batch_execute("PRAGMA query_only=OFF").unwrap();
        }
        // And it really did not land.
        assert_eq!(rows(&pool)[0].content, "");
    }

    /// The opposite trade, and the reason it is the opposite: by the time this
    /// runs the tool has already touched the world. Losing the row costs the
    /// transcript a line; aborting costs the model any chance to react to what
    /// its own call just did.
    #[tokio::test]
    async fn a_tool_result_that_cannot_be_written_does_not_stop_the_turn() {
        let pool = test_db();
        conversation(&pool);
        let assistant = begin_assistant(&pool, "c1", "t1", (None, None), "m", None)
            .await
            .unwrap();

        let landed = append_tool_result(&pool, "c1", "t1", "call-1", "done", "success", Some(&assistant)).await;
        assert!(landed.is_some());
        assert_eq!(head(&pool), landed, "the cursor moves onto it");

        // Same call against a conversation that does not exist: refused, and
        // said so without unwinding.
        let lost = append_tool_result(&pool, "nope", "t1", "call-2", "done", "success", Some(&assistant)).await;
        assert!(lost.is_none(), "None is how the caller knows to leave the cursor alone");
    }

    /// A refusal has to survive a reload, or the card comes back as a green
    /// tick with the refusal text displayed as the tool's output.
    #[tokio::test]
    async fn a_tool_row_records_how_the_call_went() {
        let pool = test_db();
        conversation(&pool);

        for (call, outcome) in [("a", "success"), ("b", "denied"), ("c", "error")] {
            append_tool_result(&pool, "c1", "t1", call, "x", outcome, None)
                .await
                .unwrap();
        }

        let stored: Vec<Option<String>> = rows(&pool)
            .into_iter()
            .filter(|m| m.role == "tool")
            .map(|m| m.tool_outcome)
            .collect();
        assert_eq!(
            stored,
            [Some("success".into()), Some("denied".into()), Some("error".into())]
        );
    }

    /// Written before the work, not after — whatever is stored when the process
    /// dies is where it died.
    #[tokio::test]
    async fn the_phase_is_recorded_while_the_work_runs_and_given_back_after() {
        let pool = test_db();
        conversation(&pool);

        let seen = in_phase(&pool, "t1", TurnPhase::RunningTool, Some("edit_file"), async {
            phase(&pool)
        })
        .await;

        assert_eq!(seen, (Some(TurnPhase::RunningTool), Some("edit_file".into())));
        assert_eq!(
            phase(&pool),
            (Some(TurnPhase::Streaming), None),
            "and it does not linger"
        );
    }

    /// It brackets an approval the same way, which is a window the process can
    /// sit in for as long as the user takes to answer.
    #[tokio::test]
    async fn the_same_bracket_covers_waiting_on_a_person() {
        let pool = test_db();
        conversation(&pool);

        let seen = in_phase(&pool, "t1", TurnPhase::AwaitingApproval, Some("run_command"), async {
            phase(&pool)
        })
        .await;

        assert_eq!(seen, (Some(TurnPhase::AwaitingApproval), Some("run_command".into())));
    }

    /// A turn that has already ended does not move, so a bracket that outlives
    /// it cannot rewrite how it finished.
    #[tokio::test]
    async fn a_bracket_on_a_finished_turn_changes_nothing() {
        let pool = test_db();
        conversation(&pool);
        {
            let mut conn = pool.get().unwrap();
            crate::db::ops::turn::finish(&mut conn, "t1", TurnStatus::Done, None, 1500).unwrap();
        }

        in_phase(&pool, "t1", TurnPhase::RunningTool, Some("edit_file"), async {}).await;

        assert_eq!(phase(&pool), (Some(TurnPhase::Streaming), None), "as `begin` left it");
    }
}
