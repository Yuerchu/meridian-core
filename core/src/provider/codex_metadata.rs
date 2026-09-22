//! What a Codex-shaped request says about the turn it belongs to.
//!
//! Codex sends this as `x-codex-turn-metadata`, a JSON object beside
//! `x-codex-installation-id` and `x-codex-window-id`
//! (`core/src/responses_metadata.rs`). A measured capture of a real client
//! reaching a relayed backend carried, among others: `installation_id`,
//! `session_id`, `thread_id`, `turn_id`, `window_id`, `window_number`,
//! `request_kind`, `thread_source`, `turn_trigger`, `sandbox`, `sandbox_mode`,
//! `auto_review_enabled`, `model`, `reasoning_effort`, `analytics_enabled`,
//! `workspace_kind` and `turn_started_at_unix_ms`.
//!
//! **Every field here is our own answer, never Codex's.** That is the whole
//! rule of this module. The header exists so the other end knows what kind of
//! request this is; a fabricated `sandbox: "windows_elevated"` on a turn that
//! ran unsandboxed is worse than no field at all, because it is the sort of
//! claim something downstream may act on. Where this app has no equivalent —
//! Codex's node REPL flags, its context-window id, its fork lineage — the key
//! is simply absent, which is also what Codex does when it has no answer.
//!
//! The exception that proves it: `analytics_enabled` is always `false`, because
//! this app has no analytics. That is a real value that happens to be constant.

use serde::Serialize;

use crate::provider::ChatParams;

/// Why this request is being made, in Codex's vocabulary.
///
/// Codex separates `turn` from its background passes, and so does this app —
/// `UsageDimension::Kind` already distinguishes answering the user from
/// summarising, naming and reviewing. Mapping one onto the other is what stops
/// a summariser's request from being reported as a turn the user took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexRequestKind {
    /// A turn the user is waiting on.
    Turn,
    /// History being summarised, local or remote.
    Compaction,
    /// A model looking at something on this app's own behalf: the automatic
    /// approval reviewer, the hook reviewer.
    Review,
    /// Everything else this app asks for without being asked: a title, an
    /// extraction pass.
    Background,
}

/// Where the conversation came from.
///
/// Ours, not Codex's: a QQ group and a hosted ACP session are things Codex has
/// no word for, and inventing `user` for them would say a person typed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexThreadSource {
    /// Somebody typed it in this app.
    User,
    /// A OneBot/QQ message.
    Onebot,
    /// One of the loopback hook gates.
    Hook,
    /// A delegated run this app started for itself.
    SubAgent,
}

/// Everything a Codex-shaped request can honestly say about its turn.
///
/// Assembled where the answers actually live — `commands::chat` and the
/// headless runners know the turn id and why they are asking; `resolve_turn_params`
/// knows the sandbox and whether the automatic reviewer is on. Carried on
/// [`ChatParams`] rather than threaded through every adapter, because only one
/// adapter reads it and only under one switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexTurnMetadata {
    /// Stable per install, ours, and not a fingerprint we invent per request —
    /// see `agent::codex_install::installation_id`.
    pub installation_id: String,
    /// This app's turn id. `None` for a request made outside a turn.
    pub turn_id: Option<String>,
    pub request_kind: CodexRequestKind,
    pub thread_source: CodexThreadSource,
    /// When the turn started, not when this round was built: a turn is however
    /// many requests it took, and Codex's field means the former.
    pub turn_started_at_unix_ms: Option<i64>,
    /// How commands in this conversation are confined, in this app's own words
    /// (`sandbox.enabled`: `auto`, `container`, `off`). Absent where nothing has
    /// been decided rather than reported as unconfined.
    pub sandbox: Option<String>,
    /// Whether the conversation belongs to a project.
    pub workspace_kind: Option<&'static str>,
    /// Whether the automatic approval reviewer will answer this turn's
    /// approvals (`autoreview.enabled` *and* a model named).
    pub auto_review_enabled: bool,
}

impl CodexTurnMetadata {
    /// What this install can say about itself, before a turn adds its own ids.
    ///
    /// Reads three preferences, all of them ours: the installation id, how
    /// commands are confined, and whether the automatic reviewer answers
    /// approvals. A read that fails leaves the field absent rather than
    /// guessing — see the module note.
    ///
    /// `None` when there is no installation id to be had, because
    /// `installation_id` is the one field Codex always sends and a metadata
    /// object without it describes no installation at all.
    pub fn for_install(
        conn: &mut diesel::sqlite::SqliteConnection,
        request_kind: CodexRequestKind,
        thread_source: CodexThreadSource,
    ) -> Option<Self> {
        let installation_id = crate::agent::codex_install::installation_id(conn)?;
        let mut read = |key: &str| crate::db::ops::preference::get_preference(conn, key).ok().flatten();
        Some(Self {
            installation_id,
            turn_id: None,
            request_kind,
            thread_source,
            turn_started_at_unix_ms: None,
            sandbox: read("sandbox.enabled"),
            workspace_kind: None,
            // The same conjunction `AutoReviewed::wrap` uses: the switch alone
            // leaves an inert wrapper, so a turn with no model named is not
            // reviewed and must not say it is.
            auto_review_enabled: read("autoreview.enabled").is_some_and(|value| value == "1" || value == "true")
                && read("autoreview.model").is_some_and(|model| !model.trim().is_empty()),
        })
    }

