use std::sync::LazyLock;

use regex::Regex;

use super::protocol::MessageSegment;

const MAX_MSG_LEN: usize = 4000;

/// Private-use sentinels standing in for image/voice placeholders inside parsed
/// text. Users cannot type these, so media processing can split on them without
/// colliding with a literal "[图片]"/"[语音]" the user actually wrote.
pub const IMAGE_SENTINEL: char = '\u{E000}';
pub const RECORD_SENTINEL: char = '\u{E001}';
pub const STICKER_SENTINEL: char = '\u{E002}';
/// A merged-forward bubble, which cannot be resolved without a second API call
/// (`get_forward_msg`) and so is a sentinel for the same reason the others are:
/// the async layer replaces it by position once the contents arrive.
pub const FORWARD_SENTINEL: char = '\u{E003}';

static AT_MENTION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[@[^(\]]*\((\d+)\)\]").unwrap());
static CQ_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[CQ:([A-Za-z0-9_-]+)((?:,[^\]]*)?)\]").unwrap());
static XML_BRIEF_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"brief\s*=\s*"([^"]*)""#).unwrap());
static XML_TITLE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<title[^>]*>([^<]*)</title>").unwrap());

fn decode_cq(value: &str) -> String {
    value
        .replace("&#91;", "[")
        .replace("&#93;", "]")
        .replace("&#44;", ",")
        .replace("&amp;", "&")
}

fn cq_to_segments(message: &str) -> Vec<serde_json::Value> {
    let mut segments = Vec::new();
    let mut cursor = 0;
    for found in CQ_RE.captures_iter(message) {
        let whole = found.get(0).unwrap();
        if whole.start() > cursor {
            segments.push(serde_json::json!({
                "type": "text",
                "data": { "text": decode_cq(&message[cursor..whole.start()]) }
            }));
        }
        let mut data = serde_json::Map::new();
        for pair in found
            .get(2)
            .map(|value| value.as_str())
            .unwrap_or("")
            .trim_start_matches(',')
            .split(',')
        {
            if let Some((key, value)) = pair.split_once('=') {
                data.insert(key.to_string(), serde_json::Value::String(decode_cq(value)));
            }
        }
        segments.push(serde_json::json!({ "type": &found[1], "data": data }));
        cursor = whole.end();
    }
    if cursor < message.len() {
        segments.push(serde_json::json!({
            "type": "text",
            "data": { "text": decode_cq(&message[cursor..]) }
        }));
    }
    segments
}

/// Read a string field off a segment's `data`, tolerating the number an adapter
/// sometimes sends where the spec says string (ids and sizes, mostly).
fn seg_str(data: Option<&serde_json::Value>, key: &str) -> Option<String> {
    data.and_then(|d| d.get(key)).and_then(|v| {
        v.as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| v.as_i64().map(|number| number.to_string()))
    })
}

/// How much of a card's description survives into the transcript. These run to
/// several hundred characters of marketing copy on a shared article, and what
/// the reader needs from one is what it is about.
const CARD_DESC_CHARS: usize = 80;

fn truncate_chars(value: &str, limit: usize) -> String {
    let mut out: String = value.chars().take(limit).collect();
    if out.chars().count() < value.chars().count() {
        out.push('…');
    }
    out
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Render a structured-message card — a shared link, a mini-app, a music post —
/// as the one line a person would see of it.
///
/// The payload is a JSON document inside a JSON string, and its shape is set by
/// whichever app built it: `com.tencent.structmsg` files the interesting part
/// under `meta.news`, the mini-app one under `meta.detail_1`, and there are more
/// of these than are worth enumerating. So the first object under `meta` is what
/// gets read, and `prompt` — which every one of them carries, being what QQ
/// itself shows in the conversation list — is the fallback.
fn render_json_card(data: Option<&serde_json::Value>) -> String {
    let payload = match data.and_then(|d| d.get("data")) {
        Some(serde_json::Value::String(raw)) => serde_json::from_str::<serde_json::Value>(raw).ok(),
        Some(value @ serde_json::Value::Object(_)) => Some(value.clone()),
        _ => None,
    };
    let Some(payload) = payload else {
        return "[卡片]".to_string();
    };

    let detail = payload
        .get("meta")
        .and_then(|meta| meta.as_object())
        .and_then(|meta| meta.values().find(|value| value.is_object()));
    let field = |key: &str| -> Option<&str> {
        detail
            .and_then(|d| d.get(key))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };

    let prompt = payload
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let title = field("title").or(prompt);
    let desc = field("desc").or_else(|| field("summary"));
    let url = ["jumpUrl", "qqdocurl", "jump_url", "url", "musicUrl"]
        .iter()
        .find_map(|key| field(key));

    let mut out = String::from("[卡片");
    if let Some(title) = title {
        out.push_str(&format!(": {}", truncate_chars(title, CARD_DESC_CHARS)));
    }
    if let Some(desc) = desc.filter(|d| Some(*d) != title) {
        out.push_str(&format!(" — {}", truncate_chars(desc, CARD_DESC_CHARS)));
    }
    if let Some(url) = url {
        out.push_str(&format!(" ({url})"));
    }
    out.push(']');
    out
}

/// The XML flavour of the same thing, from before cards were JSON. Parsing XML
/// properly to recover one line of it is not worth a dependency: every one of
/// these carries a `brief` attribute, which is exactly that line.
fn render_xml_card(data: Option<&serde_json::Value>) -> String {
    let Some(raw) = data.and_then(|d| d.get("data")).and_then(|v| v.as_str()) else {
        return "[卡片]".to_string();
    };
    let brief = XML_BRIEF_RE
        .captures(raw)
        .or_else(|| XML_TITLE_RE.captures(raw))
        .and_then(|caps| caps.get(1))
        .map(|m| decode_cq(m.as_str()).replace("&quot;", "\"").replace("&#39;", "'"))
        .map(|s| truncate_chars(s.trim(), CARD_DESC_CHARS))
        .filter(|s| !s.is_empty());
    match brief {
        Some(brief) => format!("[卡片: {brief}]"),
        None => "[卡片]".to_string(),
    }
}

/// Where a piece of media can be fetched from.
///
/// `file` is whatever the adapter called it, which is not always a name: some
/// send `base64://…` in that field rather than a filename, and some send an
/// http URL there instead of in `url`. Both spellings are the fetcher's problem,
/// not the parser's — this type carries what arrived and nothing more.
#[derive(Debug, Clone, Default)]
pub struct MediaRef {
    pub url: Option<String>,
    pub file: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StickerRef {
    pub source: &'static str,
    pub source_key: Option<String>,
    pub native_payload: serde_json::Value,
    pub url: Option<String>,
    pub file: Option<String>,
    pub summary: Option<String>,
}

/// A merged-forward bubble. Some adapters inline the messages in the segment
/// (`content`), most give only an id that has to be exchanged for them.
#[derive(Debug, Clone, Default)]
pub struct ForwardRef {
    pub id: Option<String>,
    pub inline: Option<serde_json::Value>,
}

/// Parsed OneBot message: plain text (with placeholders) plus media references.
#[derive(Debug, Clone, Default)]
pub struct ParsedMessage {
    pub text: String,
    /// What the person actually typed, with everything we stood in for them
    /// left out — no sentinels, no `[图片]`, no `[表情]`.
    ///
    /// `text` cannot answer this. By the time it exists a picture has become
    /// either a private-use codepoint or the literal characters `[图片]`, and
    /// neither is distinguishable from something the user wrote. That does not
    /// matter where the whole message is context, which is most places; it
    /// matters wherever the message is being read as an answer, because a
    /// sticker sent while a tool waits is not a yes, not a reason, and not a
    /// reply to a question.
    pub typed: String,
    pub images: Vec<MediaRef>,
    pub stickers: Vec<StickerRef>,
    /// Filled by the capture layer in the same order as `stickers`.
    pub sticker_ids: Vec<Option<String>>,
    /// Merged-forward bubbles, in the same order as `FORWARD_SENTINEL` appears
    /// in `text`. Resolved by `quote::expand_forwards`, which is async.
    pub forwards: Vec<ForwardRef>,
    /// Voice notes, in the same order as `RECORD_SENTINEL` appears in `text` —
    /// the same rule `images` and `forwards` follow.
    ///
    /// This was a bare `bool` until the corpus work: the segment's `url` and
    /// `file` were read by nobody, so every voice note that arrived had its
    /// audio thrown away and only its transcript kept. A `bool` cannot say
    /// where the audio is, and it cannot say that there were two of them.
    pub records: Vec<MediaRef>,
}

impl ParsedMessage {
    /// A message that arrived as text and nothing else, so all of it was typed.
    pub fn from_text(text: &str) -> Self {
        Self {
            text: text.to_string(),
            typed: text.to_string(),
            ..Default::default()
        }
    }

    pub fn has_media(&self) -> bool {
        !self.images.is_empty() || !self.stickers.is_empty() || !self.forwards.is_empty() || !self.records.is_empty()
    }
}

/// Extract plain text from OneBot message segments (array format).
/// Strips @bot mentions when `self_id` is provided. Media sentinels are restored
/// to human-readable "[图片]"/"[语音]" for display paths.
pub fn segments_to_text(message: &serde_json::Value, self_id: Option<i64>) -> String {
    restore_sentinels(&parse_segments(message, self_id).text)
}

/// Turn the private-use placeholders back into something a person can read.
///
/// This is the *last* thing done to a piece of text, and doing it earlier is
/// how a quoted sticker used to become the five literal characters `[动画表情]`
/// — see `quote::fetch`.
pub fn restore_sentinels(text: &str) -> String {
    text.replace(IMAGE_SENTINEL, "[图片]")
        .replace(RECORD_SENTINEL, "[语音]")
        .replace(STICKER_SENTINEL, "[动画表情]")
        .replace(FORWARD_SENTINEL, "[聊天记录]")
}

/// Parse OneBot message segments into text + media references.
/// Image, voice, and sticker segments become private-use sentinels so downstream
/// media merging can align them by position; use `segments_to_text` when a
/// human-readable string is needed instead. Array and CQ-string messages share
/// the same extraction path.
pub fn parse_segments(message: &serde_json::Value, self_id: Option<i64>) -> ParsedMessage {
    let segments = match message.as_array() {
        Some(arr) => arr,
        None => {
            let Some(raw) = message.as_str() else {
                return ParsedMessage::default();
            };
            let cq = cq_to_segments(raw);
            if cq.is_empty() {
                return ParsedMessage::from_text(raw);
            }
            return parse_segments(&serde_json::Value::Array(cq), self_id);
        }
    };

    let mut parsed = ParsedMessage::default();
    let mut text = String::new();
    // Built alongside rather than filtered out of `text` afterwards: once a
    // placeholder is in there it is just characters, and `[图片]` is a string a
    // person can type.
    let mut typed = String::new();
    for seg in segments {
        let seg_type = seg.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let data = seg.get("data");
        match seg_type {
            "text" => {
                if let Some(t) = data.and_then(|d| d.get("text")).and_then(|v| v.as_str()) {
                    text.push_str(t);
                    typed.push_str(t);
                }
            }
            "at" => {
                let qq_id: Option<i64> = data
                    .and_then(|d| d.get("qq"))
                    .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_i64()));
                if let (Some(sid), Some(qid)) = (self_id, qq_id)
                    && sid == qid
                {
                    continue;
                }
                if let Some(qid) = qq_id {
                    let name = data.and_then(|d| d.get("name")).and_then(|v| v.as_str());
                    match name {
                        Some(n) if !n.is_empty() => text.push_str(&format!("[@{}({})]", n, qid)),
                        _ => text.push_str(&format!("[@{}]", qid)),
                    }
                }
            }
            "image" => {
                let get_str = |key: &str| seg_str(data, key);
                let summary = get_str("summary");
                let emoji_id = get_str("emoji_id");
                let package_id = get_str("emoji_package_id");
                // LLOneBot OB11 reports user-uploaded/favourite stickers as an
                // ordinary image segment with camelCase `subType: 1`. OB12 and
                // some other adapters expose the same fact as
                // `sub_type: "sticker"`. Market packs carry emoji_id instead.
                let sticker_subtype = ["subType", "sub_type"].iter().any(|key| {
                    data.and_then(|value| value.get(*key)).is_some_and(|value| {
                        value.as_i64() == Some(1) || matches!(value.as_str(), Some("1") | Some("sticker"))
                    })
                });
                let is_sticker = emoji_id.is_some()
                    || sticker_subtype
                    || matches!(summary.as_deref(), Some("[动画表情]") | Some("[商城表情]"));
                if is_sticker {
                    text.push(STICKER_SENTINEL);
                    let source_key = match (package_id.as_deref(), emoji_id.as_deref()) {
                        (Some(package), Some(id)) => Some(format!("{package}:{id}")),
                        (_, Some(id)) => Some(id.to_string()),
                        _ => get_str("key")
                            .or_else(|| get_str("resource_id"))
                            .or_else(|| sticker_subtype.then(|| get_str("file")).flatten()),
                    };
                    parsed.stickers.push(StickerRef {
                        source: if emoji_id.is_some() {
                            "onebot_mface"
                        } else {
                            "onebot_image"
                        },
                        source_key,
                        native_payload: data.cloned().unwrap_or_else(|| serde_json::json!({})),
                        url: get_str("url").or_else(|| get_str("temp_url")),
                        file: get_str("file"),
                        summary,
                    });
                } else {
                    text.push(IMAGE_SENTINEL);
                    parsed.images.push(MediaRef {
                        url: get_str("url"),
                        file: get_str("file"),
                    });
                }
            }
            "face" => {
                text.push(STICKER_SENTINEL);
                let id = data.and_then(|value| value.get("id")).and_then(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| value.as_i64().map(|v| v.to_string()))
                });
                parsed.stickers.push(StickerRef {
                    source: "onebot_face",
                    source_key: id.clone(),
                    native_payload: data.cloned().unwrap_or_else(|| serde_json::json!({})),
                    url: id.map(|id| format!("https://qzonestyle.gtimg.cn/qzone/em/e{id}.gif")),
                    file: None,
                    summary: None,
                });
            }
            "mface" | "market_face" => {
                let get_str = |key: &str| seg_str(data, key);
                text.push(STICKER_SENTINEL);
                let emoji_id = get_str("emoji_id").or_else(|| get_str("id"));
                let package_id = get_str("emoji_package_id").or_else(|| get_str("package_id"));
                let source_key = match (package_id.as_deref(), emoji_id.as_deref()) {
                    (Some(package), Some(id)) => Some(format!("{package}:{id}")),
                    (_, Some(id)) => Some(id.to_string()),
                    _ => get_str("key"),
                };
                parsed.stickers.push(StickerRef {
                    source: "onebot_mface",
                    source_key,
                    native_payload: data.cloned().unwrap_or_else(|| serde_json::json!({})),
                    url: get_str("url").or_else(|| get_str("temp_url")),
                    file: get_str("file"),
                    summary: get_str("summary"),
                });
            }
            "record" => {
                text.push(RECORD_SENTINEL);
                // Read the same three fields the `image` branch reads. Until the
                // corpus work this branch set a bool and dropped all of them,
                // which is why a voice note's audio never survived arrival.
                //
                // Deliberately not reading `path`: NapCat often sends the
                // adapter's own absolute path there, and that only means
                // anything when the adapter shares a filesystem with this
                // process — which `onebot.host` cannot tell us, being a listen
                // address rather than a peer.
                parsed.records.push(MediaRef {
                    url: seg_str(data, "url"),
                    file: seg_str(data, "file"),
                });
            }
            "video" => match seg_str(data, "file").or_else(|| seg_str(data, "name")) {
                Some(name) if !name.starts_with("http") => text.push_str(&format!("[视频: {name}]")),
                _ => text.push_str("[视频]"),
            },
            "file" => {
                let name = seg_str(data, "name")
                    .or_else(|| seg_str(data, "file_name"))
                    .or_else(|| seg_str(data, "file").filter(|f| !f.starts_with("http")));
                let size = seg_str(data, "size")
                    .or_else(|| seg_str(data, "file_size"))
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(human_size);
                match (name, size) {
                    (Some(name), Some(size)) => text.push_str(&format!("[文件: {name} ({size})]")),
                    (Some(name), None) => text.push_str(&format!("[文件: {name}]")),
                    (None, _) => text.push_str("[文件]"),
                }
            }
            // A merged-forward bubble. What is in it takes a second API call, so
            // all that can be done here is mark the position and record the
            // handle; `quote::expand_forwards` fills it in.
            "forward" => {
                text.push(FORWARD_SENTINEL);
                parsed.forwards.push(ForwardRef {
                    id: seg_str(data, "id"),
                    inline: data.and_then(|d| d.get("content")).filter(|c| c.is_array()).cloned(),
                });
            }
            "json" => text.push_str(&render_json_card(data)),
            "xml" => text.push_str(&render_xml_card(data)),
            // Everything below is something a person can send that carries no
            // media we could fetch, and whose whole content is its own label.
            // Left unhandled they arrived as an empty message, which reads as
            // the person having said nothing at all.
            "redbag" | "hongbao" => match seg_str(data, "title") {
                Some(title) => text.push_str(&format!("[红包: {title}]")),
                None => text.push_str("[红包]"),
            },
            "location" => {
                let title = seg_str(data, "title").unwrap_or_default();
                let content = seg_str(data, "content").unwrap_or_default();
                let coords = match (seg_str(data, "lat"), seg_str(data, "lon")) {
                    (Some(lat), Some(lon)) => format!(" ({lat}, {lon})"),
                    _ => String::new(),
                };
                let label = [title, content].join(" ").trim().to_string();
                match label.is_empty() {
                    true => text.push_str(&format!("[位置{coords}]")),
                    false => text.push_str(&format!("[位置: {label}{coords}]")),
                }
            }
            "contact" => {
                let kind = match seg_str(data, "type").as_deref() {
                    Some("group") => "群",
                    _ => "好友",
                };
                match seg_str(data, "id") {
                    Some(id) => text.push_str(&format!("[推荐{kind}: {id}]")),
                    None => text.push_str(&format!("[推荐{kind}]")),
                }
            }
            "music" => match seg_str(data, "title") {
                Some(title) => text.push_str(&format!("[音乐: {title}]")),
                None => text.push_str("[音乐分享]"),
            },
            "share" => {
                let title = seg_str(data, "title").unwrap_or_else(|| "分享".into());
                match seg_str(data, "url") {
                    Some(url) => text.push_str(&format!("[分享: {title} ({url})]")),
                    None => text.push_str(&format!("[分享: {title}]")),
                }
            }
            "dice" => match seg_str(data, "result").or_else(|| seg_str(data, "value")) {
                Some(result) => text.push_str(&format!("[骰子: {result}]")),
                None => text.push_str("[骰子]"),
            },
            "rps" => match seg_str(data, "result").or_else(|| seg_str(data, "value")) {
                // 1/2/3 is the wire encoding; the words are what was meant.
                Some(result) => {
                    let played = match result.as_str() {
                        "1" => "布",
                        "2" => "剪刀",
                        "3" => "石头",
                        other => other,
                    };
                    text.push_str(&format!("[猜拳: {played}]"));
                }
                None => text.push_str("[猜拳]"),
            },
            "poke" => text.push_str("[戳了戳]"),
            "shake" => text.push_str("[窗口抖动]"),
            "reply" => {
                // Ignore reply context; we use conversation history instead
            }
            other => {
                if !other.is_empty() {
                    tracing::debug!(segment = other, "unhandled OneBot message segment");
                }
            }
        }
    }
    parsed.text = text.trim().to_string();
    // Mentions are deliberately absent. Addressing the bot is how a message
    // gets here at all, not something said in it.
    parsed.typed = typed.trim().to_string();
    parsed
}

