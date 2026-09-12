//! Where an event goes once it has happened.
//!
//! There used to be exactly one destination — the window — and `app.emit` was
//! called wherever something worth reporting occurred. A second destination (a
//! remote client over a socket) cannot be added that way without every call site
//! learning about it, so the call sites now name the bus and the bus knows the
//! destinations.
//!
//! The bus deliberately does not fan out over a channel. A `broadcast` would
//! make every send infallible from the sender's point of view, and that is the
//! one thing this cannot do: the desktop's events *are* its answer, so a window
//! that missed one is showing a transcript that never catches up, and the turn
//! producing it has to fail rather than carry on talking to nobody. Delivery is
//! therefore synchronous and a sink can be marked `critical`, meaning its
//! failure is the caller's failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tracing::debug;

/// The one first-party streaming channel shared by the desktop and remote UI.
pub const CHAT_STREAM_CHANNEL: &str = "chat-stream";
pub const CONVERSATION_UPDATED_CHANNEL: &str = "conversation-updated";
pub const QUEUE_UPDATED_CHANNEL: &str = "queue-updated";
pub const COMPACT_START_CHANNEL: &str = "compact-start";
pub const COMPACT_DONE_CHANNEL: &str = "compact-done";
pub const USER_COMMAND_CHANNEL: &str = "user-command";
pub const VOICE_MODEL_DOWNLOAD_CHANNEL: &str = "voice-model-download";
pub const VOICE_MODEL_DOWNLOAD_DONE_CHANNEL: &str = "voice-model-download-done";
pub const PLAN_REVIEW_REQUESTED_CHANNEL: &str = "plan-review-requested";
pub const PLAN_REVIEW_UPDATED_CHANNEL: &str = "plan-review-updated";

/// A conversation row changed and every client must re-read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationUpdatedEvent {
    pub conversation_id: String,
}

impl ConversationUpdatedEvent {
    pub fn new(conversation_id: impl Into<String>) -> Self {
        Self {
            conversation_id: conversation_id.into(),
        }
    }
}

/// The queue ledger moved. `delivered` is required even when false: omission
/// used to make an enqueue indistinguishable from a producer on an older wire
/// shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueUpdatedEvent {
    pub conversation_id: String,
    pub delivered: bool,
}

impl QueueUpdatedEvent {
    pub fn new(conversation_id: impl Into<String>, delivered: bool) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            delivered,
        }
    }
}

/// A plan review changed. The event is deliberately only an invalidation key;
/// the durable review bundle is read back from SQLite so a missed event or an
/// app restart cannot become a different state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanReviewEvent {
    pub review_id: String,
    pub conversation_id: String,
    pub document_id: String,
    pub revision_id: String,
    pub turn_id: String,
    pub status: String,
    pub lock_version: i64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub delivery_state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    Manual,
    Threshold,
    ApiError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactOutcome {
    Completed,
    RemoteCompacted,
    Fallback,
    Failed,
}

/// A compaction pass began. Every producer sends the same three fields; manual
/// and automatic passes no longer invent channel-specific partial shapes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactStartEvent {
    pub conversation_id: String,
    pub mid_turn: bool,
    pub trigger: CompactTrigger,
}

/// A compaction pass ended. Nullable values are present on the wire as `null`,
/// not omitted, so a client can distinguish the current contract from an
/// incomplete payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactDoneEvent {
    pub conversation_id: String,
    pub mid_turn: bool,
    pub trigger: CompactTrigger,
    pub outcome: CompactOutcome,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub tokens_reclaimed: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub error: Option<String>,
}

/// Progress is tagged even though this channel currently has one variant. A
/// second phase must therefore be added to both Rust and TypeScript rather than
/// being inferred from whichever optional fields happen to be present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VoiceModelDownloadEvent {
    Progress {
        downloaded: u64,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        total: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VoiceModelDownloadDoneEvent {
    Completed {},
    Cancelled {},
    Failed { error: String },
}

/// Serde normally treats a missing `Option<T>` exactly like an explicit null.
/// Event contracts do not: nullable keys are still required keys.
pub fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// A local tool call's terminal state.
///
/// This is a closed contract. A producer cannot put a new spelling on the wire
/// until the Rust and TypeScript unions have both been updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Success,
    Denied,
    Error,
}

