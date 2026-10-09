//! Reading and writing the agent board's cards.
//!
//! No function here opens a transaction: a write takes the caller's `WriteTx`,
//! which is what makes "append at the end of a column" and "renumber the
//! column a card moved into" one step with the reads they depend on.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};

use crate::db::entity::board_task::{self, BoardAgentKind, BoardSource, BoardStage};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::EpochMs;

/// A card about to be put on the board, at the end of its column.
pub struct BoardTaskInsert<'a> {
    pub id: &'a str,
    pub project_id: &'a str,
    pub source: BoardSource,
    pub title: &'a str,
    pub request: Option<&'a str>,
    pub stage: BoardStage,
    pub now: EpochMs,
}

/// What starting a card writes: the conversation that works it, which agent
/// that is, and where it works. The card moves to `running` with it.
pub struct BoardTaskStartChangeset<'a> {
    pub conversation_id: &'a str,
    pub agent_kind: BoardAgentKind,
    pub worktree_path: &'a str,
    pub now: EpochMs,
}

/// A person's edit of what the card says. `None` leaves a field alone;
/// `request: Some(None)` clears it.
pub struct BoardTaskChangeset<'a> {
    pub title: Option<&'a str>,
    pub request: Option<Option<&'a str>>,
    pub now: EpochMs,
}

async fn last_position(tx: &WriteTx, stage: BoardStage) -> Result<Option<i32>, DbErr> {
    board_task::Entity::find()
        .select_only()
        .column_as(board_task::Column::Position.max(), "position")
        .filter(board_task::Column::Stage.eq(stage))
        .into_tuple::<Option<i32>>()
        .one(tx.conn()?)
        .await
        .map(Option::flatten)
}

pub async fn insert(tx: &WriteTx, row: &BoardTaskInsert<'_>) -> Result<board_task::Model, DbErr> {
    let position = last_position(tx, row.stage).await?.map_or(0, |last| last + 1);
    let model = board_task::ActiveModel {
        id: Set(row.id.to_string()),
        project_id: Set(row.project_id.to_string()),
        conversation_id: Set(None),
        source: Set(row.source),
        title: Set(row.title.to_string()),
        request: Set(row.request.map(str::to_string)),
        stage: Set(row.stage),
        position: Set(position),
        agent_kind: Set(None),
        worktree_path: Set(None),
        created_at: Set(row.now),
        updated_at: Set(row.now),
        worktree_removed_at: Set(None),
    };
    board_task::Entity::insert(model).exec_with_returning(tx.conn()?).await
}

pub async fn get(db: &impl Read, id: &str) -> Result<Option<board_task::Model>, DbErr> {
    board_task::Entity::find_by_id(id).one(db.conn()?).await
}

/// Every card, column by column in board order.
pub async fn list(db: &impl Read) -> Result<Vec<board_task::Model>, DbErr> {
    board_task::Entity::find()
        .order_by_asc(board_task::Column::Stage)
        .order_by_asc(board_task::Column::Position)
        .order_by_asc(board_task::Column::CreatedAt)
        .all(db.conn()?)
        .await
}

/// The card a conversation works, if it is one.
pub async fn for_conversation(db: &impl Read, conversation_id: &str) -> Result<Option<board_task::Model>, DbErr> {
    board_task::Entity::find()
        .filter(board_task::Column::ConversationId.eq(conversation_id))
        .one(db.conn()?)
        .await
}

/// Where a conversation's card has it work — `None` for a conversation that
/// is not a card, or whose worktree has been removed.
pub async fn worktree_for_conversation(db: &impl Read, conversation_id: &str) -> Result<Option<String>, DbErr> {
    Ok(for_conversation(db, conversation_id)
        .await?
        .and_then(|card| card.worktree_path))
}

/// Start a card: written once. A card that already has a conversation answers
/// 0 rather than being given a second one.
pub async fn set_started(tx: &WriteTx, id: &str, start: &BoardTaskStartChangeset<'_>) -> Result<u64, DbErr> {
    let done = board_task::Entity::update_many()
        .col_expr(
            board_task::Column::ConversationId,
            Expr::value(start.conversation_id.to_string()),
        )
        .col_expr(board_task::Column::AgentKind, Expr::value(start.agent_kind))
        .col_expr(
            board_task::Column::WorktreePath,
            Expr::value(start.worktree_path.to_string()),
        )
        .col_expr(board_task::Column::Stage, Expr::value(BoardStage::Running))
        .col_expr(board_task::Column::UpdatedAt, Expr::value(start.now))
        .filter(board_task::Column::Id.eq(id))
        .filter(board_task::Column::ConversationId.is_null())
        .exec(tx.conn()?)
        .await?;
    Ok(done.rows_affected)
}

