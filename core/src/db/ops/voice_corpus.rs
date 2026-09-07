//! 语料的读写：抢所有权、发布、记一次采集、删除。
//!
//! 这一层的形状由一件事决定——**文件先于行存在**。要落盘的字节必须先下载完
//! 才知道它的 sha 和大小，而 blob 行的去重键就建在 sha 上。所以流程是"先写
//! 临时文件算出 sha，再拿 sha 来抢所有权，最后发布"，而不是反过来。
//!
//! 抢所有权用 token + epoch 而不是进程 id，理由在迁移 39 的注释里：同一个
//! 进程内两个任务的 `CAS WHERE owner=旧值` 会写回相同的值并双双成功。

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::voice_corpus::{
    VoiceBlobInsert, VoiceBlobRow, VoiceBlobStatus, VoiceClipInsert, VoiceClipRow, VoiceSenderOptoutInsert, blob_status,
};
use crate::db::schema::{voice_blobs, voice_clips, voice_sender_optouts};

/// 一个 blob 的去重身份。账号是其中一维，不是附注——两个 bot 各自被拉进同一个
/// 群是两次独立的同意，共用一份文件会让删除其中一个牵连另一个。
#[derive(Debug, Clone)]
pub struct BlobKey<'a> {
    pub bot_self_id: i64,
    pub source_type: &'a str,
    pub source_id: &'a str,
    pub sha256: &'a str,
    pub file_format: &'a str,
}

/// 抢所有权的结果。
#[derive(Debug)]
pub enum ClaimOutcome {
    /// 本任务是 owner，可以发布文件。`token` 与 `epoch` 要一路带到 publish。
    Owned { id: String, token: String, epoch: i64 },
    /// 已经有一份发布好的。调用方仍要校验磁盘上那个文件确实对得上。
    Ready(VoiceBlobRow),
    /// 有人正在写，且 lease 未过期。退避重试。
    PendingElsewhere(VoiceBlobRow),
    /// 有人写坏了，或者写到一半死了而 lease 已过期。调用方可以接管。
    Takeable(VoiceBlobRow),
    /// 文件是坏的。不在采集路径上修——那是恢复器的事。
    Damaged(VoiceBlobRow),
    /// 墓碑。用户刚要求删掉这段音频，此时再存一份是违背意图；
    /// 墓碑清完之后同样的音频再来会正常采集。
    Deleting(VoiceBlobRow),
}

fn find_blob(conn: &mut SqliteConnection, key: &BlobKey<'_>) -> QueryResult<Option<VoiceBlobRow>> {
    voice_blobs::table
        .filter(voice_blobs::bot_self_id.eq(key.bot_self_id))
        .filter(voice_blobs::source_type.eq(key.source_type))
        .filter(voice_blobs::source_id.eq(key.source_id))
        .filter(voice_blobs::file_format.eq(key.file_format))
        .filter(voice_blobs::sha256.eq(key.sha256))
        .select(VoiceBlobRow::as_select())
        .first(conn)
        .optional()
}

fn classify(blob: VoiceBlobRow, now: i64) -> QueryResult<ClaimOutcome> {
    let status = VoiceBlobStatus::parse(&blob.status).map_err(|error| {
        diesel::result::Error::DeserializationError(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error,
        )))
    })?;
    Ok(match status {
        VoiceBlobStatus::Ready => ClaimOutcome::Ready(blob),
        VoiceBlobStatus::Damaged => ClaimOutcome::Damaged(blob),
        VoiceBlobStatus::Deleting => ClaimOutcome::Deleting(blob),
        VoiceBlobStatus::Pending => match blob.lease_expires_at {
            Some(expiry) if expiry <= now => ClaimOutcome::Takeable(blob),
            _ => ClaimOutcome::PendingElsewhere(blob),
        },
    })
}

