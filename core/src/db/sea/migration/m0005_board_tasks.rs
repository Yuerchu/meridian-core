//! `board_tasks`: the cards of the agent board.
//!
//! A card is a task a person put on the board; once started it is one
//! conversation working in one git worktree beside the repository. The card
//! outlives neither its project (`CASCADE`) nor needs its conversation to
//! exist: a card waits in the backlog with no conversation at all, and a
//! conversation deleted from under it leaves the card to say so
//! (`SET NULL`). `UNIQUE` on `conversation_id`: one card per conversation.
//!
//! The column a card is in is a person's choice (`stage`), never derived from
//! what the agent is doing; `position` orders the cards within a stage.
//!
//! `source` names where a card came from. Only the board itself today; a
//! ticket system later adds a value to the `CHECK`, which is a rebuild
//! rather than a data migration.
//!
//! The table and its indexes are sea-query builders; the `CHECK`s are SQLite
//! text, passed through (backend: sqlite-only), each a plain `IN` list.

use sea_orm::sea_query::ForeignKeyAction::{Cascade, SetNull};
use sea_orm::sea_query::IndexOrder::Asc;

use super::{Object, big_int, check, fk, index, integer, render_sqlite, t, table, text};

/// The statements SQLite runs for this migration, in order.
pub fn sqlite_statements() -> Vec<String> {
    render_sqlite(&objects())
}

#[rustfmt::skip]
pub(super) fn objects() -> Vec<Object> {
    vec![
        t(table("board_tasks")
            .col(text("id").not_null().primary_key())
            .col(text("project_id").not_null())
            .col(text("conversation_id"))
            .col(text("source").not_null())
            .col(text("title").not_null())
            .col(text("request"))
            .col(text("stage").not_null())
            .col(integer("position").not_null())
            .col(text("agent_kind"))
            .col(text("worktree_path"))
            .col(big_int("created_at").not_null())
            .col(big_int("updated_at").not_null())
            .col(big_int("worktree_removed_at"))
            .foreign_key(&mut fk(&["project_id"], "projects", &["id"], Cascade))
            .foreign_key(&mut fk(&["conversation_id"], "conversations", &["id"], SetNull))
            .check(check(r"source IN ('local')"))
            .check(check(r"stage IN ('backlog', 'running', 'review', 'done')"))
            .check(check(r"agent_kind IN ('native', 'claude_code')"))
        ),
        // A column of the board, in order.
        index("idx_board_tasks_stage", "board_tasks", &[("stage", Asc), ("position", Asc)], false, None),
        // One card per conversation, and the lookup the working-directory
        // resolver makes on every turn.
        index("idx_board_tasks_conversation", "board_tasks", &[("conversation_id", Asc)], true, None),
    ]
}
