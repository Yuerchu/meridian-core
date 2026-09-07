use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct OneBotResponse {
    pub status: Option<String>,
    pub retcode: Option<i32>,
    pub data: Option<serde_json::Value>,
    pub echo: Option<String>,
    /// Why it was refused, when it was. `status` is one of a handful of fixed
    /// words (`ok` / `failed` / `async`) and says nothing a person can act on;
    /// these two carry the actual complaint. Adapters disagree about which to
    /// use — NapCat and LLOneBot send both, go-cqhttp sent `wording` alone —
    /// so a caller reads whichever arrived.
    pub message: Option<String>,
    pub wording: Option<String>,
}

impl OneBotResponse {
    /// The refusal in words, for a log line or a tool result.
    pub fn complaint(&self) -> Option<&str> {
        self.message
            .as_deref()
            .or(self.wording.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

pub enum OneBotFrame {
    Event(OneBotEvent),
    Response(OneBotResponse),
}

pub fn parse_frame(text: &str) -> Option<OneBotFrame> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("post_type").is_some() {
        serde_json::from_value(v).ok().map(OneBotFrame::Event)
    } else if v.get("retcode").is_some() || v.get("echo").is_some() {
        serde_json::from_value(v).ok().map(OneBotFrame::Response)
    } else {
        None
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OneBotEvent {
    pub self_id: Option<i64>,
    pub post_type: String,
    pub message_type: Option<String>,
    pub sub_type: Option<String>,
    pub message_id: Option<i64>,
    pub user_id: Option<i64>,
    pub group_id: Option<i64>,
    pub message: Option<serde_json::Value>,
    pub raw_message: Option<String>,
    pub sender: Option<Sender>,
    pub meta_event_type: Option<String>,
    // notice events
    pub notice_type: Option<String>,
    pub target_id: Option<i64>,
    pub operator_id: Option<i64>,
    // request events
    pub request_type: Option<String>,
    pub comment: Option<String>,
    pub flag: Option<String>,
    pub via: Option<String>,
    pub invitor_id: Option<i64>,
    pub source_group_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Sender {
    pub nickname: Option<String>,
    pub card: Option<String>,
    pub role: Option<String>,
    /// The group's bespoke honorific for this member, when one was awarded.
    /// Often a joke or a standing the room granted, which reads very differently
    /// from the flat `role` — worth passing on for that reason.
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSegment {
    #[serde(rename = "type")]
    pub seg_type: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct OneBotAction {
    pub action: String,
    pub params: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echo: Option<String>,
}

impl OneBotAction {
    /// Give an action an echo so its response can be matched back to it.
    ///
    /// The send helpers below leave it off, because almost nothing needs to
    /// hear back from a message it sent. An approval prompt does: what comes
    /// back names the message the answer will have to quote.
    pub fn with_echo(mut self, echo: String) -> Self {
        self.echo = Some(echo);
        self
    }

    pub fn get_msg(message_id: i64, echo: String) -> Self {
        Self {
            action: "get_msg".into(),
            params: serde_json::json!({ "message_id": message_id }),
            echo: Some(echo),
        }
    }

    /// Exchange a merged-forward handle for the messages inside it. The id is a
    /// string on the wire even where it is all digits, and adapters disagree on
    /// which parameter carries it — `id` is the OneBot 11 name, `message_id`
    /// what several implementations actually read, so both are sent.
    pub fn get_forward_msg(id: &str, echo: String) -> Self {
        Self {
            action: "get_forward_msg".into(),
            params: serde_json::json!({ "id": id, "message_id": id }),
            echo: Some(echo),
        }
    }

    pub fn send_private_msg(user_id: i64, message: Vec<MessageSegment>) -> Self {
        Self {
            action: "send_private_msg".into(),
            params: serde_json::json!({
                "user_id": user_id,
                "message": message,
            }),
            echo: None,
        }
    }

    pub fn send_group_msg(group_id: i64, message: Vec<MessageSegment>) -> Self {
        Self {
            action: "send_group_msg".into(),
            params: serde_json::json!({
                "group_id": group_id,
                "message": message,
            }),
            echo: None,
        }
    }

    /// Private-chat only: shows the "typing…" indicator (event_type 1 = typing).
    pub fn set_input_status(user_id: i64, event_type: i32) -> Self {
        Self {
            action: "set_input_status".into(),
            params: serde_json::json!({
                "user_id": user_id,
                "event_type": event_type,
            }),
            echo: None,
        }
    }

    /// Group-chat only: reacts to a message with a QQ emoji.
    pub fn set_msg_emoji_like(message_id: i64, emoji_id: &str) -> Self {
        let emoji: serde_json::Value = match emoji_id.parse::<i64>() {
            Ok(n) => n.into(),
            Err(_) => emoji_id.into(),
        };
        Self {
            action: "set_msg_emoji_like".into(),
            params: serde_json::json!({
                "message_id": message_id,
                "emoji_id": emoji,
                "set": true,
            }),
            echo: None,
        }
    }

    pub fn voice_msg_to_text(message_id: i64, echo: String) -> Self {
        Self {
            action: "voice_msg_to_text".into(),
            params: serde_json::json!({ "message_id": message_id }),
            echo: Some(echo),
        }
    }

    pub fn ocr_image(image: &str, echo: String) -> Self {
        Self {
            action: "ocr_image".into(),
            params: serde_json::json!({ "image": image }),
            echo: Some(echo),
        }
    }

    pub fn get_group_msg_history(group_id: i64, message_seq: Option<i64>, count: i64, echo: String) -> Self {
        let mut params = serde_json::json!({ "group_id": group_id, "count": count });
        if let Some(seq) = message_seq {
            params["message_seq"] = seq.into();
        }
        Self {
            action: "get_group_msg_history".into(),
            params,
            echo: Some(echo),
        }
    }

    pub fn get_friend_msg_history(user_id: i64, message_seq: Option<i64>, count: i64, echo: String) -> Self {
        let mut params = serde_json::json!({ "user_id": user_id, "count": count });
        if let Some(seq) = message_seq {
            params["message_seq"] = seq.into();
        }
        Self {
            action: "get_friend_msg_history".into(),
            params,
            echo: Some(echo),
        }
    }

    pub fn set_friend_add_request(flag: &str, approve: bool, remark: Option<&str>, echo: String) -> Self {
        let mut params = serde_json::json!({ "flag": flag, "approve": approve });
        if let Some(r) = remark.filter(|r| !r.is_empty()) {
            params["remark"] = r.into();
        }
        Self {
            action: "set_friend_add_request".into(),
            params,
            echo: Some(echo),
        }
    }

    pub fn set_group_add_request(
        flag: &str,
        sub_type: &str,
        approve: bool,
        reason: Option<&str>,
        echo: String,
    ) -> Self {
        let mut params = serde_json::json!({ "flag": flag, "sub_type": sub_type, "approve": approve });
        if let Some(r) = reason.filter(|r| !r.is_empty()) {
            params["reason"] = r.into();
        }
        Self {
            action: "set_group_add_request".into(),
            params,
            echo: Some(echo),
        }
    }

    pub fn get_group_info(group_id: i64, echo: String) -> Self {
        Self {
            action: "get_group_info".into(),
            params: serde_json::json!({ "group_id": group_id }),
            echo: Some(echo),
        }
    }

    pub fn get_group_member_list(group_id: i64, echo: String) -> Self {
        Self {
            action: "get_group_member_list".into(),
            params: serde_json::json!({ "group_id": group_id }),
            echo: Some(echo),
        }
    }

    pub fn get_group_member_info(group_id: i64, user_id: i64, echo: String) -> Self {
        Self {
            action: "get_group_member_info".into(),
            params: serde_json::json!({ "group_id": group_id, "user_id": user_id }),
            echo: Some(echo),
        }
    }

    pub fn get_stranger_info(user_id: i64, echo: String) -> Self {
        Self {
            action: "get_stranger_info".into(),
            params: serde_json::json!({ "user_id": user_id }),
            echo: Some(echo),
        }
    }

    pub fn get_friend_list(echo: String) -> Self {
        Self {
            action: "get_friend_list".into(),
            params: serde_json::json!({}),
            echo: Some(echo),
        }
    }

    pub fn get_group_list(echo: String) -> Self {
        Self {
            action: "get_group_list".into(),
            params: serde_json::json!({}),
            echo: Some(echo),
        }
    }

    pub fn delete_msg(message_id: i64, echo: String) -> Self {
        Self {
            action: "delete_msg".into(),
            params: serde_json::json!({ "message_id": message_id }),
            echo: Some(echo),
        }
    }

    /// llbot extension; pokes `user_id` in `group_id` when given, else in private.
    pub fn send_poke(user_id: i64, group_id: Option<i64>, echo: String) -> Self {
        let mut params = serde_json::json!({ "user_id": user_id });
        if let Some(g) = group_id {
            params["group_id"] = g.into();
        }
        Self {
            action: "send_poke".into(),
            params,
            echo: Some(echo),
        }
    }

    pub fn send_like(user_id: i64, times: i64, echo: String) -> Self {
        Self {
            action: "send_like".into(),
            params: serde_json::json!({ "user_id": user_id, "times": times }),
            echo: Some(echo),
        }
    }

    pub fn set_essence_msg(message_id: i64, echo: String) -> Self {
        Self {
            action: "set_essence_msg".into(),
            params: serde_json::json!({ "message_id": message_id }),
            echo: Some(echo),
        }
    }

    /// `duration` in seconds; 0 lifts the ban.
    pub fn set_group_ban(group_id: i64, user_id: i64, duration: i64, echo: String) -> Self {
        Self {
            action: "set_group_ban".into(),
            params: serde_json::json!({
                "group_id": group_id,
                "user_id": user_id,
                "duration": duration,
            }),
            echo: Some(echo),
        }
    }

    pub fn set_group_kick(group_id: i64, user_id: i64, echo: String) -> Self {
        Self {
            action: "set_group_kick".into(),
            params: serde_json::json!({ "group_id": group_id, "user_id": user_id }),
            echo: Some(echo),
        }
    }

    pub fn set_group_card(group_id: i64, user_id: i64, card: &str, echo: String) -> Self {
        Self {
            action: "set_group_card".into(),
            params: serde_json::json!({ "group_id": group_id, "user_id": user_id, "card": card }),
            echo: Some(echo),
        }
    }

    pub fn set_group_name(group_id: i64, name: &str, echo: String) -> Self {
        Self {
            action: "set_group_name".into(),
            params: serde_json::json!({ "group_id": group_id, "group_name": name }),
            echo: Some(echo),
        }
    }

    pub fn send_group_notice(group_id: i64, content: &str, echo: String) -> Self {
        Self {
            action: "_send_group_notice".into(),
            params: serde_json::json!({ "group_id": group_id, "content": content }),
            echo: Some(echo),
        }
    }
}

impl MessageSegment {
    pub fn raw(seg_type: &str, data: serde_json::Value) -> Self {
        Self {
            seg_type: seg_type.into(),
            data,
        }
    }

    pub fn text(text: &str) -> Self {
        Self {
            seg_type: "text".into(),
            data: serde_json::json!({ "text": text }),
        }
    }

    pub fn at(user_id: i64) -> Self {
        Self {
            seg_type: "at".into(),
            data: serde_json::json!({ "qq": user_id.to_string() }),
        }
    }

    pub fn reply(message_id: i64) -> Self {
        Self {
            seg_type: "reply".into(),
            data: serde_json::json!({ "id": message_id.to_string() }),
        }
    }

    pub fn image(file: &str) -> Self {
        Self::raw("image", serde_json::json!({ "file": file }))
    }

    /// 一条语音消息。
    ///
    /// 适配器要把它转成 SILK，所以 `file` 里放什么它未必都收——`base64://` 是
    /// 这里唯一用的形式，和贴纸的图片走同一条路。
    pub fn record(file: &str) -> Self {
        Self::raw("record", serde_json::json!({ "file": file }))
    }
}

#[cfg(test)]
mod tests {
    use super::{OneBotFrame, parse_frame};

    #[test]
    fn test_parse_notice_event_fields() {
        let json = r#"{
            "post_type": "notice", "notice_type": "notify", "sub_type": "poke",
            "time": 1700000000, "self_id": 10001, "user_id": 20002,
            "target_id": 10001, "group_id": 30003
        }"#;
        let Some(OneBotFrame::Event(e)) = parse_frame(json) else {
            panic!("expected event frame");
        };
        assert_eq!(e.post_type, "notice");
        assert_eq!(e.notice_type.as_deref(), Some("notify"));
        assert_eq!(e.sub_type.as_deref(), Some("poke"));
        assert_eq!(e.target_id, Some(10001));
        assert_eq!(e.group_id, Some(30003));
    }

    #[test]
    fn test_parse_recall_notice() {
        let json = r#"{
            "post_type": "notice", "notice_type": "group_recall",
            "time": 1700000000, "self_id": 10001, "user_id": 20002,
            "operator_id": 40004, "group_id": 30003, "message_id": 555
        }"#;
        let Some(OneBotFrame::Event(e)) = parse_frame(json) else {
            panic!("expected event frame");
        };
        assert_eq!(e.notice_type.as_deref(), Some("group_recall"));
        assert_eq!(e.operator_id, Some(40004));
        assert_eq!(e.message_id, Some(555));
    }

    #[test]
    fn test_history_action_includes_message_seq() {
        let with_seq = super::OneBotAction::get_group_msg_history(1, Some(99), 20, "e".into());
        assert_eq!(with_seq.params["message_seq"], 99);
        let without = super::OneBotAction::get_group_msg_history(1, None, 20, "e".into());
        assert!(without.params.get("message_seq").is_none());
    }

    #[test]
    fn test_send_poke_group_and_private() {
        let group = super::OneBotAction::send_poke(2, Some(3), "e".into());
        assert_eq!(group.params["group_id"], 3);
        let private = super::OneBotAction::send_poke(2, None, "e".into());
        assert!(private.params.get("group_id").is_none());
    }
}
