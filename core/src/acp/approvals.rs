//! The agent's questions, asked with this app's cards.
//!
//! `session/request_permission` is answered out of the same register every
//! other approval uses (`services.approvals`) and announced with the same
//! `tool_approval_req` event. That is deliberate and it is what makes the whole
//! attention machinery work here for free: the queue outside `sessions`, the
//! toast stack, `all_pending_approvals` rebuilding after a reload, the turn
//! guard sweeping an abandoned card. A private register would have had to
//! reimplement each of them and would have been the copy that drifted.
//!
//! The cost is that ACP's answer is richer than this app's. A `PendingApproval`
//! is answered `Approved` or `Denied`, while ACP offers up to four options and
//! wants an `optionId` back. So the two lasting choices — `allow_always`,
//! `reject_always` — are not offered in this step: a card with two buttons must
//! not be able to produce a decision the user was never shown. `once` is what
//! both buttons mean, and anything durable waits until the card can say so.
//!
//! That rule holds even when it costs an answer. An adapter that offers no
//! `_once` option gets `cancelled` rather than its lasting one — see
//! [`Choices::pick`], where substituting it was a real bug and not a shortcut.

use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::agent::engine::{self, ApprovalDecision};
use crate::db::models::turn::TurnPhase;
use crate::events::ChatStreamEvent;
use crate::services::Services;
use crate::state::PendingApproval;

use super::protocol::{self, PermissionOption, RequestPermissionParams};
use super::{mapping, plan_review};

/// Which option answers each of the two buttons this app draws.
///
/// Resolved before anything is registered, because a question this app cannot
/// answer must be refused while the agent is still waiting on a reply rather
/// than after a card has been put in front of someone.
struct Choices {
    allow: String,
    reject: String,
}

impl Choices {
    /// The single-shot option in each direction, or nothing.
    ///
    /// **No fallback to the lasting options, deliberately.** An earlier version
    /// took `allow_always` when no `allow_once` was offered, on the reasoning
    /// that answering is better than refusing. That is the exact thing the
    /// header forbids: the card says "Allow", the user means this once, and the
    /// answer sent back is "allow this for ever". Nobody involved would find
    /// out — least of all the user, whose next dangerous call simply does not
    /// stop to ask. `reject_always` is the same mistake pointed the other way,
    /// silently withdrawing a permission that was only meant to be withheld
    /// once.
    ///
    /// So a question this app cannot put honestly on a two-button card is not
    /// put on one. In practice both `_once` kinds are always offered — the spec
    /// lists four and `claude-code-acp` sends them — so this is a guard against
    /// an adapter that is unusual rather than a case with a user behind it.
    fn pick(options: &[PermissionOption]) -> Option<Self> {
        let once = |want_allow: bool| -> Option<&PermissionOption> {
            options
                .iter()
                .find(|o| o.is_once() && if want_allow { o.is_allow() } else { o.is_reject() })
        };
        Some(Self {
            allow: once(true)?.option_id.clone(),
            reject: once(false)?.option_id.clone(),
        })
    }
}

/// Everything about the turn the question interrupts.
pub struct TurnContext {
    pub turn_id: String,
    pub assistant_message_id: String,
    pub cancel: CancellationToken,
    pub(super) plan_reviews: Arc<plan_review::ReviewControl>,
}