/// 尝试成为这段音频的 owner。
///
/// 插入成功就是 owner；撞上唯一索引说明别人先到，读回那一行让调用方决定怎么办。
/// `id` 与 `token` 由调用方生成，因为它们要在失败重试时换新的。
pub fn claim_blob(
    conn: &mut SqliteConnection,
    key: &BlobKey<'_>,
    id: &str,
    token: &str,
    file_name: &str,
    file_size: i64,
    now: i64,
    lease_ms: i64,
) -> QueryResult<ClaimOutcome> {
    let inserted = diesel::insert_into(voice_blobs::table)
        .values(&VoiceBlobInsert {
            id,
            bot_self_id: key.bot_self_id,
            source_type: key.source_type,
            source_id: key.source_id,
            sha256: key.sha256,
            file_format: key.file_format,
            file_name,
            file_size,
            status: blob_status::PENDING,
            owner_token: Some(token),
            fence_epoch: 0,
            lease_expires_at: Some(now + lease_ms),
            created_at: now,
            updated_at: now,
        })
        .execute(conn);

    match inserted {
        Ok(_) => Ok(ClaimOutcome::Owned {
            id: id.to_string(),
            token: token.to_string(),
            epoch: 0,
        }),
        Err(diesel::result::Error::DatabaseError(diesel::result::DatabaseErrorKind::UniqueViolation, _)) => {
            match find_blob(conn, key)? {
                Some(blob) => classify(blob, now),
                // 插入撞了唯一索引，回读却没有——只可能是这中间被删掉了。
                // 当作可以重来，调用方会再转一圈。
                None => Ok(ClaimOutcome::Takeable(VoiceBlobRow {
                    id: id.to_string(),
                    bot_self_id: key.bot_self_id,
                    source_type: key.source_type.to_string(),
                    source_id: key.source_id.to_string(),
                    sha256: key.sha256.to_string(),
                    file_format: key.file_format.to_string(),
                    file_name: file_name.to_string(),
                    file_size,
                    status: blob_status::PENDING.to_string(),
                    owner_token: None,
                    fence_epoch: 0,
                    lease_expires_at: None,
                    created_at: now,
                    updated_at: now,
                })),
            }
        }
        Err(e) => Err(e),
    }
}

/// 接管一个 lease 已过期的 pending。
///
/// 条件里带 `observed_*` 是全部的意义所在：旧 owner 醒来时带的是旧 token 和旧
/// epoch，这个 UPDATE 对它不成立，它写不进去。返回 `None` 就是没抢到——别人
/// 刚刚接管了，或者它自己活过来了。
pub fn takeover_blob(
    conn: &mut SqliteConnection,
    blob_id: &str,
    observed_token: Option<&str>,
    observed_epoch: i64,
    new_token: &str,
    now: i64,
    lease_ms: i64,
) -> QueryResult<Option<i64>> {
    let new_epoch = observed_epoch + 1;
    let mut query = diesel::update(
        voice_blobs::table
            .filter(voice_blobs::id.eq(blob_id))
            .filter(voice_blobs::status.eq(blob_status::PENDING))
            .filter(voice_blobs::fence_epoch.eq(observed_epoch))
            .filter(voice_blobs::lease_expires_at.le(now)),
    )
    .into_boxed();
    query = match observed_token {
        Some(token) => query.filter(voice_blobs::owner_token.eq(token)),
        None => query.filter(voice_blobs::owner_token.is_null()),
    };
    let affected = query
        .set((
            voice_blobs::owner_token.eq(new_token),
            voice_blobs::fence_epoch.eq(new_epoch),
            voice_blobs::lease_expires_at.eq(now + lease_ms),
            voice_blobs::updated_at.eq(now),
        ))
        .execute(conn)?;
    Ok((affected == 1).then_some(new_epoch))
}

/// 续租。长下载期间调用，条件与 publish 相同。
pub fn renew_lease(
    conn: &mut SqliteConnection,
    blob_id: &str,
    token: &str,
    epoch: i64,
    now: i64,
    lease_ms: i64,
) -> QueryResult<bool> {
    let affected = diesel::update(
        voice_blobs::table
            .filter(voice_blobs::id.eq(blob_id))
            .filter(voice_blobs::status.eq(blob_status::PENDING))
            .filter(voice_blobs::owner_token.eq(token))
            .filter(voice_blobs::fence_epoch.eq(epoch)),
    )
    .set((
        voice_blobs::lease_expires_at.eq(now + lease_ms),
        voice_blobs::updated_at.eq(now),
    ))
    .execute(conn)?;
    Ok(affected == 1)
}

/// 发布：pending -> ready。
///
/// **返回 false 就是丢了所有权**，那个任务不能发布、也不能写 clip；它只能重读
/// 那一行，等它变 ready 之后走复用的路。这是 fencing 的落点。
pub fn publish_blob(
    conn: &mut SqliteConnection,
    blob_id: &str,
    token: &str,
    epoch: i64,
    now: i64,
) -> QueryResult<bool> {
    let affected = diesel::update(
        voice_blobs::table
            .filter(voice_blobs::id.eq(blob_id))
            .filter(voice_blobs::status.eq(blob_status::PENDING))
            .filter(voice_blobs::owner_token.eq(token))
            .filter(voice_blobs::fence_epoch.eq(epoch)),
    )
    .set((
        voice_blobs::status.eq(blob_status::READY),
        voice_blobs::owner_token.eq::<Option<String>>(None),
        voice_blobs::lease_expires_at.eq::<Option<i64>>(None),
        voice_blobs::updated_at.eq(now),
    ))
    .execute(conn)?;
    Ok(affected == 1)
}

