//! The places the loop reaches outside itself.
//!
//! One trait per thing the two runners genuinely disagree about, and nothing
//! else. The test of whether something belongs here is not "does it differ" but
//! "does it differ *irreconcilably*" — the row writes differ in wording and the
//! retry ladder differs in whether it logs, and neither earned a port. What did:
//! how an answer is asked for, where mid-turn commentary goes, which tools live
//! outside the registry, what arrives while the turn is running, and whether the
//! conversation can change mode at all.
//!
//! `None` on an optional port is not a degraded mode. It is the honest statement
//! that this runner has no such thing: the desktop has no inbox and OneBot has
//! no plan mode, and a loop that faked either would be inventing behaviour that
//! this refactor is explicitly not adding.
//!
//! Every trait is `Send + Sync` because the whole turn runs as one detached task
//! on the OneBot side. A port that forgets it fails to compile at the call site
//! rather than at runtime, which is the good kind of failure.

use crate::provider::{SenderRef, ToolCall};

use super::transitions::Transitions;
use super::{ApprovalDecision, Emit};

/// Putting a tool call in front of whoever decides, and waiting.
///
/// **Only `ask_user` may consume a [`ApprovalDecision::Response`].** Everywhere
/// else — a registry tool, an MCP tool, a surface tool, a sandbox escalation, a
/// mode switch — the sole thing that authorises the call is `Approved`, and a
/// `Response` is refused exactly as `None` is.
///
/// The distinction is easy to lose, because both plainly mean "the person did
/// something rather than nothing". But `Response` is the *answer to a question*,
/// and only `ask_user` asked one. A transport that can only carry a boolean has
/// to choose which to send, and choosing `Response` for everything would turn
/// somebody typing a sentence into permission to run a command. So the mapping
/// belongs to the adapter and is per tool: `ask_user`'s yes becomes a
/// `Response`, everything else's becomes `Approved`.
#[async_trait::async_trait]
pub trait Approvals: Send + Sync {
    /// `Ok(None)` is nobody answered — cancelled, timed out, or the waiter went
    /// away. It is not a refusal with an empty reason, and the loop tells the
    /// two apart when it words the tool result.
    ///
    /// `Err` ends the turn. Only the desktop can produce one: drawing its card
    /// *is* an event, so a send that fails means the user is looking at a
    /// question that will never appear.
    ///
    /// Implementations own the phase bracket. The wait is the longest window a
    /// turn has — a person can leave a card on screen for an hour — and what the
    /// record says during it is the difference between "stopped waiting for you"
    /// and "may have already run".
    async fn ask(
        &self,
        assistant_message_id: &str,
        call: &ToolCall,
        retry_reason: Option<&str>,
    ) -> Result<Option<ApprovalDecision>, String>;
}

/// Where the model's text goes when it is not the last thing it says.
///
/// The desktop has no use for it: every chunk was already streamed to the
/// window as it arrived. OneBot streams nowhere, so without this the only text
/// that ever reaches the chat is the final iteration's — everything the model
/// said on the way to a tool call would exist solely in the database.
#[async_trait::async_trait]
pub trait Commentary: Send + Sync {
    async fn say(&self, text: String);
}

/// Tools that belong to the surface the turn is running on.
///
/// QQ's are neither in the registry nor behind MCP: they are scoped to one
/// group and one session, and handing them to the registry would mean the
/// registry knowing about chat platforms. Their definitions are merged into
/// `tool_defs` by the caller; this only says who owns a name and what happens
/// when it is called.
#[async_trait::async_trait]
pub trait SurfaceTools: Send + Sync {
    fn owns(&self, name: &str) -> bool;
    fn requires_approval(&self, name: &str) -> bool;
    async fn execute(&self, name: &str, arguments: &str) -> Result<String, String>;
}

/// Running a whole turn of somebody else's, and waiting for its answer.
///
/// Same shape as `SurfaceTools`: the loop recognises the name and hands over,
/// and everything about what happens next lives in the implementation. It has
/// to be a port rather than something the loop does itself, because starting a
/// turn means resolving a provider and a key, and this module is not allowed to
/// know that those exist.
///
/// It is also what closes recursion. A sub-agent's own ports carry `None` here,
/// so a delegated run cannot delegate. That is a property of the type rather
/// than a depth counter somebody has to remember to check.
/// Nothing implements this yet, so the compiler cannot see anything reading the
/// types below. The runner that does is the next piece of work; declaring the
/// shape first is what lets the loop, the tool definition and the dispatch
/// branch be reviewed on their own.
#[async_trait::async_trait]
pub trait SubAgents: Send + Sync {
    async fn run(&self, spec: SubAgentSpec) -> Result<SubAgentReport, String>;
}

/// What the parent asked for.
pub struct SubAgentSpec {
    pub kind: crate::agent::sub_agents::SubAgentKind,
    /// Three to five words, shown on the card while it runs and kept as the
    /// sub-agent's conversation title.
    pub description: String,
    /// The whole briefing. The parent's transcript is not passed along, so this
    /// is everything the sub-agent will know.
    pub prompt: String,
    /// `"<provider_id>:<model_id>"`, or `None` for the configured default. Not
    /// resolved here: which models exist is a question for the runner that owns
    /// the database.
    pub model: Option<String>,
    /// The tool call that asked. Both halves, because provider call ids repeat
    /// within one conversation and the card is found by the pair.
    pub parent_message_id: String,
    pub parent_call_id: String,
}

