use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Permission, Tool, ToolContext};

use crate::db::entity::emoji;
use crate::db::sea::cap::Db;
use crate::db::sea::ops::{emoji as emoji_ops, emoji_pack as pack_ops};

fn context_parts(context: &ToolContext) -> Result<(&Db, &str), String> {
    let db = context.db.as_ref().ok_or("Sticker tools require a database context")?;
    let assistant_id = context
        .assistant_id
        .as_deref()
        .ok_or("Sticker tools require an active assistant")?;
    Ok((db, assistant_id))
}

/// Something [`pick_from_roster`] can rank: an id, a display name, and optional tags.
pub trait RosterItem {
    fn roster_id(&self) -> &str;
    fn roster_name(&self) -> &str;
    fn roster_tags(&self) -> Option<&str>;
}

impl RosterItem for emoji::Model {
    fn roster_id(&self) -> &str {
        &self.id
    }
    fn roster_name(&self) -> &str {
        &self.name
    }
    fn roster_tags(&self) -> Option<&str> {
        self.tags.as_deref()
    }
}

const MISS_NAME_CAP: usize = 20;

/// Pick one sticker from a confirmed roster.
///
/// `sticker_id` wins when both are given. Rank is exact name, then name contains,
/// then a tag contains the query (tags split on comma / Chinese comma / dunhao /
/// semicolon / whitespace). Ties keep roster order.
pub fn pick_from_roster<'a, T: RosterItem>(
    roster: &'a [T],
    sticker_id: Option<&str>,
    query: Option<&str>,
) -> Result<&'a T, String> {
    let sticker_id = sticker_id.map(str::trim).filter(|s| !s.is_empty());
    let query = query.map(str::trim).filter(|s| !s.is_empty());

    if let Some(id) = sticker_id {
        return roster
            .iter()
            .find(|item| item.roster_id() == id)
            .ok_or_else(|| "That sticker is not in this assistant's confirmed roster".into());
    }

    let Some(query) = query else {
        return Err("Give send_sticker a query (a name or tag) or a sticker_id".into());
    };

    if roster.is_empty() {
        return Err("No confirmed stickers are available".into());
    }

    let q = query.to_lowercase();
    let mut best: Option<(u8, &'a T)> = None;
    for item in roster {
        let Some(rank) = match_rank(item, &q) else {
            continue;
        };
        match best {
            Some((best_rank, _)) if rank <= best_rank => {}
            _ => best = Some((rank, item)),
        }
    }

    if let Some((_, item)) = best {
        return Ok(item);
    }

    Err(miss_message(roster))
}

fn match_rank<T: RosterItem>(item: &T, q: &str) -> Option<u8> {
    let name = item.roster_name().to_lowercase();
    if name == q {
        return Some(3);
    }
    if name.contains(q) {
        return Some(2);
    }
    if tags_of(item.roster_tags()).any(|tag| tag.contains(q)) {
        return Some(1);
    }
    None
}

