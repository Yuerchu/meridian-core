//! What crosses the pipe.
//!
//! ACP is JSON-RPC 2.0 like MCP next door, and the resemblance stops there. MCP
//! is a caller: one request, one reply, and [`crate::mcp::stdio`] discards
//! everything in between. ACP is a peer — a single `session/prompt` stays open
//! for the length of a turn while the agent narrates it in notifications and
//! stops in the middle to ask the user a question. So a line off the pipe is one
//! of three things and [`Frame`] is the parse that says which.
//!
//! Inbound types are deliberately lax: unknown `sessionUpdate` variants and
//! unknown content kinds parse rather than fail. The adapter is versioned
//! separately from this app and gains update kinds on its own schedule; a strict
//! parse would turn "the agent added an update we don't draw" into "the turn
//! died".

use serde::{Deserialize, Serialize};

/// The MAJOR version this client speaks. A single integer, per the spec.
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------- JSON-RPC

#[derive(Debug, Serialize)]
pub struct Request {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl Request {
    pub fn new(id: u64, method: &str, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Notification {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: serde_json::Value,
}

impl Notification {
    pub fn new(method: &str, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

/// One line off the agent's stdout, before it is known which shape it is.
///
/// Every field is optional because the three shapes overlap: a response has
/// `id` and one of `result`/`error`, a server-initiated request has `id` and
/// `method`, a notification has `method` alone.
#[derive(Debug, Deserialize)]
pub struct Incoming {
    /// Untyped on purpose. Ours are `u64`, but an id the *agent* mints is
    /// whatever JSON-RPC allows, and a reply has to carry back exactly what
    /// arrived — reading it as a number would corrupt a string id.
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// **Doubly optional, and it has to be.** JSON-RPC distinguishes "no
    /// `result` member" from "`result: null`", and the second is a *successful*
    /// answer carrying nothing — which is what the spec's own
    /// `LoadSessionResponse` permits, every field of it being optional.
    ///
    /// A plain `Option` collapses the two: serde reads an explicit `null` for
    /// `Option<T>` as `None`, [`classify`] then finds neither a result nor an
    /// error and calls the line [`Frame::Junk`], and the caller parked on that
    /// id waits for ever. `Option<Option<_>>` with `default` keeps them apart —
    /// absent is `None`, `null` is `Some(None)`.
    ///
    /// Not hypothetical for a different agent: `claude-agent-acp` answers
    /// `session/load` with a whole `NewSessionResponse`, which is more than the
    /// schema asks of it, and that generosity is the only reason this has not
    /// hung yet.
    ///
    /// The derive alone will not do it — `Option<Option<T>>` still collapses,
    /// because the *outer* `Option`'s own `Deserialize` is what turns `null`
    /// into `None`. [`present`] is only called when the member exists, which is
    /// what puts the distinction back.
    ///
    /// [`classify`]: Incoming::classify
    /// [`present`]: self::present
    #[serde(default, deserialize_with = "present")]
    pub result: Option<Option<serde_json::Value>>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// Records that a member was *there*, whatever it said.
///
/// serde only calls a field's `deserialize_with` when the member is present, so
/// reaching here is itself the answer to the question the plain `Option` cannot
/// express. Absent comes from `#[serde(default)]` instead and stays `None`.
fn present<'de, D>(deserializer: D) -> Result<Option<Option<serde_json::Value>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<serde_json::Value>::deserialize(deserializer).map(Some)
}

/// What a line actually was.
pub enum Frame {
    /// An answer to something we sent. `Err` is the agent refusing, which says
    /// nothing about the health of the pipe.
    Response {
        id: u64,
        result: Result<serde_json::Value, String>,
    },
    /// The agent asking *us*. Owes a reply carrying the same id back.
    Request {
        id: serde_json::Value,
        method: String,
        params: serde_json::Value,
    },
    Notification {
        method: String,
        params: serde_json::Value,
    },
    /// Parsed as JSON but fits none of the three. Logged and dropped: an
    /// adapter that prints something conversational to stdout must not be able
    /// to kill a session.
    Junk,
}

impl Incoming {
    pub fn classify(self) -> Frame {
        match (self.id, self.method) {
            (Some(id), Some(method)) => Frame::Request {
                id,
                method,
                params: self.params.unwrap_or(serde_json::Value::Null),
            },
            (Some(id), None) => {
                // Only our own ids can be answered, and ours are all u64.
                let Some(id) = id.as_u64() else {
                    return Frame::Junk;
                };
                match (self.result, self.error) {
                    (_, Some(e)) => Frame::Response {
                        id,
                        result: Err(format!("ACP error {}: {}", e.code, e.message)),
                    },
                    // `result: null` is a success that carries nothing, and it
                    // reaches the caller as `Value::Null` rather than being
                    // dropped — see the field's own note.
                    (Some(value), None) => Frame::Response {
                        id,
                        result: Ok(value.unwrap_or(serde_json::Value::Null)),
                    },
                    // An id with neither member is not an answer to anything.
                    (None, None) => Frame::Junk,
                }
            }
            (None, Some(method)) => Frame::Notification {
                method,
                params: self.params.unwrap_or(serde_json::Value::Null),
            },
            (None, None) => Frame::Junk,
        }
    }
}

// -------------------------------------------------------------- initialize

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
    pub client_info: Implementation,
}

/// What this client can do for the agent.
///
/// Written out rather than omitted so the choice is visible: the spec says an
/// omitted capability is unsupported, which makes an accidental omission and a
/// deliberate refusal look identical in the source.
///
/// Turning `fs` on means implementing `fs/read_text_file` and
/// `fs/write_text_file` as inbound requests, after which every file the agent
/// touches goes through this app — which is what a changes panel and a
/// `FileAccess` policy would need. Until then the agent does its own IO and we
/// only hear about it in `tool_call` notifications.
///
/// `elicitation.form` is the one that is on, and it is not optional polish: the
/// adapter reads it at `session/new` and puts `AskUserQuestion` in
/// `disallowedTools` when it is absent, so a client that stays silent here does
/// not merely miss a form — it takes the agent's ability to ask a question away
/// and tells it the tool is disabled. See [`super::elicitation`].
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FsCapabilities,
    pub terminal: bool,
    pub elicitation: ElicitationCapabilities,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

/// Which elicitation modes the agent may use on this client.
///
/// **Presence is the answer, not a boolean.** Each mode is typed
/// object-or-null, so `{}` means supported and `null` means not; `false` is not
/// a legal value for either and a validating agent would reject the whole
/// `initialize`. That is why these are `Option<Supported>` rather than the
/// `bool`s their neighbours above use.
///
/// `url` stays off: it hands the user a link to open and then waits for an
/// `elicitation/complete` notification to say they are done, which is a second
/// mechanism with a second failure mode for something no Claude Code flow
/// currently asks for.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationCapabilities {
    pub form: Option<Supported>,
    pub url: Option<Supported>,
}

impl Default for ElicitationCapabilities {
    fn default() -> Self {
        Self {
            form: Some(Supported {}),
            url: None,
        }
    }
}

/// The empty object a supported capability is spelled as.
#[derive(Debug, Serialize)]
pub struct Supported {}

#[derive(Debug, Serialize, Deserialize)]
pub struct Implementation {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default)]
    pub agent_capabilities: AgentCapabilities,
    /// Empty means the agent is already authenticated — for `claude-code-acp`
    /// that is the ordinary case, since it reuses whatever `claude` itself is
    /// logged in as.
    #[serde(default)]
    pub auth_methods: Vec<AuthMethod>,
    #[serde(default)]
    pub agent_info: Option<Implementation>,
    /// Extensions, which is where steering is advertised — at the *top level*,
    /// a sibling of `agentCapabilities` rather than a member of it.
    #[serde(default, rename = "_meta")]
    pub meta: Option<InitializeMeta>,
}

impl InitializeResult {
    /// Whether this agent takes `_session/steering`.
    ///
    /// Must be asked before the method is used. Steering is an extension, not
    /// part of the protocol, and an adapter that has never heard of it answers
    /// a request with `-32601` — which reaches the user as an RPC error in the
    /// middle of a turn that was working fine.
    pub fn steering_supported(&self) -> bool {
        self.meta
            .as_ref()
            .and_then(|m| m.steering.as_ref())
            .is_some_and(|s| s.supported)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct InitializeMeta {
    #[serde(default)]
    pub steering: Option<SteeringCapability>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SteeringCapability {
    #[serde(default)]
    pub supported: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default)]
    pub load_session: bool,
    /// The session lifecycle methods beyond the four every agent must have.
    ///
    /// `session/load` is deliberately *not* in here — the schema says so in as
    /// many words ("still handled by the top-level `load_session` capability")
    /// — so the two have to be asked separately.
    #[serde(default)]
    pub session_capabilities: SessionCapabilities,
}

/// Presence is the answer. Each of these is `{}` when supported and absent or
/// `null` when not, so the value carries nothing and only the option does.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilities {
    #[serde(default)]
    pub list: Option<serde_json::Value>,
}

impl AgentCapabilities {
    /// Whether this agent answers `session/list`.
    ///
    /// `null` is a legal way to say no, which `Option::is_some` would read as
    /// yes — serde hands back `Some(Value::Null)` for an explicit null.
    pub fn lists_sessions(&self) -> bool {
        self.session_capabilities.list.as_ref().is_some_and(|v| !v.is_null())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

// ----------------------------------------------------------------- session

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: String,
    /// Required by the spec even when empty.
    ///
    /// This carries exactly one server and it is Meridian's own
    /// [`crate::acp::bridge`] — a loopback endpoint this app owns, offering a
    /// short list of read-only tools each pinned to this conversation.
    ///
    /// **The user's configured MCP servers are still not forwarded, and that is
    /// a different question.** Those are wired to this app's tool loop and its
    /// approvals; handing them to another agent would give it a second, unowned
    /// route to the same side effects. The distinction is ownership rather than
    /// the field: one of these is ours to answer for.
    pub mcp_servers: Vec<serde_json::Value>,
    /// `None` is the second attempt — see [`SessionMeta`] for why there is one.
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<SessionMeta>,
}

/// What a session-opening request asks the adapter to pass through to the SDK.
///
/// `_meta.claudeCode.options` is spread into the Claude Agent SDK's `query()`
/// options, by `session/new` and by `session/load` alike — the load hands its
/// own `_meta` on through `getOrCreateSession` — and it is the only route a
/// client has to a knob ACP itself has no field for.
///
/// **Thinking is one of those, and without this a hosted turn shows none of
/// it.** Recent models default `thinking.display` to `omitted`, which streams
/// signature-only thinking blocks whose text is empty; the adapter emits an
/// `agent_thought_chunk` only for a block that has text, so every one of them is
/// dropped and nothing reaches `Effect::Reasoning`. Measured against adapter
/// 0.70.0, one prompt asked twice: without this, zero thought chunks; with it,
/// four. It is also why an imported transcript has reasoning on most of its rows
/// while every row a live hosted turn wrote has none — the CLI asks for the
/// display when a person is at the terminal, and nobody was asking here.
///
/// **Sent as `extraArgs` rather than as the SDK's own `thinking` option**, which
/// is a tagged union: setting the display through it means also declaring
/// `adaptive` or a fixed token budget, which decides *whether* the model thinks
/// — a different question from whether we are shown it, and one the model and
/// the user's own settings should keep. This adds `--thinking-display
/// summarized` to the child's command line and nothing else (measured by
/// capturing its argv through `CLAUDE_CODE_EXECUTABLE`).
///
/// **And that is exactly why the ask has to be droppable.** An `extraArgs` entry
/// reaches the CLI as a flag verbatim, and a `claude` that has never heard of it
/// does not shrug: measured, an unknown flag exits 1 with `error: unknown option
/// '--…'` before the process does anything, which the SDK reports as `Claude
/// Code process exited with code 1` and the adapter turns into an ordinary
/// request error. The CLI is not this app's to pin — it is whatever the user
/// installed, and `acp.command` may point at any adapter at all. So both
/// openers try once with this and once without (see `Session::handshake` and
/// `Session::load`), because a visible thought process is worth less than the
/// session it would otherwise cost. `--help` and `--version` are no evidence
/// here: commander answers both before it validates anything, which is what made
/// an unknown flag look harmless.
#[derive(Debug, Serialize)]
pub struct SessionMeta(serde_json::Value);

impl Default for SessionMeta {
    fn default() -> Self {
        Self(serde_json::json!({
            "claudeCode": { "options": { "extraArgs": { "thinking-display": "summarized" } } }
        }))
    }
}

/// Pick a session up where it was left, instead of starting one.
///
/// The same shape as `session/new` plus the id, and it answers with the same
/// [`NewSessionResult`] — which is not a shortcut on this side: the agent
/// really does resume through `createSession(..., { resume })` and hand back a
/// whole new session description.
///
/// **The `sessionId` that comes back is not necessarily this one.** What id the
/// SDK actually recovered is its answer, so the reply is what gets written down
/// rather than the request. Ask with a stale id often enough and the stored one
/// stops naming anything that exists.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionParams {
    pub session_id: String,
    /// Must be absolute — the agent refuses a relative one outright.
    pub cwd: String,
    pub mcp_servers: Vec<serde_json::Value>,
    /// The same options a new session carries, for the same reason: a resumed
    /// session builds its query through the same call, so leaving it off here
    /// would mean thinking is shown until the app is restarted and never after.
    /// `None` is the second attempt, as above.
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<SessionMeta>,
}

/// Everything the agent has on disk, optionally narrowed to one directory.
///
/// `cwd` absent means every project on the machine, which is what a panel
/// offering to import a session wants — the whole point is the sessions started
/// somewhere this app has never heard of.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// From a previous reply's `next_cursor`. `claude-agent-acp` ignores it and
    /// answers with everything in one go; the field exists because the protocol
    /// says an agent may paginate and a client that cannot follow would silently
    /// show a prefix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsResult {
    #[serde(default)]
    pub sessions: Vec<SessionInfo>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One session the agent knows about.
///
/// `title` is the SDK's own summary of the conversation — a `/rename` if there
/// was one, otherwise a generated line, otherwise the first prompt. Far better
/// than naming the folder, which is all a session started here gets.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub session_id: String,
    /// Absolute. The adapter drops any session that has none, so this is never
    /// the empty string in practice.
    pub cwd: String,
    #[serde(default)]
    pub title: Option<String>,
    /// ISO 8601. Kept as the string it arrived as rather than parsed here:
    /// nothing in core sorts on it, and the front end formats it anyway.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// What `session/load` answers with.
///
/// **Every field is optional, `sessionId` most of all — the schema does not
/// have one.** `LoadSessionResponse` is `{ modes?, configOptions?, _meta? }`,
/// so a conforming agent may answer `{}` or even `null`, and this parse has to
/// survive both. Reusing [`NewSessionResult`] here read the adapter rather than
/// the spec: `claude-agent-acp` returns a whole `NewSessionResponse` from its
/// load, which is more than it owes, and against any agent that answers what
/// the schema says the parse would have failed — sending a reopen down the
/// `session/new` fallback and losing the agent's memory of the conversation
/// without a word.
///
/// An absent `session_id` means the agent recovered the session that was asked
/// for; there is nothing else it could mean.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionResult {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub config_options: Vec<SessionConfigOption>,
}

