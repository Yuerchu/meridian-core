//! What a delegated run is, and which models it may be given.
//!
//! The loop knows only that some tool call means "hand this to another agent";
//! everything about *which* agent and *what it costs* lives here, so that
//! neither the engine nor the runner has to carry it.

use diesel::sqlite::SqliteConnection;

use crate::decimal::Decimal;
use crate::provider::capabilities;

/// The tool that delegates. Handled by the loop, like `ask_user` and the mode
/// transitions: the registry entry exists so the definition has one home, and
/// its `execute` refuses.
pub const RUN_AGENT_TOOL: &str = "run_agent";

/// Which built-in agent to run.
///
/// Two, deliberately. `Explore` is the one that can be handed out freely
/// because its tool set cannot change anything; `Agent` inherits whatever the
/// main assistant may do, including the user's standing yes to edits. A third
/// kind is a settings feature, not a loop feature.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, strum::IntoStaticStr, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SubAgentKind {
    Explore,
    Agent,
}

impl SubAgentKind {
    /// Stored on the sub-agent's conversation, and named in the event the card
    /// is drawn from.
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| {
            format!(
                "unknown agent '{value}'. Use \"explore\" for read-only investigation \
                 or \"agent\" for work that changes things."
            )
        })
    }
}

/// One model a sub-agent may be given, with the facts a model needs to choose
/// between them.
///
/// Prices remain optional so an unconfigured model is distinct from an
/// explicitly free model. Amounts are exact decimals all the way into the
/// model-facing catalog.
#[derive(Clone)]
pub struct AgentModel {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    /// The user's own name for it, if they set one. The one place they can put
    /// a hint like "cheap, good for bulk edits" in front of the model without
    /// anything new being built for it.
    pub display_name: Option<String>,
    pub context_window: Option<u32>,
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub supports_thinking: bool,
}

impl AgentModel {
    /// How the model names it. Qualified because two providers serving the same
    /// model id is ordinary, and an ambiguous name resolves to whichever row
    /// came back first.
    pub(crate) fn qualified(&self) -> String {
        format!("{}:{}", self.provider_id, self.model_id)
    }

    /// One line of the roster: what it can hold, whether it reasons, what it
    /// costs.
    fn describe(&self) -> String {
        let mut line = format!("- {}", self.qualified());
        if let Some(name) = self.display_name.as_deref().filter(|n| !n.trim().is_empty()) {
            line.push_str(&format!(" ({name})"));
        }
        line.push_str(&format!(" [{}]", self.provider_name));

        let mut facts: Vec<String> = Vec::new();
        if let Some(ctx) = self.context_window {
            facts.push(format!("{}K context", ctx / 1000));
        }
        if self.supports_thinking {
            facts.push("reasoning".to_string());
        }
        match (&self.input_price, &self.output_price) {
            (Some(i), Some(o)) => facts.push(format!("${i}/${o} per Mtok")),
            _ => facts.push("no price configured".to_string()),
        }
        line.push_str(" — ");
        line.push_str(&facts.join(", "));
        line
    }
}

/// The models on offer this turn, cheapest first.
#[derive(Clone)]
pub struct SubAgentCatalog {
    pub models: Vec<AgentModel>,
}

impl SubAgentCatalog {
    /// The roster, as the tool's description carries it.
    pub(crate) fn describe(&self) -> String {
        let mut out = String::from(
            "\n\nModels available to sub-agents, cheapest first. Prefer the cheapest one that \
             can do the job. \"No price configured\" means nobody has entered its rates — it \
             does not mean it is free:\n",
        );
        for m in &self.models {
            out.push_str(&m.describe());
            out.push('\n');
        }
        out
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.models.iter().map(AgentModel::qualified).collect()
    }
}

