//! `custom_tools`: shell commands the user has wrapped as tools.
//!
//! `parameters_schema` decodes at the read into a JSON object, and `permission`
//! into [`Permission`]: a row with either broken fails the query instead of
//! loading as a tool with no parameters, or as one that asks when it was meant
//! never to run.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, Json, SqlBool, text_enum_column};
use crate::tools::Permission;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "custom_tools")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    #[sea_orm(unique)]
    pub name: String,
    pub description: String,
    pub category_id: Option<String>,
    pub parameters_schema: Json<serde_json::Map<String, serde_json::Value>>,
    pub command: String,
    pub args_template: Option<String>,
    pub working_directory: Option<String>,
    pub timeout_ms: Option<i32>,
    pub permission: Permission,
    pub is_enabled: SqlBool,
    pub sort_order: i32,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::tool_category::Entity",
        from = "Column::CategoryId",
        to = "super::tool_category::Column::Id",
        on_delete = "SetNull"
    )]
    Category,
}

impl Related<super::tool_category::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Category.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// A partial update: a field left `None` is not written. The nullable columns
/// are `Option<Option<_>>`, so `Some(None)` clears one.
#[derive(Debug, Default, DeriveIntoActiveModel)]
pub struct CustomToolChangeset {
    pub name: Option<String>,
    pub description: Option<String>,
    pub category_id: Option<Option<String>>,
    pub parameters_schema: Option<Json<serde_json::Map<String, serde_json::Value>>>,
    pub command: Option<String>,
    pub args_template: Option<Option<String>>,
    pub working_directory: Option<Option<String>>,
    pub timeout_ms: Option<Option<i32>>,
    pub permission: Option<Permission>,
    pub is_enabled: Option<SqlBool>,
    pub sort_order: Option<i32>,
    pub updated_at: Option<EpochMs>,
}

// `Permission` is the tool system's own type, used far beyond this table. The
// column has no `CHECK`; `Permission::parse` is what keeps the stored set
// closed.
text_enum_column!(Permission);

#[cfg(test)]
mod tests {
    use sea_orm::sea_query::ValueType;

    use super::*;

    /// The stored spelling is the wire spelling, for each of the three.
    #[test]
    fn stored_and_wire_spellings_are_one_list() {
        for permission in [Permission::Always, Permission::Ask, Permission::Never] {
            let stored = match Value::from(permission) {
                Value::String(Some(raw)) => raw,
                other => panic!("stored as {other:?}"),
            };
            assert_eq!(
                serde_json::to_value(permission).unwrap().as_str(),
                Some(stored.as_str())
            );
            assert_eq!(
                <Permission as ValueType>::try_from(Value::String(Some(stored))).unwrap(),
                permission
            );
        }
        assert!(<Permission as ValueType>::try_from(Value::String(Some("sometimes".into()))).is_err());
    }
}
