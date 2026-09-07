//! 入站语音语料：物理文件与采集事件。
//!
//! 两个类型对应两张表，分开的理由在迁移 39 的注释里：同一段音频被两个人发出来
//! 是两次采集，合并成一行会丢掉第二个人的身份，而那正是"按人删除"要用的东西。

use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::{voice_blobs, voice_clips, voice_sender_optouts};

/// OneBot conversation type persisted with a voice sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum VoiceCorpusSourceType {
    OnebotGroup,
    OnebotPrivate,
}

impl VoiceCorpusSourceType {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown voice corpus source_type '{value}'"))
    }
}

/// Publication state persisted for a voice blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum VoiceBlobStatus {
    Pending,
    Ready,
    Damaged,
    Deleting,
}

impl VoiceBlobStatus {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown voice blob status '{value}'"))
    }
}

/// blob 的发布状态。
///
/// 字符串而不是整数：它会出现在日志和导出里，而一个人读到 `damaged` 就知道
/// 发生了什么，读到 `2` 不会。
pub mod blob_status {
    /// 有 owner 正在发布，或者那个 owner 已经死了（靠 lease + fencing 分辨）。
    pub const PENDING: &str = "pending";
    /// 可列出、可导出。
    pub const READY: &str = "ready";
    /// 文件缺失，或 size/sha 校验不过。不导出，也不假装正常。
    pub const DAMAGED: &str = "damaged";
    /// 墓碑：文件删成功之后才删行。
    pub const DELETING: &str = "deleting";
}

#[derive(Debug, Clone, Queryable, Selectable, Identifiable, Serialize)]
#[diesel(table_name = voice_blobs)]
pub struct VoiceBlobRow {
    pub id: String,
    pub bot_self_id: i64,
    pub source_type: String,
    pub source_id: String,
    pub sha256: String,
    pub file_format: String,
    pub file_name: String,
    pub file_size: i64,
    pub status: String,
    /// 每次 claim 唯一。进程 id 不够——同一个进程里两个任务的
    /// `CAS WHERE owner=旧值` 会写回相同的值并双双成功。
    pub owner_token: Option<String>,
    /// 单调递增，让接管可排序：旧 owner 醒来时带的是旧 epoch，写不进去。
    pub fence_epoch: i64,
    pub lease_expires_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = voice_blobs)]
pub struct VoiceBlobInsert<'a> {
    pub id: &'a str,
    pub bot_self_id: i64,
    pub source_type: &'a str,
    pub source_id: &'a str,
    pub sha256: &'a str,
    pub file_format: &'a str,
    pub file_name: &'a str,
    pub file_size: i64,
    pub status: &'a str,
    pub owner_token: Option<&'a str>,
    pub fence_epoch: i64,
    pub lease_expires_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Queryable, Selectable, Identifiable, Serialize)]
#[diesel(table_name = voice_clips)]
pub struct VoiceClipRow {
    pub id: String,
    pub blob_id: String,
    pub bot_self_id: i64,
    pub source_type: String,
    pub source_id: String,
    /// 消息的**发送者**，不是声学意义上的说话人：转发别人的语音时两者不同。
    pub sender_id: String,
    pub platform_message_id: Option<i64>,
    pub segment_index: i32,
    /// `None` 表示转写没拿到，或者这条消息有多个 record 段——按消息作答的
    /// 转写不能归给其中任何一段。
    pub transcript: Option<String>,
    pub transcript_source: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = voice_clips)]
pub struct VoiceClipInsert<'a> {
    pub id: &'a str,
    pub blob_id: &'a str,
    pub bot_self_id: i64,
    pub source_type: &'a str,
    pub source_id: &'a str,
    pub sender_id: &'a str,
    pub platform_message_id: Option<i64>,
    pub segment_index: i32,
    pub transcript: Option<&'a str>,
    pub transcript_source: Option<&'a str>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// "以后别再录我"。与删除历史是两件事，见迁移 39。
#[derive(Debug, Clone, Queryable, Selectable, Identifiable, Serialize)]
#[diesel(table_name = voice_sender_optouts)]
#[diesel(primary_key(sender_id))]
pub struct VoiceSenderOptoutRow {
    pub sender_id: String,
    pub created_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = voice_sender_optouts)]
pub struct VoiceSenderOptoutInsert<'a> {
    pub sender_id: &'a str,
    pub created_at: i64,
}
