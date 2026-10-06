//! `notification_webhooks`: where an alert goes.
//!
//! The signing secret is absent from [`Model`] on purpose: the row is what a
//! list response is built from, so a secret column would be a secret handed
//! back on every list. It lives in the keyring instead — see
//! `notify::webhook_secret_name`.
//!
//! Two columns are JSON text and both decode at the read, into
//! [`NotificationEvents`] and [`BodyTemplate`]: a row that does not parse fails
//! the query rather than arriving as an empty subscription or an empty
//! document. `TEXT` is storage, not a contract.

use sea_orm::entity::prelude::*;
use sea_orm::sea_query::{ArrayType, ColumnType, Nullable, ValueType, ValueTypeErr};
use sea_orm::{ActiveValue, ColIdx, IntoActiveValue, Iterable, QueryResult, TryGetError, TryGetable, Value};
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;

use crate::db::types::{EpochMs, SqlBool};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notification_webhooks")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    pub url: String,
    pub format: NotificationFormat,
    pub events: NotificationEvents,
    pub is_enabled: SqlBool,
    pub body_template: Option<BodyTemplate>,
    pub last_attempt_at: Option<EpochMs>,
    pub last_success_at: Option<EpochMs>,
    pub last_error: Option<String>,
    pub consecutive_failures: i32,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Model {
    /// The stored template, as JSON.
    ///
    /// The column has already been decoded at the read, so the one thing left
    /// to check is that `custom` has one: its absence *on* `custom` is an error
    /// rather than an empty body, which would post nothing and be recorded as
    /// delivered.
    pub fn body_template(&self) -> Result<Option<&serde_json::Value>, String> {
        match &self.body_template {
            None if self.format.needs_body_template() => Err(format!(
                "webhook `{}` is `custom` but has no body template; there is nothing to post",
                self.id
            )),
            template => Ok(template.as_ref().map(BodyTemplate::as_value)),
        }
    }

    /// Whether this endpoint asked for that alert. Infallible: a subscription
    /// that could not be read never became a `Model`.
    pub fn wants(&self, event: NotificationEventKind) -> bool {
        self.events.contains(event)
    }
}

/// What an endpoint speaks.
///
/// Only [`NotificationFormat::Generic`] is our own contract. Four of the rest
/// are somebody else's published product and follow their upstream — including
/// their signing schemes, which are all different from ours and from each
/// other, and their habit of reporting failure inside an HTTP 200.
/// [`NotificationFormat::Custom`] is the case none of those cover: a company's
/// own alert pipe, whose schema is whatever its operations team wrote down.
///
/// The stored spelling (`string_value`), the wire spelling (serde) and the
/// `CHECK (format IN (…))` in the schema are one list; a test below holds the
/// three together. `EnumIter` is SeaORM's re-export, which `ActiveEnum`
/// requires, not the crate's own strum.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum NotificationFormat {
    #[sea_orm(string_value = "generic")]
    Generic,
    #[sea_orm(string_value = "dingtalk")]
    Dingtalk,
    #[sea_orm(string_value = "feishu")]
    Feishu,
    #[sea_orm(string_value = "wecom")]
    Wecom,
    #[sea_orm(string_value = "slack")]
    Slack,
    /// The body comes from `body_template` and the stored secret is a bearer
    /// token. See [`NotificationFormat::secret_meaning`].
    #[sea_orm(string_value = "custom")]
    Custom,
}

impl NotificationFormat {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    /// What this format does with the endpoint's stored secret.
    ///
    /// **The format has always decided this**, which is why `custom` needs no
    /// second keyring entry: DingTalk signs a URL with it, Feishu signs a body,
    /// `generic` computes an HMAC header, and `custom` sends it as a bearer
    /// token. Written down as one function because a settings page and a
    /// configuration file both have to explain it, and two explanations drift.
    pub fn secret_meaning(self) -> SecretMeaning {
        match self {
            NotificationFormat::Generic => SecretMeaning::HmacSignature,
            NotificationFormat::Dingtalk => SecretMeaning::SignsTheUrl,
            NotificationFormat::Feishu => SecretMeaning::SignsTheBody,
            // Both authenticate with a key already in the URL, so a secret
            // configured here would sign nothing.
            NotificationFormat::Wecom | NotificationFormat::Slack => SecretMeaning::Unused,
            NotificationFormat::Custom => SecretMeaning::BearerToken,
        }
    }

    /// Whether this format needs a body template, which only `custom` does.
    pub fn needs_body_template(self) -> bool {
        matches!(self, NotificationFormat::Custom)
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown notification format `{value}`"))
    }

    pub fn all() -> Vec<&'static str> {
        <Self as Iterable>::iter().map(|v| v.as_str()).collect()
    }
}

