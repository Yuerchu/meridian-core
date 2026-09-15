//! Where an alert goes, and what has already been said.
//!
//! The signing secret is absent from [`NotificationWebhookRow`] on purpose: the
//! row is what a list response is built from, so a secret column would be a
//! secret handed back on every list. It lives in the keyring instead — see
//! `notify::webhook_secret_name`.

use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use strum::{EnumIter, IntoEnumIterator};

use crate::db::schema::{notification_alert_state, notification_webhooks};

/// How many endpoints one install may hold.
///
/// Not a storage limit — it is what stops a single alert from becoming a
/// hundred outbound requests while a watcher tick is holding no lock but a lot
/// of patience.
pub const MAX_WEBHOOKS: usize = 32;

/// What an endpoint speaks.
///
/// Only [`NotificationFormat::Generic`] is our own contract. Four of the rest
/// are somebody else's published product and follow their upstream — including
/// their signing schemes, which are all different from ours and from each
/// other, and their habit of reporting failure inside an HTTP 200.
/// [`NotificationFormat::Custom`] is the case none of those cover: a company's
/// own alert pipe, whose schema is whatever its operations team wrote down.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, EnumIter, Serialize, Deserialize, strum::IntoStaticStr, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum NotificationFormat {
    Generic,
    Dingtalk,
    Feishu,
    Wecom,
    Slack,
    /// The body comes from `body_template` and the stored secret is a bearer
    /// token. See [`NotificationFormat::secret_meaning`].
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
        Self::iter().map(|v| v.as_str()).collect()
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
    Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter, Serialize, Deserialize, strum::IntoStaticStr, strum::EnumString,
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

/// The stored JSON array, as a typed set.
///
/// Duplicates are an error rather than being folded away: they mean the writer
/// and this reader disagree about what the column holds, and the next thing
/// that disagreement produces is a doubled notification.
pub fn decode_events(raw: &str) -> Result<Vec<NotificationEventKind>, String> {
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
    Ok(out)
}

pub fn encode_events(events: &[NotificationEventKind]) -> Result<String, String> {
    let names: Vec<&str> = events.iter().map(|event| event.as_str()).collect();
    serde_json::to_string(&names).map_err(|error| format!("failed to encode notification events: {error}"))
}

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = notification_webhooks)]
pub struct NotificationWebhookRow {
    pub id: String,
    pub name: String,
    pub url: String,
    pub format: String,
    pub events: String,
    pub is_enabled: i32,
    pub body_template: Option<String>,
    pub last_attempt_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub last_error: Option<String>,
    pub consecutive_failures: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

impl NotificationWebhookRow {
    pub fn format(&self) -> Result<NotificationFormat, String> {
        NotificationFormat::parse(&self.format)
    }

    /// The stored template, as JSON.
    ///
    /// `TEXT` is storage, not a public string contract — the same rule `events`
    /// follows. A template that will not parse fails the read rather than
    /// becoming `{}`, which would post an empty document and be recorded as
    /// delivered.
    ///
    /// `None` for every format but `custom`, and its absence *on* `custom` is
    /// an error rather than an empty body, for the same reason.
    pub fn body_template(&self) -> Result<Option<serde_json::Value>, String> {
        let Some(raw) = self.body_template.as_deref() else {
            if self.format()?.needs_body_template() {
                return Err(format!(
                    "webhook `{}` is `custom` but has no body template; there is nothing to post",
                    self.id
                ));
            }
            return Ok(None);
        };
        serde_json::from_str(raw)
            .map(Some)
            .map_err(|error| format!("webhook `{}` has a malformed body template: {error}", self.id))
    }

    pub fn events(&self) -> Result<Vec<NotificationEventKind>, String> {
        decode_events(&self.events)
    }

    pub fn is_enabled(&self) -> bool {
        self.is_enabled != 0
    }

