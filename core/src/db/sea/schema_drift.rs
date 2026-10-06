//! Holds the entities against the schema, and the schema against its snapshot.
//!
//! Diesel's `schema.rs` made a column that moved under a struct a compile
//! error. SeaORM has nothing of the kind: an entity names its columns and types
//! and the database is trusted to agree. These tests are the replacement —
//! every registered entity's shape (`db::entity::shape_of`) is compared with
//! the live table read back through `introspect`, every table is either
//! registered or on the closed backlog, and the DDL the migrations build is
//! pinned to `schema.snapshot.sql` so the checkers outside this crate read
//! what the code builds.
//!
//! Why the comparison is structural rather than "select once and see": with
//! `Option<T>: TryGetable`, a column the query did not return reads as `None`
//! instead of `ColumnNotFound`, so `find().all()` cannot notice that a nullable
//! column is gone from the table. The smoke query on the fixture below is only
//! a smoke; the shape comparison is the gate.

use std::collections::BTreeSet;

use sea_orm::EntityTrait;

use super::cap::sealed::Access;
use super::introspect::{self, Schema};
use super::sea_test_db;
use crate::db::entity::{EntityShape, ForeignKeyShape, PENDING_TABLES, registered, shape_of};

/// Every way `shape` disagrees with the table of the same name in `schema`,
/// one line each. Empty means the entity is a complete description of its
/// table: same columns with the same affinity and nullability, same primary
/// key, and the same foreign keys in both directions — a key the schema has
/// and the entity does not mention is a problem too.
pub fn drift(shape: &EntityShape, schema: &Schema) -> Vec<String> {
    let table = &shape.table;
    let Some(actual) = schema.tables.iter().find(|t| &t.name == table) else {
        return vec![format!("table `{table}` does not exist in the schema")];
    };
    let mut problems = Vec::new();

    let declared: BTreeSet<&str> = shape.columns.iter().map(|c| c.name.as_str()).collect();
    let present: BTreeSet<&str> = actual.columns.iter().map(|c| c.name.as_str()).collect();
    for name in present.difference(&declared) {
        problems.push(format!("entity for `{table}` lacks column `{name}` the schema has"));
    }
    for name in declared.difference(&present) {
        problems.push(format!(
            "entity for `{table}` declares column `{name}` the schema lacks"
        ));
    }
    for column in &shape.columns {
        let Some(live) = actual.columns.iter().find(|c| c.name == column.name) else {
            continue;
        };
        let live_affinity = introspect::affinity(&live.declared_type);
        if live_affinity != column.affinity {
            problems.push(format!(
                "column `{table}.{}`: entity declares affinity {}, schema has {live_affinity} (declared `{}`)",
                column.name, column.affinity, live.declared_type
            ));
        }
        // SQLite reports `INTEGER PRIMARY KEY` with notnull=0 although it can
        // never hold NULL; a key column is NOT NULL whatever the pragma says.
        let live_not_null = live.not_null || live.in_primary_key;
        if live_not_null == column.nullable {
            problems.push(format!(
                "column `{table}.{}`: entity declares it {}, schema has it {}",
                column.name,
                if column.nullable { "nullable" } else { "NOT NULL" },
                if live_not_null { "NOT NULL" } else { "nullable" },
            ));
        }
    }

    let declared_key: BTreeSet<&str> = shape.primary_key.iter().map(String::as_str).collect();
    let present_key: BTreeSet<&str> = actual.primary_key.iter().map(String::as_str).collect();
    if declared_key != present_key {
        problems.push(format!(
            "primary key of `{table}`: entity declares ({}), schema has ({})",
            shape.primary_key.join(", "),
            actual.primary_key.join(", ")
        ));
    }

    let describe = |fk: &ForeignKeyShape| {
        format!(
            "foreign key `{table}`({}) → `{}`({}) ON DELETE {}",
            fk.columns.join(", "),
            fk.references_table,
            fk.references_columns.join(", "),
            fk.on_delete
        )
    };
    let live_keys: Vec<ForeignKeyShape> = actual
        .foreign_keys
        .iter()
        .map(|fk| ForeignKeyShape {
            columns: fk.columns.clone(),
            references_table: fk.references_table.clone(),
            references_columns: fk.references_columns.clone(),
            on_delete: fk.on_delete.clone(),
        })
        .collect();
    for fk in &shape.foreign_keys {
        if !live_keys.contains(fk) {
            problems.push(format!(
                "{}: the entity declares it, the schema has no such key",
                describe(fk)
            ));
        }
    }
    for fk in &live_keys {
        if !shape.foreign_keys.contains(fk) {
            problems.push(format!(
                "{}: the schema has it, the entity does not declare it",
                describe(fk)
            ));
        }
    }
    problems
}