/// What a format does with the endpoint's stored secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretMeaning {
    /// Ours: an HMAC over the timestamp and body, in a header.
    HmacSignature,
    /// DingTalk: query parameters on the URL.
    SignsTheUrl,
    /// Feishu: two fields inside the body.
    SignsTheBody,
    /// `Authorization: Bearer <secret>`.
    BearerToken,
    /// The endpoint authenticates by its URL alone.
    Unused,
}

/// What an endpoint is subscribed to.
///
/// A strict enum in both directions. An unknown value stored in `events` is a
/// contract violation and fails the read, rather than being dropped — silently
/// narrowing a subscription is how an endpoint stops receiving the one alert it
/// was created for.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumIter,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum NotificationEventKind {
    /// The upstream still serves requests, but the balance is under the
    /// configured floor.
    BalanceLow,
    /// The upstream itself says the account cannot be used. Separate from
    /// `BalanceLow` because it has already happened rather than being a
    /// warning, and because a postpaid account can reach it with a healthy
    /// figure on screen.
    BalanceUnavailable,
    UsageSurge,
    /// Only ever sent by hand, from the settings page. It is the one way to
    /// find out whether a vendor's signing scheme was implemented correctly.
    Test,
}

impl NotificationEventKind {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown notification event `{value}`"))
    }

    pub fn all() -> Vec<&'static str> {
        Self::iter().map(|v| v.as_str()).collect()
    }
}

/// The `events` column: a JSON array of event names, decoded.
///
/// Written by hand rather than with `#[derive(DeriveValueType)]`, which would
/// only decode a `String` and validate nothing. The decoder is strict: an
/// unknown name, a repeat and malformed JSON each fail the read, so a bad row
/// fails the query instead of becoming an empty subscription. Duplicates are
/// an error rather than being folded away: they mean the writer and this
/// reader disagree about what the column holds, and the next thing that
/// disagreement produces is a doubled notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationEvents(Vec<NotificationEventKind>);

impl NotificationEvents {
    pub fn contains(&self, event: NotificationEventKind) -> bool {
        self.0.contains(&event)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, NotificationEventKind> {
        self.0.iter()
    }

    pub fn as_slice(&self) -> &[NotificationEventKind] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<NotificationEventKind> {
        self.0
    }

    /// The stored text, as the typed set.
    pub fn decode(raw: &str) -> Result<Self, String> {
        let names: Vec<String> =
            serde_json::from_str(raw).map_err(|error| format!("malformed notification events JSON: {error}"))?;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let kind = NotificationEventKind::parse(&name)?;
            if out.contains(&kind) {
                return Err(format!("notification events list repeats `{name}`"));
            }
            out.push(kind);
        }
        Ok(Self(out))
    }

    /// The canonical JSON array the column holds.
    pub fn encode(&self) -> String {
        let names: Vec<&str> = self.0.iter().map(|event| event.as_str()).collect();
        serde_json::to_string(&names).expect("a list of static names serializes")
    }
}

impl From<Vec<NotificationEventKind>> for NotificationEvents {
    fn from(events: Vec<NotificationEventKind>) -> Self {
        Self(events)
    }
}

impl From<NotificationEvents> for Value {
    fn from(events: NotificationEvents) -> Self {
        Value::String(Some(events.encode()))
    }
}

impl TryGetable for NotificationEvents {
    fn try_get_by<I: ColIdx>(res: &QueryResult, index: I) -> Result<Self, TryGetError> {
        let raw = String::try_get_by(res, index)?;
        Self::decode(&raw).map_err(|error| TryGetError::DbErr(DbErr::Type(error)))
    }
}

impl ValueType for NotificationEvents {
    fn try_from(value: Value) -> Result<Self, ValueTypeErr> {
        match value {
            Value::String(Some(raw)) => Self::decode(&raw).map_err(|_| ValueTypeErr),
            _ => Err(ValueTypeErr),
        }
    }

    fn type_name() -> String {
        "NotificationEvents".to_owned()
    }

    fn array_type() -> ArrayType {
        ArrayType::String
    }

    fn column_type() -> ColumnType {
        ColumnType::Text
    }
}

impl Nullable for NotificationEvents {
    fn null() -> Value {
        Value::String(None)
    }
}

impl IntoActiveValue<NotificationEvents> for NotificationEvents {
    fn into_active_value(self) -> ActiveValue<NotificationEvents> {
        ActiveValue::Set(self)
    }
}

/// The `body_template` column: the JSON document a `custom` endpoint posts.
///
/// Hand-written for the same reason as [`NotificationEvents`]: a template that
/// will not parse fails the read rather than becoming `{}`, which would post an
/// empty document and be recorded as delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyTemplate(serde_json::Value);