/// Check if the bot is @mentioned in a group message.
pub fn is_at_bot(message: &serde_json::Value, self_id: i64) -> bool {
    let cq;
    let segments = match message.as_array() {
        Some(arr) => arr,
        None => {
            let Some(raw) = message.as_str() else { return false };
            cq = cq_to_segments(raw);
            &cq
        }
    };
    segments.iter().any(|seg| {
        seg.get("type").and_then(|v| v.as_str()) == Some("at")
            && seg
                .get("data")
                .and_then(|d| d.get("qq"))
                .and_then(|v| v.as_str().and_then(|s| s.parse::<i64>().ok()).or(v.as_i64()))
                == Some(self_id)
    })
}

/// Which message a reply is quoting, if it is one.
///
/// **Both wire formats**, like [`is_at_bot`] two functions up. Whether
/// `message` arrives as an array of segments or as a CQ string is an
/// implementation's own setting — go-cqhttp and its successors offer both —
/// and reading only the array made a reply from a string-format server look
/// like an ordinary message. What that costs is the whole quoted half of the
/// turn: no quoted text, no quoted media, and on a phone that is the only way
/// to show the bot a sticker.
pub fn extract_reply_message_id(message: &serde_json::Value) -> Option<i64> {
    let cq;
    let segments = match message.as_array() {
        Some(arr) => arr,
        None => {
            cq = cq_to_segments(message.as_str()?);
            &cq
        }
    };
    segments.iter().find_map(|seg| {
        if seg.get("type").and_then(|v| v.as_str()) == Some("reply") {
            seg.get("data")
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_i64()))
        } else {
            None
        }
    })
}