pub async fn update(tx: &WriteTx, id: &str, change: &BoardTaskChangeset<'_>) -> Result<u64, DbErr> {
    let mut update = board_task::Entity::update_many().col_expr(board_task::Column::UpdatedAt, Expr::value(change.now));
    if let Some(title) = change.title {
        update = update.col_expr(board_task::Column::Title, Expr::value(title.to_string()));
    }
    if let Some(request) = change.request {
        update = update.col_expr(board_task::Column::Request, Expr::value(request.map(str::to_string)));
    }
    let done = update.filter(board_task::Column::Id.eq(id)).exec(tx.conn()?).await?;
    Ok(done.rows_affected)
}

/// Put a card at `index` in `stage` — another column or its own — and
/// number that column's cards from 0 again, in their order with the card
/// placed. An index past the end appends. Answers whether the card exists.
pub async fn move_to(tx: &WriteTx, id: &str, stage: BoardStage, index: usize, now: EpochMs) -> Result<bool, DbErr> {
    if get(tx, id).await?.is_none() {
        return Ok(false);
    }
    let mut order: Vec<String> = board_task::Entity::find()
        .select_only()
        .column(board_task::Column::Id)
        .filter(board_task::Column::Stage.eq(stage))
        .filter(board_task::Column::Id.ne(id))
        .order_by_asc(board_task::Column::Position)
        .order_by_asc(board_task::Column::CreatedAt)
        .into_tuple()
        .all(tx.conn()?)
        .await?;
    order.insert(index.min(order.len()), id.to_string());
    for (position, card) in order.iter().enumerate() {
        let mut update = board_task::Entity::update_many().col_expr(
            board_task::Column::Position,
            Expr::value(i32::try_from(position).map_err(|_| DbErr::Custom("a column of 2^31 cards".into()))?),
        );
        if card == id {
            update = update
                .col_expr(board_task::Column::Stage, Expr::value(stage))
                .col_expr(board_task::Column::UpdatedAt, Expr::value(now));
        }
        update
            .filter(board_task::Column::Id.eq(card.as_str()))
            .exec(tx.conn()?)
            .await?;
    }
    Ok(true)
}

/// Record that a person removed the card's worktree: the path goes, the time
/// stays, and the card is done.
pub async fn clear_worktree(tx: &WriteTx, id: &str, now: EpochMs) -> Result<u64, DbErr> {
    let done = board_task::Entity::update_many()
        .col_expr(board_task::Column::WorktreePath, Expr::value(Option::<String>::None))
        .col_expr(board_task::Column::WorktreeRemovedAt, Expr::value(now))
        .col_expr(board_task::Column::UpdatedAt, Expr::value(now))
        .filter(board_task::Column::Id.eq(id))
        .filter(board_task::Column::WorktreePath.is_not_null())
        .exec(tx.conn()?)
        .await?;
    Ok(done.rows_affected)
}

