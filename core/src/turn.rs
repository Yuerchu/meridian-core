//! Who is allowed to write to a conversation right now.
//!
//! The resource being protected is not the runner but the rows: `messages` plus
//! the `head_message_id` that names which of them are on the active path. Two
//! turns writing the same conversation do not merge into two branches the way
//! the tree was designed for — whichever finishes last owns the head, and the
//! other one's entire output stops being reachable. So the key here is the
//! conversation, and everything that writes one passes through this table:
//! desktop turns, OneBot turns, and the handful of commands that rewrite
//! history without being a turn at all.
//!
//! Leases rather than a checked flag. `if busy { return }` followed by an
//! `await` and then a write leaves room for a turn to start in between, which is
//! the race it was meant to prevent. A lease is taken atomically and released by
//! `Drop`, so every early return and every panic frees it — and turns have
//! roughly thirty ways out.
//!
//! The lock is `std::sync::Mutex`: every critical section is one map operation
//! with nothing awaited inside, and `Drop` cannot await. Poisoning is recovered
//! from rather than propagated — one turn panicking while holding it must not
//! stop every later turn in the app from starting.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio_util::sync::CancellationToken;

/// Which end started a turn. Decides what happens to a message that arrives for
/// a conversation someone else is already answering: the OneBot inbox is only
/// drained by the OneBot runner, so parking a message there while the desktop
/// holds the conversation would strand it until the next QQ message happened to
/// come along.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, strum::IntoStaticStr, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TurnOrigin {
    Desktop,
    #[strum(serialize = "onebot")]
    #[serde(rename = "onebot")]
    OneBot,
    /// A turn a `run_agent` call delegated. It runs in a conversation of its
    /// own, so it contends with nobody — but the distinction is what lets an
    /// interruption report say "a sub-agent was partway through" instead of
    /// naming a conversation the user has never seen.
    SubAgent,
    /// A turn some other agent's plan asked for, arriving over the hook
    /// endpoint rather than from anything inside this app. Like `SubAgent` it
    /// runs in a conversation of its own, so it contends with nobody; unlike
    /// it, nobody here started it, which is why an interruption report must not
    /// word it as work the user asked for.
    PlanReview,
    /// The same, for a review of what was written rather than what was
    /// proposed. Its own variant because a report that cannot tell the two
    /// apart would name the wrong gate.
    ImplReview,
    /// A turn running inside a hosted Claude Code session, over ACP. The user
    /// started it and is watching it, like `Desktop` — but the work is happening
    /// in another process, so an interruption report must not claim this app
    /// knows how far it got.
    ClaudeCode,
    /// A command the person typed with the composer's `!` prefix. It is a
    /// cancellable occupant because it may run for minutes and write files,
    /// but it does not query a model or create a `turns` row.
    UserShell,
}

impl TurnOrigin {
    /// How it is stored. Also what a crash report says the turn was, so it
    /// outlives the process that decided it.
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    /// Read side, for whoever reports on a stored turn.
    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown turn origin '{value}'"))
    }
}

struct ActiveTurn {
    turn_id: String,
    cancel: CancellationToken,
    origin: TurnOrigin,
}

enum Occupant {
    Turn(ActiveTurn),
    /// A short write that is not a turn: compaction, a subtree delete, a branch
    /// switch. No cancellation token — nothing sends these a stop.
    Mutation {
        operation_id: String,
        kind: &'static str,
    },
}

impl Occupant {
    fn id(&self) -> &str {
        match self {
            Occupant::Turn(t) => &t.turn_id,
            Occupant::Mutation { operation_id, .. } => operation_id,
        }
    }

    fn busy(&self) -> Busy {
        match self {
            Occupant::Turn(t) => Busy::Turn(t.origin),
            Occupant::Mutation { kind, .. } => Busy::Mutation(kind),
        }
    }
}

