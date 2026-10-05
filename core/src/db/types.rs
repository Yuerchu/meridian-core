//! Column types the SeaORM entities share.

use sea_orm::sea_query::{ArrayType, ColumnType, Nullable, ValueType, ValueTypeErr};
use sea_orm::{ActiveValue, ColIdx, DbErr, IntoActiveValue, QueryResult, TryGetError, TryGetable, Value};

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