fn tags_of(tags: Option<&str>) -> impl Iterator<Item = String> {
    tags.unwrap_or("")
        .split(|c: char| matches!(c, ',' | '，' | '、' | ';' | '；') || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
}

fn miss_message<T: RosterItem>(roster: &[T]) -> String {
    let total = roster.len();
    let shown: Vec<&str> = roster.iter().take(MISS_NAME_CAP).map(|s| s.roster_name()).collect();
    let rest = total.saturating_sub(shown.len());
    let list = shown.join("、");
    if rest > 0 {
        format!("没有匹配的表情。可用：{list}（还有 {rest} 个）")
    } else {
        format!("没有匹配的表情。可用：{list}")
    }
}

pub struct ListStickersTool;

#[async_trait]
impl Tool for ListStickersTool {
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::Read,
            loop_handled: false,
            plan_mode: false,
            explore: false,
            reviewer: false,
            parallel: false,
        }
    }

    fn name(&self) -> &str {
        "list_stickers"
    }

    fn description(&self) -> &str {
        "List the confirmed stickers this assistant may send. Prefer send_sticker with a query; \
         use a returned id only when you already know which one."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Optional name/tag filter" }
            }
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let (db, assistant_id) = context_parts(context)?;
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let stickers = db
            .read(async |tx| {
                let pack_ids = pack_ops::list_assigned_pack_ids(tx, assistant_id).await?;
                emoji_ops::list_confirmed_for_packs(tx, &pack_ids).await
            })
            .await
            .map_err(|e| e.to_string())?;
        {
            let items: Vec<Value> = stickers
                .into_iter()
                .filter(|sticker| {
                    query.is_empty()
                        || format!("{} {}", sticker.name, sticker.tags.as_deref().unwrap_or(""))
                            .to_lowercase()
                            .contains(&query)
                })
                .take(100)
                .map(|sticker| {
                    json!({
                        "sticker_id": sticker.id,
                        "name": sticker.name,
                        "tags": sticker.tags,
                    })
                })
                .collect();
            serde_json::to_string(&items).map_err(|e| e.to_string())
        }
    }
}

pub struct SendStickerTool {
    sent_turns: Mutex<HashSet<String>>,
}

impl SendStickerTool {
    pub fn new() -> Self {
        Self {
            sent_turns: Mutex::new(HashSet::new()),
        }
    }
}

