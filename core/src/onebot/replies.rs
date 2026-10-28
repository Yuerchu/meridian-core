//! Configurable QQ session copy: empty approval text, busy, 5xx, and the two
//! permission acks.
//!
//! Stored as `preferences.onebot.replies`, not on [`super::OneBotConfig`]. Host
//! / token / admin_users are a start snapshot; these phrases have to be hot —
//! the send path reads them each time, and the manage tool writes them without
//! restarting the listener. Empty and missing slots fall back to the seed, so a
//! 5xx is never silent. Corrupt JSON on the send path does the same and logs;
//! load of settings and the manage tool fail on invalid JSON, because broken
//! data is not a default.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::db::sea::cap::{Db, Read, WriteTx};
use crate::db::sea::ops as sea_ops;
use crate::util::now_ms;

pub const PREF_KEY: &str = "onebot.replies";
pub const MAX_VARIANTS: usize = 16;
pub const MAX_CHARS: usize = 200;

pub const SEED_EMPTY_REPLY: &str = "没搞明白你在说什么。";
pub const SEED_BUSY: &str = "这个对话正在电脑端处理，请等它结束后再发。";
pub const SEED_SERVER_ERROR: &str = "这边出了点问题。";
pub const SEED_ACK_APPROVED: &str = "已批准执行。";
pub const SEED_ACK_DENIED: &str = "已拒绝，你写的理由会一并转达。";

const FORBIDDEN_PLACEHOLDERS: &[&str] = &["{e}", "{error}", "{detail}"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ReplySlot {
    EmptyReply,
    Busy,
    ServerError,
    AckApproved,
    AckDenied,
}

impl ReplySlot {
    pub fn all() -> impl Iterator<Item = Self> {
        <Self as strum::IntoEnumIterator>::iter()
    }

    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(name: &str) -> Result<Self, String> {
        name.parse().map_err(|_| {
            format!(
                "unknown reply slot {name:?}; expected empty_reply, busy, server_error, ack_approved, or ack_denied"
            )
        })
    }

    pub fn seed(self) -> &'static str {
        match self {
            Self::EmptyReply => SEED_EMPTY_REPLY,
            Self::Busy => SEED_BUSY,
            Self::ServerError => SEED_SERVER_ERROR,
            Self::AckApproved => SEED_ACK_APPROVED,
            Self::AckDenied => SEED_ACK_DENIED,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManageAction {
    Add,
    Remove,
    Reset,
}

impl ManageAction {
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "add" => Ok(Self::Add),
            "remove" => Ok(Self::Remove),
            "reset" => Ok(Self::Reset),
            other => Err(format!("unknown action {other:?}; expected add, remove, or reset")),
        }
    }
}

/// The five slots as stored and as sent over IPC. Unknown slot names fail;
/// missing slots deserialize as empty and pick the seed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyBank {
    #[serde(default)]
    pub empty_reply: Vec<String>,
    #[serde(default)]
    pub busy: Vec<String>,
    #[serde(default)]
    pub server_error: Vec<String>,
    #[serde(default)]
    pub ack_approved: Vec<String>,
    #[serde(default)]
    pub ack_denied: Vec<String>,
}

/// Built-in fallbacks, returned on `get` so the settings page does not copy
/// the Chinese seeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplySeeds {
    pub empty_reply: String,
    pub busy: String,
    pub server_error: String,
    pub ack_approved: String,
    pub ack_denied: String,
}

fn last_used() -> &'static Mutex<HashMap<ReplySlot, String>> {
    static LAST: OnceLock<Mutex<HashMap<ReplySlot, String>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn choose(variants: &[String], seed: &str, last: Option<&str>, token: u64) -> String {
    let pool: Vec<&str> = if variants.is_empty() {
        vec![seed]
    } else if variants.len() >= 2 {
        let filtered: Vec<&str> = variants
            .iter()
            .map(String::as_str)
            .filter(|variant| last != Some(*variant))
            .collect();
        if filtered.is_empty() {
            variants.iter().map(String::as_str).collect()
        } else {
            filtered
        }
    } else {
        variants.iter().map(String::as_str).collect()
    };
    pool[(token as usize) % pool.len()].to_string()
}

fn validate_variant(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("a reply variant cannot be empty".into());
    }
    if text.chars().count() > MAX_CHARS {
        return Err(format!("a reply variant cannot exceed {MAX_CHARS} characters"));
    }
    for needle in FORBIDDEN_PLACEHOLDERS {
        if text.contains(needle) {
            return Err(format!("session replies cannot contain {needle}"));
        }
    }
    Ok(())
}

impl ReplyBank {
    pub fn seeds() -> ReplySeeds {
        ReplySeeds {
            empty_reply: SEED_EMPTY_REPLY.into(),
            busy: SEED_BUSY.into(),
            server_error: SEED_SERVER_ERROR.into(),
            ack_approved: SEED_ACK_APPROVED.into(),
            ack_denied: SEED_ACK_DENIED.into(),
        }
    }

