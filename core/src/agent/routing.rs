//! `SwitchImpact`-driven multi-provider model routing and failover.
//!
//! Core rule (confirmed with user):
//! - The sole criterion for automatic cross-provider failover is whether
//!   switching causes observable negative side effects (`SwitchImpact`).
//! - `SwitchImpact::Stateless` (image/video generation, web search, TTS,
//!   embeddings, stateless one-shot calls) permits automatic priority failover
//!   on retryable errors (429 / 5xx / network timeout).
//! - `SwitchImpact::Stateful { breaks_prompt_cache, breaks_reasoning_signature,
//!   breaks_upstream_session }` (multi-turn chat with prompt cache / Anthropic
//!   thinking signatures / Responses encrypted reasoning) **never** silently
//!   fails over: it stays session-sticky and surfaces the exact impact +
//!   alternative deployments for the user to decide, unless the user explicitly
//!   enabled `RoutingPolicy::UserOptInFailover`.
//!
//! Ordering rule (Official Standard Protocol Policy):
//! - Among enabled deployments for the same logical model, deployments using an
//!   official standard protocol (`ProtocolCompliance::Standard`) sort ahead of
//!   `Unknown` and `Degraded` compatibility relays, then by ascending `priority`.

use serde::{Deserialize, Serialize};

use crate::provider::{ProtocolCompliance, RoutingPolicy, SwitchImpact};

/// One candidate provider deployment serving a logical model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentCandidate {
    pub deployment_id: String,
    pub provider_id: String,
    pub provider_name: String,
    pub remote_model_id: String,
    pub protocol_id: String,
    pub is_enabled: bool,
    pub priority: i32,
    pub compliance: ProtocolCompliance,
}

impl DeploymentCandidate {
    /// Lower rank is preferred: official standard protocols always outrank
    /// degraded compatibility relays by default.
    fn compliance_rank(&self) -> u8 {
        match self.compliance {
            ProtocolCompliance::Standard => 0,
            ProtocolCompliance::Unknown => 1,
            ProtocolCompliance::Degraded { .. } => 2,
        }
    }
}

/// Ordered candidate selection for the start of a turn or stateless request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePlan {
    pub primary: DeploymentCandidate,
    pub alternatives: Vec<DeploymentCandidate>,
    pub switch_impact: SwitchImpact,
    pub sticky_locked: bool,
}

/// Runtime failover decision after a deployment returns an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverDecision {
    /// Non-retryable error (4xx validation/auth) or no healthy alternative exists.
    SurfaceError,
    /// Safe stateless automatic switch to the next highest-priority deployment.
    AutoSwitch { next: DeploymentCandidate },
    /// Stateful turn with `RoutingPolicy::UserOptInFailover`: switch automatically
    /// AND emit an explicit system notice carrying `impact`.
    AutoSwitchWithNotice {
        next: DeploymentCandidate,
        impact: SwitchImpact,
    },
    /// Stateful turn under `StickyManual` / `AutoFailoverStateless`: NEVER
    /// silently switch; surface the failure, the exact switching cost, and the
    /// available backup deployments so the user decides.
    RequireUserDecision {
        impact: SwitchImpact,
        alternatives: Vec<DeploymentCandidate>,
    },
}

/// Sort enabled candidates by Official Standard Protocol rank first, then
/// ascending user `priority`, then deterministic `deployment_id`.
pub fn rank_enabled_candidates(candidates: &[DeploymentCandidate]) -> Vec<DeploymentCandidate> {
    let mut enabled: Vec<DeploymentCandidate> = candidates.iter().filter(|c| c.is_enabled).cloned().collect();
    enabled.sort_by(|a, b| {
        a.compliance_rank()
            .cmp(&b.compliance_rank())
            .then_with(|| a.priority.cmp(&b.priority))
            .then_with(|| a.deployment_id.cmp(&b.deployment_id))
    });
    enabled
}

/// Choose the initial deployment and backup list for a request.
///
/// If `pinned_deployment_id` is set and enabled, it is chosen as `primary`
/// (preserving session stickiness even if global priorities changed); otherwise
/// the highest-ranked enabled candidate is selected.
pub fn plan_route(
    candidates: &[DeploymentCandidate],
    pinned_deployment_id: Option<&str>,
    switch_impact: SwitchImpact,
) -> Result<RoutePlan, String> {
    let ranked = rank_enabled_candidates(candidates);
    if ranked.is_empty() {
        return Err("no enabled provider deployment is configured for this model".into());
    }

    if let Some(pinned_id) = pinned_deployment_id
        && let Some(pos) = ranked.iter().position(|c| c.deployment_id == pinned_id)
    {
        let mut alternatives = ranked;
        let primary = alternatives.remove(pos);
        return Ok(RoutePlan {
            primary,
            alternatives,
            switch_impact,
            sticky_locked: true,
        });
    }

    let mut iter = ranked.into_iter();
    let primary = iter.next().expect("non-empty ranked candidates");
    let alternatives: Vec<DeploymentCandidate> = iter.collect();
    let sticky_locked = !switch_impact.allows_automatic_failover();
    Ok(RoutePlan {
        primary,
        alternatives,
        switch_impact,
        sticky_locked,
    })
}

