//! Exact decimal values at the database and protocol boundaries.
//!
//! SQLite's NUMERIC affinity silently stores non-integers as IEEE-754 doubles,
//! so monetary columns use TEXT instead. `Decimal` is the one representation
//! allowed in Rust: it validates the project's precision contract, maps to a
//! canonical database string, and serializes to a JSON string so JavaScript
//! never receives money as `number`.

use std::fmt;
use std::ops::{Add, AddAssign, Mul, Sub, SubAssign};
use std::str::FromStr;

use bigdecimal::{BigDecimal, Signed, Zero};
use diesel::FromSqlRow;
use diesel::deserialize::{self, FromSql};
use diesel::expression::AsExpression;
use diesel::serialize::{self, IsNull, Output, ToSql};
use diesel::sql_types::Text;
use diesel::sqlite::{Sqlite, SqliteValue};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Database/API monetary values follow the same NUMERIC(38, 18) contract as
/// foxline-pro: twenty integer digits and eighteen fractional digits.
pub const DECIMAL_PRECISION: usize = 38;
pub const DECIMAL_SCALE: usize = 18;
pub const DECIMAL_INTEGER_DIGITS: usize = DECIMAL_PRECISION - DECIMAL_SCALE;

/// An exact base-10 value with at most 18 fractional digits.
///
/// The inner library has no NaN or infinity states. `FromStr` normalizes plain
/// base-10 input for internal and migration use; serde accepts only the
/// canonical spelling emitted by [`Decimal::canonical`].
#[derive(Clone, Default, Eq, PartialEq, Ord, PartialOrd, Hash, AsExpression, FromSqlRow)]
#[diesel(sql_type = Text)]
pub struct Decimal(BigDecimal);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecimalError(String);

impl DecimalError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for DecimalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecimalError {}

impl Decimal {
    pub fn zero() -> Self {
        Self(BigDecimal::zero())
    }

    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    pub fn is_negative(&self) -> bool {
        self.0.is_negative() && !self.0.is_zero()
    }

    pub fn abs(&self) -> Self {
        Self(self.0.abs())
    }

    /// Reject negative values at price/rate request boundaries.
    pub fn require_non_negative(self, field: &str) -> Result<Self, DecimalError> {
        if self.is_negative() {
            Err(DecimalError::new(format!("{field} must be non-negative")))
        } else {
            Ok(self)
        }
    }

    pub fn canonical(&self) -> String {
        if self.0.is_zero() {
            return "0".to_owned();
        }
        self.0.normalized().to_plain_string()
    }

    fn from_canonical_str(raw: &str) -> Result<Self, DecimalError> {
        let value: Self = raw.parse()?;
        if value.canonical() != raw {
            return Err(DecimalError::new(
                "decimal string must use canonical fixed-point notation",
            ));
        }
        Ok(value)
    }
}

impl FromStr for Decimal {
    type Err = DecimalError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.is_empty() {
            return Err(DecimalError::new("decimal string is empty"));
        }
        if raw.contains(['e', 'E']) {
            return Err(DecimalError::new("decimal must use plain base-10 notation"));
        }
        let unsigned = raw.strip_prefix(['-', '+']).unwrap_or(raw);
        let mut parts = unsigned.split('.');
        let whole = parts.next().unwrap_or_default();
        let fraction = parts.next();
        if whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.is_some_and(|digits| digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()))
            || parts.next().is_some()
        {
            return Err(DecimalError::new(
                "decimal must contain only digits and at most one decimal point",
            ));
        }
        if fraction.is_some_and(|digits| digits.len() > DECIMAL_SCALE) {
            return Err(DecimalError::new(format!(
                "decimal exceeds NUMERIC({DECIMAL_PRECISION}, {DECIMAL_SCALE})"
            )));
        }
        let value = BigDecimal::from_str(raw).map_err(|error| DecimalError::new(error.to_string()))?;
        let value = Self(value.normalized());
        let canonical = value.canonical();
        let unsigned = canonical.strip_prefix('-').unwrap_or(&canonical);
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        let integer_digits = whole.trim_start_matches('0').len().max(1);
        if integer_digits > DECIMAL_INTEGER_DIGITS || fraction.len() > DECIMAL_SCALE {
            return Err(DecimalError::new(format!(
                "decimal exceeds NUMERIC({DECIMAL_PRECISION}, {DECIMAL_SCALE})"
            )));
        }
        Ok(value)
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical())
    }
}

