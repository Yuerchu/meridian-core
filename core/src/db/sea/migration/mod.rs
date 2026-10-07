//! The SeaORM migrations, from the baseline on.
//!
//! The baseline is the 65 Diesel migrations folded into one: a database they
//! produced was read back (`introspect`) and written out as sea-query builders
//! by `baseline_gen`, so the file is generated and the equivalence tests are
//! what prove it right, not reading it. What sea-query has a builder for —
//! tables, columns, keys, foreign keys, indexes — is held as a builder and
//! rendered for the backend at hand. What it does not — trigger bodies,
//! `CHECK` expressions, a partial index's `WHERE` — is SQLite text and is
//! marked `backend: sqlite-only` where it appears; a PostgreSQL build renders
//! the builders for PostgreSQL and supplies its own text for the rest.
//!
//! Every migration exposes the SQLite statements it would run, and
//! `diesel_test_db` executes that same list: for as long as both ORMs are in
//! the tree there is one source for the schema and two readers of it.

pub mod m0001_baseline;
pub mod m0002_skill_keys;

use sea_orm::sea_query::{
    ColumnDef, ConditionalStatement, Expr, ForeignKey, ForeignKeyAction, ForeignKeyCreateStatement, Index,
    IndexCreateStatement, IndexOrder, SqliteQueryBuilder, Table, TableCreateStatement,
};
use sea_orm::{ConnectionTrait, DbBackend, DbErr};
use sea_orm_migration::{MigrationName, MigrationTrait, MigratorTrait, SchemaManager};

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(M0001Baseline), Box::new(M0002SkillKeys)]
    }
}

/// The name under which the baseline is recorded in `seaql_migrations`. The
/// bridge writes it by hand for a database that reached the same schema the
/// long way, so it is a constant rather than something derived from a file.
pub const BASELINE_NAME: &str = "m0001_baseline";

pub struct M0001Baseline;