impl LoadSessionResult {
    /// Read one out of whatever came back, `null` included.
    ///
    /// A struct cannot deserialize from `null` — serde refuses with "invalid
    /// type" — and `null` is precisely the emptiest conforming answer, so the
    /// one shape most likely to arrive from an agent that implements the schema
    /// exactly is the one a plain `from_value` rejects.
    pub fn read(value: serde_json::Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value).map_err(|e| e.to_string())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResult {
    pub session_id: String,
    /// Present from the moment the session opens, which is what lets the first
    /// row of the first turn record the right model rather than a placeholder.
    #[serde(default)]
    pub config_options: Vec<SessionConfigOption>,
}

/// One knob the agent exposes for the session.
///
/// This is how ACP reports the model: not as a field of its own, but as a
/// configuration option whose `category` is `model`, whose `current_value` is
/// the model id, and which is re-sent whenever it changes. Everything else in
/// here — modes, thought levels, whatever an agent invents — is read the same
/// way and ignored.
///
/// Deliberately lax. The shape is a `oneOf` on `type` (`select` or `boolean`)
/// and gains members; a strict parse would refuse the whole notification over a
/// knob this app does not care about.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOption {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `model` / `mode` / `model_config` / `thought_level`, or something an
    /// agent made up. Optional in the schema and described there as UX-only, so
    /// it is a hint rather than a guarantee.
    #[serde(default)]
    pub category: Option<String>,
    /// `select` or `boolean`. Only `select` is offered as a picker; a knob of
    /// some other shape is carried through so the front end can say it exists
    /// rather than pretend it does not.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// A value id for a `select`, a boolean for a toggle. Untyped because this
    /// only ever reads the one case it understands.
    #[serde(default)]
    pub current_value: Option<serde_json::Value>,
    /// What a `select` may be set to. Absent for a toggle, and absent on the
    /// `config_option_update` notification for options that did not change —
    /// which is why the session keeps the last full set rather than replacing
    /// it wholesale.
    #[serde(default)]
    pub options: Vec<ConfigOptionValue>,
}

