//! `background_tasks`: work that outlives the turn that started it.
//!
//! A command run in the background keeps going after the model has moved on,
//! and when it ends the model has to hear about it — in the same process, by
//! waking a turn of its own, or after a restart, from the next turn that runs.
//! That last case is why this is a table and not a map: the process that was
//! watching a task can die, and the task's ending (or the fact that nobody saw
//! it end) is still something the model is owed.
//!
//! One table for every runner. A hosted Claude Code session reports its own
//! background tasks, and the task list, the stop button and the wake-up are the
//! same whoever ran the command; `runner` says whose it is, and `external_id`
//! carries the other side's id where there is one.
//!
//! The table and its index are sea-query builders, so they render for any
//! backend. The `CHECK`s are SQLite text, passed through (backend:
//! sqlite-only), though each is a plain `IN` list any SQL accepts.

use sea_orm::sea_query::ForeignKeyAction::Cascade;
use sea_orm::sea_query::IndexOrder::Asc;

use super::{Object, big_int, check, fk, index, integer, render_sqlite, t, table, text};

/// The statements SQLite runs for this migration, in order.
pub fn sqlite_statements() -> Vec<String> {
    render_sqlite(&objects())
}

#[rustfmt::skip]
pub(super) fn objects() -> Vec<Object> {
    vec![
        t(table("background_tasks")
            .col(text("id").not_null().primary_key())
            .col(text("conversation_id").not_null())
            .col(text("runner").not_null())
            .col(text("external_id"))
            .col(text("spawned_turn_id"))
            .col(text("spawned_call_id"))
            .col(text("kind").not_null())
            .col(text("command"))
            .col(text("description"))
            .col(text("cwd"))
            .col(text("sandbox"))
            .col(text("state").not_null())
            .col(integer("exit_code"))
            .col(text("ended_reason"))
            .col(text("output_path"))
            .col(big_int("output_bytes").not_null().default(0))
            .col(integer("output_truncated").not_null().default(0))
            .col(big_int("started_at").not_null())
            .col(big_int("ended_at"))
            .col(big_int("notified_at"))
            .col(text("notified_turn_id"))
            .foreign_key(&mut fk(&["conversation_id"], "conversations", &["id"], Cascade))
            .check(check(r"runner IN ('native', 'claude_code')"))
            .check(check(r"kind IN ('command', 'agent')"))
            .check(check(r"state IN ('running', 'completed', 'failed', 'stopped', 'lost')"))
            .check(check(r"output_truncated IN (0, 1)"))
        ),
        // Every read is "this conversation's tasks", oldest first, and the
        // wake-up asks for the unpaid ones among them.
        index("idx_background_tasks_conversation", "background_tasks", &[("conversation_id", Asc), ("started_at", Asc)], false, None),
    ]
}