impl ToolOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Denied => "denied",
            Self::Error => "error",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "success" => Ok(Self::Success),
            "denied" => Ok(Self::Denied),
            "error" => Ok(Self::Error),
            _ => Err(format!("unknown tool outcome `{value}`")),
        }
    }
}

/// Why a streamed turn stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatStopReason {
    EndTurn,
    Error,
    LoopDetected,
    Cancelled,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
}

impl ChatStopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::Error => "error",
            Self::LoopDetected => "loop_detected",
            Self::Cancelled => "cancelled",
            Self::MaxTokens => "max_tokens",
            Self::MaxTurnRequests => "max_turn_requests",
            Self::Refusal => "refusal",
        }
    }
}

impl TryFrom<&str> for ChatStopReason {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, String> {
        match value {
            "end_turn" => Ok(Self::EndTurn),
            "error" => Ok(Self::Error),
            "loop_detected" => Ok(Self::LoopDetected),
            "cancelled" => Ok(Self::Cancelled),
            "max_tokens" => Ok(Self::MaxTokens),
            "max_turn_requests" => Ok(Self::MaxTurnRequests),
            "refusal" => Ok(Self::Refusal),
            _ => Err(format!("unknown chat stop reason `{value}`")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDelegation {
    pub parent_call_id: String,
    pub sub_conversation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRetry {
    pub reason: String,
    pub origin_call_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoReviewOutcome {
    Allow,
    Deny,
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoReviewRisk {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoReviewAuthorization {
    Unknown,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoReviewStage {
    Quick,
    Investigate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoReviewEvidence {
    pub tool: String,
    pub arguments: String,
}

/// The public projection of one automatic-review decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoReviewVerdict {
    pub outcome: AutoReviewOutcome,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub risk: Option<AutoReviewRisk>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub authorization: Option<AutoReviewAuthorization>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub rationale: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub stage: Option<AutoReviewStage>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub model: Option<String>,
    pub evidence: Vec<AutoReviewEvidence>,
}

/// One value accepted by an ACP session option after it crosses into the
/// first-party event contract. ACP itself may omit descriptions; Meridian
/// always emits the key and spells absence as `null`.
#[cfg(not(target_os = "android"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcpConfigOptionValueEvent {
    pub value: String,
    pub name: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub description: Option<String>,
}

#[cfg(not(target_os = "android"))]
impl From<crate::acp::protocol::ConfigOptionValue> for AcpConfigOptionValueEvent {
    fn from(value: crate::acp::protocol::ConfigOptionValue) -> Self {
        Self {
            value: value.value,
            name: value.name,
            description: value.description,
        }
    }
}

/// A complete first-party projection of an ACP session option. Nullable keys
/// remain required, so adding, removing, or omitting a field is a contract
/// change instead of a compatibility fallback.
#[cfg(not(target_os = "android"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcpConfigOptionEvent {
    pub id: String,
    pub name: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub description: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub category: Option<String>,
    #[serde(rename = "type", deserialize_with = "deserialize_required_nullable")]
    pub kind: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub current_value: Option<serde_json::Value>,
    pub options: Vec<AcpConfigOptionValueEvent>,
}

#[cfg(not(target_os = "android"))]
impl From<crate::acp::protocol::SessionConfigOption> for AcpConfigOptionEvent {
    fn from(option: crate::acp::protocol::SessionConfigOption) -> Self {
        Self {
            id: option.id,
            name: option.name,
            description: option.description,
            category: option.category,
            kind: option.kind,
            current_value: option.current_value,
            options: option.options.into_iter().map(Into::into).collect(),
        }
    }
}

/// The broad group a hosted session's incident belongs to, as the adapter
/// files it. Drives iconography and nothing else; the text is the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpNoticeCategory {
    Connection,
    Access,
    Limit,
    Request,
    Service,
    Unknown,
}

impl AcpNoticeCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Access => "access",
            Self::Limit => "limit",
            Self::Request => "request",
            Self::Service => "service",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "connection" => Ok(Self::Connection),
            "access" => Ok(Self::Access),
            "limit" => Ok(Self::Limit),
            "request" => Ok(Self::Request),
            "service" => Ok(Self::Service),
            "unknown" => Ok(Self::Unknown),
            _ => Err(format!("unknown notice category `{value}`")),
        }
    }
}

/// Whether an incident ended something or merely reported on it. A `warning`
/// never ends a turn; an `error` needs a person or another request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpNoticeSeverity {
    Warning,
    Error,
}

impl AcpNoticeSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "warning" => Ok(Self::Warning),
            "error" => Ok(Self::Error),
            _ => Err(format!("unknown notice severity `{value}`")),
        }
    }
}

