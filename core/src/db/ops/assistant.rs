use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::assistant::{AssistantChangeset, AssistantInsert, AssistantRow};
use crate::db::schema::assistants;

pub fn list_assistants(conn: &mut SqliteConnection) -> QueryResult<Vec<AssistantRow>> {
    assistants::table
        .order(assistants::sort_order.asc())
        .load::<AssistantRow>(conn)
}

pub fn get_assistant(conn: &mut SqliteConnection, id: &str) -> QueryResult<AssistantRow> {
    assistants::table.find(id).first::<AssistantRow>(conn)
}

pub fn get_default_assistant(conn: &mut SqliteConnection) -> QueryResult<Option<AssistantRow>> {
    assistants::table
        .filter(assistants::is_default.eq(1))
        .first::<AssistantRow>(conn)
        .optional()
}

pub fn create_assistant(conn: &mut SqliteConnection, new: &AssistantInsert) -> QueryResult<AssistantRow> {
    diesel::insert_into(assistants::table).values(new).execute(conn)?;
    assistants::table.find(new.id).first::<AssistantRow>(conn)
}

pub fn update_assistant(
    conn: &mut SqliteConnection,
    id: &str,
    changeset: &AssistantChangeset,
) -> QueryResult<AssistantRow> {
    diesel::update(assistants::table.find(id))
        .set(changeset)
        .execute(conn)?;
    assistants::table.find(id).first::<AssistantRow>(conn)
}

pub fn delete_assistant(conn: &mut SqliteConnection, id: &str) -> QueryResult<()> {
    diesel::delete(assistants::table.find(id)).execute(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn make_new_assistant<'a>(id: &'a str, name: &'a str, sort_order: i32) -> AssistantInsert<'a> {
        AssistantInsert {
            id,
            name,
            description: None,
            avatar: None,
            system_prompt: "You are helpful",
            provider_id: None,
            model_id: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            is_default: 0,
            sort_order,
            created_at: 1000,
            updated_at: 1000,
            context_limit: 128000,
            compact_keep_recent: 5,
            enabled_tools: None,
            thinking_enabled: 0,
            thinking_budget: None,
            tool_preset_id: None,
            auto_compact_enabled: 0,
        }
    }

    #[test]
    fn test_create_and_get() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let new = make_new_assistant("a1", "Test Assistant", 0);
        let created = create_assistant(&mut conn, &new).unwrap();
        assert_eq!(created.id, "a1");
        assert_eq!(created.name, "Test Assistant");

        let fetched = get_assistant(&mut conn, "a1").unwrap();
        assert_eq!(fetched.name, "Test Assistant");
    }

    #[test]
    fn test_list_ordered() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_assistant(&mut conn, &make_new_assistant("a2", "Second", 2)).unwrap();
        create_assistant(&mut conn, &make_new_assistant("a1", "First", 1)).unwrap();
        create_assistant(&mut conn, &make_new_assistant("a3", "Third", 3)).unwrap();

        let list = list_assistants(&mut conn).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].name, "First");
        assert_eq!(list[1].name, "Second");
        assert_eq!(list[2].name, "Third");
    }

    #[test]
    fn test_get_default() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let mut new = make_new_assistant("a1", "Default One", 0);
        new.is_default = 1;
        create_assistant(&mut conn, &new).unwrap();

        let default = get_default_assistant(&mut conn).unwrap();
        assert!(default.is_some());
        assert_eq!(default.unwrap().name, "Default One");
    }

    #[test]
    fn test_get_default_returns_none_when_empty() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        assert!(get_default_assistant(&mut conn).unwrap().is_none());
    }

    #[test]
    fn test_update() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_assistant(&mut conn, &make_new_assistant("a1", "Old Name", 0)).unwrap();

        let changeset = AssistantChangeset {
            name: Some("New Name".into()),
            system_prompt: Some("Updated prompt".into()),
            ..Default::default()
        };
        let updated = update_assistant(&mut conn, "a1", &changeset).unwrap();
        assert_eq!(updated.name, "New Name");
        assert_eq!(updated.system_prompt, "Updated prompt");
    }

    #[test]
    fn test_delete() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_assistant(&mut conn, &make_new_assistant("a1", "To Delete", 0)).unwrap();
        delete_assistant(&mut conn, "a1").unwrap();
        assert!(get_assistant(&mut conn, "a1").is_err());
    }
}