/// One choice on a `select` config option.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ConfigOptionValue {
    pub value: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// What Claude Code's model knob says when the user has never picked one.
///
/// A real value on the wire and not a model id, so it must not be recorded as
/// one: `messages.model_id` is read back as the model that answered, and
/// `claude-code` is already the agreed way of saying "it did not say". Measured
/// — a session with no explicit model comes back with `currentValue:
/// "default"`, and an import bakes whatever this returns into every row it
/// writes.
const UNSPECIFIED_MODEL: &str = "default";

impl SessionConfigOption {
    /// The model id this option names, if it is the model selector.
    ///
    /// Falls back to matching the id when the category is absent: `category` is
    /// documented as advisory, and an agent that omits it still calls the knob
    /// `model`.
    pub fn as_model(&self) -> Option<&str> {
        if !self.names_a("model") {
            return None;
        }
        self.current_value
            .as_ref()?
            .as_str()
            .filter(|v| !v.is_empty() && *v != UNSPECIFIED_MODEL)
    }

    /// Whether this option is the one called `what`, by category or by id.
    ///
    /// `category` is documented as advisory, so an agent may omit it and still
    /// call the knob `model`. Both are accepted; neither is required to be
    /// present for the *other* to work.
    pub fn names_a(&self, what: &str) -> bool {
        match self.category.as_deref() {
            Some(category) => category.eq_ignore_ascii_case(what),
            None => self.id.eq_ignore_ascii_case(what),
        }
    }