/// Why a request was refused, in terms the caller can hand to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Turn(TurnOrigin),
    Mutation(&'static str),
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Busy::Turn(TurnOrigin::Desktop) => {
                write!(
                    f,
                    "This conversation is already answering. Wait for it to finish, or stop it first."
                )
            }
            Busy::Turn(TurnOrigin::OneBot) => {
                write!(
                    f,
                    "This conversation is being answered from QQ right now. Wait for that turn to finish."
                )
            }
            // Reachable from the sub-agent's own conversation, which the user
            // can open while it runs. Not from the parent's: a delegated run
            // occupies a conversation nobody else is writing to.
            Busy::Turn(TurnOrigin::SubAgent) => {
                write!(
                    f,
                    "A sub-agent is working in this conversation. Wait for it to finish, or stop it first."
                )
            }
            // Two hooks firing for one session, most likely because the plan
            // was resubmitted before the first review came back.
            Busy::Turn(TurnOrigin::PlanReview) => {
                write!(
                    f,
                    "A plan review is already running for this session. Wait for it to finish."
                )
            }
            Busy::Turn(TurnOrigin::ImplReview) => {
                write!(
                    f,
                    "A review of these changes is already running. Wait for it to finish."
                )
            }
            Busy::Turn(TurnOrigin::ClaudeCode) => {
                write!(
                    f,
                    "Claude Code is still working on this. Wait for it to finish, or stop it first."
                )
            }
            Busy::Turn(TurnOrigin::UserShell) => {
                write!(
                    f,
                    "A shell command is still running in this conversation. Wait for it to finish, or stop it first."
                )
            }
            Busy::Mutation(kind) => {
                write!(f, "This conversation is busy: {kind} is in progress.")
            }
        }
    }
}

/// The occupancy table, and how often each conversation's entry has changed.
///
/// The counts live inside the same lock as the table because they are only
/// useful if the two cannot be read a moment apart — a reader that saw the
/// table at one instant and a count at another has learned nothing.
///
/// Counted per conversation rather than once for everything. A single counter
/// is simpler and was what this had, but it makes every conversation's snapshot
/// sensitive to every other one's turns: with a few sessions busy, four
/// consecutive reads can all be invalidated by activity that has nothing to do
/// with the conversation being read, and the fallback that follows refuses to
/// call anything interrupted. The result is that the busier the application
/// gets, the less able it becomes to report a turn that really did crash —
/// exactly backwards.
#[derive(Default)]
struct Table {
    by_conversation: HashMap<String, Occupant>,
    /// Kept for conversations nothing is holding, which is the whole point: the
    /// sequence that has to be detectable is a turn that started and finished
    /// while a reader was away, and it leaves no occupant behind to count.
    ///
    /// Never pruned. An entry is a short string and a `u64`, and the number of
    /// them is the number of conversations that have been active in this
    /// process. Removing one on delete would reset its count, which is safe —
    /// ids are uuids, so a later count cannot collide with a remembered one —
    /// but it buys nothing worth the code.
    revisions: HashMap<String, u64>,
}

impl Table {
    /// Every insert and every removal goes through here. A change that forgets
    /// to bump is a change a snapshot will not notice.
    fn bump(&mut self, conversation_id: &str) {
        let counter = self.revisions.entry(conversation_id.to_string()).or_insert(0);
        *counter = counter.wrapping_add(1);
    }

    fn revision(&self, conversation_id: &str) -> u64 {
        self.revisions.get(conversation_id).copied().unwrap_or(0)
    }
}

#[derive(Default)]
pub struct TurnCoordinator {
    occupied: Mutex<Table>,
}

/// The coordinator as it was at one instant, for a reader that then goes and
/// reads something else and needs to know whether it moved in between.
///
/// The revision is what makes it a snapshot. Comparing the held turn id alone
/// would miss `None` → a turn that started and finished → `None`, which is
/// exactly the sequence that leaves a `running` row behind for a turn that is
/// genuinely over — or, read the other way round, has a live turn's row judged
/// against a moment before it existed.
pub struct Observed {
    conversation_id: String,
    held: Option<String>,
    revision: u64,
}

impl Observed {
    pub fn held(&self) -> Option<&str> {
        self.held.as_deref()
    }