    pub fn slot(&self, slot: ReplySlot) -> &[String] {
        match slot {
            ReplySlot::EmptyReply => &self.empty_reply,
            ReplySlot::Busy => &self.busy,
            ReplySlot::ServerError => &self.server_error,
            ReplySlot::AckApproved => &self.ack_approved,
            ReplySlot::AckDenied => &self.ack_denied,
        }
    }

    fn slot_mut(&mut self, slot: ReplySlot) -> &mut Vec<String> {
        match slot {
            ReplySlot::EmptyReply => &mut self.empty_reply,
            ReplySlot::Busy => &mut self.busy,
            ReplySlot::ServerError => &mut self.server_error,
            ReplySlot::AckApproved => &mut self.ack_approved,
            ReplySlot::AckDenied => &mut self.ack_denied,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        for slot in ReplySlot::all() {
            let variants = self.slot(slot);
            if variants.len() > MAX_VARIANTS {
                return Err(format!("{} has more than {MAX_VARIANTS} variants", slot.as_str()));
            }
            for variant in variants {
                validate_variant(variant).map_err(|error| format!("{}: {error}", slot.as_str()))?;
            }
        }
        Ok(())
    }

    pub fn pick(&self, slot: ReplySlot) -> String {
        let last = last_used().lock().ok().and_then(|guard| guard.get(&slot).cloned());
        let mut hasher = DefaultHasher::new();
        now_ms().hash(&mut hasher);
        slot.as_str().hash(&mut hasher);
        last.hash(&mut hasher);
        let chosen = choose(self.slot(slot), slot.seed(), last.as_deref(), hasher.finish());
        if let Ok(mut guard) = last_used().lock() {
            guard.insert(slot, chosen.clone());
        }
        chosen
    }

    pub fn apply(&mut self, action: ManageAction, slot: ReplySlot, text: Option<&str>) -> Result<(), String> {
        match action {
            ManageAction::Reset => {
                self.slot_mut(slot).clear();
                Ok(())
            }
            ManageAction::Add => {
                let raw = text.ok_or("add requires text")?;
                let trimmed = raw.trim();
                validate_variant(trimmed)?;
                let variants = self.slot_mut(slot);
                if variants.iter().any(|variant| variant == trimmed) {
                    return Err(format!("already present: {trimmed}"));
                }
                if variants.len() >= MAX_VARIANTS {
                    return Err(format!("at most {MAX_VARIANTS} variants per slot"));
                }
                variants.push(trimmed.to_string());
                Ok(())
            }
            ManageAction::Remove => {
                let raw = text.ok_or("remove requires text")?;
                let variants = self.slot_mut(slot);
                let before = variants.len();
                variants.retain(|variant| variant != raw);
                if variants.len() == before {
                    return Err(format!("not found: {raw}"));
                }
                Ok(())
            }
        }
    }

    pub fn format_listing(&self) -> String {
        let mut lines = Vec::new();
        for slot in ReplySlot::all() {
            let variants = self.slot(slot);
            if variants.is_empty() {
                lines.push(format!("{} (empty — using seed): {}", slot.as_str(), slot.seed()));
            } else {
                lines.push(format!("{}:", slot.as_str()));
                for variant in variants {
                    lines.push(format!("  - {variant}"));
                }
            }
        }
        lines.join("\n")
    }
}

fn parse_bank(raw: Option<String>) -> Result<ReplyBank, String> {
    match raw {
        None => Ok(ReplyBank::default()),
        Some(raw) => {
            let bank: ReplyBank = serde_json::from_str(&raw)
                .map_err(|error| format!("preference {PREF_KEY} has invalid JSON: {error}"))?;
            bank.validate()?;
            Ok(bank)
        }
    }
}

pub async fn load_in(db: &impl Read) -> Result<ReplyBank, String> {
    let raw = sea_ops::preference::get_preference(db, PREF_KEY)
        .await
        .map_err(|error| format!("failed to read preference {PREF_KEY}: {error}"))?;
    parse_bank(raw)
}

pub async fn load(db: &Db) -> Result<ReplyBank, String> {
    load_in(db).await
}

/// Send path: a corrupt row must not silence a 5xx. Settings and the manage
/// tool keep [`load`], which fails on invalid JSON.
pub async fn load_for_send(db: &Db) -> ReplyBank {
    match load(db).await {
        Ok(bank) => bank,
        Err(error) => {
            tracing::error!(error = %error, "onebot.replies is unreadable; using seeds");
            ReplyBank::default()
        }
    }
}

pub async fn save_in(tx: &WriteTx, bank: &ReplyBank) -> Result<(), String> {
    bank.validate()?;
    let json = serde_json::to_string(bank).map_err(|error| format!("could not serialize {PREF_KEY}: {error}"))?;
    sea_ops::preference::set_preference(tx, PREF_KEY, &json, now_ms())
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::sea_test_db;

    #[test]
    fn an_empty_slot_picks_the_seed() {
        let bank = ReplyBank::default();
        assert_eq!(
            choose(&bank.empty_reply, ReplySlot::EmptyReply.seed(), None, 0),
            SEED_EMPTY_REPLY
        );
        assert_eq!(
            choose(&bank.server_error, ReplySlot::ServerError.seed(), None, 7),
            SEED_SERVER_ERROR
        );
    }

    #[test]
    fn two_variants_skip_the_last_used() {
        let variants = vec!["甲".into(), "乙".into()];
        assert_eq!(choose(&variants, "seed", Some("甲"), 0), "乙");
        assert_eq!(choose(&variants, "seed", Some("乙"), 0), "甲");
        assert_eq!(choose(&variants, "seed", None, 0), "甲");
        assert_eq!(choose(&variants, "seed", None, 1), "乙");
    }

    #[test]
    fn a_single_variant_is_not_skipped() {
        let variants = vec!["只此一句".into()];
        assert_eq!(choose(&variants, "seed", Some("只此一句"), 99), "只此一句");
    }

    #[test]
    fn empty_object_is_an_empty_bank() {
        let bank: ReplyBank = serde_json::from_str("{}").unwrap();
        assert_eq!(bank, ReplyBank::default());
    }

    #[test]
    fn an_unknown_slot_name_fails_to_deserialize() {
        let error = serde_json::from_str::<ReplyBank>(r#"{"oops":[]}"#).unwrap_err();
        assert!(error.to_string().contains("oops"), "{error}");
    }

    #[test]
    fn validate_refuses_empty_too_long_too_many_and_placeholders() {
        let mut bank = ReplyBank {
            empty_reply: vec!["".into()],
            ..Default::default()
        };
        assert!(bank.validate().unwrap_err().contains("empty"));

        bank.empty_reply = vec!["x".repeat(MAX_CHARS + 1)];
        assert!(bank.validate().unwrap_err().contains("200"));

        bank.empty_reply = (0..MAX_VARIANTS + 1).map(|i| format!("v{i}")).collect();
        assert!(bank.validate().unwrap_err().contains("16"));

        bank.empty_reply = vec!["出错了 {e}".into()];
        assert!(bank.validate().unwrap_err().contains("{e}"));
        bank.empty_reply = vec!["见 {error}".into()];
        assert!(bank.validate().unwrap_err().contains("{error}"));
        bank.empty_reply = vec!["见 {detail}".into()];
        assert!(bank.validate().unwrap_err().contains("{detail}"));
    }

    #[test]
    fn apply_adds_removes_and_resets() {
        let mut bank = ReplyBank::default();
        bank.apply(ManageAction::Add, ReplySlot::Busy, Some("  稍等。  "))
            .unwrap();
        assert_eq!(bank.busy, vec!["稍等。"]);
        let dup = bank
            .apply(ManageAction::Add, ReplySlot::Busy, Some("稍等。"))
            .unwrap_err();
        assert!(dup.contains("already present"), "{dup}");
        bank.apply(ManageAction::Remove, ReplySlot::Busy, Some("稍等。"))
            .unwrap();
        assert!(bank.busy.is_empty());
        bank.apply(ManageAction::Add, ReplySlot::Busy, Some("一句")).unwrap();
        bank.apply(ManageAction::Reset, ReplySlot::Busy, None).unwrap();
        assert!(bank.busy.is_empty());
        let missing = bank
            .apply(ManageAction::Remove, ReplySlot::Busy, Some("没有"))
            .unwrap_err();
        assert!(missing.contains("not found"), "{missing}");
    }

    #[test]
    fn listing_notes_that_an_empty_slot_uses_the_seed() {
        let listing = ReplyBank::default().format_listing();
        assert!(listing.contains("empty_reply (empty — using seed): 没搞明白你在说什么。"));
        assert!(listing.contains("server_error (empty — using seed): 这边出了点问题。"));
    }

    #[tokio::test]
    async fn a_missing_key_loads_as_the_empty_bank() {
        let db = sea_test_db().await;
        assert_eq!(load(&db).await.unwrap(), ReplyBank::default());
    }

    #[tokio::test]
    async fn invalid_json_fails_load_and_send_falls_back_to_seeds() {
        let db = sea_test_db().await;
        db.write(async |tx| sea_ops::preference::set_preference(tx, PREF_KEY, "{not json", 1).await)
            .await
            .unwrap();
        let error = load(&db).await.unwrap_err();
        assert!(error.contains("invalid JSON"), "{error}");
        let send = load_for_send(&db).await;
        assert_eq!(send, ReplyBank::default());
        assert_eq!(send.pick(ReplySlot::ServerError), SEED_SERVER_ERROR);
    }

    #[tokio::test]
    async fn save_round_trips_and_rejects_placeholders() {
        let db = sea_test_db().await;
        let mut bank = ReplyBank {
            ack_approved: vec!["好，做。".into()],
            ..Default::default()
        };
        db.write(async |tx| save_in(tx, &bank).await.map_err(sea_orm::DbErr::Custom))
            .await
            .unwrap();
        assert_eq!(load(&db).await.unwrap().ack_approved, vec!["好，做。"]);

        bank.server_error = vec!["挂了 {e}".into()];
        let error = db
            .write(async |tx| save_in(tx, &bank).await.map_err(sea_orm::DbErr::Custom))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("{e}"), "{error}");
    }
}
