//! What crosses the socket.
//!
//! The inbound half is written by the `meridian-plan-gate` plugin, not by
//! Claude Code itself: the plugin has already pulled the plan out of the hook
//! payload, counted the round and decided whether the plan actually changed.
//! So this is our own shape and we can require what we need, rather than
//! tracking whatever Claude Code's hook JSON looks like this month.
//!
//! The outbound half is Claude Code's, and that one is not ours to change —
//! see `HookOutput`.

use std::fmt;

use serde::de::{self, DeserializeOwned};
use serde::{Deserialize, Deserializer, Serialize};

/// A nullable wire field whose key is nevertheless required.
///
/// Serde deliberately treats a bare `Option<T>` as `None` when the key is
/// missing. The request implementations first deserialize these fields as
/// required JSON values, then decode an explicitly supplied `null` as `None`.
fn required_nullable<T, E>(value: serde_json::Value, field: &'static str) -> Result<Option<T>, E>
where
    T: DeserializeOwned,
    E: de::Error,
{
    serde_json::from_value::<Option<T>>(value).map_err(|error| E::custom(format!("invalid field {field:?}: {error}")))
}

/// What is being reviewed. The two differ in what the reviewer is told to look
/// for, where the transcript is filed, and nothing else — which is why they
/// share a runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A plan, before anything has been written.
    Plan,
    /// A diff, after it has.
    Implementation,
}

impl Kind {
    /// Stamped on the conversation, and the only thing that makes a
    /// client-supplied conversation id acceptable. Also what keeps the two
    /// kinds from being handed each other's transcripts.
    pub(crate) fn agent_kind(self) -> &'static str {
        match self {
            Kind::Plan => "plan_review",
            Kind::Implementation => "impl_review",
        }
    }

    pub(crate) fn title_prefix(self) -> &'static str {
        match self {
            Kind::Plan => "计划审查",
            Kind::Implementation => "改动审查",
        }
    }
}

/// One review, after the wire format has been read.
///
/// Both routes build this; the runner below knows nothing about which one it
/// came from beyond [`Kind`].
pub(crate) struct ReviewJob {
    pub kind: Kind,
    pub session_id: String,
    pub cwd: String,
    pub conversation_id: Option<String>,
    /// The text under review — a plan, or a diff.
    pub subject: String,
    /// What the writer said it had done. Only implementation reviews have one,
    /// and it is a claim to check rather than evidence: the diff is evidence.
    pub note: Option<String>,
    pub round: u32,
    pub max_rounds: Option<u32>,
    pub stagnant: bool,
    pub history: Vec<HistoryEntry>,
}

/// One `Stop` submission: the code as it now stands, and what the agent
/// claims it did.
#[derive(Debug)]
pub(crate) struct StopReviewRequest {
    pub session_id: String,
    pub cwd: String,
    pub conversation_id: Option<String>,
    /// Everything not committed, as the plugin computed it. Sent rather than
    /// fetched because the reviewer has no shell — it reads files, it does not
    /// run `git`.
    pub diff: String,
    pub note: Option<String>,
    pub round: u32,
    pub max_rounds: Option<u32>,
    pub stagnant: bool,
    pub history: Vec<HistoryEntry>,
}

impl<'de> Deserialize<'de> for StopReviewRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            #[serde(rename = "sessionId")]
            session_id: String,
            cwd: String,
            #[serde(rename = "conversationId")]
            conversation_id: serde_json::Value,
            diff: String,
            note: serde_json::Value,
            round: u32,
            max_rounds: serde_json::Value,
            stagnant: bool,
            history: Vec<HistoryEntry>,
        }

        let fields = Fields::deserialize(deserializer)?;
        Ok(Self {
            session_id: fields.session_id,
            cwd: fields.cwd,
            conversation_id: required_nullable(fields.conversation_id, "conversationId")?,
            diff: fields.diff,
            note: required_nullable(fields.note, "note")?,
            round: fields.round,
            max_rounds: required_nullable(fields.max_rounds, "max_rounds")?,
            stagnant: fields.stagnant,
            history: fields.history,
        })
    }
}

impl StopReviewRequest {
    pub(crate) fn into_job(self) -> ReviewJob {
        ReviewJob {
            kind: Kind::Implementation,
            session_id: self.session_id,
            cwd: self.cwd,
            conversation_id: self.conversation_id,
            subject: self.diff,
            note: self.note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
            round: self.round,
            max_rounds: self.max_rounds,
            stagnant: self.stagnant,
            history: self.history,
        }
    }
}