    /// Which conversation this reading is about. A reader judging several at
    /// once — a turn and the sub-agents it delegated to — keeps one of these per
    /// conversation and needs to know which is which.
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
}

impl TurnCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Table> {
        self.occupied.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the conversation for a turn, or say who already has it.
    pub fn try_acquire_turn(self: &Arc<Self>, conversation_id: &str, origin: TurnOrigin) -> Result<TurnLease, Busy> {
        self.try_acquire_turn_as(conversation_id, origin, uuid::Uuid::new_v4().to_string())
    }

    /// Same, under an id the caller already has.
    ///
    /// The desktop mints its id in the front end, before it sends, because the
    /// composer locks at that moment and everything arriving afterwards has to
    /// be measurable against it — including the previous turn's stop, which can
    /// be delivered after that turn's rejection has already unlocked the
    /// composer. An id minted here would not exist until the command had been
    /// dispatched, taken the conversation and resolved a provider, and the gap
    /// is where a stale stop gets mistaken for this turn's.
    ///
    /// The id is not trusted for anything but matching: it names a turn within
    /// one conversation, and the occupancy check below is what actually decides
    /// whether the turn may run.
    pub fn try_acquire_turn_as(
        self: &Arc<Self>,
        conversation_id: &str,
        origin: TurnOrigin,
        turn_id: String,
    ) -> Result<TurnLease, Busy> {
        self.try_acquire_turn_with(conversation_id, origin, turn_id, CancellationToken::new())
    }

    /// Same again, under a token the caller already has.
    ///
    /// Which matters when the turn is not the only thing that can stop it. A
    /// delegated run has to end when its own conversation is stopped *and* when
    /// the turn that spawned it is, so it is entered under a child of the
    /// parent's token. Minting a fresh one here instead would put a different
    /// token in the register from the one the runner is watching, and Stop on
    /// the sub-agent's conversation would cancel something nobody was waiting
    /// on — a button that reads as broken rather than as unimplemented.
    pub fn try_acquire_turn_with(
        self: &Arc<Self>,
        conversation_id: &str,
        origin: TurnOrigin,
        turn_id: String,
        cancel: CancellationToken,
    ) -> Result<TurnLease, Busy> {
        {
            let mut map = self.lock();
            if let Some(occupant) = map.by_conversation.get(conversation_id) {
                return Err(occupant.busy());
            }
            map.by_conversation.insert(
                conversation_id.to_string(),
                Occupant::Turn(ActiveTurn {
                    turn_id: turn_id.clone(),
                    cancel: cancel.clone(),
                    origin,
                }),
            );
            map.bump(conversation_id);
        }
        Ok(TurnLease {
            coordinator: Arc::clone(self),
            conversation_id: conversation_id.to_string(),
            turn_id,
            cancel,
        })
    }

    /// Take the conversation for a write that is not a turn.
    ///
    /// Shares the occupancy table with turns on purpose: two tables would mean
    /// two lookups, and a turn could start between them.
    pub fn try_acquire_mutation(
        self: &Arc<Self>,
        conversation_id: &str,
        kind: &'static str,
    ) -> Result<MutationLease, Busy> {
        let operation_id = uuid::Uuid::new_v4().to_string();
        {
            let mut map = self.lock();
            if let Some(occupant) = map.by_conversation.get(conversation_id) {
                return Err(occupant.busy());
            }
            map.by_conversation.insert(
                conversation_id.to_string(),
                Occupant::Mutation {
                    operation_id: operation_id.clone(),
                    kind,
                },
            );
            map.bump(conversation_id);
        }
        Ok(MutationLease {
            coordinator: Arc::clone(self),
            conversation_id: conversation_id.to_string(),
            operation_id,
        })
    }

