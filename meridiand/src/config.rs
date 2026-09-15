//! The file an operator writes, and what it is allowed to say.
//!
//! Declarative on purpose: the file is the desired state, applied on every
//! start. That is the shape every monitoring daemon has settled on
//! (`prometheus.yml`, `alertmanager.yml`), and it is the only one that survives
//! being deployed by a machine — there is no settings page here to press Save
//! in, and a daemon configured by a sequence of commands has no way to converge
//! after a container is recreated.
//!
//! **No credential is in this file.** A key or a signing secret is named by the
//! *environment variable* that holds it, so the file itself can be committed
//! and reviewed. That is also why the parser refuses an inline key outright
//! rather than accepting one and warning: a warning in a log nobody reads is
//! how a secret ends up in a git history.

use std::collections::HashSet;
use std::path::Path;

use meridian_core::db::models::notification::{MAX_WEBHOOKS, NotificationEventKind, NotificationFormat};
use meridian_core::decimal::Decimal;
use serde::Deserialize;

/// The whole document.
///
/// `deny_unknown_fields` everywhere, the same rule the IPC contracts follow: a
/// misspelled key is the operator believing they configured something. Silence
/// there is worse than a refusal to start, because the thing they thought they
/// configured is an alert that will not fire.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    #[serde(default)]
    pub notify: NotifySection,
    #[serde(default)]
    pub provider: Vec<ProviderEntry>,
    #[serde(default)]
    pub webhook: Vec<WebhookEntry>,
}

/// The watcher's own settings.
///
/// Defaults are deliberately *not* repeated here — they live in
/// `meridian_core::notify::NotifyConfig::default()`, and an absent key means
/// "whatever that says". Restating them would be a second place for them to
/// drift.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NotifySection {
    pub enabled: Option<bool>,
    /// Absent switches balance alerting off; `"0"` keeps checking and says
    /// something only when an upstream reports the account unusable.
    pub balance_threshold: Option<Decimal>,
    pub balance_interval_minutes: Option<u32>,
    pub usage_enabled: Option<bool>,
    pub usage_check_interval_minutes: Option<u32>,
    pub usage_window_hours: Option<u32>,
    pub usage_baseline_days: Option<u32>,
    pub usage_multiplier: Option<Decimal>,
    pub usage_min_cost: Option<Decimal>,
    pub usage_cooldown_minutes: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    /// Stable, and the operator's to choose. It is what makes applying this
    /// file twice the same as applying it once — and it is also what the API
    /// key's keyring entry is named after, so it may not change casually.
    pub id: String,
    pub name: String,
    /// Matched against `provider::balance::supports_balance`, which today is
    /// only `deepseek`. Validated at apply time rather than here, so the error
    /// can name what is supported.
    #[serde(rename = "type")]
    pub provider_type: String,
    pub base_url: String,
    /// The *name* of the environment variable holding the key, never the key.
    pub api_key_env: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookEntry {
    pub id: String,
    pub name: String,
    pub url: String,
    pub format: NotificationFormat,
    pub events: Vec<NotificationEventKind>,
    /// The name of the environment variable holding the signing secret, if the
    /// format has one. Absent means unsigned — which for DingTalk and Feishu
    /// means the robot must be configured without a signature check.
    pub secret_env: Option<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

/// An id that can survive being turned into a keyring entry name.
///
/// `SecretName` accepts `A-Z 0-9 _` only, and `provider_secret_name` gets there
/// by uppercasing and replacing `-`. Anything else — a space, a dot, a CJK
/// character — produces a name that is refused at the moment a key is stored,
/// which is long after the file looked fine.
fn validate_id(kind: &str, id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err(format!("{kind} id must not be empty"));
    }
    if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(format!(
            "{kind} id `{id}` must contain only letters, digits, `-` or `_`"
        ));
    }
    Ok(())
}

