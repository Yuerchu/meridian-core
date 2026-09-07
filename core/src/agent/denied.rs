//! Not asking twice about something already refused.
//!
//! A model that has a call turned down often tries it again — sometimes
//! straight away, more often after a detour that changes nothing. Each attempt
//! is another card in front of somebody who has already said no, and the
//! refusals stop being read after the second one.
//!
//! [`ToolLoopGuard`] does not cover this and is not meant to. It watches for a
//! model *stuck*, which is a run of identical calls with nothing in between —
//! it holds one fingerprint, so a denial, a different call, then the first call
//! again resets it to nothing. That is right for what it does. This holds a
//! set, for the length of the turn, and the set never forgets.
//!
//! # What it does not do
//!
//! **Different arguments ask again.** This is the line between the two guards
//! and it is worth keeping sharp: a model that reads the refusal and proposes
//! something narrower is doing what it was asked to. Remembering by tool name
//! would turn one "no" into a ban and hide exactly the improvement that was
//! wanted.
//!
//! **A `None` is not remembered.** Nobody answering — a card that expired, a
//! cancelled turn — is not a refusal, and treating it as one would let a
//! deadline quietly become a policy. The same distinction
//! [`crate::approval`] draws, kept here.
//!
//! **`ask_user` and the mode transitions pass through.** The same exclusions
//! [`crate::agent::auto_review`] makes and for the same reasons: a `Response`
//! is an answer rather than a permission, and being shown a plan is not a
//! permission being checked. A declined plan followed by a revised one has to
//! reach the user.
//!
//! # Where it sits
//!
//! Outside `AutoReviewed`, always: `DeniedMemory(AutoReviewed(asker))`. The
//! reviewer answers some calls without the inner asker ever seeing them, so a
//! memory underneath it would miss precisely the denials that are cheapest to
//! repeat — nothing stopped to ask a person, so nothing slowed the model down.
//!
//! # Who is covered
//!
//! Native turns and OneBot. **Not a hosted ACP session**, which is not an
//! oversight but a fact about the path: `session/request_permission` is
//! answered by [`crate::acp::approvals::ask`] directly and never touches a
//! `dyn Approvals`, so there is no decorator position to occupy. Giving hosted
//! Claude Code the same memory means lifting this set to somewhere both paths
//! can reach and wiring it in on that side — a change to how ACP asks, not a
//! wrapper. Recorded here rather than left for somebody to discover.

use std::collections::HashMap;
use std::sync::Mutex;

use super::call_identity::{Aspect, CallIdentity, identify};
use super::engine::{ApprovalDecision, Approvals};
use crate::provider::ToolCall;

/// An asker that remembers what has already been turned down in this turn.
///
/// Borrows, like [`crate::agent::auto_review::AutoReviewed`] and for the same
/// reason: no implementation of the port is `'static`, so this is a stack value
/// living exactly as long as the turn — which is also exactly how long the
/// memory should last.
pub struct DeniedMemory<'a> {
    inner: &'a dyn Approvals,
    /// The reason given, kept against the identity so the second refusal can
    /// repeat the first one's words rather than inventing its own.
    denied: Mutex<HashMap<CallIdentity, Option<String>>>,
}

impl<'a> DeniedMemory<'a> {
    pub fn wrap(inner: &'a dyn Approvals) -> Self {
        Self {
            inner,
            denied: Mutex::new(HashMap::new()),
        }
    }