/// The registry test's logic, over names only so it can be exercised on
/// lists that the real schema never produces. Every table in the schema is
/// registered or pending, never both, and neither list names a table the
/// schema does not have.
pub fn registry_problems(registered_tables: &[&str], pending: &[&str], schema_tables: &[&str]) -> Vec<String> {
    let registered: BTreeSet<&str> = registered_tables.iter().copied().collect();
    let pending: BTreeSet<&str> = pending.iter().copied().collect();
    let schema: BTreeSet<&str> = schema_tables.iter().copied().collect();
    let mut problems = Vec::new();
    for table in &schema {
        match (registered.contains(table), pending.contains(table)) {
            (true, true) => problems.push(format!("`{table}` is registered, remove it from PENDING_TABLES")),
            (false, false) => problems.push(format!(
                "`{table}` has no entity and is not in PENDING_TABLES; a new table comes with its entity \
                 (PENDING_TABLES is closed)"
            )),
            _ => {}
        }
    }
    for table in registered.difference(&schema) {
        problems.push(format!(
            "`{table}` has a registered entity but the schema has no such table"
        ));
    }
    for table in pending.difference(&schema) {
        problems.push(format!(
            "`{table}` is in PENDING_TABLES but the schema has no such table"
        ));
    }
    problems
}

#[tokio::test]
async fn the_snapshot_is_what_the_migrations_build() {
    let db = sea_test_db().await;
    let ddl = introspect::ddl(db.conn().unwrap()).await.unwrap();
    assert!(
        ddl == include_str!("../../../schema.snapshot.sql"),
        "schema.snapshot.sql is not what the migrations build; regenerate it with\n  \
         cargo run -p meridian-core --example gen_schema_snapshot --features test-support"
    );
}

#[tokio::test]
async fn every_table_is_registered_or_pending() {
    let db = sea_test_db().await;
    let schema = Schema::read(db.conn().unwrap()).await.unwrap();
    let schema_tables: Vec<&str> = schema.tables.iter().map(|t| t.name.as_str()).collect();
    let registered = registered();
    let registered_tables: Vec<&str> = registered.iter().map(|s| s.table.as_str()).collect();
    let problems = registry_problems(&registered_tables, PENDING_TABLES, &schema_tables);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn pending_tables_are_sorted_and_unique() {
    let mut sorted = PENDING_TABLES.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        PENDING_TABLES,
        sorted.as_slice(),
        "PENDING_TABLES must be sorted and unique"
    );
}

#[tokio::test]
async fn every_registered_entity_matches_its_table() {
    let db = sea_test_db().await;
    let schema = Schema::read(db.conn().unwrap()).await.unwrap();
    let problems: Vec<String> = registered().iter().flat_map(|shape| drift(shape, &schema)).collect();
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn registry_problems_name_each_kind_of_mismatch() {
    let problems = registry_problems(
        &["a", "both", "ghost"],
        &["both", "pending", "gone"],
        &["a", "both", "pending", "orphan"],
    );
    assert_eq!(
        problems,
        [
            "`both` is registered, remove it from PENDING_TABLES",
            "`orphan` has no entity and is not in PENDING_TABLES; a new table comes with its entity \
             (PENDING_TABLES is closed)",
            "`ghost` has a registered entity but the schema has no such table",
            "`gone` is in PENDING_TABLES but the schema has no such table",
        ]
    );
    assert!(registry_problems(&["a"], &["b"], &["a", "b"]).is_empty());
}

/// Fixtures on `queued_prompts`: one `belongs_to` with `ON DELETE CASCADE`
/// (so a lost action is visible), six nullable columns, a text primary key,
/// and no CHECK or UNIQUE that the shape would not see anyway.
mod fixtures {
    /// The referenced side, as far as a `belongs_to` needs it to exist.
    pub mod conversations {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "conversations")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    pub mod correct {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "queued_prompts")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: String,
            pub conversation_id: String,
            pub content: String,
            pub delivery: String,
            pub position: i64,
            pub created_at: i64,
            pub dispatched_at: Option<i64>,
            pub dispatched_turn_id: Option<String>,
            pub settled_at: Option<i64>,
            pub settled_message_id: Option<String>,
            pub held_at: Option<i64>,
            pub reported_at: Option<i64>,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {
            #[sea_orm(
                belongs_to = "super::conversations::Entity",
                from = "Column::ConversationId",
                to = "super::conversations::Column::Id",
                on_delete = "Cascade"
            )]
            Conversation,
        }

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// `reported_at` deleted from the model.
    pub mod missing_column {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "queued_prompts")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: String,
            pub conversation_id: String,
            pub content: String,
            pub delivery: String,
            pub position: i64,
            pub created_at: i64,
            pub dispatched_at: Option<i64>,
            pub dispatched_turn_id: Option<String>,
            pub settled_at: Option<i64>,
            pub settled_message_id: Option<String>,
            pub held_at: Option<i64>,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {
            #[sea_orm(
                belongs_to = "super::conversations::Entity",
                from = "Column::ConversationId",
                to = "super::conversations::Column::Id",
                on_delete = "Cascade"
            )]
            Conversation,
        }

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// `dispatched_turn_id` as `String` where the column is nullable.
    pub mod wrong_nullability {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "queued_prompts")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: String,
            pub conversation_id: String,
            pub content: String,
            pub delivery: String,
            pub position: i64,
            pub created_at: i64,
            pub dispatched_at: Option<i64>,
            pub dispatched_turn_id: String,
            pub settled_at: Option<i64>,
            pub settled_message_id: Option<String>,
            pub held_at: Option<i64>,
            pub reported_at: Option<i64>,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {
            #[sea_orm(
                belongs_to = "super::conversations::Entity",
                from = "Column::ConversationId",
                to = "super::conversations::Column::Id",
                on_delete = "Cascade"
            )]
            Conversation,
        }

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// The `belongs_to` without its `on_delete`.
    pub mod wrong_on_delete {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "queued_prompts")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: String,
            pub conversation_id: String,
            pub content: String,
            pub delivery: String,
            pub position: i64,
            pub created_at: i64,
            pub dispatched_at: Option<i64>,
            pub dispatched_turn_id: Option<String>,
            pub settled_at: Option<i64>,
            pub settled_message_id: Option<String>,
            pub held_at: Option<i64>,
            pub reported_at: Option<i64>,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {
            #[sea_orm(
                belongs_to = "super::conversations::Entity",
                from = "Column::ConversationId",
                to = "super::conversations::Column::Id"
            )]
            Conversation,
        }

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// A table the schema does not have.
    pub mod wrong_table {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "queued_prompts_v2")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// A `priority` column the table does not have.
    pub mod extra_column {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "queued_prompts")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: String,
            pub conversation_id: String,
            pub content: String,
            pub delivery: String,
            pub position: i64,
            pub priority: i64,
            pub created_at: i64,
            pub dispatched_at: Option<i64>,
            pub dispatched_turn_id: Option<String>,
            pub settled_at: Option<i64>,
            pub settled_message_id: Option<String>,
            pub held_at: Option<i64>,
            pub reported_at: Option<i64>,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {
            #[sea_orm(
                belongs_to = "super::conversations::Entity",
                from = "Column::ConversationId",
                to = "super::conversations::Column::Id",
                on_delete = "Cascade"
            )]
            Conversation,
        }

        impl ActiveModelBehavior for ActiveModel {}
    }
}

