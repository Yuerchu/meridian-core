//! Framework-free protocol contracts, wire helpers, capability traits, and
//! declarative schemas for Meridian providers.

pub mod auth;
pub mod balance;
pub mod chat;
pub mod chat_state;
pub mod client;
pub mod codex_metadata;
#[cfg(feature = "conformance")]
pub mod conformance;
pub mod decimal;
pub mod domain;
pub mod embedding;
pub mod media;
pub mod media_gen;
pub mod schema;
pub mod tts;
pub mod web_search;
pub mod wire;

/// Optional SeaORM `TEXT` column mapping for closed domain enums that expose
/// `as_str(&self) -> &'static str` and `parse(&str) -> Result<Self, String>`.
#[cfg(feature = "sea-orm")]
#[doc(hidden)]
pub use sea_orm as __sea_orm;

#[cfg(feature = "sea-orm")]
#[macro_export]
macro_rules! impl_sea_orm_text_enum {
    ($ty:ty) => {
        impl From<$ty> for $crate::__sea_orm::Value {
            fn from(value: $ty) -> Self {
                $crate::__sea_orm::Value::String(Some(value.as_str().to_owned()))
            }
        }

        impl $crate::__sea_orm::TryGetable for $ty {
            fn try_get_by<I: $crate::__sea_orm::ColIdx>(
                res: &$crate::__sea_orm::QueryResult,
                index: I,
            ) -> Result<Self, $crate::__sea_orm::TryGetError> {
                let raw = String::try_get_by(res, index)?;
                <$ty>::parse(&raw)
                    .map_err(|error| $crate::__sea_orm::TryGetError::DbErr($crate::__sea_orm::DbErr::Type(error)))
            }
        }

        impl $crate::__sea_orm::sea_query::ValueType for $ty {
            fn try_from(
                value: $crate::__sea_orm::Value,
            ) -> Result<Self, $crate::__sea_orm::sea_query::ValueTypeErr> {
                match value {
                    $crate::__sea_orm::Value::String(Some(raw)) => {
                        <$ty>::parse(&raw).map_err(|_| $crate::__sea_orm::sea_query::ValueTypeErr)
                    }
                    _ => Err($crate::__sea_orm::sea_query::ValueTypeErr),
                }
            }

            fn type_name() -> String {
                stringify!($ty).to_owned()
            }

            fn array_type() -> $crate::__sea_orm::sea_query::ArrayType {
                $crate::__sea_orm::sea_query::ArrayType::String
            }

            fn column_type() -> $crate::__sea_orm::sea_query::ColumnType {
                $crate::__sea_orm::sea_query::ColumnType::Text
            }
        }

        impl $crate::__sea_orm::sea_query::Nullable for $ty {
            fn null() -> $crate::__sea_orm::Value {
                $crate::__sea_orm::Value::String(None)
            }
        }

        impl $crate::__sea_orm::IntoActiveValue<$ty> for $ty {
            fn into_active_value(self) -> $crate::__sea_orm::ActiveValue<$ty> {
                $crate::__sea_orm::ActiveValue::Set(self)
            }
        }
    };
}
