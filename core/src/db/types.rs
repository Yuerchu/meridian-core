//! Column types the SeaORM entities share.

use sea_orm::sea_query::{ArrayType, ColumnType, Nullable, ValueType, ValueTypeErr};
use sea_orm::{ActiveValue, ColIdx, DbErr, IntoActiveValue, QueryResult, TryGetError, TryGetable, Value};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Milliseconds since the Unix epoch, which is what every `*_at` column holds.
/// An alias rather than a newtype: timestamps are compared and subtracted
/// everywhere and there is nothing to validate.
pub type EpochMs = i64;

/// A flag stored as INTEGER 0 or 1.
///
/// Not `bool`: sqlx decodes any non-zero integer as `true`, so a 2 written by
/// hand or by a bad migration would read as a valid flag. This type refuses
/// anything but 0 and 1 at the boundary, the row failing to decode, and once
/// it exists it is always one of the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SqlBool(bool);

impl SqlBool {
    pub const FALSE: Self = Self(false);
    pub const TRUE: Self = Self(true);

    pub fn get(self) -> bool {
        self.0
    }

    fn stored(self) -> i32 {
        i32::from(self.0)
    }

    fn from_stored(raw: i32) -> Option<Self> {
        match raw {
            0 => Some(Self::FALSE),
            1 => Some(Self::TRUE),
            _ => None,
        }
    }
}

impl From<bool> for SqlBool {
    fn from(value: bool) -> Self {
        Self(value)
    }
}

/// A 0/1 flag carried as an integer outside the database (the prepared
/// context items do); anything else is an error, as it is at the read.
impl TryFrom<i32> for SqlBool {
    type Error = String;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self(false)),
            1 => Ok(Self(true)),
            other => Err(format!("expected a 0/1 flag, found {other}")),
        }
    }
}

impl From<SqlBool> for bool {
    fn from(value: SqlBool) -> Self {
        value.0
    }
}

impl From<SqlBool> for Value {
    fn from(value: SqlBool) -> Self {
        Value::Int(Some(value.stored()))
    }
}

impl TryGetable for SqlBool {
    fn try_get_by<I: ColIdx>(res: &QueryResult, index: I) -> Result<Self, TryGetError> {
        let raw = i32::try_get_by(res, index)?;
        Self::from_stored(raw).ok_or_else(|| TryGetError::DbErr(DbErr::Type(format!("a 0/1 flag column holds {raw}"))))
    }
}

impl ValueType for SqlBool {
    fn try_from(value: Value) -> Result<Self, ValueTypeErr> {
        match value {
            Value::Int(Some(raw)) => Self::from_stored(raw).ok_or(ValueTypeErr),
            _ => Err(ValueTypeErr),
        }
    }

    fn type_name() -> String {
        "SqlBool".to_owned()
    }

    fn array_type() -> ArrayType {
        ArrayType::Int
    }

    fn column_type() -> ColumnType {
        ColumnType::Integer
    }
}

impl Nullable for SqlBool {
    fn null() -> Value {
        Value::Int(None)
    }
}

impl IntoActiveValue<SqlBool> for SqlBool {
    fn into_active_value(self) -> ActiveValue<SqlBool> {
        ActiveValue::Set(self)
    }
}

/// A `TEXT` column that holds JSON, decoded into `T` at the read.
///
/// `TEXT` is storage, not a contract: a row whose JSON does not parse as `T`
/// fails the query, rather than arriving as an empty list or an empty object.
/// One generic type rather than a hand-written newtype per column, because the
/// rule is the same for each and the shape is already `T`; a column that needs
/// more than serde's own checking (a closed set of names, no repeats) still
/// gets its own type, the way `notification_webhook::NotificationEvents` does.
/// `FromJsonQueryResult` is not used for this: it is forbidden, because it
/// leaves the column's error handling to a default nobody wrote down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Json<T>(pub T);

