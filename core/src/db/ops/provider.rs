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
                icon: None,
                codex_request_shape: 0,
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

    /// The chosen logo needs the same two layers, and for a sharper reason
    /// than `catalog_id`: clearing it is a thing the user does by hand — it is
    /// the "follow the catalog again" entry in the picker — rather than
    /// something recomputed behind them. Collapsed to one layer that entry
    /// would silently do nothing, leaving the old mark on the row while the
    /// picker showed the default as selected.
    #[test]
    fn the_chosen_logo_can_be_set_changed_and_put_back_to_the_default() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let seed = seeded(&mut conn);
        assert_eq!(seed.icon, None, "a new row follows the catalog");

        let chosen = update_provider(
            &mut conn,
            "p1",
            &ProviderChangeset {
                icon: Some(Some("vertexai".into())),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(chosen.icon.as_deref(), Some("vertexai"));

        let renamed = update_provider(
            &mut conn,
            "p1",
            &ProviderChangeset {
                name: Some("Claude on Vertex".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(renamed.icon.as_deref(), Some("vertexai"), "an ordinary edit keeps it");

        let defaulted = update_provider(
            &mut conn,
            "p1",
            &ProviderChangeset {
                icon: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            defaulted.icon, None,
            "back to the catalog is a written answer, not a skip"
        );
    }
}