    /// A `select` is the only shape with something to pick from. Everything
    /// else is carried but not offered.
    pub fn is_select(&self) -> bool {
        // Absent `type` with values listed is still a select: the field is
        // optional in the schema and the values are the stronger evidence.
        matches!(self.kind.as_deref(), Some("select")) || (self.kind.is_none() && !self.options.is_empty())
    }

    /// The current value as a string, for a `select`.
    pub fn current_str(&self) -> Option<&str> {
        self.current_value.as_ref()?.as_str().filter(|v| !v.is_empty())
    }
}

/// Setting one of the knobs above. The reply carries the whole option set back,
/// because changing one can reshape another — picking a model re-derives which
/// modes are available.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionParams {
    pub session_id: String,
    pub config_id: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionResult {
    #[serde(default)]
    pub config_options: Vec<SessionConfigOption>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    /// `end_turn` / `max_tokens` / `max_turn_requests` / `refusal` /
    /// `cancelled`. Kept as a string: it is reported, never branched on except
    /// to tell `cancelled` apart.
    #[serde(default)]
    pub stop_reason: String,
}

// ------------------------------------------------------------------ steering

/// Put a message into the turn that is *already running*, instead of waiting
/// for it to end and sending a fresh `session/prompt`.
///
/// An extension rather than protocol, which is why the name is underscored and
/// why [`InitializeResult::steering_supported`] has to be asked first. The
/// adapter hands it to the SDK at its `now` priority, so it lands at the next
/// point the model accepts input — between two tool calls of a multi-step turn,
/// which is the case worth having.
pub const STEER_METHOD: &str = "_session/steering";

/// The same shape as the part of a prompt that carries the message, plus the
/// one decision this client makes about what happens when there is no turn to
/// steer.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SteerParams {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
    #[serde(rename = "_meta")]
    pub meta: SteerMeta,
}

impl SteerParams {
    pub fn text(session_id: impl Into<String>, text: &str) -> Self {
        Self {
            session_id: session_id.into(),
            prompt: vec![ContentBlock::text(text)],
            meta: SteerMeta::default(),
        }
    }
}

/// **`promptRequired` is opt-in and this client opts in.**
///
/// Left out, the adapter keeps its older behaviour for a steer that finds no
/// turn running: it starts one *detached* — `this.prompt(…).catch(…)`, not
/// awaited — and answers `startedNewTurn`. That turn would then narrate itself
/// into this session with no `session/prompt` reply for anyone to wait on, no
/// turn lease, no assistant row to write into and no stop button that reaches
/// it. Asking for `promptRequired` hands the message back unconsumed instead,
/// and this app delivers it down the path it owns.
#[derive(Debug, Serialize)]
pub struct SteerMeta {
    pub steering: SteerBehaviour,
}

impl Default for SteerMeta {
    fn default() -> Self {
        Self {
            steering: SteerBehaviour {
                idle_behavior: "promptRequired",
            },
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SteerBehaviour {
    pub idle_behavior: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct SteerResult {
    #[serde(default)]
    pub outcome: String,
}

impl SteerResult {
    pub fn outcome(&self) -> SteerOutcome {
        match self.outcome.as_str() {
            "injected" => SteerOutcome::Injected,
            "promptRequired" => SteerOutcome::PromptRequired,
            "startedNewTurn" => SteerOutcome::StartedNewTurn,
            other => SteerOutcome::Unknown(other.to_string()),
        }
    }
}

/// What the agent did with a steer, and the three answers are three different
/// things — none of them a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerOutcome {
    /// It joined the running turn. The message is delivered and the agent may
    /// already be acting on it.
    Injected,
    /// There was no turn to join, and because this client asked for it the
    /// message was **not** consumed. It still has to be delivered, as an
    /// ordinary prompt.
    PromptRequired,
    /// There was no turn to join and the agent started a detached one anyway —
    /// which only happens if it ignored the `_meta` above. Delivered, but by a
    /// turn this app cannot see or stop.
    StartedNewTurn,
    /// An outcome added after this build. Delivered as far as anyone can tell,
    /// so treated as such: the alternative is re-sending something the agent
    /// has already read.
    Unknown(String),
}

/// One piece of a message.
///
/// Outbound this app only produces text. Inbound the `kind` is kept rather than
/// matched, so an agent that starts sending images does not break the text
/// arriving beside them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            kind: "text".into(),
            text: Some(s.into()),
        }
    }