/// 标成坏的。文件缺失或校验不过时用，不静默当正常。
pub fn mark_damaged(conn: &mut SqliteConnection, blob_id: &str, now: i64) -> QueryResult<usize> {
    diesel::update(voice_blobs::table.filter(voice_blobs::id.eq(blob_id)))
        .set((
            voice_blobs::status.eq(blob_status::DAMAGED),
            voice_blobs::owner_token.eq::<Option<String>>(None),
            voice_blobs::lease_expires_at.eq::<Option<i64>>(None),
            voice_blobs::updated_at.eq(now),
        ))
        .execute(conn)
}

/// 一次采集事件的落库结果。
#[derive(Debug, PartialEq, Eq)]
pub enum ClipOutcome {
    Inserted,
    /// 同一次出现已经在了，这次只把当时缺的转写补上。
    TranscriptFilled,
    /// 同一次出现已经在了，什么都不用做。
    AlreadyRecorded,
    /// 同一次出现指向的是**另一个** blob。保留先提交的那个；这次带来的 blob
    /// 没有任何 clip 引用，交给恢复器当孤儿清掉。
    KeptExisting,
}

/// 记一次采集。
///
/// 幂等：同一个 OneBot 事件重投不会写出第二行，也不会失败。
///
/// 冗余的账号/会话三列**从 blob 行读**而不是从参数传，这样两边不可能不一致。
#[allow(clippy::too_many_arguments)]
pub fn record_clip(
    conn: &mut SqliteConnection,
    blob: &VoiceBlobRow,
    id: &str,
    sender_id: &str,
    platform_message_id: Option<i64>,
    segment_index: i32,
    transcript: Option<&str>,
    transcript_source: Option<&str>,
    now: i64,
) -> QueryResult<ClipOutcome> {
    // 没有 message id 就没有"同一次出现"可言（唯一索引也是部分索引，只覆盖
    // 非 NULL 的）。直接插。
    let existing = match platform_message_id {
        Some(message_id) => voice_clips::table
            .filter(voice_clips::bot_self_id.eq(blob.bot_self_id))
            .filter(voice_clips::source_type.eq(&blob.source_type))
            .filter(voice_clips::source_id.eq(&blob.source_id))
            .filter(voice_clips::platform_message_id.eq(message_id))
            .filter(voice_clips::segment_index.eq(segment_index))
            .select(VoiceClipRow::as_select())
            .first(conn)
            .optional()?,
        None => None,
    };

    if let Some(existing) = existing {
        if existing.blob_id != blob.id {
            return Ok(ClipOutcome::KeptExisting);
        }
        if existing.transcript.is_none() && transcript.is_some() {
            diesel::update(voice_clips::table.filter(voice_clips::id.eq(&existing.id)))
                .set((
                    voice_clips::transcript.eq(transcript),
                    voice_clips::transcript_source.eq(transcript_source),
                    voice_clips::updated_at.eq(now),
                ))
                .execute(conn)?;
            return Ok(ClipOutcome::TranscriptFilled);
        }
        return Ok(ClipOutcome::AlreadyRecorded);
    }

    diesel::insert_into(voice_clips::table)
        .values(&VoiceClipInsert {
            id,
            blob_id: &blob.id,
            bot_self_id: blob.bot_self_id,
            source_type: &blob.source_type,
            source_id: &blob.source_id,
            sender_id,
            platform_message_id,
            segment_index,
            transcript,
            transcript_source,
            created_at: now,
            updated_at: now,
        })
        .execute(conn)?;
    Ok(ClipOutcome::Inserted)
}

// ---------------------------------------------------------------------------
// 删除
// ---------------------------------------------------------------------------

