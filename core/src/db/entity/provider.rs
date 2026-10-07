//! `providers`: the endpoints models are requested from.
//!
//! Four columns are the provider registry's own enums and decode into them —
//! the three that pick an adapter (`provider_type`, `api_format`,
//! `transport_profile`) and `credential_kind`. A row with a spelling the
//! registry does not know fails the read rather than reaching adapter
//! selection as a string.
//!
//! The entity exists ahead of the provider ops, which still run on Diesel
//! inside the plan-review barrier transactions: assistants reference this
//! table, and the drift test checks a reference only when both ends have an
//! entity.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, SqlBool, text_enum_column};
use crate::provider::registry::{ApiFormat, CredentialKind, ProviderType, TransportProfile};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "providers")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    pub provider_type: ProviderType,
    pub base_url: String,
    pub is_enabled: SqlBool,
    pub sort_order: i32,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
    pub api_format: ApiFormat,
    /// Which entry in `provider_catalog.json` this row is an instance of, or
    /// `None` for one the catalog does not describe. Display and prefill only.
    pub catalog_id: Option<String>,
    pub credential_kind: CredentialKind,
    pub transport_profile: TransportProfile,
    /// A logo's name in the front end's icon set: free text, read by nothing
    /// but the renderer.
    pub icon: Option<String>,
    /// Whether requests to this row are shaped exactly the way Codex shapes its
    /// own — see migration 63.
    pub codex_request_shape: SqlBool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::assistant::Entity")]
    Assistant,
}

impl Related<super::assistant::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Assistant.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// A partial update: a field left `None` is not written. `catalog_id` and
/// `icon` are nullable, so `Some(None)` clears one.
#[derive(Debug, Default, DeriveIntoActiveModel)]
pub struct ProviderChangeset {
    pub name: Option<String>,
    pub provider_type: Option<ProviderType>,
    pub base_url: Option<String>,
    pub is_enabled: Option<SqlBool>,
    pub sort_order: Option<i32>,
    pub updated_at: Option<EpochMs>,
    pub api_format: Option<ApiFormat>,
    pub credential_kind: Option<CredentialKind>,
    pub transport_profile: Option<TransportProfile>,
    pub catalog_id: Option<Option<String>>,
    pub icon: Option<Option<String>>,
    pub codex_request_shape: Option<SqlBool>,
}

// The registry's enums are used far beyond this table; none of the four
// columns has a `CHECK`, so each type's `parse` is what keeps it closed.
text_enum_column!(ProviderType);
text_enum_column!(ApiFormat);
text_enum_column!(CredentialKind);
text_enum_column!(TransportProfile);

#[cfg(test)]
mod tests {
    use sea_orm::sea_query::ValueType;
    use strum::IntoEnumIterator;

    use super::*;

    /// The stored spelling of every variant is its wire spelling and reads
    /// back as itself; an unknown one fails.
    fn round_trips<T>()
    where
        T: IntoEnumIterator + serde::Serialize + ValueType + Into<Value> + PartialEq + std::fmt::Debug + Copy,
    {
        for variant in T::iter() {
            let stored = match variant.into() {
                Value::String(Some(raw)) => raw,
                other => panic!("{variant:?} stored as {other:?}"),
            };
            assert_eq!(
                serde_json::to_value(variant).unwrap().as_str(),
                Some(stored.as_str()),
                "{variant:?}"
            );
            assert_eq!(
                <T as ValueType>::try_from(Value::String(Some(stored))).unwrap(),
                variant
            );
        }
        assert!(<T as ValueType>::try_from(Value::String(Some("invented".into()))).is_err());
    }

    #[test]
    fn the_registry_enums_store_their_wire_spelling() {
        round_trips::<ProviderType>();
        round_trips::<ApiFormat>();
        round_trips::<CredentialKind>();
        round_trips::<TransportProfile>();
    }
}
