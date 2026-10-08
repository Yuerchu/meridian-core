//! The SeaORM entities, one module per table.
//!
//! Empty at the start of Phase 3: every table is still read and written
//! through Diesel, and each transaction root that moves over brings the
//! entities it needs and takes their tables out of [`PENDING_TABLES`]. The
//! drift test (`sea::schema_drift`) holds each registered entity against the
//! live schema — columns, affinities, nullability, keys, foreign keys — so a
//! migration that changes a table without its entity following is a red test
//! rather than a runtime surprise. Diesel gave that check for free at compile
//! time through `schema.rs`; this is where it lives now.

pub mod acp_context_delivery;
pub mod acp_session;
pub mod acp_session_notice;
pub mod assistant;
pub mod assistant_emoji_pack;
pub mod audit_message;
pub mod background_task;
pub mod cached_model;
pub mod composer_draft;
pub mod conversation;
pub mod custom_tool;
pub mod emoji;
pub mod emoji_pack;
pub mod journal_blob;
pub mod journal_file;
pub mod journal_version;
pub mod mcp_server;
pub mod memory;
pub mod memory_proposal;
pub mod memory_subject;
pub mod message;
pub mod message_context_item;
pub mod message_sticker;
pub mod mode_artifact;
pub mod model_config;
pub mod model_profile;
pub mod notification_alert_state;
pub mod notification_webhook;
pub mod plan_comment;
pub mod plan_document;
pub mod plan_materialization;
pub mod plan_review_delivery;
pub mod plan_review_draft;
pub mod plan_review_session;
pub mod plan_revision;
pub mod preference;
pub mod project;
pub mod provider;
pub mod queued_prompt;
pub mod queued_prompt_context_item;
pub mod redaction_rule;
pub mod skill;
pub mod skill_binding_assistant;
pub mod skill_binding_global;
pub mod skill_binding_project;
pub mod todo_item;
pub mod todo_list;
pub mod tool_category;
pub mod tool_preset;
pub mod turn;
pub mod voice_blob;
pub mod voice_clip;
pub mod voice_sender_optout;

/// The tables that have no entity yet.
///
/// Written once, at the end of Phase 2, with every table the baseline builds;
/// from then on names only leave. The transaction-graph checker keeps the
/// list as part of `docs/migration-counters.json` and refuses a name that was
/// not in it — a table created during the coexistence period comes with its
/// entity, it does not join the backlog — and refuses a list that grew. Phase
/// 5 needs it empty. Sorted and one per line, because the checker parses it.
pub const PENDING_TABLES: &[&str] = &[];

/// What an entity claims about its table, in the vocabulary the schema reader
/// (`sea::introspect`) speaks: affinities rather than Rust or sea-query types,
/// foreign keys as `pragma foreign_key_list` reports them. The drift test
/// compares one of these with the live table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityShape {
    pub table: String,
    pub columns: Vec<ColumnShape>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKeyShape>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnShape {
    pub name: String,
    /// `TEXT`, `INTEGER`, `REAL`, `BLOB` or `NUMERIC`, as `introspect::affinity`
    /// reduces a declared type.
    pub affinity: &'static str,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ForeignKeyShape {
    pub columns: Vec<String>,
    pub references_table: String,
    pub references_columns: Vec<String>,
    /// As the pragma spells it: `CASCADE`, `SET NULL`, `NO ACTION`,
    /// `RESTRICT`, `SET DEFAULT`.
    pub on_delete: String,
}