#[async_trait]
impl Tool for SendStickerTool {
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::Messaging,
            loop_handled: false,
            plan_mode: false,
            explore: false,
            reviewer: false,
            parallel: false,
        }
    }

    fn name(&self) -> &str {
        "send_sticker"
    }

    fn description(&self) -> &str {
        "Send one confirmed sticker as a separate visual message part. Pass a short query (name, \
         tag, or mood); the tool picks the best match. At most one call may succeed per assistant \
         turn. A sticker is already in the chat; do not narrate that it was sent."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Name, tag, or mood to pick from the confirmed roster"
                },
                "sticker_id": {
                    "type": "string",
                    "description": "Exact id from list_stickers; wins over query when both are given"
                },
                "description": super::description_property(),
            }
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let sticker_id = args.get("sticker_id").and_then(Value::as_str);
        let query = args.get("query").and_then(Value::as_str);
        let turn_id = context.turn_id.clone().ok_or("Sticker send requires a turn context")?;
        {
            let sent = self.sent_turns.lock().unwrap_or_else(|e| e.into_inner());
            if sent.contains(&turn_id) {
                return Err("A sticker has already been sent in this turn".into());
            }
        }
        let (db, assistant_id) = context_parts(context)?;
        let stickers = db
            .read(async |tx| {
                let pack_ids = pack_ops::list_assigned_pack_ids(tx, assistant_id).await?;
                emoji_ops::list_confirmed_for_packs(tx, &pack_ids).await
            })
            .await
            .map_err(|e| e.to_string())?;
        let sticker = pick_from_roster(&stickers, sticker_id, query)?;

        let mut sent = self.sent_turns.lock().unwrap_or_else(|e| e.into_inner());
        if sent.contains(&turn_id) {
            return Err("A sticker has already been sent in this turn".into());
        }
        if sent.len() >= 4096 {
            sent.clear();
        }
        sent.insert(turn_id);
        serde_json::to_string(&json!({ "sticker_id": sticker.id, "name": sticker.name })).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{RosterItem, pick_from_roster};

    #[derive(Debug)]
    struct Item {
        id: &'static str,
        name: &'static str,
        tags: Option<&'static str>,
    }

    impl RosterItem for Item {
        fn roster_id(&self) -> &str {
            self.id
        }
        fn roster_name(&self) -> &str {
            self.name
        }
        fn roster_tags(&self) -> Option<&str> {
            self.tags
        }
    }

    fn item(id: &'static str, name: &'static str, tags: Option<&'static str>) -> Item {
        Item { id, name, tags }
    }

    #[test]
    fn exact_name_beats_a_contains_match() {
        let roster = [item("a", "点赞一下", None), item("b", "点赞", None)];
        let picked = pick_from_roster(&roster, None, Some("点赞")).unwrap();
        assert_eq!(picked.id, "b");
    }

    #[test]
    fn name_contains_beats_a_tag_match() {
        let roster = [item("tag", "挥手", Some("点赞")), item("name", "点赞包", None)];
        let picked = pick_from_roster(&roster, None, Some("点赞")).unwrap();
        assert_eq!(picked.id, "name");
    }

    #[test]
    fn same_rank_keeps_roster_order() {
        let roster = [
            item("first", "大笑", Some("高兴")),
            item("second", "微笑", Some("高兴")),
        ];
        let picked = pick_from_roster(&roster, None, Some("高兴")).unwrap();
        assert_eq!(picked.id, "first");
    }

    #[test]
    fn sticker_id_wins_over_a_query() {
        let roster = [item("wave", "挥手", None), item("thumbs", "点赞", None)];
        let picked = pick_from_roster(&roster, Some("wave"), Some("点赞")).unwrap();
        assert_eq!(picked.id, "wave");
    }

    #[test]
    fn unknown_id_is_an_error() {
        let roster = [item("wave", "挥手", None)];
        let err = pick_from_roster(&roster, Some("missing"), None).unwrap_err();
        assert!(err.contains("confirmed roster"), "{err}");
    }

    #[test]
    fn neither_argument_is_an_error() {
        let roster = [item("wave", "挥手", None)];
        let err = pick_from_roster(&roster, None, None).unwrap_err();
        assert!(err.contains("query"), "{err}");
        let err = pick_from_roster(&roster, Some("  "), Some("")).unwrap_err();
        assert!(err.contains("query"), "{err}");
    }

    #[test]
    fn a_miss_lists_names_and_the_remainder() {
        #[derive(Debug)]
        struct Owned {
            id: String,
            name: String,
        }
        impl RosterItem for Owned {
            fn roster_id(&self) -> &str {
                &self.id
            }
            fn roster_name(&self) -> &str {
                &self.name
            }
            fn roster_tags(&self) -> Option<&str> {
                None
            }
        }
        let roster: Vec<Owned> = (0..22)
            .map(|i| Owned {
                id: format!("id{i}"),
                name: format!("n{i}"),
            })
            .collect();
        let err = pick_from_roster(&roster, None, Some("没有这个")).unwrap_err();
        assert!(err.contains("没有匹配的表情"), "{err}");
        assert!(err.contains("n0"), "{err}");
        assert!(err.contains("n19"), "{err}");
        assert!(!err.contains("n20"), "{err}");
        assert!(err.contains("还有 2 个"), "{err}");
    }

    #[test]
    fn empty_roster_is_its_own_error() {
        let roster: [Item; 0] = [];
        let err = pick_from_roster(&roster, None, Some("点赞")).unwrap_err();
        assert!(err.contains("No confirmed stickers"), "{err}");
    }

    #[test]
    fn tags_split_on_comma_dunhao_semicolon_and_space() {
        let roster = [
            item("a", "甲", Some("开心,高兴")),
            item("b", "乙", Some("无语、尴尬")),
            item("c", "丙", Some("害羞；脸红")),
            item("d", "丁", Some("挥手 再见")),
        ];
        assert_eq!(pick_from_roster(&roster, None, Some("高兴")).unwrap().id, "a");
        assert_eq!(pick_from_roster(&roster, None, Some("尴尬")).unwrap().id, "b");
        assert_eq!(pick_from_roster(&roster, None, Some("脸红")).unwrap().id, "c");
        assert_eq!(pick_from_roster(&roster, None, Some("再见")).unwrap().id, "d");
    }
}