/// An environment variable name, checked for the shape a shell can actually
/// export. A lowercase or punctuated name is almost always a key pasted in by
/// mistake, which is the case worth catching loudly.
///
/// **The offending value is never echoed.** By construction the most likely
/// reason this fails is that somebody pasted the credential itself into the
/// field that names its variable — so repeating it back puts the credential on
/// stdout, which for a daemon is whatever the supervisor collects. Log the
/// length instead is the standing rule, and the note beside it says not to lean
/// on the log redactor either: it matches four shapes, and the provider keys
/// this field attracts are not all of them.
///
/// The entry's own id is enough to find the line — it is right there in the
/// message, and nothing about an id is a secret.
fn validate_env_name(kind: &str, owner: &str, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("{kind} `{owner}` names an empty environment variable"));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
    {
        // No shell syntax here: this runs on Windows too, where `export` is
        // wrong and the difference between a shell variable and an environment
        // variable is exactly what people get caught by. Both spellings are in
        // the README, which is where a worked example belongs.
        return Err(format!(
            "{kind} `{owner}`: this field takes the *name* of an environment variable \
             (A-Z, 0-9, _), not its value — the {} characters given are not one. \
             Name a variable here, e.g. `api_key_env = \"DEEPSEEK_DEV_KEY\"`, and put the \
             credential in that variable in the environment this process runs under. \
             The value is not repeated here in case it is the credential.",
            name.chars().count()
        ));
    }
    Ok(())
}