/// 把没有任何 clip 引用的 blob 标成墓碑，返回它们。
///
/// **这是共享 blob 的安全阀。** 多个发送者的 clip 可以指向同一个 blob，按发送者
/// 删除时直接墓碑化会连带删掉别人的合法样本，所以只碰真正没人引用的那些。
/// 调用方拿着返回的行去删文件，删成功了才 [`delete_blob_rows`]。
///
/// 三件事让它成立，而它们都是被同一个场景逼出来的——**另一个会话的采集正在同时
/// 跑**：
///
/// - 整个在一个 `BEGIN IMMEDIATE` 里。查完再无条件按 id 更新，中间那一瞬别人
///   插进来的 clip 会连同它刚落盘的 blob 一起被删掉（`ON DELETE CASCADE`）。
/// - UPDATE 自己也带 `NOT EXISTS`，回读只认真的转成墓碑的那些。写事务本该让上
///   一条足够，但一个只在注释里成立的前提，改天会被一次"顺手挪出事务"推翻。
/// - `damaged` 一并处理。它是"文件对不上"，不是"这行不算数"——留在外面，一个
///   没人引用的坏 blob 的文件永远不会被删掉。
pub fn tombstone_unreferenced(conn: &mut SqliteConnection, now: i64) -> QueryResult<Vec<VoiceBlobRow>> {
    let collectable = || {
        voice_blobs::table
            .filter(voice_blobs::status.eq_any([blob_status::READY, blob_status::DAMAGED]))
            .filter(diesel::dsl::not(diesel::dsl::exists(
                voice_clips::table.filter(voice_clips::blob_id.eq(voice_blobs::id)),
            )))
    };
    conn.immediate_transaction(|conn| {
        let doomed: Vec<String> = collectable().select(voice_blobs::id).load(conn)?;
        if doomed.is_empty() {
            return Ok(Vec::new());
        }
        diesel::update(collectable().filter(voice_blobs::id.eq_any(&doomed)))
            .set((
                voice_blobs::status.eq(blob_status::DELETING),
                voice_blobs::owner_token.eq::<Option<String>>(None),
                voice_blobs::lease_expires_at.eq::<Option<i64>>(None),
                voice_blobs::updated_at.eq(now),
            ))
            .execute(conn)?;
        voice_blobs::table
            .filter(voice_blobs::id.eq_any(&doomed))
            .filter(voice_blobs::status.eq(blob_status::DELETING))
            .select(VoiceBlobRow::as_select())
            .load(conn)
    })
}

/// 文件已经删掉了，行才走。失败的留着 `deleting` 给恢复器重试。
pub fn delete_blob_rows(conn: &mut SqliteConnection, ids: &[String]) -> QueryResult<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    diesel::delete(voice_blobs::table.filter(voice_blobs::id.eq_any(ids))).execute(conn)
}

pub fn delete_clips_by_sender(conn: &mut SqliteConnection, sender_id: &str) -> QueryResult<usize> {
    diesel::delete(voice_clips::table.filter(voice_clips::sender_id.eq(sender_id))).execute(conn)
}

pub fn delete_clips_by_session(
    conn: &mut SqliteConnection,
    bot_self_id: i64,
    source_type: &str,
    source_id: &str,
) -> QueryResult<usize> {
    diesel::delete(
        voice_clips::table
            .filter(voice_clips::bot_self_id.eq(bot_self_id))
            .filter(voice_clips::source_type.eq(source_type))
            .filter(voice_clips::source_id.eq(source_id)),
    )
    .execute(conn)
}

pub fn delete_all_clips(conn: &mut SqliteConnection) -> QueryResult<usize> {
    diesel::delete(voice_clips::table).execute(conn)
}

/// 待重试的墓碑，恢复器用。
pub fn tombstones(conn: &mut SqliteConnection) -> QueryResult<Vec<VoiceBlobRow>> {
    voice_blobs::table
        .filter(voice_blobs::status.eq(blob_status::DELETING))
        .select(VoiceBlobRow::as_select())
        .load(conn)
}

/// 启动时还停在 `pending` 的行。
///
/// 只有拿到了语料目录独占锁才该调用这个：那时"还有 owner 活着"是不可能的，
/// 剩下的必然是上一次进程死掉留下的。
pub fn stale_pending(conn: &mut SqliteConnection) -> QueryResult<Vec<VoiceBlobRow>> {
    voice_blobs::table
        .filter(voice_blobs::status.eq(blob_status::PENDING))
        .select(VoiceBlobRow::as_select())
        .load(conn)
}

/// 落了盘但没有任何 clip 指着它。
///
/// 采集在写完 blob 与写 clip 之间死掉就会留下一个。文件占着地方，而没有任何
/// 一次采集会承认它。`damaged` 也算：那是"文件对不上"，一个连 clip 都没有的
/// 坏 blob 没有任何东西还需要它。
pub fn orphaned(conn: &mut SqliteConnection) -> QueryResult<Vec<VoiceBlobRow>> {
    voice_blobs::table
        .filter(voice_blobs::status.eq_any([blob_status::READY, blob_status::DAMAGED]))
        .filter(diesel::dsl::not(diesel::dsl::exists(
            voice_clips::table.filter(voice_clips::blob_id.eq(voice_blobs::id)),
        )))
        .select(VoiceBlobRow::as_select())
        .load(conn)
}

