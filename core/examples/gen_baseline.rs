//! Regenerates `src/db/sea/migration/m0001_baseline.rs` from the schema the 65
//! Diesel migrations build.
//!
//! ```text
//! cargo run -p meridian-core --example gen_baseline --features test-support
//! ```
//!
//! The migrations are replayed into a private in-memory database — nothing on
//! disk is opened — read back through the pragmas, and written out as the
//! baseline module. Run `cargo fmt` afterwards; the equivalence tests are what
//! say whether the result is right.

use std::path::Path;

use meridian_core::db::sea::baseline_gen;
use meridian_core::db::sea::introspect::Schema;
use meridian_core::db::sea::legacy;
use sea_orm::{ConnectionTrait, Database};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = Database::connect("sqlite::memory:").await?;
    // As production replays them: a table rebuild must not fire ON DELETE.
    db.execute_unprepared("PRAGMA foreign_keys = OFF").await?;
    legacy::replay_all(&db).await?;
    let schema = Schema::read(&db).await?;

    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/db/sea/migration/m0001_baseline.rs");
    std::fs::write(&target, baseline_gen::render(&schema))?;
    println!(
        "wrote {} ({} tables, {} indexes, {} triggers)",
        target.display(),
        schema.tables.len(),
        schema.indexes.len(),
        schema.triggers.len()
    );
    Ok(())
}