/// What the adapter recommends doing about an incident. The app decides which
/// of these it can actually offer; the list is never a promise of a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpNoticeAction {
    Retry,
    Login,
    NewSession,
}

impl AcpNoticeAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Login => "login",
            Self::NewSession => "new_session",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "retry" => Ok(Self::Retry),
            "login" => Ok(Self::Login),
            "new_session" => Ok(Self::NewSession),
            _ => Err(format!("unknown notice action `{value}`")),
        }
    }
}

/// One incident a hosted Claude Code session reported, at its latest revision.
///
/// The persisted row (`acp_session_notices`) and this projection carry the
/// same fields; the row stores `actions` as JSON text and this decodes it
/// strictly, so a row that cannot be read is an error rather than an incident
/// with no recommendations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpSessionNoticeEvent {
    pub id: String,
    pub conversation_id: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub turn_id: Option<String>,
    pub notice_id: String,
    pub revision: u32,
    pub category: AcpNoticeCategory,
    pub severity: AcpNoticeSeverity,
    pub title: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub details: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub reason: Option<String>,
    pub actions: Vec<AcpNoticeAction>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl TryFrom<crate::db::models::acp_session_notice::AcpSessionNoticeRow> for AcpSessionNoticeEvent {
    type Error = String;

    fn try_from(row: crate::db::models::acp_session_notice::AcpSessionNoticeRow) -> Result<Self, String> {
        let actions: Vec<String> = serde_json::from_str(&row.actions).map_err(|e| {
            format!(
                "acp_session_notices.actions for `{}` is not a JSON array of strings: {e}",
                row.id
            )
        })?;
        let actions = actions
            .iter()
            .map(|a| AcpNoticeAction::parse(a))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            id: row.id,
            conversation_id: row.conversation_id,
            turn_id: row.turn_id,
            notice_id: row.notice_id,
            revision: u32::try_from(row.revision)
                .map_err(|_| "acp_session_notices.revision is negative".to_string())?,
            category: AcpNoticeCategory::parse(&row.category)?,
            severity: AcpNoticeSeverity::parse(&row.severity)?,
            title: row.title,
            details: row.details,
            reason: row.reason,
            actions,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

/// One hunk of the diff a hosted agent reported for an Edit or Write.
///
/// The same shape is persisted in `messages.tool_diffs` (a map from call id
/// to a list of these) and carried on the `tool_call_diff` event, so the
/// stored form and the live form cannot drift. `old_text` is `None` for a
/// file that did not exist; `line` is the hunk's first line after the edit,
/// `None` when the adapter did not say.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallDiff {
    pub path: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub old_text: Option<String>,
    pub new_text: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub line: Option<u32>,
}

/// Every payload permitted on [`CHAT_STREAM_CHANNEL`].
///
/// Unlike the former `json!` convention, each variant states its required
/// fields. Deserialisation rejects both unknown variants and unknown fields;
/// adding either is a coordinated contract change, not a compatibility
/// fallback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatStreamEvent {
    Text {
        content: String,
        message_id: String,
        conversation_id: String,
    },
    Reasoning {
        content: String,
        message_id: String,
        conversation_id: String,
    },
    MessageStart {
        message_id: String,
        turn_id: String,
        conversation_id: String,
    },
    UserMessage {
        content: String,
        message_id: String,
        conversation_id: String,
    },
    Retry {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        message_id: String,
        conversation_id: String,
    },
    Reset {
        message_id: String,
        conversation_id: String,
    },
    ServerTool {
        message_id: String,
        conversation_id: String,
        call: crate::provider::ServerToolCall,
    },
    ToolCall {
        call_id: String,
        tool_name: String,
        arguments: String,
        message_id: String,
        conversation_id: String,
    },
    ToolCallRevised {
        call_id: String,
        tool_name: String,
        arguments: String,
        message_id: String,
        conversation_id: String,
    },
    ToolResult {
        call_id: String,
        result: String,
        outcome: ToolOutcome,
        message_id: String,
        conversation_id: String,
    },
    ToolApprovalReq {
        approval_id: String,
        call_id: String,
        tool_name: String,
        arguments: String,
        message_id: String,
        conversation_id: String,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        delegation: Option<ApprovalDelegation>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        retry: Option<ApprovalRetry>,
    },
    ToolApprovalExpired {
        approval_id: String,
        call_id: String,
        tool_name: String,
        message_id: String,
        conversation_id: String,
    },
    SubAgentStarted {
        conversation_id: String,
        message_id: String,
        call_id: String,
        sub_conversation_id: String,
        spawned_turn_id: String,
        kind: crate::agent::sub_agents::SubAgentKind,
        description: String,
    },
    AutoReview {
        conversation_id: String,
        turn_id: String,
        message_id: String,
        call_id: String,
        tool_name: String,
        verdict: AutoReviewVerdict,
    },
    #[cfg(not(target_os = "android"))]
    AcpConfig {
        conversation_id: String,
        config_options: Vec<AcpConfigOptionEvent>,
    },
    #[cfg(not(target_os = "android"))]
    AcpUsage {
        conversation_id: String,
        used: u64,
        size: u64,
    },
    /// A hosted session reported an incident, or a new revision of one. The
    /// frontend keeps the latest revision per `notice_id`.
    #[cfg(not(target_os = "android"))]
    AcpNotice {
        conversation_id: String,
        notice: AcpSessionNoticeEvent,
    },
    /// The hosted agent reported what an Edit or Write actually changed. The
    /// whole hunk list for the call; a later one for the same call replaces it.
    #[cfg(not(target_os = "android"))]
    ToolCallDiff {
        conversation_id: String,
        message_id: String,
        call_id: String,
        diffs: Vec<ToolCallDiff>,
    },
    RedactionNotice {
        conversation_id: String,
        turn_id: String,
        redacted_count: usize,
        rules: Vec<String>,
    },
    Stop {
        reason: ChatStopReason,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        message_id: Option<String>,
        turn_id: String,
        conversation_id: String,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        input_tokens: Option<i32>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        output_tokens: Option<i32>,
    },
}

