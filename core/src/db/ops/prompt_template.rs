use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::prompt_template::{PromptTemplateChangeset, PromptTemplateInsert, PromptTemplateRow};
use crate::db::schema::prompt_templates;

pub fn list_templates(conn: &mut SqliteConnection) -> QueryResult<Vec<PromptTemplateRow>> {
    prompt_templates::table
        .order(prompt_templates::sort_order.asc())
        .load::<PromptTemplateRow>(conn)
}

#[cfg(test)]
pub fn get_template(conn: &mut SqliteConnection, id: &str) -> QueryResult<PromptTemplateRow> {
    prompt_templates::table.find(id).first::<PromptTemplateRow>(conn)
}

pub fn create_template(conn: &mut SqliteConnection, new: &PromptTemplateInsert) -> QueryResult<PromptTemplateRow> {
    diesel::insert_into(prompt_templates::table).values(new).execute(conn)?;
    prompt_templates::table.find(new.id).first::<PromptTemplateRow>(conn)
}

pub fn update_template(
    conn: &mut SqliteConnection,
    id: &str,
    changeset: &PromptTemplateChangeset,
) -> QueryResult<PromptTemplateRow> {
    diesel::update(prompt_templates::table.find(id))
        .set(changeset)
        .execute(conn)?;
    prompt_templates::table.find(id).first::<PromptTemplateRow>(conn)
}

pub fn delete_template(conn: &mut SqliteConnection, id: &str) -> QueryResult<()> {
    diesel::delete(prompt_templates::table.find(id)).execute(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn make_template<'a>(id: &'a str, name: &'a str) -> PromptTemplateInsert<'a> {
        PromptTemplateInsert {
            id,
            name,
            description: None,
            category: "general",
            template_text: "You are {{assistant_name}}.",
            is_builtin: 0,
            sort_order: 0,
            created_at: 1000,
            updated_at: 1000,
        }
    }

    #[test]
    fn test_create_and_list() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_template(&mut conn, &make_template("t1", "Template 1")).unwrap();
        create_template(&mut conn, &make_template("t2", "Template 2")).unwrap();
        let list = list_templates(&mut conn).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_update() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_template(&mut conn, &make_template("t1", "Old")).unwrap();
        let updated = update_template(
            &mut conn,
            "t1",
            &PromptTemplateChangeset {
                name: Some("New".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(updated.name, "New");
    }

    #[test]
    fn test_delete() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        create_template(&mut conn, &make_template("t1", "Del")).unwrap();
        delete_template(&mut conn, "t1").unwrap();
        assert!(get_template(&mut conn, "t1").is_err());
    }
}