#[tokio::test]
async fn a_correct_entity_has_no_drift_and_queries() {
    let db = sea_test_db().await;
    let conn = db.conn().unwrap();
    let schema = Schema::read(conn).await.unwrap();
    let shape = shape_of::<fixtures::correct::Entity>();
    assert_eq!(shape.table, "queued_prompts");
    assert_eq!(shape.primary_key, ["id"]);
    assert_eq!(
        shape.foreign_keys,
        [ForeignKeyShape {
            columns: vec!["conversation_id".into()],
            references_table: "conversations".into(),
            references_columns: vec!["id".into()],
            on_delete: "CASCADE".into(),
        }]
    );
    assert_eq!(drift(&shape, &schema), Vec::<String>::new());
    // Only a smoke (see the module doc): the structural comparison above is
    // what would notice a missing nullable column.
    let rows = fixtures::correct::Entity::find().all(conn).await.unwrap();
    assert!(rows.is_empty());
}

/// One mutant per kind of drift, each asserted to produce a message that
/// names the column or key it broke.
#[tokio::test]
async fn each_kind_of_drift_is_reported() {
    let db = sea_test_db().await;
    let schema = Schema::read(db.conn().unwrap()).await.unwrap();

    fn expect_one(problems: Vec<String>, needle: &str) {
        assert_eq!(problems.len(), 1, "expected one problem, got {problems:#?}");
        assert!(
            problems[0].contains(needle),
            "{:?} does not mention {needle:?}",
            problems[0]
        );
    }

    expect_one(
        drift(&shape_of::<fixtures::missing_column::Entity>(), &schema),
        "entity for `queued_prompts` lacks column `reported_at` the schema has",
    );
    expect_one(
        drift(&shape_of::<fixtures::wrong_nullability::Entity>(), &schema),
        "column `queued_prompts.dispatched_turn_id`: entity declares it NOT NULL, schema has it nullable",
    );
    let on_delete = drift(&shape_of::<fixtures::wrong_on_delete::Entity>(), &schema);
    assert_eq!(
        on_delete,
        [
            "foreign key `queued_prompts`(conversation_id) → `conversations`(id) ON DELETE NO ACTION: \
             the entity declares it, the schema has no such key",
            "foreign key `queued_prompts`(conversation_id) → `conversations`(id) ON DELETE CASCADE: \
             the schema has it, the entity does not declare it",
        ]
    );
    expect_one(
        drift(&shape_of::<fixtures::wrong_table::Entity>(), &schema),
        "table `queued_prompts_v2` does not exist in the schema",
    );
    expect_one(
        drift(&shape_of::<fixtures::extra_column::Entity>(), &schema),
        "entity for `queued_prompts` declares column `priority` the schema lacks",
    );
}
