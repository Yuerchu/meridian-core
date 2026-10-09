use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::entity::assistant;
use crate::db::models::assistant::AssistantRow;
use crate::db::schema::assistants;

/// A Diesel row as the entity model; a bad flag or tool list fails the read.
fn model(row: AssistantRow) -> QueryResult<assistant::Model> {
    assistant::Model::try_from(row).map_err(super::contract_violation)
}

pub fn get_default_assistant(conn: &mut SqliteConnection) -> QueryResult<Option<assistant::Model>> {
    assistants::table
        .filter(assistants::is_default.eq(1))
        .first::<AssistantRow>(conn)
        .optional()?
        .map(model)
        .transpose()
}