pub fn format_enriched_message(
    text: &str,
    sender_prefix: Option<&str>,
    quoted_message: Option<(&str, &str)>,
) -> String {
    let mut result = String::new();
    if let Some((sender, content)) = quoted_message {
        result.push_str(&format!(
            "<quoted_message sender=\"{}\">{}</quoted_message>\n",
            sender, content
        ));
    }
    if let Some(prefix) = sender_prefix {
        result.push_str(&format!("[{}] ", prefix));
    }
    result.push_str(text);
    result
}

pub fn text_to_rich_segments(text: &str) -> Vec<MessageSegment> {
    let mut segments = Vec::new();
    let mut last_end = 0;
    for cap in AT_MENTION_RE.captures_iter(text) {
        let full_match = cap.get(0).unwrap();
        if full_match.start() > last_end {
            segments.push(MessageSegment::text(&text[last_end..full_match.start()]));
        }
        if let Ok(qq) = cap[1].parse::<i64>() {
            segments.push(MessageSegment::at(qq));
        }
        last_end = full_match.end();
    }
    if last_end < text.len() {
        segments.push(MessageSegment::text(&text[last_end..]));
    }
    if segments.is_empty() {
        segments.push(MessageSegment::text(text));
    }
    segments
}

/// Split a long message into chunks respecting a max length.
/// Tries to split on paragraph boundaries first, then sentence boundaries.
pub fn split_long_message(text: &str) -> Vec<String> {
    if text.len() <= MAX_MSG_LEN {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= MAX_MSG_LEN {
            chunks.push(remaining.to_string());
            break;
        }

        let split_at = find_split_point(remaining, MAX_MSG_LEN);
        let (chunk, rest) = remaining.split_at(split_at);
        chunks.push(chunk.trim_end().to_string());
        remaining = rest.trim_start();
    }

    chunks
}