/// 所有已发布的行，用来对着磁盘核一遍。
pub fn all_ready(conn: &mut SqliteConnection) -> QueryResult<Vec<VoiceBlobRow>> {
    voice_blobs::table
        .filter(voice_blobs::status.eq(blob_status::READY))
        .select(VoiceBlobRow::as_select())
        .load(conn)
}

/// 导出要读的行：每个 clip 连同它的 blob。
///
/// manifest 的每一行两边都要——转写和发送者在 clip 上，文件名和格式在 blob 上。
/// 只读 `ready` 的：`damaged` 的文件对不上，`deleting` 的正在消失。
pub fn export_rows(conn: &mut SqliteConnection) -> QueryResult<Vec<(VoiceClipRow, VoiceBlobRow)>> {
    voice_clips::table
        .inner_join(voice_blobs::table.on(voice_blobs::id.eq(voice_clips::blob_id)))
        .filter(voice_blobs::status.eq(blob_status::READY))
        .order(voice_clips::created_at.asc())
        .select((VoiceClipRow::as_select(), VoiceBlobRow::as_select()))
        .load(conn)
}

/// 每个会话有多少条还没有转写。设置页要能说"导出会跳过多少"。
pub fn untranscribed_by_session(conn: &mut SqliteConnection) -> QueryResult<std::collections::HashMap<String, i64>> {
    let rows: Vec<(i64, String, String)> = voice_clips::table
        .filter(voice_clips::transcript.is_null())
        .select((
            voice_clips::bot_self_id,
            voice_clips::source_type,
            voice_clips::source_id,
        ))
        .load(conn)?;
    let mut out = std::collections::HashMap::new();
    for (bot, kind, id) in rows {
        *out.entry(format!("{bot}|{kind}|{id}")).or_insert(0) += 1;
    }
    Ok(out)
}

/// 一个会话攒了多少语料。给设置页看的，所以**不含转写、不含发送者**——
/// 它回答的是"占了多少地方、要不要清"，不是"里面说了什么"。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionTotal {
    pub bot_self_id: i64,
    pub source_type: String,
    pub source_id: String,
    pub clips: i64,
    pub bytes: i64,
    pub last_captured_at: i64,
}

/// 在 Rust 里聚合而不是写 `GROUP BY`：语料按会话最多几千行，一次读完比一段
/// diesel 的聚合类型体操便宜得多，而这个函数只在设置页打开时调用一次。
///
/// **数的是 clip，不是 blob**，两个数在同一段音频被两个人发过的时候就分家了：
/// 按 blob 数，那个会话看起来少了一条，而"删掉这个会话"实际会带走两条。占用
/// 的字节则相反——一份文件只占一次地方，所以每个 blob 只加一次。
/// `last_captured_at` 也来自 clip：blob 的 `created_at` 是它**第一次**被存下来
/// 的时刻，此后同一段音频再被发一百次，那个会话也永远显示着几个月前。
///
/// **不借用 `export_rows`，因为两个调用者对 `damaged` 的答案相反。**导出跳过
/// 它是对的——文件对不上；但这里是删除的入口：`resolve_handle` 在这份列表上
/// 重算 HMAC，不在列表上的会话就没有句柄，clips 连同发送者和转写却都还在库
/// 里。恢复器把一个会话的文件全标成 `damaged` 之后，按 `ready` 过滤正好把
/// 最该被删的那批数据变成删不掉的。`deleting` 不用排——墓碑只立在没有任何
/// clip 引用的 blob 上，join 过 clips 之后它本来就贡献不了行。
pub fn session_totals(conn: &mut SqliteConnection) -> QueryResult<Vec<SessionTotal>> {
    use std::collections::{HashMap, HashSet};

    let rows: Vec<(VoiceClipRow, VoiceBlobRow)> = voice_clips::table
        .inner_join(voice_blobs::table.on(voice_blobs::id.eq(voice_clips::blob_id)))
        .filter(voice_blobs::status.eq_any([blob_status::READY, blob_status::DAMAGED]))
        .order(voice_clips::created_at.asc())
        .select((VoiceClipRow::as_select(), VoiceBlobRow::as_select()))
        .load(conn)?;

    let mut totals: HashMap<(i64, String, String), SessionTotal> = HashMap::new();
    let mut counted_blobs: HashSet<(i64, String, String, String)> = HashSet::new();
    for (clip, blob) in rows {
        let key = (blob.bot_self_id, blob.source_type.clone(), blob.source_id.clone());
        let entry = totals.entry(key.clone()).or_insert_with(|| SessionTotal {
            bot_self_id: blob.bot_self_id,
            source_type: blob.source_type.clone(),
            source_id: blob.source_id.clone(),
            clips: 0,
            bytes: 0,
            last_captured_at: 0,
        });
        entry.clips += 1;
        if counted_blobs.insert((key.0, key.1, key.2, blob.id.clone())) {
            entry.bytes += blob.file_size;
        }
        entry.last_captured_at = entry.last_captured_at.max(clip.created_at);
    }
    let mut out: Vec<SessionTotal> = totals.into_values().collect();
    out.sort_by_key(|b| std::cmp::Reverse(b.last_captured_at));
    Ok(out)
}