/// One destination for events.
///
/// Takes the payload by reference because a bus with two sinks would otherwise
/// clone it once per sink for no reason; implementations that need an owned
/// value clone it themselves.
pub trait EventSink: Send + Sync {
    fn emit(&self, channel: &str, payload: &serde_json::Value) -> Result<(), String>;
}

/// Names a registration so it can be taken back out. A sink that outlives its
/// purpose — the socket server after it stops listening — must not keep
/// receiving, and it cannot be identified by its address once it is behind an
/// `Arc<dyn _>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SinkId(u64);

struct SinkEntry {
    id: SinkId,
    sink: Arc<dyn EventSink>,
    /// Whether this sink failing is the emitting turn's problem. True for the
    /// window, false for anything a turn can carry on without.
    critical: bool,
}

#[derive(Clone, Default)]
pub struct EventBus(Arc<Inner>);

#[derive(Default)]
struct Inner {
    /// A `std::sync::RwLock`, not tokio's: every critical section is one walk of
    /// a list of two or three entries with nothing awaited inside, and `Drop`
    /// implementations emit, which cannot await.
    sinks: std::sync::RwLock<Vec<SinkEntry>>,
    next_id: AtomicU64,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start delivering to `sink`. `critical` says whether a failed delivery
    /// should be reported back to whoever emitted.
    pub fn register(&self, sink: Arc<dyn EventSink>, critical: bool) -> SinkId {
        let id = SinkId(self.0.next_id.fetch_add(1, Ordering::Relaxed));
        self.lock().push(SinkEntry { id, sink, critical });
        id
    }

    /// Stop delivering to a sink that has outlived its purpose. Remote access
    /// is the caller: it stops when the user turns listening off, and a fan-out
    /// left registered would be handed every event in the app for the rest of
    /// the process's life, queueing for connections that are gone.
    pub fn unregister(&self, id: SinkId) {
        self.lock().retain(|entry| entry.id != id);
    }