/// Put an ACP permission request in front of the user and wait.
///
/// Runtime uncertainty still becomes the protocol's `cancelled` reply. A broken
/// first-party approval preference is different: it is returned as an error so
/// the caller cannot silently substitute another deadline.
pub async fn ask(
    services: &Services,
    conversation_id: &str,
    turn: &TurnContext,
    params: RequestPermissionParams,
) -> Result<serde_json::Value, String> {
    // ExitPlanMode is not a normal permission. Its raw input is the canonical
    // full snapshot that has to survive before anybody can approve or annotate
    // it. Detect it by the vendor's real tool name; title/kind are display
    // fields and would collapse it into the generic approval path.
    if let Some(submission) = plan_review::exit_plan_submission(&params)? {
        let choices = plan_review::ReviewChoices::pick(&params.options)
            .ok_or("ACP ExitPlanMode did not offer both one-shot approval and rejection options")?;
        let submitted = plan_review::submit(
            services,
            conversation_id,
            &turn.turn_id,
            &turn.assistant_message_id,
            submission,
        )
        .await?;
        let wait = plan_review::install_permission_wait(&turn.plan_reviews, &submitted, &turn.turn_id, choices)?;
        // The row is already durable. Missing this invalidation event costs a
        // live refresh, not the review, so do not turn it into a JSON-RPC error
        // that leaves Claude Code guessing whether its submission landed.
        if let Err(error) = services.events.emit_plan_review_requested(&submitted.event) {
            tracing::warn!(%error, review_id = %submitted.event.review_id, "could not announce ACP plan review");
        }
        return Ok(plan_review::await_permission_decision(
            &turn.plan_reviews,
            &submitted,
            &turn.turn_id,
            &turn.cancel,
            wait,
        )
        .await);
    }

    let Some(choices) = Choices::pick(&params.options) else {
        tracing::warn!(
            option_count = params.options.len(),
            "an ACP permission request offered no option this app can answer"
        );
        return Ok(protocol::permission_cancelled());
    };

    let approval_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel();

    // The card names the call the same way the transcript does, so an approval
    // and the tool block it belongs to agree about what is being asked.
    let call_id = params.tool_call.tool_call_id.clone();
    let tool_name = mapping::tool_name_of(&params.tool_call);
    let arguments = mapping::arguments_of(&params.tool_call);

    // Worked out once, here, rather than by the waiter. The two would be the
    // same number, but only one of them can be the answer to "when does this
    // stop standing" — and the listing paths read the stored one.
    let ttl = crate::approval::ttl(services)?;

    // Registered before the event goes out, so an answer cannot arrive before
    // there is somewhere to put it.
    services.approvals.lock().insert(
        approval_id.clone(),
        PendingApproval {
            conversation_id: conversation_id.to_string(),
            turn_id: turn.turn_id.clone(),
            assistant_message_id: turn.assistant_message_id.clone(),
            provider_call_id: call_id.clone(),
            origin_call_id: None,
            tool_name: tool_name.clone(),
            arguments: arguments.clone(),
            retry_reason: None,
            // Nothing delegated here: an ACP session is watched in its own
            // conversation, so the question is asked where it happens.
            bubble: None,
            expires_at: ttl.map(|ttl| std::time::Instant::now() + ttl),
            sender: tx,
        },
    );

    let event = ChatStreamEvent::ToolApprovalReq {
        approval_id: approval_id.clone(),
        call_id: call_id.clone(),
        tool_name: tool_name.clone(),
        arguments,
        message_id: turn.assistant_message_id.clone(),
        conversation_id: conversation_id.to_string(),
        delegation: None,
        retry: None,
    };
    if let Err(e) = services.events.emit_chat(event) {
        // Nobody can answer a card that was never drawn.
        services.approvals.claim(&approval_id);
        tracing::warn!(error = %e, "could not draw an ACP approval card");
        return Ok(protocol::permission_cancelled());
    }

    let pool = services.db.clone();
    let decision = engine::in_phase(
        &pool,
        &turn.turn_id,
        TurnPhase::AwaitingApproval,
        Some(&tool_name),
        crate::approval::wait(services, &approval_id, rx, &turn.cancel, ttl),
    )
    .await;

    Ok(match decision {
        Some(ApprovalDecision::Approved) => protocol::permission_selected(&choices.allow),
        Some(ApprovalDecision::Denied(_)) => protocol::permission_selected(&choices.reject),
        // `Response` is the answer to `ask_user`, which is a question the agent
        // asked a person — not a permission being checked. Nothing in an ACP
        // session produces one, and reading it as consent would turn a typed
        // sentence into an approval.
        Some(ApprovalDecision::Response(_)) => protocol::permission_cancelled(),
        None => protocol::permission_cancelled(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(id: &str, kind: &str) -> PermissionOption {
        PermissionOption {
            option_id: id.into(),
            name: id.into(),
            kind: kind.into(),
        }
    }

    /// The ordinary case, and the rule that matters in it: the *once* options
    /// are picked even though the lasting ones are listed first.
    #[test]
    fn the_single_shot_options_win() {
        let options = [
            option("always-yes", "allow_always"),
            option("yes", "allow_once"),
            option("always-no", "reject_always"),
            option("no", "reject_once"),
        ];
        let picked = Choices::pick(&options).unwrap();
        assert_eq!(picked.allow, "yes");
        assert_eq!(picked.reject, "no");
    }

    /// The rule the header states, as behaviour: a lasting option is never
    /// taken to answer a card that only ever offered "this once".
    ///
    /// This test used to assert the opposite — that an adapter offering only
    /// `allow_always` got that answer, on the grounds that answering beats
    /// refusing. It does not: pressing "Allow" would have granted a standing
    /// permission, and the next call it covers never reaches a card at all.
    /// A question that cannot be asked honestly is left unanswered instead.
    #[test]
    fn a_lasting_option_is_never_substituted_for_a_single_shot_one() {
        let options = [
            option("always-yes", "allow_always"),
            option("always-no", "reject_always"),
        ];
        assert!(Choices::pick(&options).is_none());

        // Half of it is just as bad: an honest "No" beside an "Allow" that
        // silently means "for ever" is still the wrong card.
        let half = [option("always-yes", "allow_always"), option("no", "reject_once")];
        assert!(Choices::pick(&half).is_none());
    }

    /// A question with no way to say no cannot be drawn as a two-button card,
    /// and inventing a "no" that maps to nothing would hang the agent.
    #[test]
    fn a_request_with_no_answer_in_one_direction_is_refused() {
        assert!(Choices::pick(&[option("yes", "allow_once")]).is_none());
        assert!(Choices::pick(&[option("no", "reject_once")]).is_none());
        assert!(Choices::pick(&[]).is_none());
    }
}