impl MigrationName for M0001Baseline {
    fn name(&self) -> &str {
        BASELINE_NAME
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0001Baseline {
    /// One transaction for the 52 tables and the ledger row that says they
    /// exist. Without it the migrator runs `up` first and records it second,
    /// and a crash in between leaves tables with no record — which the bridge
    /// then refuses as a database it cannot classify. SQLite's DDL is
    /// transactional, so this costs nothing.
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        match conn.get_database_backend() {
            DbBackend::Sqlite => {
                for statement in m0001_baseline::sqlite_statements() {
                    conn.execute_unprepared(&statement).await?;
                }
                Ok(())
            }
            other => Err(DbErr::Migration(format!(
                "the baseline is rendered for SQLite only; {other:?} is not shipped yet"
            ))),
        }
    }
}

/// Every statement this build's migrations run on SQLite, in order: what
/// `diesel_test_db` executes, so the Diesel tests see the schema the SeaORM
/// migrator builds.
pub fn sqlite_statements() -> Vec<String> {
    let mut all = m0001_baseline::sqlite_statements();
    all.extend(m0002_skill_keys::sqlite_statements());
    all
}

pub struct M0002SkillKeys;

impl MigrationName for M0002SkillKeys {
    fn name(&self) -> &str {
        "m0002_skill_keys"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0002SkillKeys {
    /// The rebuild and its ledger row commit together, as the baseline's do.
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        if conn.get_database_backend() != DbBackend::Sqlite {
            return Err(DbErr::Migration(
                "m0002_skill_keys rebuilds SQLite tables; other backends start from the constrained schema".into(),
            ));
        }
        // With foreign keys on, dropping `skills` would cascade into every
        // binding. Refused before anything is touched rather than trusted.
        let on = conn
            .query_one_raw(sea_orm::Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA foreign_keys",
            ))
            .await?
            .map(|row| row.try_get_by_index::<i64>(0))
            .transpose()?
            .unwrap_or(0);
        if on != 0 {
            return Err(DbErr::Migration(
                "m0002_skill_keys must run with foreign keys off; dropping skills would cascade into its bindings"
                    .into(),
            ));
        }
        for statement in m0002_skill_keys::sqlite_statements() {
            conn.execute_unprepared(&statement).await?;
        }
        // What the rebuild leaves must satisfy the references it touched —
        // every foreign key that points at `skills` — checked before the
        // transaction commits. Only those: a binding whose assistant was
        // deleted while foreign keys were off is an older violation of another
        // key, real databases have them, and they are not this migration's to
        // judge or to delete (`foreign_key_check` names the parent in column 2).
        for table in m0002_skill_keys::CHECKED_TABLES {
            let rows = conn
                .query_all_raw(sea_orm::Statement::from_string(
                    DbBackend::Sqlite,
                    format!("PRAGMA foreign_key_check(\"{table}\")"),
                ))
                .await?;
            let mut onto_skills = 0;
            for row in rows {
                if row.try_get_by_index::<String>(2)? == "skills" {
                    onto_skills += 1;
                }
            }
            if onto_skills > 0 {
                return Err(DbErr::Migration(format!(
                    "m0002_skill_keys left {onto_skills} reference(s) from {table} to a skill that is not there"
                )));
            }
        }
        Ok(())
    }
}

/// One schema object of a migration, held before rendering.
#[derive(Debug, Clone)]
pub enum Object {
    Table(TableCreateStatement),
    Index(IndexCreateStatement),
    /// backend: sqlite-only — text sea-query has no builder for (triggers).
    Sqlite(String),
}

/// Renders `objects` as the statements SQLite runs, in order.
pub fn render_sqlite(objects: &[Object]) -> Vec<String> {
    objects
        .iter()
        .map(|object| match object {
            Object::Table(table) => table.to_string(SqliteQueryBuilder),
            Object::Index(index) => index.to_string(SqliteQueryBuilder),
            Object::Sqlite(sql) => sql.clone(),
        })
        .collect()
}

// The vocabulary the generated migration is written in. Small on purpose: a
// reader of `m0001_baseline.rs` should see the schema, not sea-query.

pub(crate) fn table(name: &'static str) -> TableCreateStatement {
    Table::create().table(name).to_owned()
}

pub(crate) fn text(name: &'static str) -> ColumnDef {
    ColumnDef::new(name).text().to_owned()
}

pub(crate) fn integer(name: &'static str) -> ColumnDef {
    ColumnDef::new(name).integer().to_owned()
}

pub(crate) fn big_int(name: &'static str) -> ColumnDef {
    ColumnDef::new(name).big_integer().to_owned()
}

pub(crate) fn double(name: &'static str) -> ColumnDef {
    ColumnDef::new(name).double().to_owned()
}

/// `FOREIGN KEY (columns) REFERENCES table (to) ON DELETE action`. Only
/// `ON DELETE` is ever set; every foreign key here leaves `ON UPDATE` at the
/// default.
pub(crate) fn fk(
    columns: &[&'static str],
    references: &'static str,
    to: &[&'static str],
    on_delete: ForeignKeyAction,
) -> ForeignKeyCreateStatement {
    let mut key = ForeignKey::create();
    for column in columns {
        key.from_col(*column);
    }
    key.to_tbl(references);
    for column in to {
        key.to_col(*column);
    }
    if !matches!(on_delete, ForeignKeyAction::NoAction) {
        key.on_delete(on_delete);
    }
    key.to_owned()
}

/// A table-level `UNIQUE (columns)` constraint.
pub(crate) fn unique(columns: &[&'static str]) -> IndexCreateStatement {
    let mut index = Index::create();
    index.unique();
    for column in columns {
        index.col(*column);
    }
    index.to_owned()
}

/// A table-level `PRIMARY KEY (columns)`.
pub(crate) fn key(columns: &[&'static str]) -> IndexCreateStatement {
    let mut index = Index::create();
    for column in columns {
        index.col(*column);
    }
    index.to_owned()
}

/// A `CHECK` expression. backend: sqlite-only — the text is passed through.
pub(crate) fn check(expression: &'static str) -> Expr {
    Expr::cust(expression)
}

pub(crate) fn t(statement: &mut TableCreateStatement) -> Object {
    Object::Table(statement.to_owned())
}

/// `CREATE [UNIQUE] INDEX name ON table (columns) [WHERE filter]`. The filter,
/// when there is one, is passed through: backend: sqlite-only.
pub(crate) fn index(
    name: &'static str,
    on: &'static str,
    columns: &[(&'static str, IndexOrder)],
    is_unique: bool,
    filter: Option<&'static str>,
) -> Object {
    let mut index = Index::create();
    index.name(name).table(on);
    if is_unique {
        index.unique();
    }
    for (column, order) in columns {
        index.col((*column, order.clone()));
    }
    if let Some(filter) = filter {
        index.and_where(Expr::cust(filter));
    }
    Object::Index(index.to_owned())
}

/// A trigger, verbatim. backend: sqlite-only.
pub(crate) fn trigger(sql: &'static str) -> Object {
    Object::Sqlite(sql.to_owned())
}