/// The entities that have moved over, each as its shape.
///
/// Phase 3 appends `shape_of::<x::Entity>()` here for every entity it adds and
/// removes the table from [`PENDING_TABLES`] in the same commit; the drift
/// test holds each shape against the live table, and the registry test holds
/// the union of both lists against the schema.
pub fn registered() -> Vec<EntityShape> {
    vec![
        shape_of::<acp_context_delivery::Entity>(),
        shape_of::<acp_session::Entity>(),
        shape_of::<acp_session_notice::Entity>(),
        shape_of::<assistant::Entity>(),
        shape_of::<assistant_emoji_pack::Entity>(),
        shape_of::<audit_message::Entity>(),
        shape_of::<background_task::Entity>(),
        shape_of::<cached_model::Entity>(),
        shape_of::<composer_draft::Entity>(),
        shape_of::<conversation::Entity>(),
        shape_of::<custom_tool::Entity>(),
        shape_of::<emoji::Entity>(),
        shape_of::<emoji_pack::Entity>(),
        shape_of::<journal_blob::Entity>(),
        shape_of::<journal_file::Entity>(),
        shape_of::<journal_version::Entity>(),
        shape_of::<mcp_server::Entity>(),
        shape_of::<memory::Entity>(),
        shape_of::<memory_proposal::Entity>(),
        shape_of::<memory_subject::Entity>(),
        shape_of::<message::Entity>(),
        shape_of::<message_context_item::Entity>(),
        shape_of::<message_sticker::Entity>(),
        shape_of::<mode_artifact::Entity>(),
        shape_of::<model_config::Entity>(),
        shape_of::<model_profile::Entity>(),
        shape_of::<notification_alert_state::Entity>(),
        shape_of::<notification_webhook::Entity>(),
        shape_of::<plan_comment::Entity>(),
        shape_of::<plan_document::Entity>(),
        shape_of::<plan_materialization::Entity>(),
        shape_of::<plan_review_delivery::Entity>(),
        shape_of::<plan_review_draft::Entity>(),
        shape_of::<plan_review_session::Entity>(),
        shape_of::<plan_revision::Entity>(),
        shape_of::<preference::Entity>(),
        shape_of::<project::Entity>(),
        shape_of::<provider::Entity>(),
        shape_of::<queued_prompt::Entity>(),
        shape_of::<queued_prompt_context_item::Entity>(),
        shape_of::<redaction_rule::Entity>(),
        shape_of::<skill::Entity>(),
        shape_of::<skill_binding_assistant::Entity>(),
        shape_of::<skill_binding_global::Entity>(),
        shape_of::<skill_binding_project::Entity>(),
        shape_of::<todo_item::Entity>(),
        shape_of::<todo_list::Entity>(),
        shape_of::<tool_category::Entity>(),
        shape_of::<tool_preset::Entity>(),
        shape_of::<turn::Entity>(),
        shape_of::<voice_blob::Entity>(),
        shape_of::<voice_clip::Entity>(),
        shape_of::<voice_sender_optout::Entity>(),
    ]
}