    /// Take several conversations for one write, or none of them.
    ///
    /// Deleting a conversation takes its delegated runs with it, and each of
    /// those is a conversation something may be running on. Acquiring them one
    /// at a time would mean either holding some while another is refused — which
    /// then has to be unwound in the right order — or picking an order at all,
    /// which is where lock inversions come from. One critical section has
    /// neither problem: every id is checked and taken before anyone else can see
    /// a partial result, and a refusal leaves the table exactly as it was.
    ///
    /// Duplicates in `ids` are taken once. A conversation cannot conflict with
    /// itself, and the caller assembling a tree should not have to prove it
    /// listed each node only once.
    pub fn try_acquire_mutations(
        self: &Arc<Self>,
        ids: &[String],
        kind: &'static str,
    ) -> Result<Vec<MutationLease>, Busy> {
        let mut map = self.lock();
        let mut taken: Vec<(String, String)> = Vec::new();
        for id in ids {
            if taken.iter().any(|(held, _)| held == id) {
                continue;
            }
            if let Some(occupant) = map.by_conversation.get(id) {
                let busy = occupant.busy();
                // Nothing outside this lock has seen any of them, so undoing is
                // just removing what we put in.
                for (id, operation_id) in &taken {
                    if matches!(
                        map.by_conversation.get(id),
                        Some(Occupant::Mutation { operation_id: held, .. }) if held == operation_id
                    ) {
                        map.by_conversation.remove(id);
                        map.bump(id);
                    }
                }
                return Err(busy);
            }
            let operation_id = uuid::Uuid::new_v4().to_string();
            map.by_conversation.insert(
                id.clone(),
                Occupant::Mutation {
                    operation_id: operation_id.clone(),
                    kind,
                },
            );
            map.bump(id);
            taken.push((id.clone(), operation_id));
        }
        drop(map);
        Ok(taken
            .into_iter()
            .map(|(conversation_id, operation_id)| MutationLease {
                coordinator: Arc::clone(self),
                conversation_id,
                operation_id,
            })
            .collect())
    }

    /// Signal the turn running for this conversation to stop.
    ///
    /// `turn_id` names which run the caller meant. A stop aimed at a turn that
    /// has already ended must not cancel whatever started after it — the front
    /// end can only tell them apart by id, and without the check "stop, then
    /// send again" cancelled the new turn about as often as the old one.
    /// Returns whether a matching turn was found.
    pub fn cancel(&self, conversation_id: &str, turn_id: Option<&str>) -> bool {
        let map = self.lock();
        match map.by_conversation.get(conversation_id) {
            Some(Occupant::Turn(t)) if turn_id.is_none_or(|id| id == t.turn_id) => {
                t.cancel.cancel();
                true
            }
            _ => false,
        }
    }

    /// Which turn is running on that conversation, if any.
    ///
    /// The live answer to a question the database cannot give. A turn's row says
    /// `running` from the moment it starts until it reaches an ending, so
    /// anything that never reaches one — a kill, a panic, a task dropped at
    /// shutdown — leaves the row saying `running` for good. This says whether
    /// that is still true, and a `running` row this does not name is a turn that
    /// stopped without saying so.
    ///
    /// Answers with the turn rather than a yes or no about one, so however many
    /// rows are being judged are judged against a single read. Asking per row
    /// would let a list come back measured against a table that moved between
    /// the questions.
    ///
    /// A mutation lease is not a turn: it occupies the conversation, but nothing
    /// is running under it and nothing in `turns` belongs to it.
    pub fn held_turn(&self, conversation_id: &str) -> Option<String> {
        self.observe(conversation_id).held
    }

    /// The active literal shell command for this conversation, if any.
    ///
    /// This deliberately excludes model turns. Reload and remote-resync use it
    /// to restore a terminal card's Stop state, and treating an ordinary model
    /// turn as a shell command would attach that button to the wrong runner.
    pub fn active_user_shell_turn(&self, conversation_id: &str) -> Option<String> {
        let map = self.lock();
        match map.by_conversation.get(conversation_id) {
            Some(Occupant::Turn(turn)) if turn.origin == TurnOrigin::UserShell => Some(turn.turn_id.clone()),
            _ => None,
        }
    }