impl fmt::Debug for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Decimal").field(&self.canonical()).finish()
    }
}

impl Serialize for Decimal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.canonical())
    }
}

impl<'de> Deserialize<'de> for Decimal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DecimalVisitor;

        impl de::Visitor<'_> for DecimalVisitor {
            type Value = Decimal;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an exact decimal encoded as a JSON string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Decimal::from_canonical_str(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(DecimalVisitor)
    }
}

impl From<i32> for Decimal {
    fn from(value: i32) -> Self {
        Self(BigDecimal::from(value))
    }
}

impl From<i64> for Decimal {
    fn from(value: i64) -> Self {
        Self(BigDecimal::from(value))
    }
}

impl From<u32> for Decimal {
    fn from(value: u32) -> Self {
        Self(BigDecimal::from(value))
    }
}

impl Add for Decimal {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl AddAssign for Decimal {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl Sub for Decimal {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl SubAssign for Decimal {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

impl Mul for Decimal {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self::Output {
        Self(self.0 * rhs.0)
    }
}

impl ToSql<Text, Sqlite> for Decimal {
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Sqlite>) -> serialize::Result {
        out.set_value(self.canonical());
        Ok(IsNull::No)
    }
}

impl FromSql<Text, Sqlite> for Decimal {
    fn from_sql(value: SqliteValue<'_, '_, '_>) -> deserialize::Result<Self> {
        let pointer = <*const str as FromSql<Text, Sqlite>>::from_sql(value)?;
        // Diesel documents this pointer specifically for custom SQLite Text
        // mappings. It remains valid for this call; parse copies the digits into
        // the Decimal value before the SQLite row can move again.
        let raw = unsafe { &*pointer };
        Decimal::from_canonical_str(raw).map_err(|error| Box::new(error) as _)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_is_string_only_and_canonical() {
        let value: Decimal = "001.2300".parse().unwrap();
        assert_eq!(value.to_string(), "1.23");
        assert_eq!(serde_json::to_string(&value).unwrap(), r#""1.23""#);
        assert_eq!(serde_json::from_str::<Decimal>(r#""1.23""#).unwrap(), value);
        assert!(serde_json::from_str::<Decimal>("1.23").is_err());
        assert!(serde_json::from_str::<Decimal>(r#""+1.23""#).is_err());
        assert!(serde_json::from_str::<Decimal>(r#""001.2300""#).is_err());
        assert!(serde_json::from_str::<Decimal>(r#""-0""#).is_err());
    }

    #[test]
    fn persistence_parser_rejects_noncanonical_values() {
        assert!(Decimal::from_canonical_str("10").is_ok());
        for raw in ["10.0", "01", "+1", "-0"] {
            assert!(Decimal::from_canonical_str(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn invalid_or_over_precise_values_are_rejected() {
        assert!("1e-3".parse::<Decimal>().is_err());
        assert!(".1".parse::<Decimal>().is_err());
        assert!("1.0000000000000000000".parse::<Decimal>().is_err());
        assert!("100000000000000000000".parse::<Decimal>().is_err());
    }

    #[test]
    fn full_numeric_38_18_precision_round_trips() {
        let raw = "99999999999999999999.999999999999999999";
        let value: Decimal = raw.parse().unwrap();
        assert_eq!(value.to_string(), raw);
        assert_eq!(serde_json::to_string(&value).unwrap(), format!(r#""{raw}""#));
    }
}