/// Derives an entity's shape from SeaORM's own reflection: the table name, the
/// columns with their types and nullability, the primary key, and every
/// `belongs_to` relation as the foreign key it declares. A `has_one` or
/// `has_many` is the other side of someone else's key and declares nothing.
///
/// Panics on a column type this crate has not decided an affinity for, naming
/// the table, column and type: a new type gets a decision in [`affinity_of`],
/// not a guess. Every registered entity goes through here on every test run,
/// so the panic is caught before the entity is used.
pub fn shape_of<E: sea_orm::EntityTrait>() -> EntityShape {
    use sea_orm::sea_query::{TableName, TableRef};
    use sea_orm::{ColumnTrait, IdenStatic, Identity, Iterable, PrimaryKeyToColumn, RelationTrait, RelationType};

    let table = E::default().table_name().to_owned();

    let columns = E::Column::iter()
        .map(|column| {
            let def = column.def();
            let name = column.as_str().to_owned();
            let affinity = affinity_of(def.get_column_type()).unwrap_or_else(|type_name| {
                panic!("column {table}.{name} has type {type_name}, which has no SQLite affinity decision yet")
            });
            ColumnShape {
                name,
                affinity,
                nullable: def.is_null(),
            }
        })
        .collect();

    let primary_key = E::PrimaryKey::iter()
        .map(|key| key.into_column().as_str().to_owned())
        .collect();

    fn table_of(reference: &TableRef) -> String {
        match reference {
            TableRef::Table(TableName(_, name), _) => name.to_string(),
            other => panic!("a relation names {other:?} rather than a table"),
        }
    }
    fn names_of(identity: &Identity) -> Vec<String> {
        identity.iter().map(|iden| iden.to_string()).collect()
    }

    let foreign_keys = E::Relation::iter()
        .map(|relation| relation.def())
        // `belongs_to` builds a `HasOne` whose owner flag is off; `has_one`
        // and `has_many` are built from the reverse of a `belongs_to` with the
        // flag on. The flag, not the cardinality, says which side holds the key.
        .filter(|def| def.rel_type == RelationType::HasOne && !def.is_owner)
        .map(|def| {
            let from = table_of(&def.from_tbl);
            assert_eq!(
                from, table,
                "a belongs_to on {table} has its foreign key on {from}; the relation is defined on the wrong entity"
            );
            ForeignKeyShape {
                columns: names_of(&def.from_col),
                references_table: table_of(&def.to_tbl),
                references_columns: names_of(&def.to_col),
                on_delete: pragma_action(def.on_delete),
            }
        })
        .collect();

    EntityShape {
        table,
        columns,
        primary_key,
        foreign_keys,
    }
}

/// The SQLite affinity a sea-query column type lands on when the baseline
/// declares it, by the names sea-query's SQLite builder emits. The error
/// carries the type's name for the panic in [`shape_of`].
///
/// Money is `TEXT` here on purpose: `crate::decimal::Decimal` declares itself
/// as `ColumnType::Text`, and a `Decimal`/`Money` column type would come from
/// a floating or scaled type this crate does not use — it still maps to `TEXT`
/// so that a monetary column is held to the text affinity the contract
/// requires. `SqlBool` declares `Integer`, which is how it arrives here.
fn affinity_of(column_type: &sea_orm::sea_query::ColumnType) -> Result<&'static str, String> {
    use sea_orm::sea_query::ColumnType;

    Ok(match column_type {
        ColumnType::Char(_)
        | ColumnType::String(_)
        | ColumnType::Text
        | ColumnType::Json
        | ColumnType::JsonBinary
        | ColumnType::Decimal(_)
        | ColumnType::Money(_) => "TEXT",
        ColumnType::TinyInteger
        | ColumnType::SmallInteger
        | ColumnType::Integer
        | ColumnType::BigInteger
        | ColumnType::TinyUnsigned
        | ColumnType::SmallUnsigned
        | ColumnType::Unsigned
        | ColumnType::BigUnsigned
        | ColumnType::Boolean => "INTEGER",
        ColumnType::Float | ColumnType::Double => "REAL",
        ColumnType::Blob | ColumnType::Binary(_) | ColumnType::VarBinary(_) => "BLOB",
        other => return Err(format!("{other:?}")),
    })
}

/// `pragma foreign_key_list` spells the action in upper case with a space;
/// a relation without one is `NO ACTION`, which is also SQLite's default.
fn pragma_action(action: Option<sea_orm::sea_query::ForeignKeyAction>) -> String {
    use sea_orm::sea_query::ForeignKeyAction;

    match action {
        None | Some(ForeignKeyAction::NoAction) => "NO ACTION",
        Some(ForeignKeyAction::Cascade) => "CASCADE",
        Some(ForeignKeyAction::SetNull) => "SET NULL",
        Some(ForeignKeyAction::Restrict) => "RESTRICT",
        Some(ForeignKeyAction::SetDefault) => "SET DEFAULT",
        // The enum is `#[non_exhaustive]`; a variant sea-query adds later gets
        // a spelling here rather than a guess.
        Some(other) => panic!("foreign key action {other:?} has no pragma spelling yet"),
    }
    .to_owned()
}