fn find_split_point(text: &str, max_len: usize) -> usize {
    let search_range = crate::util::take_bytes_at_char_boundary(text, max_len);

    // Try paragraph boundary
    if let Some(pos) = search_range.rfind("\n\n")
        && pos > max_len / 4
    {
        return pos + 1;
    }

    // Try line boundary
    if let Some(pos) = search_range.rfind('\n')
        && pos > max_len / 4
    {
        return pos + 1;
    }

    // Try sentence boundary (Chinese and English)
    for sep in &["。", ".", "！", "!", "？", "?", "；", ";"] {
        if let Some(pos) = search_range.rfind(sep)
            && pos > max_len / 4
        {
            return pos + sep.len();
        }
    }

    // Last resort: split at char boundary near max_len
    search_range.len()
}

/// `ask_user`'s arguments, as a question rather than as a permission request.
///
/// The chat surface has one prompt shape for everything a tool wants, and for
/// this tool that is wrong twice over: the model is not asking to be allowed to
/// do something, and what it is actually asking never appeared — the person saw
/// the tool's name and its raw JSON and was invited to reply `Y`.
///
/// Arguments that will not parse still produce a prompt. They come from a model
/// and are the only thing anyone has to go on, so a truncated dump beats
/// silence: the alternative is a question the user is given no way to answer,
/// waiting out its minute.
pub fn ask_user_prompt(arguments: &str) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Arguments {
        questions: Vec<Question>,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Question {
        id: String,
        question: String,
        options: Option<Vec<OptionItem>>,
        multi_select: Option<bool>,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OptionItem {
        label: String,
        description: Option<String>,
    }

    const FOOTER: &str = "\n引用本条消息作答（60秒超时）";
    let parsed: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid ask_user arguments: {error}"))?;
    if !(1..=4).contains(&parsed.questions.len()) {
        return Err("ask_user.questions must contain between 1 and 4 questions".into());
    }

    let mut out = String::from("❓ 助手有个问题:\n");
    for question in parsed.questions {
        if question.id.trim().is_empty() || question.question.trim().is_empty() {
            return Err("ask_user question id and text must not be empty".into());
        }
        let _multi_select = question.multi_select.unwrap_or(false);
        out.push_str(&format!("\n{}\n", question.question));
        if let Some(options) = question.options {
            if !(2..=4).contains(&options.len()) {
                return Err("ask_user question options must contain between 2 and 4 choices".into());
            }
            for (index, option) in options.into_iter().enumerate() {
                if option.label.trim().is_empty() {
                    return Err("ask_user option labels must not be empty".into());
                }
                match option.description.as_deref() {
                    Some(description) if !description.is_empty() => {
                        out.push_str(&format!("  {}. {} — {description}\n", index + 1, option.label));
                    }
                    _ => out.push_str(&format!("  {}. {}\n", index + 1, option.label)),
                }
            }
        }
    }
    out.push_str(FOOTER);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this whole path exists for, stated as the two halves it is made
    /// of: flattening a message to text destroys its stickers, and that is
    /// correct for a *display* string and fatal for one about to be shown to a
    /// model. `quote::fetch` used to call the first and needed the second.
    #[test]
    fn flattening_a_sticker_to_text_is_lossy_and_parsing_it_is_not() {
        let message = serde_json::json!([
            { "type": "mface", "data": { "emoji_id": "abc", "summary": "[开心]", "url": "http://x/1.gif" } }
        ]);
        assert_eq!(segments_to_text(&message, None), "[动画表情]");

        let parsed = parse_segments(&message, None);
        assert_eq!(parsed.stickers.len(), 1, "the sticker survives parsing");
        assert_eq!(parsed.stickers[0].source_key.as_deref(), Some("abc"));
        assert!(parsed.text.contains(STICKER_SENTINEL), "and holds its position");
    }

    /// Each of these used to fall through the `_ => {}` arm and produce an empty
    /// message — which `handle_message` then dropped, so sharing a link with the
    /// bot @mentioned looked exactly like the bot ignoring you.
    #[test]
    fn a_message_that_is_only_a_card_is_not_an_empty_message() {
        let shared = serde_json::json!([{
            "type": "json",
            "data": { "data": r#"{"app":"com.tencent.structmsg","prompt":"[分享]标题",
                "meta":{"news":{"title":"标题","desc":"一段描述","jumpUrl":"https://example.com/a"}}}"# }
        }]);
        let text = parse_segments(&shared, None).text;
        assert!(text.contains("标题"), "{text}");
        assert!(text.contains("一段描述"), "{text}");
        assert!(text.contains("https://example.com/a"), "{text}");
    }

    #[test]
    fn a_card_with_no_meta_falls_back_to_the_prompt_qq_itself_shows() {
        let card = serde_json::json!([{
            "type": "json",
            "data": { "data": r#"{"app":"com.tencent.miniapp_01","prompt":"[QQ小程序]哔哩哔哩"}"# }
        }]);
        assert_eq!(parse_segments(&card, None).text, "[卡片: [QQ小程序]哔哩哔哩]");

        let unparseable = serde_json::json!([{ "type": "json", "data": { "data": "not json at all" } }]);
        assert_eq!(parse_segments(&unparseable, None).text, "[卡片]");
    }

    #[test]
    fn an_xml_card_is_read_off_its_brief_attribute() {
        let card = serde_json::json!([{
            "type": "xml",
            "data": { "data": r#"<?xml version="1.0"?><msg brief="&#91;聊天记录&#93;摘要" serviceID="35"></msg>"# }
        }]);
        assert_eq!(parse_segments(&card, None).text, "[卡片: [聊天记录]摘要]");
    }

    /// A forward is a handle, not content: the segment marks its position and
    /// records the id, and `quote::expand_forwards` does the fetching.
    #[test]
    fn a_forward_becomes_a_sentinel_and_a_handle() {
        let message = serde_json::json!([
            { "type": "text", "data": { "text": "看这个" } },
            { "type": "forward", "data": { "id": "7318..." } }
        ]);
        let parsed = parse_segments(&message, None);
        assert_eq!(parsed.forwards.len(), 1);
        assert_eq!(parsed.forwards[0].id.as_deref(), Some("7318..."));
        assert_eq!(parsed.text, format!("看这个{FORWARD_SENTINEL}"));
        assert!(parsed.has_media(), "a forward is content the turn must wait for");
        // Unresolved, it still has to read as something.
        assert_eq!(segments_to_text(&message, None), "看这个[聊天记录]");
    }

    #[test]
    fn a_file_carries_its_name_and_size() {
        let message = serde_json::json!([
            { "type": "file", "data": { "name": "报告.pdf", "size": 1536 } }
        ]);
        assert_eq!(parse_segments(&message, None).text, "[文件: 报告.pdf (1.5 KB)]");

        let nameless = serde_json::json!([{ "type": "file", "data": {} }]);
        assert_eq!(parse_segments(&nameless, None).text, "[文件]");
    }

    #[test]
    fn the_small_segments_say_what_they_are() {
        let cases = [
            (
                serde_json::json!({ "type": "dice", "data": { "result": 4 } }),
                "[骰子: 4]",
            ),
            (
                serde_json::json!({ "type": "rps", "data": { "result": 3 } }),
                "[猜拳: 石头]",
            ),
            (
                serde_json::json!({ "type": "redbag", "data": { "title": "恭喜发财" } }),
                "[红包: 恭喜发财]",
            ),
            (serde_json::json!({ "type": "poke", "data": {} }), "[戳了戳]"),
            (
                serde_json::json!({ "type": "contact", "data": { "type": "group", "id": "12345" } }),
                "[推荐群: 12345]",
            ),
            (
                serde_json::json!({ "type": "location", "data": { "title": "公司", "lat": "31.2", "lon": "121.4" } }),
                "[位置: 公司 (31.2, 121.4)]",
            ),
        ];
        for (segment, expected) in cases {
            let parsed = parse_segments(&serde_json::json!([segment.clone()]), None);
            assert_eq!(parsed.text, expected, "for {segment}");
            assert!(parsed.typed.is_empty(), "nobody typed this: {segment}");
        }
    }

    #[test]
    fn test_segments_to_text_basic() {
        let msg = serde_json::json!([
            {"type": "text", "data": {"text": "hello "}},
            {"type": "text", "data": {"text": "world"}}
        ]);
        assert_eq!(segments_to_text(&msg, None), "hello world");
    }

    #[test]
    fn test_segments_to_text_strips_bot_at() {
        let msg = serde_json::json!([
            {"type": "at", "data": {"qq": "12345"}},
            {"type": "text", "data": {"text": " hi there"}}
        ]);
        assert_eq!(segments_to_text(&msg, Some(12345)), "hi there");
    }

    #[test]
    fn test_is_at_bot() {
        let msg = serde_json::json!([
            {"type": "at", "data": {"qq": "12345"}},
            {"type": "text", "data": {"text": " hello"}}
        ]);
        assert!(is_at_bot(&msg, 12345));
        assert!(!is_at_bot(&msg, 99999));
    }

    #[test]
    fn test_parse_segments_image() {
        let msg = serde_json::json!([
            {"type": "text", "data": {"text": "看这个 "}},
            {"type": "image", "data": {"file": "a.jpg", "url": "https://example.com/a.jpg", "file_size": "123"}}
        ]);
        let parsed = parse_segments(&msg, None);
        assert_eq!(parsed.text, format!("看这个 {IMAGE_SENTINEL}"));
        assert_eq!(parsed.images.len(), 1);
        assert_eq!(parsed.images[0].url.as_deref(), Some("https://example.com/a.jpg"));
        assert!(parsed.has_media());
    }

    #[test]
    fn test_parse_mface_upreported_as_image() {
        let msg = serde_json::json!([{
            "type": "image",
            "data": {
                "summary": "[动画表情]",
                "emoji_id": 99,
                "emoji_package_id": "42",
                "key": "native-key",
                "url": "https://example.com/sticker.gif"
            }
        }]);
        let parsed = parse_segments(&msg, None);
        assert!(parsed.images.is_empty());
        assert_eq!(parsed.stickers.len(), 1);
        assert_eq!(parsed.stickers[0].source, "onebot_mface");
        assert_eq!(parsed.stickers[0].source_key.as_deref(), Some("42:99"));
        assert_eq!(parsed.text, STICKER_SENTINEL.to_string());
    }

    #[test]
    fn llonebot_ob11_custom_sticker_uses_numeric_camel_case_subtype() {
        let msg = serde_json::json!([{
            "type": "image",
            "data": {
                "file": "custom-sticker.gif",
                "subType": 1,
                "url": "https://example.com/custom-sticker.gif",
                "file_size": "1234"
            }
        }]);
        let parsed = parse_segments(&msg, None);
        assert!(parsed.images.is_empty());
        assert_eq!(parsed.stickers.len(), 1);
        assert_eq!(parsed.stickers[0].source, "onebot_image");
        assert_eq!(parsed.stickers[0].source_key.as_deref(), Some("custom-sticker.gif"));
        assert_eq!(parsed.stickers[0].summary, None);
        assert_eq!(parsed.text, STICKER_SENTINEL.to_string());
    }

    #[test]
    fn onebot12_custom_sticker_uses_named_snake_case_subtype() {
        let msg = serde_json::json!([{
            "type": "image",
            "data": {
                "resource_id": "resource-1",
                "sub_type": "sticker",
                "temp_url": "https://example.com/custom.webp"
            }
        }]);
        let parsed = parse_segments(&msg, None);
        assert!(parsed.images.is_empty());
        assert_eq!(parsed.stickers.len(), 1);
        assert_eq!(parsed.stickers[0].source_key.as_deref(), Some("resource-1"));
        assert_eq!(
            parsed.stickers[0].url.as_deref(),
            Some("https://example.com/custom.webp")
        );
    }

    #[test]
    fn test_parse_direct_mface_and_face() {
        let msg = serde_json::json!([
            {"type": "mface", "data": {"emoji_id": "e1", "emoji_package_id": "p1", "summary": "捂脸"}},
            {"type": "face", "data": {"id": 14}}
        ]);
        let parsed = parse_segments(&msg, None);
        assert_eq!(parsed.stickers.len(), 2);
        assert_eq!(parsed.stickers[0].source_key.as_deref(), Some("p1:e1"));
        assert_eq!(parsed.stickers[1].source_key.as_deref(), Some("14"));
        assert_eq!(
            parsed.stickers[1].url.as_deref(),
            Some("https://qzonestyle.gtimg.cn/qzone/em/e14.gif")
        );
        assert_eq!(segments_to_text(&msg, None), "[动画表情][动画表情]");
    }

    #[test]
    fn ordinary_image_summary_is_not_a_sticker() {
        let msg = serde_json::json!([{
            "type": "image",
            "data": {"summary": "[图片]", "subType": 0, "url": "https://example.com/photo.jpg"}
        }]);
        let parsed = parse_segments(&msg, None);
        assert_eq!(parsed.images.len(), 1);
        assert!(parsed.stickers.is_empty());
    }

    #[test]
    fn cq_string_fallback_detects_addressed_stickers() {
        let raw = serde_json::Value::String(
            "[CQ:at,qq=12345] [CQ:image,summary=&#91;动画表情&#93;,emoji_id=9,emoji_package_id=2,url=https://example.com/a.gif]"
                .into(),
        );
        assert!(is_at_bot(&raw, 12345));
        let parsed = parse_segments(&raw, Some(12345));
        assert_eq!(parsed.stickers.len(), 1);
        assert_eq!(parsed.stickers[0].source_key.as_deref(), Some("2:9"));
        assert_eq!(parsed.typed, "");
    }

    /// Whether a reply arrives as segments or as a CQ string is the server's
    /// own setting, and reading only the array made a string-format reply
    /// indistinguishable from an ordinary message — losing the quoted text, the
    /// quoted media, and with it the one way a phone can show the bot a
    /// sticker.
    #[test]
    fn a_reply_is_found_in_either_wire_format() {
        let array = serde_json::json!([
            { "type": "reply", "data": { "id": "998" } },
            { "type": "text", "data": { "text": " 这个" } },
        ]);
        assert_eq!(extract_reply_message_id(&array), Some(998));

        let cq = serde_json::Value::String("[CQ:reply,id=998][CQ:at,qq=12345] 这个".into());
        assert_eq!(extract_reply_message_id(&cq), Some(998));

        // And a message that quotes nothing still quotes nothing.
        let plain = serde_json::Value::String("[CQ:at,qq=12345] 在吗".into());
        assert_eq!(extract_reply_message_id(&plain), None);
    }

    #[test]
    fn cq_string_custom_sticker_uses_string_subtype() {
        let raw =
            serde_json::Value::String("[CQ:image,file=custom.gif,subType=1,url=https://example.com/custom.gif]".into());
        let parsed = parse_segments(&raw, None);
        assert!(parsed.images.is_empty());
        assert_eq!(parsed.stickers.len(), 1);
        assert_eq!(parsed.stickers[0].source_key.as_deref(), Some("custom.gif"));
    }

    #[test]
    fn test_parse_segments_record() {
        let msg = serde_json::json!([
            {"type": "record", "data": {"file": "b.amr"}}
        ]);
        let parsed = parse_segments(&msg, None);
        assert_eq!(parsed.text, RECORD_SENTINEL.to_string());
        assert_eq!(parsed.records.len(), 1);
        assert!(parsed.has_media());
    }

    /// The whole point of carrying `records` rather than a bool: a voice note
    /// says where it can be fetched from, and that used to be dropped on the
    /// floor — the transcript was kept and the audio was not.
    #[test]
    fn a_voice_note_keeps_where_it_can_be_fetched_from() {
        let array = serde_json::json!([
            {"type": "record", "data": {"file": "b.amr", "url": "https://example.com/b.amr"}}
        ]);
        let parsed = parse_segments(&array, None);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].file.as_deref(), Some("b.amr"));
        assert_eq!(parsed.records[0].url.as_deref(), Some("https://example.com/b.amr"));

        // The CQ-string wire format reaches the same branch through
        // `cq_to_segments`, and `record` had never been tested that way.
        let cq = serde_json::Value::String("[CQ:record,file=c.silk,url=https://example.com/c.silk]".into());
        let parsed = parse_segments(&cq, None);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].file.as_deref(), Some("c.silk"));
        assert_eq!(parsed.records[0].url.as_deref(), Some("https://example.com/c.silk"));
    }

    /// An inlined payload stays in `file` verbatim — recognising the
    /// `base64://` prefix belongs to whoever fetches, not to the parser.
    #[test]
    fn an_inlined_voice_note_is_carried_verbatim() {
        let msg = serde_json::json!([
            {"type": "record", "data": {"file": "base64://AAAA"}}
        ]);
        let parsed = parse_segments(&msg, None);
        assert_eq!(parsed.records[0].file.as_deref(), Some("base64://AAAA"));
        assert!(parsed.records[0].url.is_none());
    }

    /// One message, two voice notes. The bool this replaced could not say so,
    /// and a transcript covering both cannot be attributed to either.
    #[test]
    fn two_voice_notes_in_one_message_stay_two() {
        let msg = serde_json::json!([
            {"type": "record", "data": {"file": "a.amr"}},
            {"type": "record", "data": {"file": "b.amr"}}
        ]);
        let parsed = parse_segments(&msg, None);
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.text.chars().filter(|c| *c == RECORD_SENTINEL).count(), 2);
    }

    #[test]
    fn test_segments_to_text_restores_sentinels() {
        let msg = serde_json::json!([
            {"type": "text", "data": {"text": "看这个 "}},
            {"type": "image", "data": {"file": "a.jpg"}},
            {"type": "record", "data": {"file": "b.amr"}}
        ]);
        // parse_segments keeps sentinels; segments_to_text restores them.
        assert_eq!(
            parse_segments(&msg, None).text,
            format!("看这个 {IMAGE_SENTINEL}{RECORD_SENTINEL}")
        );
        assert_eq!(segments_to_text(&msg, None), "看这个 [图片][语音]");
    }

    /// Everything we stood in for the user, in one message. None of it was
    /// typed, and `text` cannot say so — by then a picture is either a
    /// private-use codepoint or the five characters `[图片]`, and a person can
    /// type the second one.
    #[test]
    fn what_the_user_typed_leaves_out_what_we_wrote_for_them() {
        let msg = serde_json::json!([
            {"type": "image", "data": {"file": "a.jpg"}},
            {"type": "record", "data": {"file": "b.amr"}},
            {"type": "face", "data": {"id": "1"}},
            {"type": "video", "data": {"file": "c.mp4"}},
            {"type": "file", "data": {"file": "d.zip"}},
        ]);
        let parsed = parse_segments(&msg, None);

        assert!(parsed.typed.is_empty(), "got: {:?}", parsed.typed);
        assert!(!parsed.text.is_empty(), "the message itself is not empty");
    }

    /// And when there are words among it, they are what survives — the ones the
    /// person wrote, without the placeholders wrapped around them.
    #[test]
    fn words_sent_alongside_media_are_kept_and_the_media_is_not() {
        let msg = serde_json::json!([
            {"type": "at", "data": {"qq": "12345"}},
            {"type": "image", "data": {"file": "a.jpg"}},
            {"type": "text", "data": {"text": " 用第二个方案"}},
            {"type": "face", "data": {"id": "1"}},
        ]);
        let parsed = parse_segments(&msg, Some(12345));

        assert_eq!(parsed.typed, "用第二个方案");
        assert!(!parsed.typed.contains(IMAGE_SENTINEL));
        assert!(!parsed.typed.contains("[表情]"));
    }

    /// A message that arrived as nothing but text is all of it typed. This is
    /// the fallback for clients that only send `raw_message`, and it must not
    /// quietly answer nothing.
    #[test]
    fn a_message_that_was_only_ever_text_is_all_typed() {
        assert_eq!(ParsedMessage::from_text("y").typed, "y");
    }

    /// What the QQ user used to be shown for this was the tool's name and its
    /// raw JSON, under "回复 Y 批准" — a permission prompt for something that
    /// was not asking permission, and which never showed the question.
    #[test]
    fn a_question_is_shown_as_a_question() {
        let prompt = ask_user_prompt(
            r#"{"questions":[{"id":"q1","question":"先修哪个?","options":[
                {"label":"压缩","description":"上下文爆了"},
                {"label":"审批"}
            ]}]}"#,
        )
        .unwrap();

        assert!(prompt.contains("先修哪个?"), "{prompt}");
        assert!(prompt.contains("1. 压缩 — 上下文爆了"), "{prompt}");
        assert!(prompt.contains("2. 审批"), "{prompt}");
        assert!(!prompt.contains("批准"), "still worded as a permission: {prompt}");
        assert!(prompt.contains("引用本条消息作答"), "{prompt}");
    }

    #[test]
    fn malformed_or_future_question_shapes_are_rejected() {
        for args in [
            "",
            "{",
            r#"{"questions":[]}"#,
            r#"{"questions":"soon"}"#,
            r#"{"questions":[{"id":"q","question":"now?","future":true}]}"#,
        ] {
            assert!(ask_user_prompt(args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn test_split_short_message() {
        let text = "short message";
        let chunks = split_long_message(text);
        assert_eq!(chunks, vec!["short message"]);
    }

    #[test]
    fn test_split_long_chinese_no_panic() {
        let text = "中".repeat(3000); // 9000 bytes, no separators
        let chunks = split_long_message(&text);
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn test_split_long_message_on_paragraph() {
        let para1 = "a".repeat(2000);
        let para2 = "b".repeat(2000);
        let text = format!("{}\n\n{}", para1, para2);
        let chunks = split_long_message(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], para1);
        assert_eq!(chunks[1], para2);
    }
}