/// How a delegated run ended.
///
/// Kept apart from the text, because `TurnOutcome::reply` is `Ok(partial)` for
/// a turn that was cancelled and for one the loop guard stopped. Handing the
/// parent that string with no verdict attached is how half an answer gets read
/// as a conclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentStatus {
    Done,
    /// Somebody pressed Stop — on the sub-agent, or on the turn that spawned it.
    Cancelled,
    /// The loop guard stopped it going round in circles.
    Aborted,
    Failed,
}

impl SubAgentStatus {
    /// What the parent's tool result is recorded as. Only a run that finished
    /// counts as a success; the rest are outcomes the model must not paper over.
    pub fn outcome(&self) -> &'static str {
        match self {
            SubAgentStatus::Done => "success",
            _ => "error",
        }
    }
}

pub struct SubAgentReport {
    pub status: SubAgentStatus,
    pub reply: String,
    /// Assistant iterations — how many times the model was asked.
    pub steps: usize,
    pub stranded: Stranded,
}

/// Messages the user typed at a run that had already stopped reading.
///
/// Told to the model rather than dropped, because they were *accepted*: the
/// command returned `Ok` and the sender watched the message go. Whoever is
/// waiting on the run has to know that something was said to it that it never
/// saw — most likely the correction they are now wondering why it ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stranded {
    /// Taken in and never delivered.
    pub accepted: usize,
    /// Of those, how many could not even be written to the transcript. Said out
    /// loud rather than hidden: the alternative is the user believing their
    /// words are on record somewhere they are not.
    pub unrecorded: usize,
}

impl Stranded {
    pub(crate) fn is_empty(&self) -> bool {
        self.accepted == 0
    }
}

/// Something that arrived while the turn was running.
pub struct Steered {
    pub text: String,
    pub origin: SteeredOrigin,
    /// The transcript row this message already has, when whoever produced it
    /// wrote one. `None` means the loop writes it, which is the ordinary case.
    ///
    /// A durable queue cannot leave it to the loop. Taking the item off the
    /// queue and writing the row it becomes have to be one transaction, or a
    /// kill in between leaves a state nobody can repair: either the item is
    /// still queued and no row exists — deliver it again, safely — or the row
    /// exists and the item is spent. So it arrives already written and says
    /// where, rather than getting a second row for the same message.
    pub row: Option<String>,
}

impl Steered {
    /// A message with no row of its own, which the loop will write.
    pub fn typed(text: String, origin: SteeredOrigin) -> Self {
        Self {
            text,
            origin,
            row: None,
        }
    }
}

/// Who put it there, which decides what the model reads it as.
///
/// An `Option<SenderRef>` used to carry this, with `None` meaning "the system
/// generated it". That worked while the only person who could steer was a chat
/// user, and stopped working the moment a desktop user could: they have no chat
/// identity, so they would have been `None` too, and what they typed would have
/// reached the model as environment noise instead of as an instruction.
pub enum SteeredOrigin {
    /// Somebody typed it. `None` is a desktop user — no chat identity, still a
    /// person talking.
    User(Option<SenderRef>),
    /// A notice we generated: a recall, a membership change. Travels as
    /// injected context rather than as a message anyone sent.
    System,
}

/// Messages that turned up mid-turn.
///
/// Drained between rounds rather than at any point in one, so a request is
/// never assembled from a history that is being appended to.
///
/// This was deliberately not `async`, on the grounds that every implementation
/// was a queue behind a lock and a future would put an await in the loop for no
/// reason anyone could name. There is a reason now: the desktop's queue is a
/// *table*, and taking an item off it means a transaction — which has to happen
/// on a blocking thread rather than on the runtime, and has to include the row
/// write (see [`Steered::row`]). Both drains are already inside `async fn`s, so
/// what it costs is this line.
#[async_trait::async_trait]
pub trait Steering: Send + Sync {
    async fn drain(&self) -> Vec<Steered>;

    /// What may still be run now that those messages have joined the turn.
    ///
    /// A turn is opened by one person and can be continued by another — a group
    /// drains whatever arrived while it was running — and the permission that
    /// opened it does not extend to whoever spoke next. Without this, an
    /// ordinary member could talk into a turn an admin had started and inherit
    /// its authority, which for a read like `qq_get_friend_list` needs no
    /// approval to become a leak.
    ///
    /// The loop intersects rather than replaces, so an answer here can only
    /// ever take tools away. `None` is a surface with one speaker, where the
    /// question does not arise.
    fn narrowed(&self) -> Option<std::collections::HashSet<String>> {
        None
    }
}

/// Everything the loop is allowed to reach outside itself.
///
/// Borrowed rather than owned so that a caller can keep using the same
/// implementations across several rounds of one turn — which is exactly what a
/// `TurnEnd::Continue` round is.
pub struct TurnPorts<'a> {
    /// `None` emits nothing at all, which is the ordinary case for a QQ turn
    /// with no window attached.
    pub emit: Option<&'a dyn Emit>,
    pub approvals: &'a dyn Approvals,
    pub interim: Option<&'a dyn Commentary>,
    pub surface_tools: Option<&'a dyn SurfaceTools>,
    pub steering: Option<&'a dyn Steering>,
    pub transitions: Option<&'a dyn Transitions>,
    /// `None` on every runner that cannot delegate — and on every sub-agent, so
    /// that a delegated run cannot delegate again.
    pub sub_agents: Option<&'a dyn SubAgents>,
}
