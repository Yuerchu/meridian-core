//! What a Codex-shaped request says about the turn it belongs to.
//!
//! Pure data model and JSON serializer; reading install-level preferences from
//! SQLite lives in `meridian-core`.

use serde::Serialize;

use crate::chat::ChatParams;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexRequestKind {
    Turn,
    Compaction,
    Review,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexThreadSource {
    User,
    Channel,
    Hook,
    SubAgent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexTurnMetadata {
    pub installation_id: String,
    pub turn_id: Option<String>,
    pub request_kind: CodexRequestKind,
    pub thread_source: CodexThreadSource,
    pub turn_started_at_unix_ms: Option<i64>,
    pub sandbox: Option<String>,
    pub workspace_kind: Option<&'static str>,
    pub auto_review_enabled: bool,
}

impl CodexTurnMetadata {
    /// The turn this request belongs to, once there is one.
    pub fn in_turn(mut self, turn_id: &str, started_at_unix_ms: i64, in_project: bool) -> Self {
        self.turn_id = Some(turn_id.to_string());
        self.turn_started_at_unix_ms = Some(started_at_unix_ms);
        self.workspace_kind = in_project.then_some("project");
        self
    }

    /// The JSON Codex puts in `x-codex-turn-metadata`, with this request's own
    /// ids merged in.
    pub fn to_json(&self, session_id: &str, thread_id: Option<&str>, params: &ChatParams) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        let mut put = |key: &str, value: serde_json::Value| {
            map.insert(key.to_string(), value);
        };

        put("installation_id", self.installation_id.as_str().into());
        put("session_id", session_id.into());
        if let Some(thread) = thread_id {
            put("thread_id", thread.into());
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
        assert_eq!(json["window_id"], "conv-1:0");
        assert_eq!(json["window_number"], 0);
        assert_eq!(json["turn_id"], "turn-1");
        assert_eq!(json["request_kind"], "turn");
        assert_eq!(json["thread_source"], "user");
        assert_eq!(json["sandbox"], "container");
        assert_eq!(json["workspace_kind"], "project");
        assert_eq!(json["auto_review_enabled"], true);
        assert_eq!(json["model"], "gpt-6-astra");
        assert_eq!(json["reasoning_effort"], "high");
        assert_eq!(json["analytics_enabled"], false);
    }

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
            thread_source: CodexThreadSource::Channel,
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
            "node_repl_disabled",
            "node_repl_auto_review_required",
            "context_window_id",
            "root_turn_id",
        ] {
            assert!(json.get(absent).is_none(), "{absent} was invented: {json}");
        }
        assert_eq!(json["request_kind"], "compaction");
        assert_eq!(json["thread_source"], "channel");
    }
}