pub async fn delete(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    let done = board_task::Entity::delete_by_id(id).exec(tx.conn()?).await?;
    Ok(done.rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    async fn board() -> Db {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO projects (id, name, path, created_at, updated_at) VALUES ('p1', 'repo', '/r', 1, 1); \
             INSERT INTO conversations (id, created_at, updated_at) VALUES ('c1', 1, 1), ('c2', 1, 1)",
        )
        .await
        .unwrap();
        db
    }

    async fn add(db: &Db, id: &str, stage: BoardStage) -> board_task::Model {
        db.write(async |tx| {
            insert(
                tx,
                &BoardTaskInsert {
                    id,
                    project_id: "p1",
                    source: BoardSource::Local,
                    title: id,
                    request: Some("do it"),
                    stage,
                    now: 10,
                },
            )
            .await
        })
        .await
        .unwrap()
    }

    async fn column(db: &Db, stage: BoardStage) -> Vec<String> {
        list(db)
            .await
            .unwrap()
            .into_iter()
            .filter(|card| card.stage == stage)
            .map(|card| card.id)
            .collect()
    }

    fn start<'a>(conversation_id: &'a str) -> BoardTaskStartChangeset<'a> {
        BoardTaskStartChangeset {
            conversation_id,
            agent_kind: BoardAgentKind::Native,
            worktree_path: "/r.worktrees/a/app",
            now: 20,
        }
    }

    #[tokio::test]
    async fn a_card_goes_on_at_the_end_of_its_column() {
        let db = board().await;
        assert_eq!(add(&db, "a", BoardStage::Backlog).await.position, 0);
        assert_eq!(add(&db, "b", BoardStage::Backlog).await.position, 1);
        assert_eq!(add(&db, "x", BoardStage::Running).await.position, 0);
        assert_eq!(column(&db, BoardStage::Backlog).await, ["a", "b"]);
    }

    #[tokio::test]
    async fn moving_renumbers_the_column_it_lands_in() {
        let db = board().await;
        for id in ["a", "b", "c"] {
            add(&db, id, BoardStage::Running).await;
        }
        add(&db, "z", BoardStage::Review).await;

        // Within a column: c to the front.
        db.write(async |tx| move_to(tx, "c", BoardStage::Running, 0, 30).await)
            .await
            .unwrap();
        assert_eq!(column(&db, BoardStage::Running).await, ["c", "a", "b"]);

        // Across: a between nothing and z, and an index past the end appends.
        db.write(async |tx| move_to(tx, "a", BoardStage::Review, 0, 31).await)
            .await
            .unwrap();
        db.write(async |tx| move_to(tx, "b", BoardStage::Review, 99, 32).await)
            .await
            .unwrap();
        assert_eq!(column(&db, BoardStage::Review).await, ["a", "z", "b"]);
        assert_eq!(column(&db, BoardStage::Running).await, ["c"]);
        let positions: Vec<i32> = list(&db)
            .await
            .unwrap()
            .into_iter()
            .filter(|card| card.stage == BoardStage::Review)
            .map(|card| card.position)
            .collect();
        assert_eq!(positions, [0, 1, 2]);

        let moved = db.write(async |tx| move_to(tx, "nope", BoardStage::Done, 0, 33).await);
        assert!(!moved.await.unwrap());
    }

    #[tokio::test]
    async fn a_card_starts_once_and_its_conversation_finds_it() {
        let db = board().await;
        add(&db, "a", BoardStage::Backlog).await;
        add(&db, "b", BoardStage::Backlog).await;

        let first = db.write(async |tx| set_started(tx, "a", &start("c1")).await);
        assert_eq!(first.await.unwrap(), 1);
        let again = db.write(async |tx| set_started(tx, "a", &start("c2")).await);
        assert_eq!(
            again.await.unwrap(),
            0,
            "a started card is not given a second conversation"
        );

        let card = get(&db, "a").await.unwrap().unwrap();
        assert_eq!(card.stage, BoardStage::Running);
        assert_eq!(card.agent_kind, Some(BoardAgentKind::Native));
        assert_eq!(
            worktree_for_conversation(&db, "c1").await.unwrap().as_deref(),
            Some("/r.worktrees/a/app")
        );
        assert_eq!(worktree_for_conversation(&db, "c2").await.unwrap(), None);

        // One card per conversation.
        let twice = db.write(async |tx| set_started(tx, "b", &start("c1")).await);
        assert!(twice.await.is_err());
    }

    #[tokio::test]
    async fn a_removed_worktree_leaves_the_card_and_no_directory() {
        let db = board().await;
        add(&db, "a", BoardStage::Backlog).await;
        db.write(async |tx| set_started(tx, "a", &start("c1")).await)
            .await
            .unwrap();
        let cleared = db.write(async |tx| clear_worktree(tx, "a", 40).await);
        assert_eq!(cleared.await.unwrap(), 1);
        let card = get(&db, "a").await.unwrap().unwrap();
        assert_eq!((card.worktree_path, card.worktree_removed_at), (None, Some(40)));
        assert_eq!(worktree_for_conversation(&db, "c1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_card_outlives_its_conversation_but_not_its_project() {
        let db = board().await;
        add(&db, "a", BoardStage::Backlog).await;
        db.write(async |tx| set_started(tx, "a", &start("c1")).await)
            .await
            .unwrap();
        db.write(async |tx| crate::db::sea::ops::conversation::delete_conversation(tx, "c1").await)
            .await
            .unwrap();
        let card = get(&db, "a").await.unwrap().unwrap();
        assert_eq!(card.conversation_id, None);

        execute_for_tests(&db, "DELETE FROM projects WHERE id = 'p1'")
            .await
            .unwrap();
        assert_eq!(get(&db, "a").await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_edit_changes_only_what_it_names() {
        let db = board().await;
        add(&db, "a", BoardStage::Backlog).await;
        let renamed = BoardTaskChangeset {
            title: Some("renamed"),
            request: None,
            now: 50,
        };
        db.write(async |tx| update(tx, "a", &renamed).await).await.unwrap();
        let card = get(&db, "a").await.unwrap().unwrap();
        assert_eq!(
            (card.title.as_str(), card.request.as_deref()),
            ("renamed", Some("do it"))
        );

        let cleared = BoardTaskChangeset {
            title: None,
            request: Some(None),
            now: 51,
        };
        db.write(async |tx| update(tx, "a", &cleared).await).await.unwrap();
        let card = get(&db, "a").await.unwrap().unwrap();
        assert_eq!(
            (card.title.as_str(), card.request, card.updated_at),
            ("renamed", None, 51)
        );

        let gone = db.write(async |tx| delete(tx, "a").await);
        assert_eq!(gone.await.unwrap(), 1);
        assert_eq!(get(&db, "a").await.unwrap(), None);
    }
}