    /// The text if this block is text, `None` for every other kind.
    pub fn as_text(&self) -> Option<&str> {
        if self.kind == "text" {
            self.text.as_deref()
        } else {
            None
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: String,
    pub update: SessionUpdate,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    #[serde(rename_all = "camelCase")]
    UserMessageChunk {
        content: ContentBlock,
        /// See [`SessionUpdate::AgentMessageChunk`] — the field means the same
        /// thing here, and for a replayed user message it is the SDK's uuid for
        /// the row rather than an API message id.
        #[serde(default)]
        message_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    AgentMessageChunk {
        content: ContentBlock,
        /// Which message these chunks belong to. The schema's own words: "All
        /// chunks belonging to the same message share the same `messageId`. A
        /// change in `messageId` indicates a new message has started."
        ///
        /// The live path does not need it — a round boundary there is a tool
        /// result landing — but a replay has no such rhythm, so this is what
        /// says where one assistant row ends and the next begins. Absent from
        /// `tool_call` and `plan`, which do not need one: they arrive in order
        /// and belong to whichever message is open.
        #[serde(default)]
        message_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    AgentThoughtChunk {
        content: ContentBlock,
        /// The same id as the prose it was thought for: thinking blocks live in
        /// the same API message.
        #[serde(default)]
        message_id: Option<String>,
    },
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCall),
    Plan {
        #[serde(default)]
        entries: Vec<PlanEntry>,
    },
    UsageUpdate(Usage),
    /// The agent's knobs and their current values, re-sent whole on every
    /// change. Read for one thing: which model is answering.
    #[serde(rename_all = "camelCase")]
    ConfigOptionUpdate {
        #[serde(default)]
        config_options: Vec<SessionConfigOption>,
    },
    /// Everything this step does not draw — `available_commands_update`,
    /// `current_mode_update`, and whatever the adapter adds next.
    #[serde(other)]
    Unhandled,
}

/// A tool call, and also its update: the update carries the same fields with
/// everything but the id optional, so one struct covers both and the mapping
/// treats a missing field as "unchanged".
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    #[serde(default)]
    pub title: Option<String>,
    /// `read` / `edit` / `execute` / `think` / `other` …
    #[serde(default)]
    pub kind: Option<String>,
    /// `pending` / `in_progress` / `completed` / `failed`.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub raw_input: Option<serde_json::Value>,
    #[serde(default)]
    pub content: Vec<ToolCallContent>,
    #[serde(default)]
    pub locations: Vec<ToolCallLocation>,
    /// Vendor extensions. `claude-code-acp` puts the *real* tool name here —
    /// `title` is display prose and `kind` is one of five categories, so this is
    /// the only field that says "Bash".
    #[serde(default, rename = "_meta")]
    pub meta: Option<ToolCallMeta>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCallMeta {
    #[serde(default, rename = "claudeCode")]
    pub claude_code: Option<ClaudeCodeMeta>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeMeta {
    #[serde(default)]
    pub tool_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCallContent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub content: Option<ContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallLocation {
    pub path: String,
    #[serde(default)]
    pub line: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    #[serde(default)]
    pub priority: Option<String>,
    /// `pending` / `in_progress` / `completed`.
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default)]
    pub used: u64,
    #[serde(default)]
    pub size: u64,
    /// The agent's own figure. Deliberately not run through `agent::pricing`:
    /// these tokens are billed by whatever the adapter is logged in as, and
    /// this app has no rate for them. A number computed from a rate we invented
    /// would look authoritative and be wrong.
    #[serde(default)]
    pub cost: Option<Cost>,
}

#[derive(Debug, Deserialize)]
pub struct Cost {
    /// ACP is an external protocol and specifies this field as a JSON number.
    /// Convert its lexical decimal spelling immediately; Meridian's own JSON
    /// contracts continue to accept monetary values only as strings.
    #[serde(deserialize_with = "deserialize_acp_decimal")]
    pub amount: crate::decimal::Decimal,
    #[serde(default)]
    pub currency: Option<String>,
}

fn deserialize_acp_decimal<'de, D>(deserializer: D) -> Result<crate::decimal::Decimal, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    let number = serde_json::Number::deserialize(deserializer)?;
    number.to_string().parse().map_err(D::Error::custom)
}

// -------------------------------------------------------------- permission

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionParams {
    pub session_id: String,
    pub tool_call: ToolCall,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    /// `allow_once` / `allow_always` / `reject_once` / `reject_always`. A hint
    /// for how to draw the choice — the `option_id` is what must be sent back.
    pub kind: String,
}

impl PermissionOption {
    pub fn is_allow(&self) -> bool {
        self.kind.starts_with("allow")
    }

    pub fn is_reject(&self) -> bool {
        self.kind.starts_with("reject")
    }

    /// This step answers with "just this once" whichever way the user goes, so
    /// a lasting choice is never made on their behalf by a card that did not
    /// offer it.
    pub fn is_once(&self) -> bool {
        self.kind.ends_with("_once")
    }
}

/// The reply body for `session/request_permission`.
pub fn permission_selected(option_id: &str) -> serde_json::Value {
    serde_json::json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
}

/// The other legal reply, owed whenever the turn is cancelled while a question
/// is still on screen.
pub fn permission_cancelled() -> serde_json::Value {
    serde_json::json!({ "outcome": { "outcome": "cancelled" } })
}

// ------------------------------------------------------------- elicitation

/// An `elicitation/create` request: the agent asking for typed input.
///
/// Only `form` is handled here — see [`ElicitationCapabilities`] for why `url`
/// is not — but `mode` is read rather than assumed, because the union has a
/// third open-ended variant and a `url` request answered as if it were a form
/// would return content for a question nobody was shown.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateElicitationParams {
    #[serde(default)]
    pub mode: Option<String>,
    /// Absent for a request-scoped elicitation, which nothing in this client
    /// currently produces.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Present when the question belongs to a tool call the transcript already
    /// has a card for — `AskUserQuestion` sets it, an MCP server's own
    /// elicitation does not.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub requested_schema: Option<ElicitationSchema>,
}

/// The form to render, as a JSON Schema object.
#[derive(Debug, Default, Deserialize)]
pub struct ElicitationSchema {
    #[serde(default)]
    pub properties: Properties,
    #[serde(default)]
    pub required: Vec<String>,
}

/// The form's fields, in the order they arrived.
///
/// **Order is meaning here** — it is the order the questions are asked in — and
/// neither obvious container keeps it. `serde_json::Map` is a `BTreeMap` unless
/// `preserve_order` is on, which sorts `question_10` between `question_1` and
/// `question_2`; turning that feature on is not a local change, since it
/// re-keys every JSON value this crate produces, including the request bodies a
/// provider caches on. `IndexMap` would do it and is a dependency for one field.
/// So the order is simply kept, and lookups are a scan over a handful of
/// entries.
#[derive(Debug, Default)]
pub struct Properties(pub Vec<(String, PropertySchema)>);

impl<'de> Deserialize<'de> for Properties {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct InOrder;