    /// Calls whose refusal says nothing about whether to run something.
    ///
    /// The same list `auto_review` passes through, for the same reasons.
    fn passthrough(name: &str) -> bool {
        name == "ask_user" || crate::agent::modes::transition_tools().any(|t| t == name)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<CallIdentity, Option<String>>> {
        self.denied.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What the model is told the second time.
///
/// It names the tool and repeats whatever reason was given, because the model
/// is being told about a decision it has already seen and a message that did
/// not match the first one would read as a new and different refusal. Saying
/// that nothing was asked is the part that matters: without it the model has no
/// way to tell this from a person refusing again immediately, and may conclude
/// they are watching and hostile rather than that it is repeating itself.
pub fn already_denied_message(name: &str, reason: Option<&str>) -> String {
    let mut out = format!(
        "`{name}` was already refused earlier in this turn with exactly these arguments, \
         so it was not put in front of the user again."
    );
    if let Some(reason) = reason.filter(|r| !r.trim().is_empty()) {
        out.push_str(&format!(" The reason given was: {reason}"));
    }
    out.push_str(" Change what you are asking for, or explain to the user why you are blocked.");
    out
}

#[async_trait::async_trait]
impl Approvals for DeniedMemory<'_> {
    async fn ask(
        &self,
        assistant_message_id: &str,
        call: &ToolCall,
        retry_reason: Option<&str>,
    ) -> Result<Option<ApprovalDecision>, String> {
        if Self::passthrough(&call.name) {
            return self.inner.ask(assistant_message_id, call, retry_reason).await;
        }

        // An escalation is its own aspect: refusing to run something with the
        // sandbox removed is not refusing to run it at all, and the ordinary
        // attempt afterwards is the safer of the two.
        let aspect = if retry_reason.is_some() {
            Aspect::Escalation
        } else {
            Aspect::Ordinary
        };
        let identity = identify(&call.name, &call.arguments, aspect);

        // Looked up and released before the await below. Holding a `std::sync`
        // guard across one is how a turn deadlocks itself.
        let remembered = self.lock().get(&identity).cloned();
        if let Some(reason) = remembered {
            tracing::debug!(
                tool = %call.name,
                identity = %identity.to_hex(),
                "a call refused earlier in this turn was not asked about again"
            );
            return Ok(Some(ApprovalDecision::Denied(Some(already_denied_message(
                &call.name,
                reason.as_deref(),
            )))));
        }

        let decision = self.inner.ask(assistant_message_id, call, retry_reason).await?;

        // Only a refusal is remembered. `None` is nobody answering, which is
        // not a decision about the action; `Approved` and `Response` are not
        // refusals at all.
        if let Some(ApprovalDecision::Denied(reason)) = &decision {
            self.lock().insert(identity, reason.clone());
        }
        Ok(decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An asker that answers as told and counts how often it was reached.
    struct Counting {
        answer: Mutex<Vec<Result<Option<ApprovalDecision>, String>>>,
        asked: AtomicUsize,
    }

    impl Counting {
        fn answering(answers: Vec<Result<Option<ApprovalDecision>, String>>) -> Self {
            Self {
                answer: Mutex::new(answers),
                asked: AtomicUsize::new(0),
            }
        }

        fn asked(&self) -> usize {
            self.asked.load(Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl Approvals for Counting {
        async fn ask(&self, _: &str, _: &ToolCall, _: Option<&str>) -> Result<Option<ApprovalDecision>, String> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            let mut answers = self.answer.lock().unwrap();
            if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers[0].clone()
            }
        }
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn denied(reason: &str) -> Result<Option<ApprovalDecision>, String> {
        Ok(Some(ApprovalDecision::Denied(Some(reason.into()))))
    }

    async fn ask(memory: &DeniedMemory<'_>, name: &str, args: &str) -> Result<Option<ApprovalDecision>, String> {
        memory.ask("m-1", &call(name, args), None).await
    }

    fn reason_of(decision: &Result<Option<ApprovalDecision>, String>) -> String {
        match decision {
            Ok(Some(ApprovalDecision::Denied(Some(r)))) => r.clone(),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The whole feature: the second identical attempt does not reach a person.
    #[tokio::test]
    async fn a_refused_call_is_not_asked_about_twice() {
        let inner = Counting::answering(vec![denied("that would delete the wrong tree")]);
        let memory = DeniedMemory::wrap(&inner);

        let first = ask(&memory, "run_command", r#"{"command":"rm -rf /"}"#).await;
        assert_eq!(inner.asked(), 1);
        assert_eq!(reason_of(&first), "that would delete the wrong tree");

        let second = ask(&memory, "run_command", r#"{"command":"rm -rf /"}"#).await;
        assert_eq!(inner.asked(), 1, "the user was asked again about a decision they made");
        // The first refusal's own words, so the model is not told two different
        // things about one decision.
        assert!(
            reason_of(&second).contains("that would delete the wrong tree"),
            "{second:?}"
        );
        assert!(reason_of(&second).contains("already refused"));
    }

    /// The line between this and the loop guard. A model that reads a refusal
    /// and proposes something narrower is doing what was asked of it, and this
    /// must not be what stops that reaching the user.
    #[tokio::test]
    async fn changing_the_arguments_asks_again() {
        let inner = Counting::answering(vec![denied("no")]);
        let memory = DeniedMemory::wrap(&inner);

        let _ = ask(&memory, "run_command", r#"{"command":"rm -rf /"}"#).await;
        let _ = ask(&memory, "run_command", r#"{"command":"rm -rf ./build"}"#).await;
        assert_eq!(inner.asked(), 2);
    }

    /// And what a tool cannot see must not count as a change, or a model
    /// reformatting its own JSON walks straight past the memory.
    #[tokio::test]
    async fn reformatting_the_same_call_does_not_ask_again() {
        let inner = Counting::answering(vec![denied("no")]);
        let memory = DeniedMemory::wrap(&inner);

        let _ = ask(&memory, "read_file", r#"{"path":"a","limit":1}"#).await;
        let _ = ask(&memory, "read_file", r#"{ "limit": 1, "path": "a" }"#).await;
        assert_eq!(inner.asked(), 1);
    }

    /// Nobody answering is not a refusal. A card that expired must not become a
    /// standing policy for the rest of the turn.
    #[tokio::test]
    async fn an_unanswered_question_is_not_remembered() {
        let inner = Counting::answering(vec![Ok(None)]);
        let memory = DeniedMemory::wrap(&inner);

        assert!(
            ask(&memory, "run_command", r#"{"command":"ls"}"#)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            ask(&memory, "run_command", r#"{"command":"ls"}"#)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(inner.asked(), 2, "a deadline became a decision");
    }

    #[tokio::test]
    async fn an_approval_is_not_remembered() {
        let inner = Counting::answering(vec![Ok(Some(ApprovalDecision::Approved))]);
        let memory = DeniedMemory::wrap(&inner);

        let _ = ask(&memory, "read_file", r#"{"path":"a"}"#).await;
        let _ = ask(&memory, "read_file", r#"{"path":"a"}"#).await;
        assert_eq!(inner.asked(), 2);
    }

    /// Refusing to run something outside the sandbox is not refusing to run it
    /// at all — and the ordinary attempt is the safer of the two, so turning it
    /// down unasked is a refusal the user never gave.
    #[tokio::test]
    async fn refusing_an_escalation_does_not_refuse_the_sandboxed_call() {
        let inner = Counting::answering(vec![denied("not outside the sandbox")]);
        let memory = DeniedMemory::wrap(&inner);
        let escalation = call("run_command", r#"{"command":"cargo test"}"#);

        let _ = memory.ask("m-1", &escalation, Some("sandbox denied")).await;
        assert_eq!(inner.asked(), 1);

        let _ = memory.ask("m-1", &escalation, None).await;
        assert_eq!(inner.asked(), 2, "the ordinary call was refused on the user's behalf");

        // And the escalation itself is still remembered.
        let _ = memory.ask("m-1", &escalation, Some("sandbox denied")).await;
        assert_eq!(inner.asked(), 2);
    }

    /// A refusal here is not about whether to run something: `ask_user`'s
    /// answer is words, and a declined plan has to be able to come back revised.
    #[tokio::test]
    async fn questions_and_plans_are_always_put_in_front_of_the_user() {
        let inner = Counting::answering(vec![denied("no")]);
        let memory = DeniedMemory::wrap(&inner);

        for name in ["ask_user", "exit_plan"] {
            let _ = ask(&memory, name, r#"{"q":"?"}"#).await;
            let _ = ask(&memory, name, r#"{"q":"?"}"#).await;
        }
        assert_eq!(inner.asked(), 4, "a refusal here was read as a standing answer");
    }

    /// A refusal with no reason still bars the repeat — the memory is of the
    /// decision, not of the words.
    #[tokio::test]
    async fn a_refusal_without_a_reason_is_still_remembered() {
        let inner = Counting::answering(vec![Ok(Some(ApprovalDecision::Denied(None)))]);
        let memory = DeniedMemory::wrap(&inner);

        let _ = ask(&memory, "run_command", r#"{"command":"ls"}"#).await;
        let second = ask(&memory, "run_command", r#"{"command":"ls"}"#).await;
        assert_eq!(inner.asked(), 1);
        assert!(reason_of(&second).contains("already refused"));
    }

    /// A stand-in for `AutoReviewed` on the calls it answers by itself, which
    /// is the case the wrapping order is about. It never reaches what it wraps.
    struct AnswersItself<'a> {
        inner: &'a dyn Approvals,
        reached: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Approvals for AnswersItself<'_> {
        async fn ask(&self, _: &str, _: &ToolCall, _: Option<&str>) -> Result<Option<ApprovalDecision>, String> {
            self.reached.fetch_add(1, Ordering::Relaxed);
            let _ = &self.inner;
            Ok(Some(ApprovalDecision::Denied(Some("the reviewer said no".into()))))
        }
    }

    /// **Why the order is `DeniedMemory(AutoReviewed(asker))` and not the
    /// reverse**, demonstrated rather than asserted in a comment.
    ///
    /// The reviewer answers some calls without the asker underneath it ever
    /// being reached. Those are the denials cheapest for a model to repeat —
    /// nothing stopped to ask a person, so nothing slowed it down — and they
    /// are exactly the ones a memory placed *underneath* would never see.
    #[tokio::test]
    async fn the_memory_only_sees_the_reviewers_refusals_from_outside_it() {
        let user = Counting::answering(vec![denied("unreached")]);
        let reviewer = AnswersItself {
            inner: &user,
            reached: AtomicUsize::new(0),
        };

        // Outside: the first call reaches the reviewer, the second does not.
        let outside = DeniedMemory::wrap(&reviewer);
        let _ = ask(&outside, "run_command", r#"{"command":"ls"}"#).await;
        let second = ask(&outside, "run_command", r#"{"command":"ls"}"#).await;
        assert_eq!(reviewer.reached.load(Ordering::Relaxed), 1);
        assert!(reason_of(&second).contains("already refused"));

        // Inside: the reviewer decides before the memory is consulted, so it
        // records nothing and every repeat costs another review.
        let inner_user = Counting::answering(vec![denied("unreached")]);
        let inside_memory = DeniedMemory::wrap(&inner_user);
        let inverted = AnswersItself {
            inner: &inside_memory,
            reached: AtomicUsize::new(0),
        };
        let _ = inverted
            .ask("m-1", &call("run_command", r#"{"command":"ls"}"#), None)
            .await;
        let _ = inverted
            .ask("m-1", &call("run_command", r#"{"command":"ls"}"#), None)
            .await;
        assert_eq!(
            inverted.reached.load(Ordering::Relaxed),
            2,
            "wrapped the other way round, the memory never learns anything"
        );
        assert!(inside_memory.lock().is_empty());
    }

    /// An `Err` ends the turn, so it must travel rather than being swallowed —
    /// and there is nothing to remember about a question that was never asked.
    #[tokio::test]
    async fn a_failure_to_ask_is_not_a_refusal() {
        let inner = Counting::answering(vec![Err("the window is gone".into())]);
        let memory = DeniedMemory::wrap(&inner);

        // `ApprovalDecision` has no `PartialEq` — it is a value the loop reads
        // rather than compares — so the error is matched rather than equated.
        assert_eq!(
            ask(&memory, "run_command", r#"{"command":"ls"}"#).await.unwrap_err(),
            "the window is gone"
        );
        ask(&memory, "run_command", r#"{"command":"ls"}"#).await.ok();
        assert_eq!(inner.asked(), 2, "a failed ask was remembered as a decision");
    }
}
