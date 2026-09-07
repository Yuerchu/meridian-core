use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::provider::{ProviderChangeset, ProviderInsert, ProviderRow};
#[allow(unused_imports)]
use crate::db::schema::providers;

pub fn list_providers(conn: &mut SqliteConnection) -> QueryResult<Vec<ProviderRow>> {
    providers::table
        .order(providers::sort_order.asc())
        .load::<ProviderRow>(conn)
}

pub fn get_provider(conn: &mut SqliteConnection, id: &str) -> QueryResult<ProviderRow> {
    providers::table.find(id).first::<ProviderRow>(conn)
}

pub fn create_provider(conn: &mut SqliteConnection, new: &ProviderInsert) -> QueryResult<ProviderRow> {
    diesel::insert_into(providers::table).values(new).execute(conn)?;
    providers::table.find(new.id).first::<ProviderRow>(conn)
}

pub fn update_provider(
    conn: &mut SqliteConnection,
    id: &str,
    changeset: &ProviderChangeset,
) -> QueryResult<ProviderRow> {
    diesel::update(providers::table.find(id)).set(changeset).execute(conn)?;
    providers::table.find(id).first::<ProviderRow>(conn)
}

pub fn delete_provider(conn: &mut SqliteConnection, id: &str) -> QueryResult<()> {
    diesel::delete(providers::table.find(id)).execute(conn)?;
    Ok(())
}

pub fn count_providers(conn: &mut SqliteConnection) -> QueryResult<i64> {
    providers::table.count().get_result(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(conn: &mut SqliteConnection) -> ProviderRow {
        create_provider(
            conn,
            &ProviderInsert {
                id: "p1",
                name: "OpenAI",
                provider_type: "openai",
                base_url: "https://api.openai.com/v1",
                is_enabled: 1,
                sort_order: 0,
                created_at: 0,
                updated_at: 0,
                api_format: "responses",
                catalog_id: Some("openai"),
                credential_kind: "api_key",
                transport_profile: "standard",
            },
        )
        .unwrap()
    }

    /// The two layers of `catalog_id` mean different things and diesel has to
    /// honour both: outer `None` leaves the column alone, `Some(None)` clears
    /// it, `Some(Some(_))` rewrites it. Collapsed to one layer, "recomputed to
    /// no identity" becomes indistinguishable from "not recomputed" — and a
    /// re-typed row keeps the old vendor's logo forever, which is the defect
    /// this field exists to end.
    #[test]
    fn the_catalog_identity_changeset_says_clear_and_keep_apart() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        seeded(&mut conn);

        let untouched = update_provider(
            &mut conn,
            "p1",
            &ProviderChangeset {
                name: Some("Renamed".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            untouched.catalog_id.as_deref(),
            Some("openai"),
            "an ordinary edit keeps it"
        );

        let cleared = update_provider(
            &mut conn,
            "p1",
            &ProviderChangeset {
                catalog_id: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cleared.catalog_id, None, "no identity is a written answer, not a skip");

        let rewritten = update_provider(
            &mut conn,
            "p1",
            &ProviderChangeset {
                catalog_id: Some(Some("anthropic".into())),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rewritten.catalog_id.as_deref(), Some("anthropic"));
    }
}