    /// The turn this request belongs to, once there is one.
    pub fn in_turn(mut self, turn_id: &str, started_at_unix_ms: i64, in_project: bool) -> Self {
        self.turn_id = Some(turn_id.to_string());
        self.turn_started_at_unix_ms = Some(started_at_unix_ms);
        self.workspace_kind = in_project.then_some("project");
        self
    }

    /// The JSON Codex puts in `x-codex-turn-metadata`, with this request's own
    /// ids merged in.
    ///
    /// `session` and `thread` come from the adapter rather than from this
    /// struct: the session is the provider instance's and the thread is the
    /// conversation, and both are already in the adapter's hands. Passing them
    /// through here would be a second place they could disagree.
    pub fn to_json(&self, session_id: &str, thread_id: Option<&str>, params: &ChatParams) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        let mut put = |key: &str, value: serde_json::Value| {
            map.insert(key.to_string(), value);
        };

        put("installation_id", self.installation_id.as_str().into());
        put("session_id", session_id.into());
        if let Some(thread) = thread_id {
            put("thread_id", thread.into());
            // Codex's own form is `{thread}:{n}`, and `window_number` is the
            // index within it. This app has one view of a conversation, so the
            // number is 0 and the id follows from the thread — not a second
            // identifier to keep in step.
            put("window_id", format!("{thread}:0").into());
            put("window_number", 0.into());
        }
        if let Some(turn) = self.turn_id.as_deref() {
            put("turn_id", turn.into());
        }
        put(
            "request_kind",
            serde_json::to_value(self.request_kind).expect("unit enum"),
        );
        put(
            "thread_source",
            serde_json::to_value(self.thread_source).expect("unit enum"),
        );
        if let Some(started) = self.turn_started_at_unix_ms {
            put("turn_started_at_unix_ms", started.into());
        }
        if let Some(sandbox) = self.sandbox.as_deref() {
            put("sandbox", sandbox.into());
        }
        if let Some(kind) = self.workspace_kind {
            put("workspace_kind", kind.into());
        }
        put("auto_review_enabled", self.auto_review_enabled.into());
        put("model", params.model.as_str().into());
        if let Some(effort) = params.thinking_effort.as_deref() {
            put("reasoning_effort", effort.into());
        }
        // A real value that happens to be constant: this app collects none.
        put("analytics_enabled", false.into());

        serde_json::Value::Object(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> CodexTurnMetadata {
        CodexTurnMetadata {
            installation_id: "install-1".into(),
            turn_id: Some("turn-1".into()),
            request_kind: CodexRequestKind::Turn,
            thread_source: CodexThreadSource::User,
            turn_started_at_unix_ms: Some(1_700_000_000_000),
            sandbox: Some("container".into()),
            workspace_kind: Some("project"),
            auto_review_enabled: true,
        }
    }

    #[test]
    fn the_metadata_carries_this_requests_own_ids() {
        let params = ChatParams {
            model: "gpt-6-astra".into(),
            thinking_effort: Some("high".into()),
            ..Default::default()
        };
        let json = metadata().to_json("sess-1", Some("conv-1"), &params);

        assert_eq!(json["installation_id"], "install-1");
        assert_eq!(json["session_id"], "sess-1");
        assert_eq!(json["thread_id"], "conv-1");
        assert_eq!(json["window_id"], "conv-1:0", "Codex's own `{{thread}}:{{n}}` form");
        assert_eq!(json["window_number"], 0);
        assert_eq!(json["turn_id"], "turn-1");
        assert_eq!(json["request_kind"], "turn");
        assert_eq!(json["thread_source"], "user");
        assert_eq!(json["sandbox"], "container");
        assert_eq!(json["workspace_kind"], "project");
        assert_eq!(json["auto_review_enabled"], true);
        assert_eq!(json["model"], "gpt-6-astra");
        assert_eq!(json["reasoning_effort"], "high");
        assert_eq!(json["analytics_enabled"], false, "this app collects none");
    }

    /// **An unknown answer is an absent key, never a plausible one.** A
    /// fabricated `sandbox` on a turn that ran unconfined is a claim something
    /// downstream may act on, and it is the exact failure this module's rule
    /// exists to prevent.
    #[test]
    fn nothing_this_app_cannot_answer_is_invented() {
        let params = ChatParams {
            model: "gpt-6-astra".into(),
            ..Default::default()
        };
        let json = CodexTurnMetadata {
            installation_id: "install-1".into(),
            turn_id: None,
            request_kind: CodexRequestKind::Compaction,
            thread_source: CodexThreadSource::Onebot,
            turn_started_at_unix_ms: None,
            sandbox: None,
            workspace_kind: None,
            auto_review_enabled: false,
        }
        .to_json("sess-1", None, &params);

        for absent in [
            "thread_id",
            "window_id",
            "window_number",
            "turn_id",
            "turn_started_at_unix_ms",
            "sandbox",
            "workspace_kind",
            "reasoning_effort",
            // Codex's, with no equivalent here at all.
            "node_repl_disabled",
            "node_repl_auto_review_required",
            "context_window_id",
            "root_turn_id",
        ] {
            assert!(json.get(absent).is_none(), "{absent} was invented: {json}");
        }
        assert_eq!(json["request_kind"], "compaction");
        assert_eq!(json["thread_source"], "onebot", "a QQ message is not a person typing");
    }
}