    /// Whether this endpoint asked for that alert.
    ///
    /// Returns the decode error rather than `false`: an endpoint whose
    /// subscription cannot be read must not be quietly treated as subscribed to
    /// nothing.
    pub fn wants(&self, event: NotificationEventKind) -> Result<bool, String> {
        Ok(self.events()?.contains(&event))
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = notification_webhooks)]
pub struct NotificationWebhookInsert<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub url: &'a str,
    pub format: &'a str,
    pub events: &'a str,
    pub is_enabled: i32,
    pub body_template: Option<&'a str>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Default, AsChangeset)]
#[diesel(table_name = notification_webhooks)]
pub struct NotificationWebhookChangeset {
    pub name: Option<String>,
    pub url: Option<String>,
    pub format: Option<String>,
    pub events: Option<String>,
    pub is_enabled: Option<i32>,
    /// Doubly wrapped: the outer `None` leaves the column alone, and
    /// `Some(None)` clears it. Switching an endpoint from `custom` to a vendor
    /// format has to be able to drop the template, and a single `Option` cannot
    /// say that.
    pub body_template: Option<Option<String>>,
    pub updated_at: Option<i64>,
}

/// The outcome of one endpoint's delivery, written back onto its row.
///
/// A separate changeset from the one above because these fields are never a
/// user edit and the two must not be settable through one another: a save from
/// the settings page has no business resetting the failure counter, and a
/// delivery has no business changing the URL.
#[derive(Debug, Default, AsChangeset)]
#[diesel(table_name = notification_webhooks)]
pub struct NotificationWebhookHealthChangeset {
    pub last_attempt_at: Option<i64>,
    pub last_success_at: Option<Option<i64>>,
    pub last_error: Option<Option<String>>,
    pub consecutive_failures: Option<i32>,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = notification_alert_state)]
pub struct NotificationAlertStateRow {
    pub alert_key: String,
    pub first_raised_at: i64,
    pub last_raised_at: i64,
    /// NULL until a delivery was actually accepted. See the migration: writing
    /// this when the alert is raised is what makes an unsent alert look sent.
    pub last_notified_at: Option<i64>,
    pub fingerprint: String,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = notification_alert_state)]
pub struct NotificationAlertStateInsert<'a> {
    pub alert_key: &'a str,
    pub first_raised_at: i64,
    pub last_raised_at: i64,
    pub last_notified_at: Option<i64>,
    pub fingerprint: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_through_their_stored_spelling() {
        let events = vec![NotificationEventKind::BalanceLow, NotificationEventKind::UsageSurge];
        let encoded = encode_events(&events).unwrap();
        assert_eq!(encoded, r#"["balance_low","usage_surge"]"#);
        assert_eq!(decode_events(&encoded).unwrap(), events);
    }

    /// A subscription that cannot be read fails the boundary. Read as an empty
    /// list it would silently unsubscribe the endpoint; read as "everything" it
    /// would send an account balance somewhere nobody asked for.
    #[test]
    fn an_unreadable_subscription_is_an_error_rather_than_an_empty_one() {
        assert!(decode_events("not json").is_err());
        assert!(decode_events(r#"["balance_low","invented"]"#).is_err());
        assert!(decode_events(r#"[1]"#).is_err());
        assert!(
            decode_events(r#"["balance_low","balance_low"]"#).is_err(),
            "a repeat means the writer and this reader disagree"
        );
        assert_eq!(decode_events("[]").unwrap(), vec![]);
    }

    #[test]
    fn formats_and_events_use_the_spelling_the_check_constraint_names() {
        assert_eq!(
            NotificationFormat::all(),
            ["generic", "dingtalk", "feishu", "wecom", "slack", "custom"]
        );
        assert_eq!(
            NotificationEventKind::all(),
            ["balance_low", "balance_unavailable", "usage_surge", "test"]
        );
        assert!(NotificationFormat::parse("teams").is_err());
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
        for format in NotificationFormat::iter() {
            assert_eq!(format.needs_body_template(), format == F::Custom, "{}", format.as_str());
        }
    }

    /// `custom` without a template would post nothing and be recorded as
    /// delivered — the worst outcome available, because it looks like it worked.
    #[test]
    fn a_body_template_is_required_by_custom_and_refused_elsewhere() {
        let row = |format: &str, template: Option<&str>| NotificationWebhookRow {
            id: "w1".into(),
            name: "w".into(),
            url: "https://a.invalid/h".into(),
            format: format.into(),
            events: r#"["test"]"#.into(),
            is_enabled: 1,
            body_template: template.map(str::to_string),
            last_attempt_at: None,
            last_success_at: None,
            last_error: None,
            consecutive_failures: 0,
            created_at: 1,
            updated_at: 1,
        };

        let error = row("custom", None).body_template().unwrap_err();
        assert!(error.contains("nothing to post"), "{error}");

        // A template that will not parse fails the read rather than becoming
        // `{}`, which would post an empty document.
        let error = row("custom", Some("{not json")).body_template().unwrap_err();
        assert!(error.contains("malformed"), "{error}");

        assert_eq!(
            row("custom", Some(r#"{"a":1}"#)).body_template().unwrap(),
            Some(serde_json::json!({"a": 1}))
        );
        assert_eq!(row("generic", None).body_template().unwrap(), None);
    }
}