    /// Deliver to every sink, in registration order.
    ///
    /// Returns the first critical failure. A non-critical one is logged and
    /// otherwise invisible, and no failure stops the remaining sinks — one
    /// client dropping its connection mid-turn must not cost the others the rest
    /// of the answer.
    ///
    /// No sinks at all is success. A headless run has no window to miss an
    /// event, and treating that as failure would end every turn it made.
    pub fn emit(&self, channel: &str, payload: serde_json::Value) -> Result<(), String> {
        let mut first_critical_error = None;
        for entry in self.read().iter() {
            if let Err(e) = entry.sink.emit(channel, &payload) {
                if entry.critical {
                    if first_critical_error.is_none() {
                        first_critical_error = Some(e);
                    }
                } else {
                    // The channel, never the payload: these carry message bodies
                    // and exported logs leave the machine.
                    debug!(channel, error = %e, "event sink refused a payload");
                }
            }
        }
        match first_critical_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Deliver one checked first-party stream event.
    pub fn emit_chat(&self, event: ChatStreamEvent) -> Result<(), String> {
        self.emit_typed(CHAT_STREAM_CHANNEL, &event)
    }

    /// Serialize a named DTO before it reaches any sink. This is the escape
    /// hatch for app-layer DTOs (notably `user-command`) that cannot live in
    /// the core crate; core-owned channels use the narrower helpers below.
    pub fn emit_typed<T: Serialize>(&self, channel: &str, event: &T) -> Result<(), String> {
        let payload = serde_json::to_value(event).map_err(|e| format!("could not serialize {channel} event: {e}"))?;
        self.emit(channel, payload)
    }

    pub fn emit_conversation_updated(&self, conversation_id: &str) -> Result<(), String> {
        self.emit_typed(
            CONVERSATION_UPDATED_CHANNEL,
            &ConversationUpdatedEvent::new(conversation_id),
        )
    }

    pub fn emit_queue_updated(&self, event: &QueueUpdatedEvent) -> Result<(), String> {
        self.emit_typed(QUEUE_UPDATED_CHANNEL, event)
    }

    pub fn emit_compact_start(&self, event: &CompactStartEvent) -> Result<(), String> {
        self.emit_typed(COMPACT_START_CHANNEL, event)
    }

    pub fn emit_compact_done(&self, event: &CompactDoneEvent) -> Result<(), String> {
        self.emit_typed(COMPACT_DONE_CHANNEL, event)
    }

    pub fn emit_voice_model_download(&self, event: &VoiceModelDownloadEvent) -> Result<(), String> {
        self.emit_typed(VOICE_MODEL_DOWNLOAD_CHANNEL, event)
    }

    pub fn emit_voice_model_download_done(&self, event: &VoiceModelDownloadDoneEvent) -> Result<(), String> {
        self.emit_typed(VOICE_MODEL_DOWNLOAD_DONE_CHANNEL, event)
    }

    pub fn emit_plan_review_requested(&self, event: &PlanReviewEvent) -> Result<(), String> {
        self.emit_typed(PLAN_REVIEW_REQUESTED_CHANNEL, event)
    }

    pub fn emit_plan_review_updated(&self, event: &PlanReviewEvent) -> Result<(), String> {
        self.emit_typed(PLAN_REVIEW_UPDATED_CHANNEL, event)
    }

    fn lock(&self) -> std::sync::RwLockWriteGuard<'_, Vec<SinkEntry>> {
        self.0.sinks.write().unwrap_or_else(|e| e.into_inner())
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<SinkEntry>> {
        self.0.sinks.read().unwrap_or_else(|e| e.into_inner())
    }
}

/// The bus, seen as a turn's progress port.
///
/// `Emit` is what a turn borrows for the length of one run; this hands whatever
/// it produces to every registered destination. The failure rule the desktop
/// depends on lives in the registration — `WindowSink` is registered as
/// critical — rather than in the type of the emitter, which is what lets one
/// implementation serve both runners.
pub struct BusEmit(pub EventBus);

impl crate::agent::engine::Emit for BusEmit {
    fn emit(&self, channel: &str, payload: serde_json::Value) -> Result<(), String> {
        self.0.emit(channel, payload)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<String>>,
        fail: bool,
    }

    impl EventSink for Recorder {
        fn emit(&self, channel: &str, _payload: &serde_json::Value) -> Result<(), String> {
            self.seen.lock().unwrap().push(channel.to_string());
            if self.fail { Err("refused".into()) } else { Ok(()) }
        }
    }

    fn text_event() -> ChatStreamEvent {
        ChatStreamEvent::Text {
            content: "hello".into(),
            message_id: "message-1".into(),
            conversation_id: "conversation-1".into(),
        }
    }

    /// A run with nowhere to send its progress is not a failed run. Getting this
    /// wrong would end every turn made without a window open.
    #[test]
    fn a_bus_with_no_sinks_succeeds() {
        let bus = EventBus::new();
        assert!(bus.emit_chat(text_event()).is_ok());
    }