        impl<'de> serde::de::Visitor<'de> for InOrder {
            type Value = Properties;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object of elicitation form fields")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Properties, M::Error> {
                let mut fields = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some(entry) = map.next_entry()? {
                    fields.push(entry);
                }
                Ok(Properties(fields))
            }
        }

        deserializer.deserialize_map(InOrder)
    }
}

/// One field of the form.
///
/// Everything is optional because this shape is shared by three producers with
/// nothing in common: the adapter's own `AskUserQuestion` bridge, its
/// refusal-fallback consent prompt, and whatever JSON Schema an MCP server
/// happened to send. A field this client cannot classify is skipped rather than
/// guessed at.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertySchema {
    #[serde(default, rename = "type")]
    pub ty: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// A single-select field's choices.
    #[serde(default)]
    pub one_of: Vec<EnumOption>,
    /// A multi-select field's choices live one level down, under `items.anyOf`.
    #[serde(default)]
    pub items: Option<ItemsSchema>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<PropertyMeta>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemsSchema {
    #[serde(default)]
    pub any_of: Vec<EnumOption>,
}

/// One choice. `const` is the value to send back; `title` is what to draw.
///
/// The two are the same string for `AskUserQuestion`, whose options *are* their
/// labels, and different for the refusal-fallback prompt, whose `const`s are
/// the CLI's own wire values. Sending a title back where a `const` was asked
/// for is how a form silently answers something other than what was picked.
#[derive(Debug, Deserialize)]
pub struct EnumOption {
    #[serde(rename = "const")]
    pub value: serde_json::Value,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// The `_meta` extensions this client reads off a field.
#[derive(Debug, Default, Deserialize)]
pub struct PropertyMeta {
    /// Marks a free-text field as the "Other" box belonging to a select field
    /// rather than a question of its own. The key is deliberately un-namespaced
    /// upstream so that Codex, Claude and any other `AskUserQuestion` bridge
    /// can be recognised by one marker.
    #[serde(default, rename = "_askUserQuestionCustomAnswer")]
    pub custom_answer: Option<CustomAnswerMeta>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomAnswerMeta {
    /// The field this box belongs to.
    #[serde(default)]
    pub question_id: Option<String>,
}

/// The reply to `elicitation/create` when the user filled the form in.
pub fn elicitation_accepted(content: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "action": "accept", "content": content })
}

/// The user chose not to answer, and the agent should carry on without it.
///
/// Distinct from [`elicitation_cancelled`] on the agent's side: the adapter
/// reads a decline as "the user skipped these questions" and lets the tool call
/// complete with empty answers, while a cancel aborts the call outright.
pub fn elicitation_declined() -> serde_json::Value {
    serde_json::json!({ "action": "decline" })
}

