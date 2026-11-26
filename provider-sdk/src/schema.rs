//! Declarative vendor manifest and form-field schemas.
//!
//! External vendor PRs declare their metadata, supported capability domains,
//! official protocols, auth methods, and non-secret configuration fields here
//! so the UI can render forms without vendor-specific frontend branches.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::domain::CapabilityDomain;

/// One non-secret or secret configuration field declared by a vendor manifest.
///
/// Fields with `secret: true` are never persisted in SQLite `config_json`;
/// they are routed exclusively through `SecretsManager`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFieldSpec {
    pub key: String,
    pub label: String,
    pub kind: ConfigFieldKind,
    pub required: bool,
    pub secret: bool,
    #[serde(deserialize_with = "crate::wire::deserialize_required_nullable")]
    pub default_value: Option<String>,
    #[serde(deserialize_with = "crate::wire::deserialize_required_nullable")]
    pub placeholder: Option<String>,
    pub options: Vec<ConfigFieldOption>,
    #[serde(deserialize_with = "crate::wire::deserialize_required_nullable")]
    pub visible_when_protocol: Option<Vec<String>>,
    #[serde(deserialize_with = "crate::wire::deserialize_required_nullable")]
    pub visible_when_auth: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFieldKind {
    Text,
    Secret,
    Select,
    Boolean,
    FilePath,
    MultilineSecret,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFieldOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebsitesSpec {
    #[serde(deserialize_with = "crate::wire::deserialize_required_nullable")]
    pub official: Option<String>,
    #[serde(deserialize_with = "crate::wire::deserialize_required_nullable")]
    pub api_key: Option<String>,
    #[serde(deserialize_with = "crate::wire::deserialize_required_nullable")]
    pub docs: Option<String>,
    #[serde(deserialize_with = "crate::wire::deserialize_required_nullable")]
    pub models: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsDevMapping {
    pub default: Vec<String>,
    pub cn: Vec<String>,
}

impl ModelsDevMapping {
    /// Resolve ordered models.dev provider ids for the given `base_url`.
    /// Mainland `.cn` endpoints prefer `cn` when declared and fall back to `default`.
    pub fn provider_ids_for_base_url(&self, base_url: &str) -> &[String] {
        if base_url.to_ascii_lowercase().contains(".cn") && !self.cn.is_empty() {
            &self.cn
        } else {
            &self.default
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthMethodSpec {
    pub id: String,
    pub credential_kind: String,
    pub transport_profile: String,
    pub api_formats: Vec<String>,
    pub default_base_url: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelGroupSpec {
    pub family: String,
    pub ids: Vec<String>,
}

/// Complete declarative description of one vendor directory (`vendors/<id>/manifest.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorManifest {
    pub version: u32,
    pub id: String,
    /// Legacy closed adapter family (`openai`, `anthropic`, `deepseek`, `xai`,
    /// `google`, `litert_lm`, or `null` for non-chat domains such as web search / TTS).
    #[serde(deserialize_with = "crate::wire::deserialize_required_nullable")]
    pub provider_type: Option<String>,
    pub name: String,
    pub icon: String,
    /// Explicit ordering in the vendor picker (lower comes first).
    pub sort_order: i32,
    pub domains: Vec<CapabilityDomain>,
    pub balance: bool,
    pub websites: WebsitesSpec,
    pub auth: Vec<AuthMethodSpec>,
    pub config_fields: Vec<ConfigFieldSpec>,
    #[serde(deserialize_with = "crate::wire::deserialize_required_nullable")]
    pub models_dev: Option<ModelsDevMapping>,
    pub models: Vec<ModelGroupSpec>,
}

impl VendorManifest {
    pub fn supports_domain(&self, domain: CapabilityDomain) -> bool {
        self.domains.contains(&domain)
    }

    pub fn default_auth(&self) -> Option<&AuthMethodSpec> {
        self.auth.first()
    }

    pub fn auth_option(&self, id: &str) -> Option<&AuthMethodSpec> {
        self.auth.iter().find(|option| option.id == id)
    }

    /// Strictly validate non-secret `config_json` values against `self.config_fields`.
    ///
    /// Rules (zero forward-compat / silent fallback):
    /// - Secret fields (`secret == true`) are rejected in `config_json` because they
    ///   must live exclusively in `SecretsManager`.
    /// - Unknown keys are rejected.
    /// - Required visible non-secret fields must be present and non-empty.
    /// - Field value types must match `ConfigFieldKind` (`Boolean`, `Select`, etc.).
    pub fn validate_non_secret_config(
        &self,
        protocol: Option<&str>,
        auth_id: Option<&str>,
        values: &BTreeMap<String, serde_json::Value>,
    ) -> Result<(), String> {
        let mut allowed_keys = BTreeSet::new();
        for field in &self.config_fields {
            if field.secret {
                if values.contains_key(&field.key) {
                    return Err(format!(
                        "secret config field `{}` must be stored in SecretsManager, not config_json",
                        field.key
                    ));
                }
                continue;
            }
            allowed_keys.insert(field.key.as_str());

            let visible = field.is_visible(protocol, auth_id);
            match values.get(&field.key) {
                None | Some(serde_json::Value::Null) => {
                    if visible && field.required && field.default_value.is_none() {
                        return Err(format!(
                            "missing required config field `{}` for vendor `{}`",
                            field.key, self.id
                        ));
                    }
                }
                Some(value) => {
                    validate_field_value(field, value)?;
                }
            }
        }

        for key in values.keys() {
            if !allowed_keys.contains(key.as_str()) {
                return Err(format!("unknown config field `{key}` for vendor `{}`", self.id));
            }
        }
        Ok(())
    }
}

impl ConfigFieldSpec {
    pub fn is_visible(&self, protocol: Option<&str>, auth_id: Option<&str>) -> bool {
        let protocol_ok = match (&self.visible_when_protocol, protocol) {
            (None, _) => true,
            (Some(allowed), Some(current)) => allowed.iter().any(|p| p == current),
            (Some(_), None) => false,
        };
        let auth_ok = match (&self.visible_when_auth, auth_id) {
            (None, _) => true,
            (Some(allowed), Some(current)) => allowed.iter().any(|a| a == current),
            (Some(_), None) => false,
        };
        protocol_ok && auth_ok
    }
}

fn validate_field_value(field: &ConfigFieldSpec, value: &serde_json::Value) -> Result<(), String> {
    match field.kind {
        ConfigFieldKind::Boolean => {
            if !value.is_boolean() {
                return Err(format!("config field `{}` must be a boolean", field.key));
            }
        }
        ConfigFieldKind::Select => {
            let s = value
                .as_str()
                .ok_or_else(|| format!("config field `{}` must be a string", field.key))?;
            if !field.options.iter().any(|opt| opt.value == s) {
                return Err(format!(
                    "config field `{}` has unsupported option `{s}`",
                    field.key
                ));
            }
        }
        ConfigFieldKind::Text | ConfigFieldKind::FilePath => {
            let s = value
                .as_str()
                .ok_or_else(|| format!("config field `{}` must be a string", field.key))?;
            if field.required && s.trim().is_empty() {
                return Err(format!("config field `{}` must not be empty", field.key));
            }
        }
        ConfigFieldKind::Secret | ConfigFieldKind::MultilineSecret => {
            return Err(format!(
                "secret config field `{}` must be stored in SecretsManager",
                field.key
            ));
        }
    }
    Ok(())
}

impl AuthMethodSpec {
    pub fn default_api_format(&self) -> Option<&str> {
        self.api_formats.first().map(String::as_str)
    }

    pub fn format_is_a_choice(&self) -> bool {
        self.api_formats.len() > 1
    }

    pub fn base_url_for(&self, api_format: &str) -> Option<&str> {
        self.default_base_url.get(api_format).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> VendorManifest {
        VendorManifest {
            version: 1,
            id: "sample".into(),
            provider_type: Some("openai".into()),
            name: "Sample".into(),
            icon: "openai".into(),
            sort_order: 10,
            domains: vec![CapabilityDomain::Chat],
            balance: false,
            websites: WebsitesSpec::default(),
            auth: vec![],
            config_fields: vec![
                ConfigFieldSpec {
                    key: "region".into(),
                    label: "Region".into(),
                    kind: ConfigFieldKind::Select,
                    required: true,
                    secret: false,
                    default_value: None,
                    placeholder: None,
                    options: vec![
                        ConfigFieldOption {
                            value: "us".into(),
                            label: "US".into(),
                        },
                        ConfigFieldOption {
                            value: "eu".into(),
                            label: "EU".into(),
                        },
                    ],
                    visible_when_protocol: Some(vec!["chat_completions".into()]),
                    visible_when_auth: None,
                },
                ConfigFieldSpec {
                    key: "enable_beta".into(),
                    label: "Enable Beta".into(),
                    kind: ConfigFieldKind::Boolean,
                    required: false,
                    secret: false,
                    default_value: Some("false".into()),
                    placeholder: None,
                    options: vec![],
                    visible_when_protocol: None,
                    visible_when_auth: None,
                },
                ConfigFieldSpec {
                    key: "api_secret".into(),
                    label: "Secret".into(),
                    kind: ConfigFieldKind::Secret,
                    required: true,
                    secret: true,
                    default_value: None,
                    placeholder: None,
                    options: vec![],
                    visible_when_protocol: None,
                    visible_when_auth: None,
                },
            ],
            models_dev: Some(ModelsDevMapping {
                default: vec!["sample-global".into()],
                cn: vec!["sample-cn".into(), "sample-global".into()],
            }),
            models: vec![],
        }
    }

    #[test]
    fn rejects_unknown_keys_secret_leakage_and_invalid_select_values() {
        let m = sample_manifest();

        // Missing required visible field fails
        let empty = BTreeMap::new();
        assert!(m.validate_non_secret_config(Some("chat_completions"), None, &empty).is_err());

        // Hidden required field is allowed to be omitted
        assert!(m.validate_non_secret_config(Some("responses"), None, &empty).is_ok());

        // Valid config passes
        let mut ok = BTreeMap::new();
        ok.insert("region".into(), serde_json::json!("us"));
        ok.insert("enable_beta".into(), serde_json::json!(true));
        assert!(m.validate_non_secret_config(Some("chat_completions"), None, &ok).is_ok());

        // Invalid select value fails
        let mut bad_opt = BTreeMap::new();
        bad_opt.insert("region".into(), serde_json::json!("ap"));
        assert!(m.validate_non_secret_config(Some("chat_completions"), None, &bad_opt).is_err());

        // Unknown field fails
        let mut unknown = ok.clone();
        unknown.insert("extra_junk".into(), serde_json::json!("x"));
        assert!(m.validate_non_secret_config(Some("chat_completions"), None, &unknown).is_err());

        // Secret field in config_json fails
        let mut leaked = ok.clone();
        leaked.insert("api_secret".into(), serde_json::json!("sk-leak"));
        assert!(m.validate_non_secret_config(Some("chat_completions"), None, &leaked).is_err());
    }

    #[test]
    fn models_dev_mapping_prefers_cn_for_mainland_host() {
        let m = sample_manifest();
        let mapping = m.models_dev.as_ref().unwrap();
        assert_eq!(
            mapping.provider_ids_for_base_url("https://api.sample.cn/v1"),
            &["sample-cn".to_string(), "sample-global".to_string()]
        );
        assert_eq!(
            mapping.provider_ids_for_base_url("https://api.sample.com/v1"),
            &["sample-global".to_string()]
        );
    }
}