// ---------------------------------------------------------------------------
// opt-out
// ---------------------------------------------------------------------------

/// "以后别再录我"。全局，不按会话——一个人说了不录，不该要求他对每个群、
/// 每个 bot 账号再分别说一次。
pub fn set_optout(conn: &mut SqliteConnection, sender_id: &str, now: i64) -> QueryResult<usize> {
    diesel::insert_into(voice_sender_optouts::table)
        .values(&VoiceSenderOptoutInsert {
            sender_id,
            created_at: now,
        })
        .on_conflict(voice_sender_optouts::sender_id)
        .do_nothing()
        .execute(conn)
}

pub fn clear_optout(conn: &mut SqliteConnection, sender_id: &str) -> QueryResult<usize> {
    diesel::delete(voice_sender_optouts::table.filter(voice_sender_optouts::sender_id.eq(sender_id))).execute(conn)
}

/// 整份名单。采集路径每次都要查，而这张表只有拒绝过的人，通常是空的。
pub fn optouts(conn: &mut SqliteConnection) -> QueryResult<Vec<String>> {
    voice_sender_optouts::table
        .select(voice_sender_optouts::sender_id)
        .load(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn key<'a>(sha: &'a str) -> BlobKey<'a> {
        BlobKey {
            bot_self_id: 1,
            source_type: "onebot_group",
            source_id: "123",
            sha256: sha,
            file_format: "amr",
        }
    }

    fn own(conn: &mut SqliteConnection, sha: &str, id: &str, token: &str) -> (String, i64) {
        match claim_blob(conn, &key(sha), id, token, "f.amr", 10, 1_000, 60_000).unwrap() {
            ClaimOutcome::Owned { id, epoch, .. } => (id, epoch),
            other => panic!("expected to own it: {other:?}"),
        }
    }

    fn ready_blob(conn: &mut SqliteConnection, sha: &str, id: &str) -> VoiceBlobRow {
        let (_, epoch) = own(conn, sha, id, "t");
        assert!(publish_blob(conn, id, "t", epoch, 1_000).unwrap());
        find_blob(conn, &key(sha)).unwrap().unwrap()
    }

    /// 同一段音频的第二个采集任务认出别人已经有了它，而不是写第二份。
    #[test]
    fn a_second_claim_on_the_same_bytes_finds_the_first() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        own(&mut conn, "aa", "b1", "t1");

        match claim_blob(&mut conn, &key("aa"), "b2", "t2", "f.amr", 10, 1_000, 60_000).unwrap() {
            ClaimOutcome::PendingElsewhere(blob) => assert_eq!(blob.id, "b1"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unknown_persisted_blob_status_is_rejected() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let mut blob = ready_blob(&mut conn, "aa", "b1");
        blob.status = "archived".to_string();

        let error = classify(blob, 1_000).unwrap_err();
        assert!(error.to_string().contains("unknown voice blob status"), "{error}");
    }

    /// 另一个群里的同一段音频是另一份语料：删掉这个群的不该动那个群的。
    #[test]
    fn the_same_bytes_in_another_session_is_another_blob() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        own(&mut conn, "aa", "b1", "t1");

        let elsewhere = BlobKey {
            source_id: "456",
            ..key("aa")
        };
        match claim_blob(&mut conn, &elsewhere, "b2", "t2", "f.amr", 10, 1_000, 60_000).unwrap() {
            ClaimOutcome::Owned { id, .. } => assert_eq!(id, "b2"),
            other => panic!("{other:?}"),
        }
    }

    /// fencing 的落点：旧 owner 醒来时带的是旧 epoch，发布不进去。
    ///
    /// 没有这一条，同一个进程里两个任务会都以为自己是 owner——`owner_instance`
    /// 那版就是这么错的，进程 id 对同进程的两个任务是同一个值。
    #[test]
    fn a_fenced_out_owner_cannot_publish() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let (id, old_epoch) = own(&mut conn, "aa", "b1", "old");

        // lease 过期后被接管。
        let new_epoch = takeover_blob(&mut conn, &id, Some("old"), old_epoch, "new", 100_000, 60_000)
            .unwrap()
            .expect("takeover should win");
        assert_eq!(new_epoch, old_epoch + 1);

        assert!(
            !publish_blob(&mut conn, &id, "old", old_epoch, 200_000).unwrap(),
            "the old owner is fenced out"
        );
        assert!(publish_blob(&mut conn, &id, "new", new_epoch, 200_000).unwrap());
    }

    /// lease 还没过期时接管不了。
    #[test]
    fn a_live_lease_cannot_be_taken_over() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let (id, epoch) = own(&mut conn, "aa", "b1", "t1");
        assert!(
            takeover_blob(&mut conn, &id, Some("t1"), epoch, "t2", 1_500, 60_000)
                .unwrap()
                .is_none()
        );
    }

    /// 同一段音频被两个人发出来是**两行** clip，各自带着自己的发送者。
    /// 单表按 sha 唯一的那版会把第二个人整个丢掉。
    #[test]
    fn two_senders_of_one_recording_are_two_clips() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let blob = ready_blob(&mut conn, "aa", "b1");

        let first = record_clip(&mut conn, &blob, "c1", "alice", Some(10), 0, Some("你好"), Some("s"), 1).unwrap();
        let second = record_clip(&mut conn, &blob, "c2", "bob", Some(11), 0, Some("你好"), Some("s"), 2).unwrap();
        assert_eq!(first, ClipOutcome::Inserted);
        assert_eq!(second, ClipOutcome::Inserted);

        let senders: Vec<String> = voice_clips::table
            .order(voice_clips::created_at.asc())
            .select(voice_clips::sender_id)
            .load(&mut conn)
            .unwrap();
        assert_eq!(senders, vec!["alice", "bob"]);
    }

    /// 同一个事件重投不写第二行，而当时缺的转写会被补上——成对补，不允许
    /// 只有文本没有来源。
    #[test]
    fn a_replayed_event_fills_the_transcript_it_lacked() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let blob = ready_blob(&mut conn, "aa", "b1");

        record_clip(&mut conn, &blob, "c1", "alice", Some(10), 0, None, None, 1).unwrap();
        let again = record_clip(&mut conn, &blob, "c2", "alice", Some(10), 0, Some("你好"), Some("s"), 2).unwrap();
        assert_eq!(again, ClipOutcome::TranscriptFilled);

        let rows: Vec<VoiceClipRow> = voice_clips::table
            .select(VoiceClipRow::as_select())
            .load(&mut conn)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].transcript.as_deref(), Some("你好"));
        assert_eq!(rows[0].transcript_source.as_deref(), Some("s"));

        // 第三次什么都不做。
        let third = record_clip(&mut conn, &blob, "c3", "alice", Some(10), 0, Some("别的"), Some("s"), 3).unwrap();
        assert_eq!(third, ClipOutcome::AlreadyRecorded);
        let rows: Vec<VoiceClipRow> = voice_clips::table
            .select(VoiceClipRow::as_select())
            .load(&mut conn)
            .unwrap();
        assert_eq!(rows[0].transcript.as_deref(), Some("你好"), "先到的那份不被覆盖");
    }

    /// 一条消息里的两个 record 段是两行。
    #[test]
    fn two_segments_of_one_message_are_two_clips() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let a = ready_blob(&mut conn, "aa", "b1");
        let b = ready_blob(&mut conn, "bb", "b2");

        assert_eq!(
            record_clip(&mut conn, &a, "c1", "alice", Some(10), 0, None, None, 1).unwrap(),
            ClipOutcome::Inserted
        );
        assert_eq!(
            record_clip(&mut conn, &b, "c2", "alice", Some(10), 1, None, None, 1).unwrap(),
            ClipOutcome::Inserted
        );
    }

    /// 按发送者删除只该带走没人再引用的文件。两个人发过同一段音频时，删掉
    /// 一个人不能让另一个人的样本消失。
    #[test]
    fn deleting_one_sender_leaves_a_shared_recording_alone() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let blob = ready_blob(&mut conn, "aa", "b1");
        record_clip(&mut conn, &blob, "c1", "alice", Some(10), 0, None, None, 1).unwrap();
        record_clip(&mut conn, &blob, "c2", "bob", Some(11), 0, None, None, 1).unwrap();

        delete_clips_by_sender(&mut conn, "alice").unwrap();
        assert!(
            tombstone_unreferenced(&mut conn, 2).unwrap().is_empty(),
            "bob 还引用着它"
        );

        delete_clips_by_sender(&mut conn, "bob").unwrap();
        let doomed = tombstone_unreferenced(&mut conn, 3).unwrap();
        assert_eq!(doomed.len(), 1);
        assert_eq!(doomed[0].id, "b1");
    }

    /// 坏掉又没人引用的 blob 也要被收走。留在外面的话，它的文件永远删不掉——
    /// 墓碑扫描只看 `ready`，而"坏"恰恰意味着这一行不会再变回 `ready`。
    #[test]
    fn a_damaged_recording_nobody_references_is_collected_too() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let blob = ready_blob(&mut conn, "aa", "b1");
        mark_damaged(&mut conn, &blob.id, 2).unwrap();

        let doomed = tombstone_unreferenced(&mut conn, 3).unwrap();
        assert_eq!(doomed.len(), 1);
        assert_eq!(doomed[0].id, "b1");
        assert_eq!(doomed[0].status, blob_status::DELETING);
    }

    /// 有 clip 指着的坏行**不动**：那份记录记着谁在什么时候说过话，
    /// 而它不该因为文件坏了就消失。
    #[test]
    fn a_damaged_recording_someone_still_references_stays() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let blob = ready_blob(&mut conn, "aa", "b1");
        record_clip(&mut conn, &blob, "c1", "alice", Some(10), 0, None, None, 1).unwrap();
        mark_damaged(&mut conn, &blob.id, 2).unwrap();

        assert!(tombstone_unreferenced(&mut conn, 3).unwrap().is_empty());
    }

    /// 统计数的是 clip，不是 blob。两个人发过同一段音频时两个数就分家了：
    /// 会话看起来少一条，而"删掉这个会话"实际会带走两条。字节反过来——
    /// 一份文件只占一次地方。
    #[test]
    fn a_shared_recording_counts_twice_but_takes_up_room_once() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let blob = ready_blob(&mut conn, "aa", "b1");
        record_clip(&mut conn, &blob, "c1", "alice", Some(10), 0, None, None, 1).unwrap();
        record_clip(&mut conn, &blob, "c2", "bob", Some(11), 0, None, None, 5).unwrap();

        let totals = session_totals(&mut conn).unwrap();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].clips, 2, "两次出现是两条");
        assert_eq!(totals[0].bytes, 10, "一份文件只占一次地方");
        assert_eq!(totals[0].last_captured_at, 5, "最近一次来自 clip，不是 blob 建立的时刻");
    }

    /// 文件坏了，会话不能从管理列表上消失：`resolve_handle` 在这份统计上重算
    /// HMAC，不在列表上的会话就没有句柄，clips 连同发送者和转写却都还在库里。
    /// 按 `ready` 过滤正好把最该被删的那批数据变成删不掉的。
    #[test]
    fn a_session_whose_files_all_went_bad_is_still_listed_for_deletion() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let blob = ready_blob(&mut conn, "aa", "b1");
        record_clip(&mut conn, &blob, "c1", "alice", Some(10), 0, None, None, 1).unwrap();
        mark_damaged(&mut conn, &blob.id, 2).unwrap();

        let totals = session_totals(&mut conn).unwrap();
        assert_eq!(totals.len(), 1, "damaged 只挡导出，不挡删除");
        assert_eq!(totals[0].clips, 1);

        // 导出这边照旧跳过它——两个调用者对 damaged 的答案相反，
        // 这正是统计不借用 export_rows 的原因。
        assert!(export_rows(&mut conn).unwrap().is_empty());
    }

    #[test]
    fn an_optout_is_global_and_idempotent() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        set_optout(&mut conn, "alice", 1).unwrap();
        set_optout(&mut conn, "alice", 2).unwrap();
        assert_eq!(optouts(&mut conn).unwrap(), vec!["alice"]);
        clear_optout(&mut conn, "alice").unwrap();
        assert!(optouts(&mut conn).unwrap().is_empty());
    }
}