/// Decide how to react when `current_deployment_id` fails.
///
/// Enforces the hard invariant: a `SwitchImpact::Stateful` turn NEVER silently
/// fails over unless the user explicitly selected `RoutingPolicy::UserOptInFailover`.
pub fn decide_failover(
    policy: RoutingPolicy,
    switch_impact: SwitchImpact,
    current_deployment_id: &str,
    candidates: &[DeploymentCandidate],
    error_is_retryable: bool,
) -> FailoverDecision {
    if !error_is_retryable {
        return FailoverDecision::SurfaceError;
    }

    let remaining: Vec<DeploymentCandidate> = rank_enabled_candidates(candidates)
        .into_iter()
        .filter(|c| c.deployment_id != current_deployment_id)
        .collect();

    let Some(next) = remaining.first().cloned() else {
        return FailoverDecision::SurfaceError;
    };

    if switch_impact.allows_automatic_failover() {
        return FailoverDecision::AutoSwitch { next };
    }

    match policy {
        RoutingPolicy::UserOptInFailover => FailoverDecision::AutoSwitchWithNotice {
            next,
            impact: switch_impact,
        },
        RoutingPolicy::StickyManual | RoutingPolicy::AutoFailoverStateless => FailoverDecision::RequireUserDecision {
            impact: switch_impact,
            alternatives: remaining,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, priority: i32, compliance: ProtocolCompliance, enabled: bool) -> DeploymentCandidate {
        DeploymentCandidate {
            deployment_id: id.into(),
            provider_id: format!("prov-{id}"),
            provider_name: format!("Provider {id}"),
            remote_model_id: "claude-sonnet-4-6".into(),
            protocol_id: match compliance {
                ProtocolCompliance::Standard => "anthropic_messages".into(),
                _ => "openai_chat".into(),
            },
            is_enabled: enabled,
            priority,
            compliance,
        }
    }

    fn stateful_chat_impact() -> SwitchImpact {
        SwitchImpact::Stateful {
            breaks_prompt_cache: true,
            breaks_reasoning_signature: true,
            breaks_upstream_session: false,
        }
    }

    #[test]
    fn official_standard_protocol_outranks_degraded_relay_even_with_lower_priority_number() {
        let degraded = candidate(
            "relay",
            0,
            ProtocolCompliance::Degraded {
                expected: vec!["anthropic_messages".into()],
                lost_capabilities: vec!["native_pdf".into()],
            },
            true,
        );
        let official = candidate("official", 10, ProtocolCompliance::Standard, true);
        let disabled = candidate("disabled", 0, ProtocolCompliance::Standard, false);

        let plan = plan_route(&[degraded, official, disabled], None, stateful_chat_impact()).unwrap();
        assert_eq!(plan.primary.deployment_id, "official");
        assert_eq!(plan.alternatives.len(), 1);
        assert_eq!(plan.alternatives[0].deployment_id, "relay");
        assert!(plan.sticky_locked);
    }

    #[test]
    fn pinned_deployment_stays_session_sticky_across_priority_changes() {
        let a = candidate("a", 0, ProtocolCompliance::Standard, true);
        let b = candidate("b", 10, ProtocolCompliance::Standard, true);

        let plan = plan_route(&[a, b], Some("b"), stateful_chat_impact()).unwrap();
        assert_eq!(plan.primary.deployment_id, "b");
        assert!(plan.sticky_locked);
    }

    #[test]
    fn stateless_request_auto_fails_over_on_retryable_error() {
        let a = candidate("a", 0, ProtocolCompliance::Standard, true);
        let b = candidate("b", 1, ProtocolCompliance::Standard, true);

        let decision = decide_failover(
            RoutingPolicy::AutoFailoverStateless,
            SwitchImpact::Stateless,
            "a",
            &[a.clone(), b.clone()],
            true,
        );
        assert_eq!(decision, FailoverDecision::AutoSwitch { next: b.clone() });

        // Non-retryable 4xx never fails over even for stateless requests
        let non_retryable = decide_failover(
            RoutingPolicy::AutoFailoverStateless,
            SwitchImpact::Stateless,
            "a",
            &[a, b],
            false,
        );
        assert_eq!(non_retryable, FailoverDecision::SurfaceError);
    }

    #[test]
    fn stateful_chat_never_silently_fails_over_unless_user_opted_in() {
        let a = candidate("a", 0, ProtocolCompliance::Standard, true);
        let b = candidate("b", 1, ProtocolCompliance::Standard, true);
        let impact = stateful_chat_impact();

        for policy in [RoutingPolicy::StickyManual, RoutingPolicy::AutoFailoverStateless] {
            let decision = decide_failover(policy, impact, "a", &[a.clone(), b.clone()], true);
            assert_eq!(
                decision,
                FailoverDecision::RequireUserDecision {
                    impact,
                    alternatives: vec![b.clone()],
                },
                "policy {policy:?} must NEVER silently fail over a stateful chat"
            );
        }

        // Only explicit opt-in switches automatically, and carries an explicit notice
        let opt_in = decide_failover(RoutingPolicy::UserOptInFailover, impact, "a", &[a, b.clone()], true);
        assert_eq!(opt_in, FailoverDecision::AutoSwitchWithNotice { next: b, impact });
    }
}