    /// The held turn together with the revision it was read at.
    ///
    /// For a reader that will go away and read something slower — the database —
    /// and then has to decide whether what it read can be judged against this.
    /// Pair it with `unchanged_since`.
    pub fn observe(&self, conversation_id: &str) -> Observed {
        let map = self.lock();
        let held = match map.by_conversation.get(conversation_id) {
            Some(Occupant::Turn(t)) => Some(t.turn_id.clone()),
            _ => None,
        };
        Observed {
            conversation_id: conversation_id.to_string(),
            held,
            revision: map.revision(conversation_id),
        }
    }

    /// Whether the observed conversation has stood still since `seen` was taken.
    ///
    /// Only that conversation. Answering for the whole table would make every
    /// snapshot fail whenever anything else was busy, and the reader that asks
    /// this gives up after a few tries and stops reporting crashes at all.
    pub fn unchanged_since(&self, seen: &Observed) -> bool {
        self.lock().revision(&seen.conversation_id) == seen.revision
    }

    /// Release, but only if the entry is still the one the lease took.
    ///
    /// A guard can outlive its own release — a task cancelled mid-drop, an old
    /// runner unwinding while a new one has already started. Removing blindly
    /// would take the new turn's token with it, and nothing would be able to
    /// stop that turn afterwards.
    fn release(&self, conversation_id: &str, id: &str) {
        let mut map = self.lock();
        if map.by_conversation.get(conversation_id).is_some_and(|o| o.id() == id) {
            map.by_conversation.remove(conversation_id);
            map.bump(conversation_id);
        }
    }
}

/// Held on the runner's stack for the whole turn, follow-up rounds included.
pub struct TurnLease {
    coordinator: Arc<TurnCoordinator>,
    conversation_id: String,
    turn_id: String,
    cancel: CancellationToken,
}