    /// The desktop rule: its events are the answer, so losing one ends the turn.
    #[test]
    fn a_critical_sink_reports_its_failure() {
        let bus = EventBus::new();
        bus.register(
            Arc::new(Recorder {
                fail: true,
                ..Default::default()
            }),
            true,
        );
        assert_eq!(bus.emit_chat(text_event()), Err("refused".into()));
    }

    /// And the opposite rule, which is why `critical` is a property of the
    /// registration: a remote client that has gone away costs the turn nothing.
    #[test]
    fn a_non_critical_sink_failing_is_invisible() {
        let bus = EventBus::new();
        bus.register(
            Arc::new(Recorder {
                fail: true,
                ..Default::default()
            }),
            false,
        );
        assert!(bus.emit_chat(text_event()).is_ok());
    }

    /// One sink refusing must not cost the others the rest of the turn.
    #[test]
    fn every_sink_is_offered_the_event_even_after_one_fails() {
        let bus = EventBus::new();
        let good = Arc::new(Recorder::default());
        bus.register(
            Arc::new(Recorder {
                fail: true,
                ..Default::default()
            }),
            false,
        );
        bus.register(Arc::clone(&good) as Arc<dyn EventSink>, false);

        let _ = bus.emit_conversation_updated("conversation-1");
        assert_eq!(good.seen.lock().unwrap().len(), 1);
    }

