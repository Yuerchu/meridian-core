//! `board_tasks`: the cards of the agent board. See
//! `db::sea::migration::m0005_board_tasks` for the table and
//! `db::sea::ops::board_task` for how it is read and written.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, checked_text_enum};

checked_text_enum!(
    /// Where a card came from.
    BoardSource {
        /// Put on the board by a person — typed into a column.
        Local = "local",
    }
);

checked_text_enum!(
    /// The column a card is in. A person's choice, never derived from what
    /// the agent is doing.
    BoardStage {
        Backlog = "backlog",
        Running = "running",
        Review = "review",
        Done = "done",
    }
);

checked_text_enum!(
    /// Which agent works the card, chosen when it starts.
    BoardAgentKind {
        /// This app's own turn loop.
        Native = "native",
        /// A hosted Claude Code session.
        ClaudeCode = "claude_code",
    }
);

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "board_tasks")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// The repository the worktree is made from.
    pub project_id: String,
    /// `None` until the card starts, and again if its conversation is deleted.
    #[sea_orm(unique)]
    pub conversation_id: Option<String>,
    pub source: BoardSource,
    pub title: String,
    /// What was asked, as typed.
    pub request: Option<String>,
    pub stage: BoardStage,
    /// Order within the stage.
    pub position: i32,
    pub agent_kind: Option<BoardAgentKind>,
    /// The directory the agent works in: the project's place inside the
    /// worktree. `None` before the card starts and after the worktree is
    /// removed.
    pub worktree_path: Option<String>,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
    /// When a person removed the worktree. The card stays, and its
    /// conversation may not run again: there is nowhere for it to work.
    pub worktree_removed_at: Option<EpochMs>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    /// `CASCADE`: a project's cards go with it.
    #[sea_orm(
        belongs_to = "super::project::Entity",
        from = "Column::ProjectId",
        to = "super::project::Column::Id",
        on_delete = "Cascade"
    )]
    Project,
    /// `SET NULL`: the card outlives a conversation deleted from under it.
    #[sea_orm(
        belongs_to = "super::conversation::Entity",
        from = "Column::ConversationId",
        to = "super::conversation::Column::Id",
        on_delete = "SetNull"
    )]
    Conversation,
}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use sea_orm::Iterable;

    use super::*;
    use crate::db::entity::allowed_by_check;

    fn stored<E: Iterable + ActiveEnum<Value = String> + Copy + std::fmt::Debug + PartialEq>(
        as_str: impl Fn(E) -> &'static str,
    ) -> BTreeSet<String> {
        E::iter()
            .map(|variant| {
                let db = variant.to_value();
                assert_eq!(as_str(variant), db, "{variant:?}: strum and the stored value disagree");
                assert_eq!(E::try_from_value(&db).unwrap(), variant);
                db
            })
            .collect()
    }

    /// Each enum and its column's `CHECK` are one list, read out of the
    /// snapshot rather than restated here.
    #[test]
    fn the_stored_spellings_are_what_the_check_constraints_name() {
        assert_eq!(
            stored::<BoardSource>(BoardSource::as_str),
            allowed_by_check("board_tasks", "source")
        );
        assert_eq!(
            stored::<BoardStage>(BoardStage::as_str),
            allowed_by_check("board_tasks", "stage")
        );
        assert_eq!(
            stored::<BoardAgentKind>(BoardAgentKind::as_str),
            allowed_by_check("board_tasks", "agent_kind")
        );
    }
}
