//! Regenerates `schema.snapshot.sql` from the schema the SeaORM migrations build.
//!
//! ```text
//! cargo run -p meridian-core --example gen_schema_snapshot --features test-support
//! ```
//!
//! A private in-memory database is migrated the way a new install is and read
//! back as DDL. The checkers in the shell (`check-db-schema`,
//! `check-model-contracts`, `check-provider-catalog`) load that file instead of
//! replaying migrations, and `schema_drift` pins it: a migration that changes
//! the schema without this file being regenerated is a red test.

use std::path::Path;

use meridian_core::db::sea::{bridge, introspect, memory_connection};
use sea_orm::ConnectionTrait;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // As `sea_test_db` does, and as a new install does: the bridge finds a fresh
    // database and the baseline builds it, with foreign keys off for the run.
    let conn = memory_connection().await;
    conn.execute_unprepared("PRAGMA foreign_keys = OFF").await?;
    bridge::migrate(&conn).await?;
    let ddl = introspect::ddl(&conn).await?;
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("schema.snapshot.sql");
    std::fs::write(&target, &ddl)?;
    println!(
        "wrote {} ({} statements)",
        target.display(),
        ddl.matches(";\n\n").count()
    );
    Ok(())
}