    /// A server that has stopped listening must stop receiving. Without this its
    /// queue would fill for the rest of the process's life.
    #[test]
    fn an_unregistered_sink_stops_receiving() {
        let bus = EventBus::new();
        let sink = Arc::new(Recorder::default());
        let id = bus.register(Arc::clone(&sink) as Arc<dyn EventSink>, false);

        let _ = bus.emit_chat(text_event());
        bus.unregister(id);
        let _ = bus.emit_chat(text_event());

        assert_eq!(sink.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn chat_stream_rejects_unknown_variants_fields_and_missing_fields() {
        for payload in [
            serde_json::json!({
                "type": "future_event",
                "conversation_id": "conversation-1",
            }),
            serde_json::json!({
                "type": "text",
                "content": "hello",
                "message_id": "message-1",
                "conversation_id": "conversation-1",
                "future_field": true,
            }),
            serde_json::json!({
                "type": "text",
                "message_id": "message-1",
                "conversation_id": "conversation-1",
            }),
            serde_json::json!({
                "type": "server_tool",
                "message_id": "message-1",
                "conversation_id": "conversation-1",
                "call": {
                    "id": "call-1",
                    "name": "web_search",
                    "sources": [],
                    "completed": false,
                },
            }),
            serde_json::json!({
                "type": "tool_approval_req",
                "approval_id": "approval-1",
                "call_id": "call-1",
                "tool_name": "run_command",
                "arguments": "{}",
                "message_id": "message-1",
                "conversation_id": "conversation-1",
            }),
            serde_json::json!({
                "type": "auto_review",
                "conversation_id": "conversation-1",
                "turn_id": "turn-1",
                "message_id": "message-1",
                "call_id": "call-1",
                "tool_name": "run_command",
                "verdict": { "outcome": "allow" },
            }),
            serde_json::json!({
                "type": "stop",
                "reason": "end_turn",
                "turn_id": "turn-1",
                "conversation_id": "conversation-1",
            }),
        ] {
            assert!(serde_json::from_value::<ChatStreamEvent>(payload).is_err());
        }
    }

    #[test]
    fn chat_stream_rejects_unknown_fields_in_nested_first_party_dtos() {
        let payload = serde_json::json!({
            "type": "server_tool",
            "message_id": "message-1",
            "conversation_id": "conversation-1",
            "call": {
                "id": "call-1",
                "name": "web_search",
                "arguments": null,
                "sources": [],
                "completed": false,
                "future_field": true,
            },
        });

        assert!(serde_json::from_value::<ChatStreamEvent>(payload).is_err());

        for payload in [
            serde_json::json!({
                "type": "acp_config",
                "conversation_id": "conversation-1",
                "config_options": [{
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "sonnet",
                    "options": []
                }]
            }),
            serde_json::json!({
                "type": "acp_config",
                "conversation_id": "conversation-1",
                "config_options": [{
                    "id": "model",
                    "name": "Model",
                    "description": null,
                    "category": "model",
                    "type": "select",
                    "currentValue": "sonnet",
                    "options": [],
                    "future_field": true
                }]
            }),
        ] {
            assert!(serde_json::from_value::<ChatStreamEvent>(payload).is_err());
        }
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn acp_config_event_serializes_every_nullable_key() {
        let payload = serde_json::to_value(ChatStreamEvent::AcpConfig {
            conversation_id: "conversation-1".into(),
            config_options: vec![AcpConfigOptionEvent {
                id: "model".into(),
                name: "Model".into(),
                description: None,
                category: None,
                kind: None,
                current_value: None,
                options: vec![AcpConfigOptionValueEvent {
                    value: "sonnet".into(),
                    name: "Sonnet".into(),
                    description: None,
                }],
            }],
        })
        .unwrap();

        let option = &payload["config_options"][0];
        assert_eq!(option["description"], serde_json::Value::Null);
        assert_eq!(option["category"], serde_json::Value::Null);
        assert_eq!(option["type"], serde_json::Value::Null);
        assert_eq!(option["currentValue"], serde_json::Value::Null);
        assert_eq!(option["options"][0]["description"], serde_json::Value::Null);
    }

    /// Nullable keys are still keys: a session-scoped notice has `turn_id`,
    /// `details` and `reason` as explicit nulls, never as absences.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn acp_notice_event_serializes_every_nullable_key() {
        let payload = serde_json::to_value(ChatStreamEvent::AcpNotice {
            conversation_id: "conversation-1".into(),
            notice: AcpSessionNoticeEvent {
                id: "n1".into(),
                conversation_id: "conversation-1".into(),
                turn_id: None,
                notice_id: "sess:notice:1:1".into(),
                revision: 1,
                category: AcpNoticeCategory::Unknown,
                severity: AcpNoticeSeverity::Warning,
                title: "Model fallback".into(),
                details: None,
                reason: None,
                actions: vec![AcpNoticeAction::Retry, AcpNoticeAction::NewSession],
                created_at: 1,
                updated_at: 1,
            },
        })
        .unwrap();

        assert_eq!(payload["type"], "acp_notice");
        let notice = &payload["notice"];
        assert_eq!(notice["turn_id"], serde_json::Value::Null);
        assert_eq!(notice["details"], serde_json::Value::Null);
        assert_eq!(notice["reason"], serde_json::Value::Null);
        assert_eq!(notice["category"], "unknown");
        assert_eq!(notice["severity"], "warning");
        assert_eq!(notice["actions"], serde_json::json!(["retry", "new_session"]));

        // And the round trip refuses an absent nullable key.
        let mut absent = payload.clone();
        absent["notice"].as_object_mut().unwrap().remove("turn_id");
        assert!(serde_json::from_value::<ChatStreamEvent>(absent).is_err());
    }

    /// A created file has `old_text: null` and an unplaced hunk `line: null`,
    /// both present rather than absent — the frontend's validator reads an
    /// absence as a broken payload.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn tool_call_diff_event_serializes_every_nullable_key() {
        let payload = serde_json::to_value(ChatStreamEvent::ToolCallDiff {
            conversation_id: "conversation-1".into(),
            message_id: "m1".into(),
            call_id: "toolu_1".into(),
            diffs: vec![ToolCallDiff {
                path: "src/lib.rs".into(),
                old_text: None,
                new_text: "fn main() {}".into(),
                line: None,
            }],
        })
        .unwrap();
        assert_eq!(payload["type"], "tool_call_diff");
        assert_eq!(payload["diffs"][0]["old_text"], serde_json::Value::Null);
        assert_eq!(payload["diffs"][0]["line"], serde_json::Value::Null);

        let mut absent = payload.clone();
        absent["diffs"][0].as_object_mut().unwrap().remove("line");
        assert!(serde_json::from_value::<ChatStreamEvent>(absent).is_err());
    }

    /// A stored row with an unreadable action list is an error, not an
    /// incident that recommends nothing.
    #[test]
    fn a_notice_row_with_bad_actions_does_not_become_an_event() {
        let row = crate::db::models::acp_session_notice::AcpSessionNoticeRow {
            id: "n1".into(),
            conversation_id: "c1".into(),
            turn_id: None,
            notice_id: "x".into(),
            revision: 1,
            category: "limit".into(),
            severity: "error".into(),
            title: "t".into(),
            details: None,
            reason: None,
            actions: r#"["retry", "teleport"]"#.into(),
            created_at: 1,
            updated_at: 1,
        };
        assert!(AcpSessionNoticeEvent::try_from(row).is_err());
    }

    #[test]
    fn a_stop_has_no_compatibility_or_monetary_side_channel() {
        let payload = serde_json::to_value(ChatStreamEvent::Stop {
            reason: ChatStopReason::EndTurn,
            message_id: Some("message-1".into()),
            turn_id: "turn-1".into(),
            conversation_id: "conversation-1".into(),
            input_tokens: Some(12),
            output_tokens: Some(3),
        })
        .unwrap();
        assert_eq!(payload["type"], "stop");
        assert!(payload.get("done").is_none());
        assert!(payload.get("cost").is_none());
        assert!(payload.get("cost_breakdown").is_none());
    }

    #[test]
    fn non_stream_event_contracts_reject_missing_extra_and_unknown_values() {
        assert!(
            serde_json::from_value::<ConversationUpdatedEvent>(serde_json::json!({ "id": "conversation-1" })).is_err()
        );
        assert!(
            serde_json::from_value::<QueueUpdatedEvent>(serde_json::json!({
                "conversation_id": "conversation-1"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<QueueUpdatedEvent>(serde_json::json!({
                "conversation_id": "conversation-1",
                "delivered": false,
                "future": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<CompactStartEvent>(serde_json::json!({
                "conversation_id": "conversation-1",
                "mid_turn": true,
                "trigger": "future"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<CompactDoneEvent>(serde_json::json!({
                "conversation_id": "conversation-1",
                "mid_turn": true,
                "trigger": "threshold",
                "outcome": "completed",
                "error": null
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<VoiceModelDownloadEvent>(serde_json::json!({
                "type": "future",
                "downloaded": 0,
                "total": null
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<VoiceModelDownloadDoneEvent>(serde_json::json!({
                "type": "completed",
                "error": "must not be ignored"
            }))
            .is_err()
        );
    }

    #[test]
    fn nullable_event_fields_are_present_when_serialized() {
        let compact = serde_json::to_value(CompactDoneEvent {
            conversation_id: "conversation-1".into(),
            mid_turn: false,
            trigger: CompactTrigger::Manual,
            outcome: CompactOutcome::Completed,
            tokens_reclaimed: None,
            error: None,
        })
        .unwrap();
        assert_eq!(compact["tokens_reclaimed"], serde_json::Value::Null);
        assert_eq!(compact["error"], serde_json::Value::Null);

        let progress = serde_json::to_value(VoiceModelDownloadEvent::Progress {
            downloaded: 1,
            total: None,
        })
        .unwrap();
        assert_eq!(progress["type"], "progress");
        assert_eq!(progress["total"], serde_json::Value::Null);

        let verdict = serde_json::to_value(AutoReviewVerdict {
            outcome: AutoReviewOutcome::Unreadable,
            risk: None,
            authorization: None,
            rationale: None,
            stage: None,
            model: None,
            evidence: Vec::new(),
        })
        .unwrap();
        for key in ["risk", "authorization", "rationale", "stage", "model"] {
            assert_eq!(verdict[key], serde_json::Value::Null, "missing required-null key {key}");
        }
        assert_eq!(verdict["evidence"], serde_json::json!([]));

        let approval = serde_json::to_value(ChatStreamEvent::ToolApprovalReq {
            approval_id: "approval-1".into(),
            call_id: "call-1".into(),
            tool_name: "run_command".into(),
            arguments: "{}".into(),
            message_id: "message-1".into(),
            conversation_id: "conversation-1".into(),
            delegation: None,
            retry: None,
        })
        .unwrap();
        assert_eq!(approval["delegation"], serde_json::Value::Null);
        assert_eq!(approval["retry"], serde_json::Value::Null);

        let stop = serde_json::to_value(ChatStreamEvent::Stop {
            reason: ChatStopReason::Error,
            message_id: None,
            turn_id: "turn-1".into(),
            conversation_id: "conversation-1".into(),
            input_tokens: None,
            output_tokens: None,
        })
        .unwrap();
        assert_eq!(stop["message_id"], serde_json::Value::Null);
        assert_eq!(stop["input_tokens"], serde_json::Value::Null);
        assert_eq!(stop["output_tokens"], serde_json::Value::Null);
    }
}