impl TurnLease {
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl Drop for TurnLease {
    fn drop(&mut self) {
        self.coordinator.release(&self.conversation_id, &self.turn_id);
    }
}

/// Held across one write. Short by construction: anything long enough to want a
/// stop button is a turn.
pub struct MutationLease {
    coordinator: Arc<TurnCoordinator>,
    conversation_id: String,
    operation_id: String,
}

impl Drop for MutationLease {
    fn drop(&mut self) {
        self.coordinator.release(&self.conversation_id, &self.operation_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coordinator() -> Arc<TurnCoordinator> {
        Arc::new(TurnCoordinator::new())
    }

    #[test]
    fn a_second_turn_is_refused_while_the_first_holds_the_conversation() {
        let c = coordinator();
        let first = c.try_acquire_turn("conv-1", TurnOrigin::Desktop).expect("free");
        assert_eq!(
            c.try_acquire_turn("conv-1", TurnOrigin::Desktop).err(),
            Some(Busy::Turn(TurnOrigin::Desktop)),
        );
        // Different conversations do not contend.
        assert!(c.try_acquire_turn("conv-2", TurnOrigin::Desktop).is_ok());
        drop(first);
        assert!(c.try_acquire_turn("conv-1", TurnOrigin::Desktop).is_ok());
    }

    /// The refusal has to say which end is holding it, because that decides what
    /// the caller does next: QQ queues behind its own runner and turns a desktop
    /// turn away.
    #[test]
    fn the_refusal_names_the_origin_that_holds_it() {
        let c = coordinator();
        let _held = c.try_acquire_turn("conv-1", TurnOrigin::OneBot).expect("free");
        assert_eq!(
            c.try_acquire_turn("conv-1", TurnOrigin::Desktop).err(),
            Some(Busy::Turn(TurnOrigin::OneBot)),
        );
    }

    /// All of a tree or none of it. A refusal partway through must leave the
    /// table exactly as it was — otherwise a delete that was turned down still
    /// blocks half the conversations it was going to remove, and nothing is left
    /// holding a lease that would ever release them.
    #[test]
    fn a_batch_that_cannot_be_completed_takes_nothing() {
        let c = coordinator();
        let busy = c.try_acquire_turn("child-b", TurnOrigin::SubAgent).expect("free");

        let refused = c.try_acquire_mutations(&["parent".into(), "child-a".into(), "child-b".into()], "a delete");
        assert_eq!(refused.err(), Some(Busy::Turn(TurnOrigin::SubAgent)));

        // The two it did take on the way are free again, and free for anyone.
        let _turn = c.try_acquire_turn("parent", TurnOrigin::Desktop).expect("released");
        let _other = c.try_acquire_turn("child-a", TurnOrigin::Desktop).expect("released");
        drop(busy);
    }

    /// The successful case holds every one of them, and gives them all back
    /// together.
    #[test]
    fn a_batch_holds_the_whole_tree_until_it_is_dropped() {
        let c = coordinator();
        let leases = c
            .try_acquire_mutations(&["parent".into(), "child".into()], "a delete")
            .expect("all free");
        assert_eq!(leases.len(), 2);

        for id in ["parent", "child"] {
            assert_eq!(
                c.try_acquire_turn(id, TurnOrigin::SubAgent).err(),
                Some(Busy::Mutation("a delete")),
                "{id}",
            );
        }

        drop(leases);
        let _reused = c.try_acquire_turn("parent", TurnOrigin::Desktop).expect("free again");
        let _also = c.try_acquire_turn("child", TurnOrigin::SubAgent).expect("free again");
    }

    /// A tree assembled from a walk can name the same node twice; being asked to
    /// prove otherwise would push that job onto every caller.
    #[test]
    fn a_repeated_id_in_a_batch_is_not_a_conflict_with_itself() {
        let c = coordinator();
        let leases = c
            .try_acquire_mutations(&["same".into(), "same".into()], "a delete")
            .expect("a conversation does not conflict with itself");
        assert_eq!(leases.len(), 1);
        drop(leases);
        let _free = c.try_acquire_turn("same", TurnOrigin::Desktop).expect("released once");
    }

    /// The whole reason turns and non-turn writes share one table. Two tables
    /// would each report the conversation free.
    #[test]
    fn a_mutation_and_a_turn_exclude_each_other() {
        let c = coordinator();
        let lease = c.try_acquire_mutation("conv-1", "compact").expect("free");
        assert_eq!(
            c.try_acquire_turn("conv-1", TurnOrigin::Desktop).err(),
            Some(Busy::Mutation("compact")),
        );
        drop(lease);

        let _turn = c.try_acquire_turn("conv-1", TurnOrigin::Desktop).expect("free");
        assert_eq!(
            c.try_acquire_mutation("conv-1", "delete").err(),
            Some(Busy::Turn(TurnOrigin::Desktop)),
        );
    }

    /// A run entered under somebody else's token answers to both of them.
    ///
    /// The register has to hold the *same* token the runner is watching. Minting
    /// a fresh one here would leave Stop on this conversation cancelling
    /// something nobody awaits, and the parent's Stop reaching a child that
    /// carries on regardless.
    #[test]
    fn a_lease_entered_under_a_caller_token_answers_to_it_and_to_stop() {
        let c = coordinator();
        let parent = CancellationToken::new();

        let lease = c
            .try_acquire_turn_with("child", TurnOrigin::SubAgent, "t-child".into(), parent.child_token())
            .expect("free");

        // Stopping the conversation stops the token the runner is holding.
        assert!(c.cancel("child", None));
        assert!(lease.cancel_token().is_cancelled());
        drop(lease);

        // And the other direction: the parent going away takes the child with it.
        let lease = c
            .try_acquire_turn_with("child", TurnOrigin::SubAgent, "t-again".into(), parent.child_token())
            .expect("released");
        assert!(!lease.cancel_token().is_cancelled());
        parent.cancel();
        assert!(lease.cancel_token().is_cancelled());
    }

    /// A panic unwinds through the lease, which is the only cleanup path a
    /// panicking turn has.
    #[test]
    fn a_panicking_turn_releases_its_conversation() {
        let c = coordinator();
        let held = Arc::clone(&c);
        let _ = std::thread::spawn(move || {
            let _lease = held.try_acquire_turn("conv-1", TurnOrigin::Desktop).expect("free");
            panic!("the turn died");
        })
        .join();

        assert!(c.try_acquire_turn("conv-1", TurnOrigin::Desktop).is_ok());
    }

    /// The reason release is identity-checked. Without it the late guard would
    /// take the new turn's entry with it, and nothing could stop that turn.
    #[test]
    fn a_late_guard_does_not_release_the_turn_that_replaced_it() {
        let c = coordinator();
        let old = c.try_acquire_turn("conv-1", TurnOrigin::Desktop).expect("free");
        let old_id = old.turn_id().to_string();
        drop(old);

        let new = c.try_acquire_turn("conv-1", TurnOrigin::Desktop).expect("free");
        assert_ne!(new.turn_id(), old_id);

        // Whatever the old guard would have done on its way out.
        c.release("conv-1", &old_id);

        assert!(
            c.try_acquire_turn("conv-1", TurnOrigin::Desktop).is_err(),
            "the new turn must still hold the conversation",
        );
        assert!(c.cancel("conv-1", Some(new.turn_id())), "and must still be stoppable");
    }

    #[test]
    fn a_stop_aimed_at_a_finished_turn_does_not_cancel_the_next_one() {
        let c = coordinator();
        let old = c.try_acquire_turn("conv-1", TurnOrigin::Desktop).expect("free");
        let old_id = old.turn_id().to_string();
        drop(old);

        let new = c.try_acquire_turn("conv-1", TurnOrigin::Desktop).expect("free");
        assert!(!c.cancel("conv-1", Some(&old_id)), "no turn under that id any more");
        assert!(!new.cancel_token().is_cancelled());

        assert!(c.cancel("conv-1", Some(new.turn_id())));
        assert!(new.cancel_token().is_cancelled());
    }

    /// The front end may not know which run it is looking at — a reload loses
    /// the id. Without one, stop means "whatever is running here now".
    #[test]
    fn a_stop_without_an_id_cancels_whatever_is_running() {
        let c = coordinator();
        assert!(!c.cancel("conv-1", None), "nothing to stop");
        let lease = c.try_acquire_turn("conv-1", TurnOrigin::Desktop).expect("free");
        assert!(c.cancel("conv-1", None));
        assert!(lease.cancel_token().is_cancelled());
    }

    #[test]
    fn active_shell_query_names_only_user_shell_turns() {
        let c = coordinator();
        assert_eq!(c.active_user_shell_turn("conv-1"), None);

        let model = c
            .try_acquire_turn_as("conv-1", TurnOrigin::Desktop, "model-turn".into())
            .expect("free");
        assert_eq!(c.active_user_shell_turn("conv-1"), None);
        drop(model);

        let shell = c
            .try_acquire_turn_as("conv-1", TurnOrigin::UserShell, "shell-turn".into())
            .expect("free");
        assert_eq!(c.active_user_shell_turn("conv-1").as_deref(), Some("shell-turn"));
        drop(shell);
        assert_eq!(c.active_user_shell_turn("conv-1"), None);

        let _mutation = c.try_acquire_mutation("conv-1", "edit").expect("free");
        assert_eq!(c.active_user_shell_turn("conv-1"), None);
    }

    /// Cancelling is not releasing: the runner still has to unwind, and until it
    /// does the conversation is still occupied.
    #[test]
    fn cancelling_leaves_the_conversation_occupied_until_the_runner_unwinds() {
        let c = coordinator();
        let lease = c.try_acquire_turn("conv-1", TurnOrigin::Desktop).expect("free");
        c.cancel("conv-1", None);
        assert!(c.try_acquire_turn("conv-1", TurnOrigin::Desktop).is_err());
        drop(lease);
        assert!(c.try_acquire_turn("conv-1", TurnOrigin::Desktop).is_ok());
    }

    /// A mutation is not a turn, so nothing can send it a stop — and a stop
    /// aimed at the conversation must not report success as though it had.
    #[test]
    fn a_mutation_cannot_be_cancelled() {
        let c = coordinator();
        let _lease = c.try_acquire_mutation("conv-1", "delete").expect("free");
        assert!(!c.cancel("conv-1", None));
    }

    /// The sequence that defeats comparing held ids: nobody, then a turn that
    /// starts and finishes, then nobody again. Both ends look identical, and
    /// in between a `running` row appeared that a reader holding the first
    /// observation would judge as a turn nobody is running.
    #[test]
    fn a_turn_that_came_and_went_is_still_a_change() {
        let c = coordinator();
        let before = c.observe("conv-1");
        assert!(before.held().is_none());

        drop(
            c.try_acquire_turn_as("conv-1", TurnOrigin::Desktop, "t1".into())
                .expect("free"),
        );

        let after = c.observe("conv-1");
        assert_eq!(after.held(), before.held(), "the ids agree, which is the trap");
        assert!(!c.unchanged_since(&before), "and the revision does not");
    }

    /// The other half: a reader that saw nothing must not be allowed to judge
    /// a turn that started while it was away.
    #[test]
    fn a_turn_starting_after_the_observation_invalidates_it() {
        let c = coordinator();
        let before = c.observe("conv-1");

        let _lease = c
            .try_acquire_turn_as("conv-1", TurnOrigin::Desktop, "t1".into())
            .unwrap();

        assert!(!c.unchanged_since(&before));
        // A fresh observation is good again, and names the turn.
        let now = c.observe("conv-1");
        assert_eq!(now.held(), Some("t1"));
        assert!(c.unchanged_since(&now));
    }

    /// Every occupant counts, turns and writes alike — a delete taking the
    /// conversation is as much a change of state as a turn taking it.
    #[test]
    fn a_mutation_moves_the_revision_too() {
        let c = coordinator();
        let before = c.observe("conv-1");

        let lease = c.try_acquire_mutation("conv-1", "a delete").unwrap();
        assert!(!c.unchanged_since(&before));
        // ...and it is not mistaken for a turn.
        assert!(c.observe("conv-1").held().is_none());

        let mid = c.observe("conv-1");
        drop(lease);
        assert!(!c.unchanged_since(&mid));
    }

    /// The reason the count is per conversation. Answered for the whole table,
    /// a snapshot of one conversation is invalidated by every turn running
    /// anywhere else — and the reader that asks this gives up after a few tries
    /// and stops reporting crashes at all, so the busier the application gets
    /// the less able it becomes to report one. Exactly backwards.
    #[test]
    fn another_conversations_turns_do_not_disturb_this_one() {
        let c = coordinator();
        let seen = c.observe("conv-1");

        // A whole turn and a whole write, elsewhere.
        drop(
            c.try_acquire_turn_as("conv-2", TurnOrigin::Desktop, "t".into())
                .unwrap(),
        );
        drop(c.try_acquire_mutation("conv-2", "a delete").unwrap());
        let _busy = c.try_acquire_turn_as("conv-3", TurnOrigin::OneBot, "u".into()).unwrap();

        assert!(c.unchanged_since(&seen), "none of that was about conv-1");

        // And conv-1's own comings and goings still are.
        drop(
            c.try_acquire_turn_as("conv-1", TurnOrigin::Desktop, "mine".into())
                .unwrap(),
        );
        assert!(!c.unchanged_since(&seen));
    }

    /// A refused acquisition changed nothing, and must not say it did — every
    /// spurious change is a snapshot re-read.
    #[test]
    fn being_turned_away_is_not_a_change() {
        let c = coordinator();
        let _held = c
            .try_acquire_turn_as("conv-1", TurnOrigin::Desktop, "t1".into())
            .unwrap();
        let before = c.observe("conv-1");

        assert!(c.try_acquire_turn("conv-1", TurnOrigin::OneBot).is_err());
        assert!(c.try_acquire_mutation("conv-1", "a delete").is_err());

        assert!(c.unchanged_since(&before));
    }

    /// One turn panicking while holding the lock must not take the rest of the
    /// app's turns down with it.
    #[test]
    fn a_poisoned_lock_still_hands_out_the_table() {
        let c = coordinator();
        let poisoner = Arc::clone(&c);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock();
            panic!("a turn died holding the lock");
        })
        .join();

        assert!(c.try_acquire_turn("conv-1", TurnOrigin::Desktop).is_ok());
    }
}