/// One `ExitPlanMode` submission, as the plugin describes it.
#[derive(Debug)]
pub(crate) struct ReviewRequest {
    pub session_id: String,
    /// The repository the plan is about. The reviewing agent is confined to it,
    /// so this is load-bearing rather than informational.
    pub cwd: String,
    /// Where the earlier rounds of this session went, as the client remembers
    /// it. This is what lets round 2 see what round 1 said without Meridian
    /// holding a session table of its own — the client keeps its state on disk
    /// and hands the continuation back, so a Meridian that crashed between the
    /// two rounds has nothing to recover.
    ///
    /// Optional, and not trusted: see `review::open_or_reuse`.
    pub conversation_id: Option<String>,
    pub plan: String,
    pub round: u32,
    pub max_rounds: Option<u32>,
    /// Whether this submission is materially the same as the previous one. The
    /// reviewer is told, because "this is the third time you have seen this"
    /// changes what a useful answer looks like.
    pub stagnant: bool,
    pub history: Vec<HistoryEntry>,
}

impl<'de> Deserialize<'de> for ReviewRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            #[serde(rename = "sessionId")]
            session_id: String,
            cwd: String,
            #[serde(rename = "conversationId")]
            conversation_id: serde_json::Value,
            plan: String,
            round: u32,
            max_rounds: serde_json::Value,
            stagnant: bool,
            history: Vec<HistoryEntry>,
        }

        let fields = Fields::deserialize(deserializer)?;
        Ok(Self {
            session_id: fields.session_id,
            cwd: fields.cwd,
            conversation_id: required_nullable(fields.conversation_id, "conversationId")?,
            plan: fields.plan,
            round: fields.round,
            max_rounds: required_nullable(fields.max_rounds, "max_rounds")?,
            stagnant: fields.stagnant,
            history: fields.history,
        })
    }
}

impl ReviewRequest {
    pub(crate) fn into_job(self) -> ReviewJob {
        ReviewJob {
            kind: Kind::Plan,
            session_id: self.session_id,
            cwd: self.cwd,
            conversation_id: self.conversation_id,
            subject: self.plan,
            note: None,
            round: self.round,
            max_rounds: self.max_rounds,
            stagnant: self.stagnant,
            history: self.history,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryEntry {
    pub round: u32,
    pub verdict: HistoryVerdict,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HistoryVerdict {
    Approve,
    Revise,
    Inconclusive,
}

impl HistoryVerdict {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Revise => "revise",
            Self::Inconclusive => "inconclusive",
        }
    }
}

impl fmt::Display for HistoryVerdict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What the plugin gets back.
///
/// Three shapes, and the difference matters: only `Revise` stops the plan.
/// `Approve` and `Inconclusive` both let Claude Code's own permission flow
/// carry on, which is what puts the plan in front of the user. Deciding
/// *for* the user is not this endpoint's job.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ReviewResponse {
    Verdict {
        verdict: &'static str,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(rename = "reviewId")]
        review_id: String,
        /// Hand back where this round was written, so the next one can ask to
        /// continue there. The client is the only thing that remembers.
        #[serde(rename = "conversationId")]
        conversation_id: String,
    },
    /// The review ran but said nothing usable. The plugin forwards this string
    /// to the user verbatim and does not block the plan.
    ///
    /// Always carries the conversation: getting here means a transcript was
    /// written, and the next round should continue in it rather than start the
    /// reviewer over. A review that never ran at all is a non-200 instead, so
    /// there is no case here with nothing to continue.
    Inconclusive {
        #[serde(rename = "systemMessage")]
        system_message: String,
        #[serde(rename = "conversationId")]
        conversation_id: String,
    },
}

impl ReviewResponse {
    pub(crate) fn approve(summary: String, review_id: String, conversation_id: String) -> Self {
        Self::Verdict {
            verdict: "approve",
            summary,
            message: None,
            review_id,
            conversation_id,
        }
    }

    pub(crate) fn revise(summary: String, message: String, review_id: String, conversation_id: String) -> Self {
        Self::Verdict {
            verdict: "revise",
            summary,
            message: Some(message),
            review_id,
            conversation_id,
        }
    }

    /// Ran, wrote a transcript, but produced no usable verdict.
    pub(crate) fn inconclusive(reason: impl Into<String>, conversation_id: String) -> Self {
        Self::Inconclusive {
            system_message: reason.into(),
            conversation_id,
        }
    }
}

/// The error body. Nothing reads it but a human with `claude --debug` open:
/// every non-200 is fail-open on the Claude Code side, so the status code is
/// the whole protocol and this is just the explanation.
#[derive(Debug, Serialize)]
pub(crate) struct ErrorBody {
    pub error: String,
}