impl<T: DeserializeOwned> Json<T> {
    /// The error names a position and a category, never serde's own message:
    /// that message quotes the offending value (`invalid type: string "sk-…"`),
    /// and `env` and `headers` columns are exactly where tokens live. An error
    /// here can end up in a log, which leaves the machine.
    pub fn decode(raw: &str) -> Result<Self, String> {
        serde_json::from_str(raw).map(Self).map_err(|error| {
            format!(
                "malformed {} JSON ({:?} error at {}:{})",
                short_type_name::<T>(),
                error.classify(),
                error.line(),
                error.column()
            )
        })
    }
}

impl<T: Serialize> Json<T> {
    pub fn encode(&self) -> String {
        // A `T` with a non-string map key could fail here; none of the columns
        // holds one, and the type for a new column is chosen by whoever adds it.
        serde_json::to_string(&self.0).expect("a JSON column's value serializes")
    }
}

impl<T> Json<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> std::ops::Deref for Json<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

fn short_type_name<T>() -> &'static str {
    let full = std::any::type_name::<T>();
    let outer = full.split('<').next().unwrap_or(full);
    outer.rsplit("::").next().unwrap_or(outer)
}

impl<T: Serialize> From<Json<T>> for Value {
    fn from(value: Json<T>) -> Self {
        Value::String(Some(value.encode()))
    }
}

impl<T: DeserializeOwned> TryGetable for Json<T> {
    fn try_get_by<I: ColIdx>(res: &QueryResult, index: I) -> Result<Self, TryGetError> {
        let raw = String::try_get_by(res, index)?;
        Self::decode(&raw).map_err(|error| TryGetError::DbErr(DbErr::Type(error)))
    }
}

impl<T: Serialize + DeserializeOwned> ValueType for Json<T> {
    fn try_from(value: Value) -> Result<Self, ValueTypeErr> {
        match value {
            Value::String(Some(raw)) => Self::decode(&raw).map_err(|_| ValueTypeErr),
            _ => Err(ValueTypeErr),
        }
    }

    fn type_name() -> String {
        format!("Json<{}>", std::any::type_name::<T>())
    }

    fn array_type() -> ArrayType {
        ArrayType::String
    }

    fn column_type() -> ColumnType {
        ColumnType::Text
    }
}

impl<T> Nullable for Json<T> {
    fn null() -> Value {
        Value::String(None)
    }
}

impl<T: Serialize> IntoActiveValue<Json<T>> for Json<T> {
    fn into_active_value(self) -> ActiveValue<Json<T>> {
        ActiveValue::Set(self)
    }
}

/// Column impls for a domain enum stored as `TEXT` through its own `as_str`
/// and `parse`.
///
/// For a type that belongs to another module and is used far beyond its
/// table — `tools::Permission`, the provider registry's enums — so the
/// persistence impls are written in the entity file that stores it rather than
/// as a `DeriveActiveEnum` on the type itself. An enum that exists only for
/// its column should be a `DeriveActiveEnum` in the entity instead. Reading an
/// unknown spelling fails the row, through `parse`'s own error.
macro_rules! text_enum_column {
    ($ty:ty) => {
        impl From<$ty> for sea_orm::Value {
            fn from(value: $ty) -> Self {
                sea_orm::Value::String(Some(value.as_str().to_owned()))
            }
        }

        impl sea_orm::TryGetable for $ty {
            fn try_get_by<I: sea_orm::ColIdx>(
                res: &sea_orm::QueryResult,
                index: I,
            ) -> Result<Self, sea_orm::TryGetError> {
                let raw = String::try_get_by(res, index)?;
                <$ty>::parse(&raw).map_err(|error| sea_orm::TryGetError::DbErr(sea_orm::DbErr::Type(error)))
            }
        }

        impl sea_orm::sea_query::ValueType for $ty {
            fn try_from(value: sea_orm::Value) -> Result<Self, sea_orm::sea_query::ValueTypeErr> {
                match value {
                    sea_orm::Value::String(Some(raw)) => {
                        <$ty>::parse(&raw).map_err(|_| sea_orm::sea_query::ValueTypeErr)
                    }
                    _ => Err(sea_orm::sea_query::ValueTypeErr),
                }
            }

            fn type_name() -> String {
                stringify!($ty).to_owned()
            }

            fn array_type() -> sea_orm::sea_query::ArrayType {
                sea_orm::sea_query::ArrayType::String
            }

            fn column_type() -> sea_orm::sea_query::ColumnType {
                sea_orm::sea_query::ColumnType::Text
            }
        }

        impl sea_orm::sea_query::Nullable for $ty {
            fn null() -> sea_orm::Value {
                sea_orm::Value::String(None)
            }
        }

        impl sea_orm::IntoActiveValue<$ty> for $ty {
            fn into_active_value(self) -> sea_orm::ActiveValue<$ty> {
                sea_orm::ActiveValue::Set(self)
            }
        }
    };
}
pub(crate) use text_enum_column;