/// The names a `CHECK (<column> IN (…))` on `table` allows, read out of the
/// schema snapshot, for tests that hold an enum's stored values to the live
/// constraint. A table's DDL can span lines there (SQLite keeps the line
/// breaks a migration wrote), so the statement is cut from its `CREATE
/// TABLE` to the blank line that ends it.
#[cfg(test)]
pub(crate) fn allowed_by_check(table: &str, column: &str) -> std::collections::BTreeSet<String> {
    let snapshot = include_str!("../../../schema.snapshot.sql");
    let start = snapshot
        .find(&format!("CREATE TABLE \"{table}\" ("))
        .unwrap_or_else(|| panic!("the snapshot builds {table}"));
    let statement = &snapshot[start..];
    let statement = &statement[..statement.find("\n\n").unwrap_or(statement.len())];
    let prefix = format!("CHECK ({column} IN (");
    let start = statement
        .find(&prefix)
        .unwrap_or_else(|| panic!("a CHECK on {table}.{column}"))
        + prefix.len();
    let end = start + statement[start..].find("))").expect("the CHECK closes");
    statement[start..end]
        .split(',')
        .map(|name| name.trim().trim_matches('\'').to_owned())
        .collect()
}

/// The domain enums stored through `text_enum_column!` against the `CHECK`
/// on their column: each listed variant writes a spelling the column allows
/// and reads back as itself, and together they are the whole list (less any
/// variant this table deliberately refuses).
#[cfg(test)]
mod stored_domain_enums {
    use std::collections::BTreeSet;

    use super::allowed_by_check;
    use crate::agent::pricing::BillingMode;
    use crate::events::{AcpNoticeCategory, AcpNoticeSeverity};
    use crate::turn::TurnTrigger;
    use crate::workspace::reference::MessageContextKind;

    fn spellings<E: Copy + std::fmt::Debug + PartialEq>(
        variants: &[E],
        write: impl Fn(E) -> &'static str,
        read: impl Fn(&str) -> Result<E, String>,
    ) -> BTreeSet<String> {
        variants
            .iter()
            .map(|&variant| {
                let stored = write(variant);
                assert_eq!(read(stored).unwrap(), variant, "{stored}");
                stored.to_owned()
            })
            .collect()
    }

    #[test]
    fn every_checked_column_allows_exactly_its_enum() {
        use TurnTrigger::*;
        assert_eq!(
            spellings(
                &[User, PlanContinuation, TaskCompletion, AgentAutonomous],
                |v| v.as_str(),
                TurnTrigger::parse
            ),
            allowed_by_check("turns", "trigger")
        );

        use MessageContextKind::*;
        let kinds = [ProjectFile, ProjectDirectory, ShellOutput, Conversation];
        let all = spellings(&kinds, MessageContextKind::as_str, MessageContextKind::parse);
        assert_eq!(all, allowed_by_check("message_context_items", "kind"));
        // A queued prompt cannot carry a command's output: that is captured
        // when the message is sent, not when it is queued.
        let mut queued = all;
        queued.remove("shell_output");
        assert_eq!(queued, allowed_by_check("queued_prompt_context_items", "kind"));

        use BillingMode::*;
        assert_eq!(
            spellings(
                &[Metered, Subscription, External],
                BillingMode::as_str,
                BillingMode::parse
            ),
            allowed_by_check("audit_messages", "billing_mode")
        );

        use AcpNoticeCategory::*;
        assert_eq!(
            spellings(
                &[Connection, Access, Limit, Request, Service, Unknown],
                AcpNoticeCategory::as_str,
                AcpNoticeCategory::parse
            ),
            allowed_by_check("acp_session_notices", "category")
        );
        use AcpNoticeSeverity::*;
        assert_eq!(
            spellings(&[Warning, Error], AcpNoticeSeverity::as_str, AcpNoticeSeverity::parse),
            allowed_by_check("acp_session_notices", "severity")
        );
    }
}