/// Nobody could be asked, or the turn ended while the form was on screen.
pub fn elicitation_cancelled() -> serde_json::Value {
    serde_json::json!({ "action": "cancel" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_a_request_and_a_notification_are_told_apart() {
        let response = r#"{"jsonrpc":"2.0","id":7,"result":{"sessionId":"s1"}}"#;
        let parsed: Incoming = serde_json::from_str(response).unwrap();
        match parsed.classify() {
            Frame::Response { id, result } => {
                assert_eq!(id, 7);
                assert_eq!(result.unwrap()["sessionId"], "s1");
            }
            _ => panic!("expected a response"),
        }

        // The agent asking us something, mid-turn.
        let request = r#"{"jsonrpc":"2.0","id":"a-1","method":"session/request_permission","params":{}}"#;
        let parsed: Incoming = serde_json::from_str(request).unwrap();
        match parsed.classify() {
            Frame::Request { id, method, .. } => {
                assert_eq!(id, serde_json::json!("a-1"), "a string id must survive intact");
                assert_eq!(method, "session/request_permission");
            }
            _ => panic!("expected a request"),
        }

        let notification = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1"}}"#;
        let parsed: Incoming = serde_json::from_str(notification).unwrap();
        assert!(matches!(parsed.classify(), Frame::Notification { .. }));
    }

    /// **`result: null` is a success, and reading it as a missing member hangs
    /// the caller for ever.**
    ///
    /// JSON-RPC requires the `result` member on success and says nothing about
    /// it being non-null; ACP's own `LoadSessionResponse` has no required field
    /// at all, so `null` is a conforming answer to `session/load`. Collapsed
    /// into "absent", the line becomes `Junk`, nothing completes the pending
    /// slot, and a reopen blocks until the process dies.
    #[test]
    fn a_null_result_completes_its_caller_instead_of_being_dropped() {
        let raw = r#"{"jsonrpc":"2.0","id":4,"result":null}"#;
        let parsed: Incoming = serde_json::from_str(raw).unwrap();
        match parsed.classify() {
            Frame::Response { id, result } => {
                assert_eq!(id, 4);
                assert_eq!(result.unwrap(), serde_json::Value::Null);
            }
            _ => panic!("a null result is an answer, not junk"),
        }

        // And an id carrying neither member still is junk — that is what the
        // double option keeps distinguishable.
        let neither: Incoming = serde_json::from_str(r#"{"jsonrpc":"2.0","id":5}"#).unwrap();
        assert!(matches!(neither.classify(), Frame::Junk));
    }

    /// The load reply has no required field, `sessionId` least of all — the
    /// schema's `LoadSessionResponse` is `{ modes?, configOptions?, _meta? }`.
    /// Parsed as a `NewSessionResult` every conforming answer would look like a
    /// failure, and a reopen would silently start a fresh session instead.
    #[test]
    fn a_load_reply_may_say_nothing_at_all() {
        let empty = LoadSessionResult::read(serde_json::json!({})).unwrap();
        assert_eq!(empty.session_id, None);
        assert!(empty.config_options.is_empty());

        let null = LoadSessionResult::read(serde_json::Value::Null).unwrap();
        assert_eq!(null.session_id, None);

        // And the adapter's own over-delivery still parses, id and all.
        let generous = LoadSessionResult::read(serde_json::json!({
            "sessionId": "sess-7-resumed",
            "configOptions": [
                {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "opus"}
            ],
        }))
        .unwrap();
        assert_eq!(generous.session_id.as_deref(), Some("sess-7-resumed"));
        assert_eq!(generous.config_options.len(), 1);
    }

    /// **Both ways of opening a session ask to be shown the thinking**, and the
    /// spelling is the whole of it: `_meta` with the underscore, `claudeCode`
    /// in camel case, and an `extraArgs` key that becomes a command-line flag
    /// verbatim. Every one of those is a place a rename would go unnoticed —
    /// the adapter reads the path with optional chaining, so a wrong one is not
    /// an error, it is a session that quietly shows no reasoning again.
    ///
    /// A load carries it for a reason of its own: a resumed session builds its
    /// query through the same call, so leaving it off there would mean thinking
    /// is shown until the app is restarted and never afterwards.
    ///
    /// **And the second attempt has to leave no trace of the first.** A `claude`
    /// that does not know the flag exits before it runs, so the retry is the
    /// only thing standing between an old binary and no hosted session at all —
    /// which means `_meta` must be *absent* rather than `null`, since an
    /// explicit null is a member the adapter would read.
    #[test]
    fn a_session_asks_for_the_thinking_to_be_displayed_and_can_stop_asking() {
        let expected = serde_json::json!({
            "claudeCode": { "options": { "extraArgs": { "thinking-display": "summarized" } } }
        });

        let new = serde_json::to_value(NewSessionParams {
            cwd: "/work".into(),
            mcp_servers: Vec::new(),
            meta: Some(SessionMeta::default()),
        })
        .unwrap();
        assert_eq!(new["_meta"], expected);

        let load = serde_json::to_value(LoadSessionParams {
            session_id: "sess-7".into(),
            cwd: "/work".into(),
            meta: Some(SessionMeta::default()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(load["_meta"], expected);

        for retry in [
            serde_json::to_value(NewSessionParams {
                cwd: "/work".into(),
                ..Default::default()
            })
            .unwrap(),
            serde_json::to_value(LoadSessionParams {
                session_id: "sess-7".into(),
                cwd: "/work".into(),
                ..Default::default()
            })
            .unwrap(),
        ] {
            assert!(
                retry.get("_meta").is_none(),
                "the retry must not send the member at all, not even as null: {retry}"
            );
        }
    }

    /// An error response is still a response: the pipe is fine, the agent said
    /// no. Treating it as a broken frame would tear down a healthy session.
    #[test]
    fn an_error_response_is_delivered_to_its_caller() {
        let raw = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"no such method"}}"#;
        let parsed: Incoming = serde_json::from_str(raw).unwrap();
        match parsed.classify() {
            Frame::Response { id, result } => {
                assert_eq!(id, 3);
                assert!(result.unwrap_err().contains("no such method"));
            }
            _ => panic!("expected a response"),
        }
    }

    /// The adapter gains update kinds on its own schedule. An unknown one has
    /// to parse, or one new variant ends every turn that sees it.
    #[test]
    fn an_unknown_update_variant_parses_instead_of_failing() {
        let raw = r#"{"sessionId":"s1","update":{"sessionUpdate":"available_commands_update","commands":[]}}"#;
        let n: SessionNotification = serde_json::from_str(raw).unwrap();
        assert!(matches!(n.update, SessionUpdate::Unhandled));
    }

    /// And the same for a content block that is not text.
    #[test]
    fn a_non_text_content_block_parses_and_reports_no_text() {
        let raw = r#"{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk",
            "content":{"type":"image","data":"...","mimeType":"image/png"}}}"#;
        let n: SessionNotification = serde_json::from_str(raw).unwrap();
        match n.update {
            SessionUpdate::AgentMessageChunk { content, .. } => {
                assert_eq!(content.kind, "image");
                assert_eq!(content.as_text(), None);
            }
            _ => panic!("expected an agent message chunk"),
        }
    }

    #[test]
    fn tool_call_and_its_update_share_one_shape() {
        let call = r#"{"sessionId":"s1","update":{"sessionUpdate":"tool_call",
            "toolCallId":"t1","title":"Run npm test","kind":"execute","status":"pending"}}"#;
        let n: SessionNotification = serde_json::from_str(call).unwrap();
        match n.update {
            SessionUpdate::ToolCall(tc) => {
                assert_eq!(tc.tool_call_id, "t1");
                assert_eq!(tc.status.as_deref(), Some("pending"));
            }
            _ => panic!("expected a tool call"),
        }

        // The update carries only what changed; everything else is absent.
        let update = r#"{"sessionId":"s1","update":{"sessionUpdate":"tool_call_update",
            "toolCallId":"t1","status":"completed",
            "content":[{"type":"content","content":{"type":"text","text":"ok"}}]}}"#;
        let n: SessionNotification = serde_json::from_str(update).unwrap();
        match n.update {
            SessionUpdate::ToolCallUpdate(tc) => {
                assert_eq!(tc.tool_call_id, "t1");
                assert!(tc.title.is_none(), "an absent field means unchanged");
                assert_eq!(tc.content[0].content.as_ref().unwrap().as_text(), Some("ok"));
            }
            _ => panic!("expected a tool call update"),
        }
    }

    #[test]
    fn permission_options_are_classified_by_kind() {
        let raw = r#"{"sessionId":"s1","toolCall":{"toolCallId":"t1"},"options":[
            {"optionId":"a","name":"Yes","kind":"allow_once"},
            {"optionId":"b","name":"Always","kind":"allow_always"},
            {"optionId":"c","name":"No","kind":"reject_once"}]}"#;
        let p: RequestPermissionParams = serde_json::from_str(raw).unwrap();

        let allow_once = p.options.iter().find(|o| o.is_allow() && o.is_once()).unwrap();
        assert_eq!(allow_once.option_id, "a");
        let reject_once = p.options.iter().find(|o| o.is_reject() && o.is_once()).unwrap();
        assert_eq!(reject_once.option_id, "c");
        assert!(p.options.iter().any(|o| o.is_allow() && !o.is_once()));
    }

    /// Steering is advertised at the top level of the greeting, beside
    /// `agentCapabilities` rather than inside it. Looking in the wrong place
    /// reads as "not supported" on every adapter that does support it, which
    /// degrades silently — every interjection would become a follow-up and
    /// nobody would know why.
    #[test]
    fn steering_is_advertised_beside_the_capabilities_not_inside_them() {
        let raw = r#"{"protocolVersion":1,"agentCapabilities":{"loadSession":true},
            "_meta":{"steering":{"supported":true},"goal":{"version":1}}}"#;
        let init: InitializeResult = serde_json::from_str(raw).unwrap();
        assert!(init.steering_supported());

        // An adapter that has never heard of the extension.
        let plain = r#"{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}"#;
        let init: InitializeResult = serde_json::from_str(plain).unwrap();
        assert!(!init.steering_supported());

        // And one that mentions it to say no.
        let refused = r#"{"protocolVersion":1,"_meta":{"steering":{"supported":false}}}"#;
        let init: InitializeResult = serde_json::from_str(refused).unwrap();
        assert!(!init.steering_supported());
    }

    /// The `_meta` on the way out is not decoration: without it the agent
    /// starts a *detached* turn when there is nothing to steer, and this app
    /// ends up with a turn it did not open, cannot stop and has no row for.
    #[test]
    fn a_steer_asks_for_the_message_back_rather_than_a_detached_turn() {
        let params = SteerParams::text("s1", "actually, stop");
        let encoded = serde_json::to_value(&params).unwrap();
        assert_eq!(encoded["sessionId"], "s1");
        assert_eq!(encoded["prompt"][0]["text"], "actually, stop");
        assert_eq!(encoded["_meta"]["steering"]["idleBehavior"], "promptRequired");
    }

    /// Every outcome is a success, and they mean three different things. Only
    /// `promptRequired` says the message was not taken.
    #[test]
    fn the_three_steer_outcomes_are_told_apart() {
        let read = |raw: &str| serde_json::from_str::<SteerResult>(raw).unwrap().outcome();
        assert_eq!(read(r#"{"outcome":"injected"}"#), SteerOutcome::Injected);
        assert_eq!(
            read(r#"{"outcome":"promptRequired","reason":"noRunningTurn"}"#),
            SteerOutcome::PromptRequired
        );
        assert_eq!(read(r#"{"outcome":"startedNewTurn"}"#), SteerOutcome::StartedNewTurn);
        assert!(matches!(read(r#"{"outcome":"teleported"}"#), SteerOutcome::Unknown(_)));
    }

    /// `session/list` is advertised inside `agentCapabilities`, and `null` is a
    /// legal way to decline it. Reading that as `Some` would have this client
    /// call a method the agent does not have, on every adapter that spells the
    /// refusal out.
    #[test]
    fn listing_is_advertised_by_presence_and_declined_by_null() {
        let read = |raw: &str| serde_json::from_str::<InitializeResult>(raw).unwrap();

        let yes = read(r#"{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"list":{}}}}"#);
        assert!(yes.agent_capabilities.lists_sessions());

        let explicit_no = read(r#"{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"list":null}}}"#);
        assert!(!explicit_no.agent_capabilities.lists_sessions());

        let silent = read(r#"{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}"#);
        assert!(!silent.agent_capabilities.lists_sessions());
        // And the two capabilities are independent: `loadSession` is top-level
        // by the schema's own admission, so neither implies the other.
        assert!(silent.agent_capabilities.load_session);
    }

    /// A session with no title or timestamp is still a session worth offering.
    /// The adapter drops anything with no `cwd`, so that one is required.
    #[test]
    fn a_listed_session_survives_its_optional_fields_being_absent() {
        let raw = r#"{"sessions":[
            {"sessionId":"s1","cwd":"/work/meridian","title":"Fix the queue","updatedAt":"2026-08-20T11:00:00.000Z"},
            {"sessionId":"s2","cwd":"/work/other"}]}"#;
        let result: ListSessionsResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.sessions.len(), 2);
        assert_eq!(result.sessions[0].title.as_deref(), Some("Fix the queue"));
        assert_eq!(result.sessions[1].title, None);
        assert_eq!(result.sessions[1].updated_at, None);
        assert_eq!(result.next_cursor, None);
    }

    /// Omitted rather than sent as null: `cwd: null` is documented as "every
    /// project", but an agent reading it strictly would see a request to filter
    /// on nothing.
    #[test]
    fn listing_everything_sends_no_filter_at_all() {
        let encoded = serde_json::to_value(ListSessionsParams::default()).unwrap();
        assert_eq!(encoded, serde_json::json!({}));

        let narrowed = serde_json::to_value(ListSessionsParams {
            cwd: Some("/work/meridian".into()),
            cursor: None,
        })
        .unwrap();
        assert_eq!(narrowed, serde_json::json!({ "cwd": "/work/meridian" }));
    }

    /// The field that tells one replayed message from the next. Absent on the
    /// live path's own chunks, which is why it has to be optional.
    #[test]
    fn a_message_chunk_carries_the_id_of_the_message_it_belongs_to() {
        let raw = r#"{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk",
            "messageId":"msg_01ABC","content":{"type":"text","text":"hello"}}}"#;
        let n: SessionNotification = serde_json::from_str(raw).unwrap();
        match n.update {
            SessionUpdate::AgentMessageChunk { message_id, .. } => {
                assert_eq!(message_id.as_deref(), Some("msg_01ABC"));
            }
            other => panic!("expected an agent message chunk, got {other:?}"),
        }

        let live = r#"{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk",
            "content":{"type":"text","text":"hello"}}}"#;
        let n: SessionNotification = serde_json::from_str(live).unwrap();
        match n.update {
            SessionUpdate::AgentMessageChunk { message_id, .. } => assert_eq!(message_id, None),
            other => panic!("expected an agent message chunk, got {other:?}"),
        }
    }

    /// The wire shape of an answer is nested — `outcome.outcome` — and getting
    /// it flat leaves the agent waiting for ever.
    #[test]
    fn a_permission_answer_nests_its_outcome() {
        assert_eq!(
            permission_selected("opt-1"),
            serde_json::json!({"outcome": {"outcome": "selected", "optionId": "opt-1"}})
        );
        assert_eq!(
            permission_cancelled(),
            serde_json::json!({"outcome": {"outcome": "cancelled"}})
        );
    }
}