/// A closed list stored as text under a `CHECK (<column> IN (…))`: the
/// `DeriveActiveEnum` with each variant's stored value, and the `as_str` /
/// `parse` pair the rest of the crate compares with, which take the same
/// snake_case spelling through strum. The stored values are written out per
/// variant because `DeriveActiveEnum` needs them as attributes; the entity's
/// tests hold them to strum's spelling and to the schema's `CHECK`.
///
/// `parse` keeps the message the Diesel-era `stored_enum!` gave, since the
/// plan-review code still reports it.
macro_rules! checked_text_enum {
    (
        $(#[$meta:meta])*
        $name:ident { $($(#[$variant_meta:meta])* $variant:ident = $value:literal),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            serde::Serialize,
            strum::IntoStaticStr,
            strum::EnumString,
            sea_orm::EnumIter,
            sea_orm::DeriveActiveEnum,
        )]
        #[serde(rename_all = "snake_case")]
        #[strum(serialize_all = "snake_case")]
        #[sea_orm(rs_type = "String", db_type = "Text")]
        pub enum $name {
            $($(#[$variant_meta])* #[sea_orm(string_value = $value)] $variant),+
        }

        impl $name {
            pub fn as_str(self) -> &'static str {
                self.into()
            }

            pub fn parse(value: &str) -> Result<Self, String> {
                value
                    .parse()
                    .map_err(|_| format!("unknown {} '{}'", stringify!($name), value))
            }
        }
    };
}
pub(crate) use checked_text_enum;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn json_round_trips_and_refuses_the_wrong_shape() {
        let list = Json(vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(list.encode(), r#"["a","b"]"#);
        assert_eq!(Json::<Vec<String>>::decode(r#"["a","b"]"#).unwrap(), list);

        // Each of these is a row that must fail, not read as empty.
        for raw in ["", "null", "{}", r#"[1]"#, "not json"] {
            assert!(Json::<Vec<String>>::decode(raw).is_err(), "{raw:?} decoded");
        }
        assert!(Json::<BTreeMap<String, String>>::decode(r#"["a"]"#).is_err());
        assert!(Json::<BTreeMap<String, String>>::decode(r#"{"k":1}"#).is_err());
        assert!(Json::<serde_json::Map<String, serde_json::Value>>::decode("[]").is_err());
    }

    /// What a failure says is a position, not the value: these columns hold
    /// tokens, and the message can reach a log.
    #[test]
    fn a_decode_error_does_not_quote_the_value() {
        let error = Json::<Vec<String>>::decode(r#"{"TOKEN": "sk-secret"}"#).unwrap_err();
        assert!(!error.contains("sk-secret"), "{error}");
        let error = Json::<BTreeMap<String, String>>::decode(r#"{"TOKEN": 987654}"#).unwrap_err();
        assert!(!error.contains("987654"), "{error}");
        assert!(
            error.starts_with("malformed BTreeMap JSON (Data error at 1:"),
            "{error}"
        );
    }
}