/// Every model a sub-agent could be given, read from what the user has already
/// configured.
///
/// Three sources, none of them new: the cached model lists behind the model
/// picker, the per-model rows behind the pricing settings, and the capability
/// table. Models that cannot call tools are left out — a sub-agent without
/// tools is a single completion, which is a different feature.
pub fn catalog(conn: &mut SqliteConnection) -> Result<SubAgentCatalog, String> {
    let providers = crate::db::ops::provider::list_providers(conn).map_err(|error| error.to_string())?;
    let mut models: Vec<AgentModel> = Vec::new();

    for p in providers.into_iter().filter(|p| p.is_enabled != 0) {
        let cached = crate::db::ops::cached_model::list_by_provider(conn, &p.id).map_err(|error| error.to_string())?;
        let configs = crate::db::ops::model_config::list_by_provider(conn, &p.id).map_err(|error| error.to_string())?;

        for c in cached {
            let cfg = configs.iter().find(|m| m.model_id == c.model_id);
            let mut caps = crate::provider::registry::get_capabilities(
                &p.provider_type,
                &p.api_format,
                &p.transport_profile,
                &c.model_id,
            )?;
            capabilities::apply_overrides(&mut caps, cfg.and_then(|m| m.capability_overrides.as_deref()))?;
            if !caps.supports_tools {
                continue;
            }
            models.push(AgentModel {
                provider_id: p.id.clone(),
                provider_name: p.name.clone(),
                model_id: c.model_id,
                display_name: cfg.and_then(|m| m.display_name.clone()),
                context_window: cfg
                    .map(|m| m.context_window as u32)
                    .filter(|w| *w > 0)
                    .or(caps.max_context_tokens),
                input_price: cfg.and_then(|m| m.input_price.clone()),
                output_price: cfg.and_then(|m| m.output_price.clone()),
                supports_thinking: caps.supports_thinking,
            });
        }
    }

    // Cheapest first, and everything with no price at the very end rather than
    // at the front where a missing number would read as zero.
    models.sort_by(|a, b| match (&a.input_price, &b.input_price) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.qualified().cmp(&b.qualified()),
    });

    Ok(SubAgentCatalog { models })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decimal(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    fn model(id: &str, price: Option<Decimal>) -> AgentModel {
        AgentModel {
            provider_id: "p".into(),
            provider_name: "P".into(),
            model_id: id.into(),
            display_name: None,
            context_window: Some(64_000),
            input_price: price.clone(),
            output_price: price,
            supports_thinking: false,
        }
    }

    /// A model nobody has priced must not read as the cheapest. The settings
    /// page stores `0` for an untouched field, so "free" and "unknown" are the
    /// same row, and sorting them first would make the model pick whichever one
    /// had been ignored longest.
    #[test]
    fn unpriced_models_sort_last_and_say_so() {
        let catalog = SubAgentCatalog {
            models: {
                let mut m = vec![
                    model("expensive", Some(decimal("3"))),
                    model("free", None),
                    model("cheap", Some(decimal("0.14"))),
                ];
                m.sort_by(|a, b| match (&a.input_price, &b.input_price) {
                    (Some(x), Some(y)) => x.cmp(y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                });
                m
            },
        };

        assert_eq!(catalog.names(), ["p:cheap", "p:expensive", "p:free"]);
        let text = catalog.describe();
        assert!(text.contains("does not mean it is free"));
        let free_line = text.lines().find(|l| l.contains("p:free")).unwrap();
        assert!(free_line.contains("no price configured"), "{free_line}");
    }

    /// The one place a user's own words reach the model that is choosing.
    #[test]
    fn a_display_name_travels_and_an_empty_one_leaves_no_brackets() {
        let mut named = model("m", Some(decimal("1")));
        named.display_name = Some("cheap, good for bulk edits".into());
        assert!(named.describe().contains("(cheap, good for bulk edits)"));

        let mut blank = model("m", Some(decimal("1")));
        blank.display_name = Some("   ".into());
        assert!(!blank.describe().contains("()"), "{}", blank.describe());
    }

    #[test]
    fn kinds_round_trip_and_an_unknown_one_explains_itself() {
        for kind in [SubAgentKind::Explore, SubAgentKind::Agent] {
            assert_eq!(SubAgentKind::parse(kind.as_str()), Ok(kind));
        }
        let err = SubAgentKind::parse("researcher").unwrap_err();
        assert!(err.contains("explore"), "{err}");
        assert!(err.contains("agent"), "{err}");
    }
}