impl BodyTemplate {
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }

    pub fn into_value(self) -> serde_json::Value {
        self.0
    }

    pub fn decode(raw: &str) -> Result<Self, String> {
        serde_json::from_str(raw)
            .map(Self)
            .map_err(|error| format!("malformed body template JSON: {error}"))
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(&self.0).expect("a serde_json::Value serializes")
    }
}

impl From<serde_json::Value> for BodyTemplate {
    fn from(value: serde_json::Value) -> Self {
        Self(value)
    }
}

impl From<BodyTemplate> for Value {
    fn from(template: BodyTemplate) -> Self {
        Value::String(Some(template.encode()))
    }
}

impl TryGetable for BodyTemplate {
    fn try_get_by<I: ColIdx>(res: &QueryResult, index: I) -> Result<Self, TryGetError> {
        let raw = String::try_get_by(res, index)?;
        Self::decode(&raw).map_err(|error| TryGetError::DbErr(DbErr::Type(error)))
    }
}

impl ValueType for BodyTemplate {
    fn try_from(value: Value) -> Result<Self, ValueTypeErr> {
        match value {
            Value::String(Some(raw)) => Self::decode(&raw).map_err(|_| ValueTypeErr),
            _ => Err(ValueTypeErr),
        }
    }

    fn type_name() -> String {
        "BodyTemplate".to_owned()
    }

    fn array_type() -> ArrayType {
        ArrayType::String
    }

    fn column_type() -> ColumnType {
        ColumnType::Text
    }
}

impl Nullable for BodyTemplate {
    fn null() -> Value {
        Value::String(None)
    }
}

impl IntoActiveValue<BodyTemplate> for BodyTemplate {
    fn into_active_value(self) -> ActiveValue<BodyTemplate> {
        ActiveValue::Set(self)
    }
}

/// A user edit to an endpoint. Every field is doubly optional in effect: the
/// outer `None` leaves the column alone.
#[derive(Debug, Default, DeriveIntoActiveModel)]
pub struct NotificationWebhookChangeset {
    pub name: Option<String>,
    pub url: Option<String>,
    pub format: Option<NotificationFormat>,
    pub events: Option<NotificationEvents>,
    pub is_enabled: Option<SqlBool>,
    /// Doubly wrapped: the outer `None` leaves the column alone, and
    /// `Some(None)` clears it. Switching an endpoint from `custom` to a vendor
    /// format has to be able to drop the template, and a single `Option` cannot
    /// say that.
    pub body_template: Option<Option<BodyTemplate>>,
    pub updated_at: Option<EpochMs>,
}