impl DaemonConfig {
    pub fn parse(source: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(source).map_err(|error| format!("invalid configuration: {error}"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn read(path: &Path) -> Result<Self, String> {
        let source =
            std::fs::read_to_string(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
        Self::parse(&source)
    }

    fn validate(&self) -> Result<(), String> {
        let mut provider_ids = HashSet::new();
        for provider in &self.provider {
            validate_id("provider", &provider.id)?;
            if !provider_ids.insert(&provider.id) {
                return Err(format!("provider id `{}` appears twice", provider.id));
            }
            if provider.name.trim().is_empty() {
                return Err(format!("provider `{}` has an empty name", provider.id));
            }
            validate_env_name("provider", &provider.id, &provider.api_key_env)?;
            meridian_core::notify::validate_webhook_url(&provider.base_url)
                .map_err(|error| format!("provider `{}`: base_url {error}", provider.id))?;
        }

        // The same ceiling the desktop's create command enforces. It is not a
        // storage limit: it is what stops one alert from becoming a hundred
        // outbound requests, and that reason does not care which path the row
        // was written through. Without this the file was the way around it.
        if self.webhook.len() > MAX_WEBHOOKS {
            return Err(format!(
                "at most {MAX_WEBHOOKS} webhooks are supported; this file names {}",
                self.webhook.len()
            ));
        }

        let mut webhook_ids = HashSet::new();
        for webhook in &self.webhook {
            validate_id("webhook", &webhook.id)?;
            if !webhook_ids.insert(&webhook.id) {
                return Err(format!("webhook id `{}` appears twice", webhook.id));
            }
            meridian_core::notify::validate_endpoint(&webhook.name, &webhook.url, &webhook.events)
                .map_err(|error| format!("webhook `{}`: {error}", webhook.id))?;
            if let Some(env) = &webhook.secret_env {
                validate_env_name("webhook", &webhook.id, env)?;
            }
        }

        // An enabled watcher with nowhere to deliver is the one setup that
        // looks configured and can never say anything. It is worth refusing to
        // start over, because the symptom is silence.
        if self.notify.enabled.unwrap_or(false) && !self.webhook.iter().any(|webhook| webhook.enabled) {
            return Err(
                "notify.enabled is true but no webhook is enabled; nothing would ever be told. \
                 Add a [[webhook]], or set notify.enabled = false."
                    .into(),
            );
        }
        Ok(())
    }

    /// The watcher settings, on top of the core defaults.
    pub fn notify_config(&self) -> Result<meridian_core::notify::NotifyConfig, String> {
        let defaults = meridian_core::notify::NotifyConfig::default();
        let section = &self.notify;
        let config = meridian_core::notify::NotifyConfig {
            enabled: section.enabled.unwrap_or(defaults.enabled),
            balance_threshold: section.balance_threshold.clone(),
            balance_interval_minutes: section
                .balance_interval_minutes
                .unwrap_or(defaults.balance_interval_minutes),
            usage_enabled: section.usage_enabled.unwrap_or(defaults.usage_enabled),
            usage_check_interval_minutes: section
                .usage_check_interval_minutes
                .unwrap_or(defaults.usage_check_interval_minutes),
            usage_window_hours: section.usage_window_hours.unwrap_or(defaults.usage_window_hours),
            usage_baseline_days: section.usage_baseline_days.unwrap_or(defaults.usage_baseline_days),
            usage_multiplier: section.usage_multiplier.clone().unwrap_or(defaults.usage_multiplier),
            usage_min_cost: section.usage_min_cost.clone().unwrap_or(defaults.usage_min_cost),
            usage_cooldown_minutes: section
                .usage_cooldown_minutes
                .unwrap_or(defaults.usage_cooldown_minutes),
        };
        // The core's own bounds, applied here so a bad file is refused before
        // anything is written rather than at the first tick.
        meridian_core::notify::validate(&config)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [notify]
        enabled = true
        balance_threshold = "5"

        [[provider]]
        id = "deepseek-prod"
        name = "DeepSeek prod"
        type = "deepseek"
        base_url = "https://api.deepseek.com"
        api_key_env = "DEEPSEEK_PROD_KEY"

        [[webhook]]
        id = "ops"
        name = "运维群"
        url = "https://oapi.dingtalk.com/robot/send?access_token=x"
        format = "dingtalk"
        events = ["balance_low", "balance_unavailable"]
        secret_env = "OPS_DINGTALK_SECRET"
    "#;

    #[test]
    fn the_documented_shape_parses() {
        let config = DaemonConfig::parse(MINIMAL).unwrap();
        assert_eq!(config.provider.len(), 1);
        assert_eq!(config.provider[0].provider_type, "deepseek");
        assert!(config.provider[0].enabled, "entries are on unless switched off");
        assert_eq!(config.webhook[0].format, NotificationFormat::Dingtalk);
        assert_eq!(config.webhook[0].events.len(), 2);
        let notify = config.notify_config().unwrap();
        assert!(notify.enabled);
        assert_eq!(notify.balance_threshold, Some("5".parse().unwrap()));
        assert_eq!(
            notify.balance_interval_minutes, 360,
            "an absent key means the core default, not a restated one"
        );
    }

    /// A misspelled key is somebody believing they configured something. The
    /// symptom of accepting it is an alert that never fires.
    #[test]
    fn an_unknown_key_refuses_the_file() {
        let typo = MINIMAL.replace("balance_threshold", "balance_thresh");
        let error = DaemonConfig::parse(&typo).unwrap_err();
        assert!(error.contains("balance_thresh"), "{error}");

        let unknown_table = format!("{MINIMAL}\n[mystery]\nx = 1\n");
        assert!(DaemonConfig::parse(&unknown_table).is_err());
    }

    /// The field names the variable, never the value. Accepting a pasted key
    /// with a warning is how one reaches a git history.
    #[test]
    fn a_pasted_secret_is_refused_rather_than_warned_about() {
        let pasted = MINIMAL.replace("\"DEEPSEEK_PROD_KEY\"", "\"sk-abc123def456\"");
        let error = DaemonConfig::parse(&pasted).unwrap_err();
        assert!(error.contains("not its value"), "{error}");
    }

    /// The refusal must not repeat what it refused.
    ///
    /// By construction the likeliest reason this field is wrong is that the
    /// credential itself was pasted into it — so echoing it back puts the
    /// credential on stdout, which for a daemon is whatever the supervisor
    /// collects. The log redactor is not a defence to lean on here: it matches
    /// four shapes, and the keys this field attracts are not all of them.
    #[test]
    fn the_refusal_never_repeats_the_value_it_refused() {
        // Deliberately shaped like a credential the log redactor would *not*
        // catch — no `sk-` prefix, no `AKIA`, no `Bearer`, no assignment — since
        // that is the case which makes echoing indefensible. Deliberately *not*
        // shaped like any real vendor's token either: a convincing fake in a
        // test file is a push blocked by secret scanning, which is how this
        // test was first written.
        let secret = "this-value-stands-in-for-a-pasted-credential";
        let pasted = MINIMAL.replace("\"DEEPSEEK_PROD_KEY\"", &format!("\"{secret}\""));
        let error = DaemonConfig::parse(&pasted).unwrap_err();

        assert!(!error.contains(secret), "the credential is in the message: {error}");
        assert!(!error.contains("stands-in"), "even a fragment of it: {error}");
        // Still findable: the entry is named, and the length says which value.
        assert!(error.contains("deepseek-prod"), "{error}");
        assert!(
            error.contains(&secret.chars().count().to_string()),
            "the length stands in for the value: {error}"
        );

        // The same for a webhook's signing secret.
        let pasted = MINIMAL.replace("\"OPS_DINGTALK_SECRET\"", &format!("\"{secret}\""));
        let error = DaemonConfig::parse(&pasted).unwrap_err();
        assert!(!error.contains(secret), "{error}");
        assert!(error.contains("ops"), "{error}");
    }

    /// An id that cannot become a keyring entry name fails now, rather than at
    /// the moment a key is stored under it.
    #[test]
    fn an_id_that_could_not_name_a_secret_is_refused() {
        for bad in ["deepseek prod", "深度求索", "prod.1", ""] {
            let broken = MINIMAL.replace("\"deepseek-prod\"", &format!("\"{bad}\""));
            assert!(DaemonConfig::parse(&broken).is_err(), "id {bad:?} must be refused");
        }
        for good in ["deepseek-prod", "deepseek_prod", "dsProd1"] {
            let fine = MINIMAL.replace("\"deepseek-prod\"", &format!("\"{good}\""));
            assert!(DaemonConfig::parse(&fine).is_ok(), "id {good:?} must be accepted");
        }
    }

    #[test]
    fn a_repeated_id_is_refused_because_applying_it_twice_would_not_converge() {
        let doubled = format!(
            "{MINIMAL}\n[[provider]]\nid = \"deepseek-prod\"\nname = \"other\"\ntype = \"deepseek\"\n\
             base_url = \"https://api.deepseek.com\"\napi_key_env = \"OTHER_KEY\"\n"
        );
        let error = DaemonConfig::parse(&doubled).unwrap_err();
        assert!(error.contains("appears twice"), "{error}");
    }

    /// The one setup that looks configured and is guaranteed to stay silent.
    #[test]
    fn watching_with_nowhere_to_report_is_refused() {
        let no_outlet = r#"
            [notify]
            enabled = true
            balance_threshold = "5"
        "#;
        let error = DaemonConfig::parse(no_outlet).unwrap_err();
        assert!(error.contains("nothing would ever be told"), "{error}");

        let switched_off = r#"
            [notify]
            enabled = false
        "#;
        assert!(
            DaemonConfig::parse(switched_off).is_ok(),
            "a watcher that is off needs no outlet"
        );
    }

    #[test]
    fn an_endpoint_subscribed_to_nothing_is_refused_by_the_core_rule() {
        let empty_events = MINIMAL.replace(r#"["balance_low", "balance_unavailable"]"#, "[]");
        assert!(DaemonConfig::parse(&empty_events).is_err());

        let unknown_event = MINIMAL.replace(r#""balance_unavailable""#, r#""everything""#);
        assert!(DaemonConfig::parse(&unknown_event).is_err());
    }

    /// Money is a decimal string in this file exactly as it is on the wire; a
    /// TOML float has already been through a binary double by the time it is
    /// parsed.
    #[test]
    fn money_is_a_string_and_a_bare_number_is_refused() {
        let floated = MINIMAL.replace(r#"balance_threshold = "5""#, "balance_threshold = 5.0");
        assert!(DaemonConfig::parse(&floated).is_err());
    }

    /// The core's bounds reach the file, so a value that would be refused at
    /// save time on the desktop is refused at parse time here.
    #[test]
    fn the_core_bounds_apply_to_the_file() {
        let bad_multiplier = format!("{MINIMAL}\n");
        let bad_multiplier = bad_multiplier.replace("[notify]", "[notify]\nusage_multiplier = \"0.5\"");
        let error = DaemonConfig::parse(&bad_multiplier)
            .unwrap()
            .notify_config()
            .unwrap_err();
        assert!(error.contains("multiplier"), "{error}");
    }

    /// The ceiling exists so one alert cannot become a hundred requests, which
    /// is a reason that does not care whether the row came from this file or
    /// from the desktop's create command. Enforced only there, the file was the
    /// documented way around it.
    #[test]
    fn the_endpoint_ceiling_is_the_same_one_the_desktop_enforces() {
        let entry = |n: usize| {
            format!(
                "[[webhook]]\nid = \"w{n}\"\nname = \"w\"\nurl = \"https://a.invalid/h\"\n\
                 format = \"generic\"\nevents = [\"test\"]\n"
            )
        };
        let at_limit: String = (0..MAX_WEBHOOKS).map(entry).collect();
        assert!(DaemonConfig::parse(&at_limit).is_ok(), "{MAX_WEBHOOKS} is the limit");

        let over: String = (0..MAX_WEBHOOKS + 1).map(entry).collect();
        let error = DaemonConfig::parse(&over).unwrap_err();
        assert!(error.contains(&(MAX_WEBHOOKS + 1).to_string()), "{error}");
    }

    #[test]
    fn an_empty_file_is_a_daemon_that_does_nothing() {
        let config = DaemonConfig::parse("").unwrap();
        assert!(config.provider.is_empty());
        assert!(config.webhook.is_empty());
        assert!(!config.notify_config().unwrap().enabled);
    }
}