/// The outcome of one endpoint's delivery, written back onto its row.
///
/// A separate changeset from the one above because these fields are never a
/// user edit and the two must not be settable through one another: a save from
/// the settings page has no business resetting the failure counter, and a
/// delivery has no business changing the URL.
#[derive(Debug, Default, DeriveIntoActiveModel)]
pub struct NotificationWebhookHealthChangeset {
    pub last_attempt_at: Option<EpochMs>,
    pub last_success_at: Option<Option<EpochMs>>,
    pub last_error: Option<Option<String>>,
    pub consecutive_failures: Option<i32>,
    pub updated_at: Option<EpochMs>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn model(format: NotificationFormat, template: Option<serde_json::Value>) -> Model {
        Model {
            id: "w1".into(),
            name: "w".into(),
            url: "https://a.invalid/h".into(),
            format,
            events: NotificationEvents::from(vec![NotificationEventKind::Test]),
            is_enabled: SqlBool::TRUE,
            body_template: template.map(BodyTemplate::from),
            last_attempt_at: None,
            last_success_at: None,
            last_error: None,
            consecutive_failures: 0,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn events_round_trip_through_their_stored_spelling() {
        let events = NotificationEvents::from(vec![
            NotificationEventKind::BalanceLow,
            NotificationEventKind::UsageSurge,
        ]);
        let encoded = events.encode();
        assert_eq!(encoded, r#"["balance_low","usage_surge"]"#);
        assert_eq!(NotificationEvents::decode(&encoded).unwrap(), events);
        assert_eq!(
            <NotificationEvents as ValueType>::try_from(Value::from(events.clone())).unwrap(),
            events
        );
    }

    /// A subscription that cannot be read fails the boundary. Read as an empty
    /// list it would silently unsubscribe the endpoint; read as "everything" it
    /// would send an account balance somewhere nobody asked for.
    #[test]
    fn an_unreadable_subscription_is_an_error_rather_than_an_empty_one() {
        assert!(NotificationEvents::decode("not json").is_err());
        assert!(NotificationEvents::decode(r#"["balance_low","invented"]"#).is_err());
        assert!(NotificationEvents::decode(r#"[1]"#).is_err());
        assert!(
            NotificationEvents::decode(r#"["balance_low","balance_low"]"#).is_err(),
            "a repeat means the writer and this reader disagree"
        );
        assert_eq!(NotificationEvents::decode("[]").unwrap().as_slice(), &[]);
        assert!(<NotificationEvents as ValueType>::try_from(Value::String(Some("not json".into()))).is_err());
        assert!(<NotificationEvents as ValueType>::try_from(Value::Int(Some(1))).is_err());
    }

    /// The stored spelling, the wire spelling and the schema's `CHECK` are one
    /// list. Read out of the snapshot rather than restated here, so the enum
    /// and the live constraint cannot drift apart silently.
    #[test]
    fn the_stored_spelling_is_the_wire_spelling_and_the_check_constraint_names_it() {
        let snapshot = include_str!("../../../schema.snapshot.sql");
        let table = snapshot
            .lines()
            .find(|line| line.starts_with("CREATE TABLE \"notification_webhooks\""))
            .expect("the snapshot builds notification_webhooks");
        let prefix = "CHECK (format IN (";
        let start = table.find(prefix).expect("a CHECK on format") + prefix.len();
        let end = start + table[start..].find("))").expect("the CHECK closes");
        let allowed: BTreeSet<String> = table[start..end]
            .split(',')
            .map(|name| name.trim().trim_matches('\'').to_owned())
            .collect();

        let mut stored = BTreeSet::new();
        for format in <NotificationFormat as Iterable>::iter() {
            let db = format.to_value();
            assert_eq!(
                serde_json::to_value(format).unwrap(),
                serde_json::Value::String(db.clone()),
                "{format:?} is spelt one way on the wire and another in the database"
            );
            assert_eq!(format.as_str(), db, "{format:?}: strum and the stored value disagree");
            assert_eq!(NotificationFormat::try_from_value(&db).unwrap(), format);
            stored.insert(db);
        }
        assert_eq!(
            stored, allowed,
            "the enum and the schema's CHECK name different formats"
        );
        assert!(NotificationFormat::try_from_value(&"teams".to_owned()).is_err());
        assert!(NotificationFormat::parse("teams").is_err());
        assert_eq!(
            NotificationFormat::all(),
            ["generic", "dingtalk", "feishu", "wecom", "slack", "custom"]
        );
        assert_eq!(
            NotificationEventKind::all(),
            ["balance_low", "balance_unavailable", "usage_surge", "test"]
        );
    }

    /// Every format has to answer both questions, because a settings page and a
    /// configuration file both ask them and a format added without an answer
    /// would silently inherit somebody else's.
    #[test]
    fn every_format_says_what_it_does_with_a_secret_and_a_body() {
        use NotificationFormat as F;
        assert_eq!(F::Generic.secret_meaning(), SecretMeaning::HmacSignature);
        assert_eq!(F::Dingtalk.secret_meaning(), SecretMeaning::SignsTheUrl);
        assert_eq!(F::Feishu.secret_meaning(), SecretMeaning::SignsTheBody);
        assert_eq!(F::Wecom.secret_meaning(), SecretMeaning::Unused);
        assert_eq!(F::Slack.secret_meaning(), SecretMeaning::Unused);
        assert_eq!(F::Custom.secret_meaning(), SecretMeaning::BearerToken);

        // Only `custom` posts a document the operator wrote; every other format
        // sends a shape this app or a vendor defines.
        for format in <NotificationFormat as Iterable>::iter() {
            assert_eq!(format.needs_body_template(), format == F::Custom, "{}", format.as_str());
        }
    }

    /// `custom` without a template would post nothing and be recorded as
    /// delivered — the worst outcome available, because it looks like it worked.
    /// A template that will not parse never becomes a `Model` at all.
    #[test]
    fn a_body_template_is_required_by_custom_and_malformed_json_never_decodes() {
        let error = model(NotificationFormat::Custom, None).body_template().unwrap_err();
        assert!(error.contains("nothing to post"), "{error}");

        let error = BodyTemplate::decode("{not json").unwrap_err();
        assert!(error.contains("malformed"), "{error}");
        assert!(<BodyTemplate as ValueType>::try_from(Value::String(Some("{not json".into()))).is_err());

        let template = serde_json::json!({"a": 1});
        assert_eq!(
            model(NotificationFormat::Custom, Some(template.clone()))
                .body_template()
                .unwrap(),
            Some(&template)
        );
        assert_eq!(model(NotificationFormat::Generic, None).body_template().unwrap(), None);
        assert_eq!(
            BodyTemplate::decode(&BodyTemplate::from(template.clone()).encode()).unwrap(),
            BodyTemplate::from(template)
        );
    }

    #[test]
    fn wants_is_membership_in_the_decoded_subscription() {
        let row = model(NotificationFormat::Generic, None);
        assert!(row.wants(NotificationEventKind::Test));
        assert!(!row.wants(NotificationEventKind::BalanceLow));
    }
}
