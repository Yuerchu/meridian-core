//! 语料的读写：抢所有权、发布、记一次采集、删除。
//!
//! 这一层的形状由一件事决定——**文件先于行存在**。要落盘的字节必须先下载完
//! 才知道它的 sha 和大小，而 blob 行的去重键就建在 sha 上。所以流程是"先写
//! 临时文件算出 sha，再拿 sha 来抢所有权，最后发布"，而不是反过来。
//!
//! 抢所有权用 token + epoch 而不是进程 id，理由在迁移 39 的注释里：同一个
//! 进程内两个任务的 `CAS WHERE owner=旧值` 会写回相同的值并双双成功。
//!
//! 这里没有一个函数自己开事务：写一律拿调用方的 `WriteTx`，调用方的
//! `Db::write` 就是那个 `BEGIN IMMEDIATE`。授权复查、抢所有权、改名、记 clip
//! 之所以能在一把锁下面完成，靠的就是这一条。

use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::sea_query::{Expr, ExprTrait, OnConflict, Query, SelectStatement};
use sea_orm::{ColumnTrait, Condition, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use crate::db::entity::voice_blob::{VoiceBlobStatus, VoiceCorpusSourceType};
use crate::db::entity::{voice_blob, voice_clip, voice_sender_optout};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, Snapshot, WriteTx};
use crate::db::types::EpochMs;

/// 一个 blob 的去重身份。账号是其中一维，不是附注——两个 bot 各自被拉进同一个
/// 群是两次独立的同意，共用一份文件会让删除其中一个牵连另一个。
#[derive(Debug, Clone)]
pub struct BlobKey<'a> {
    pub bot_self_id: i64,
    pub source_type: VoiceCorpusSourceType,
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
    Ready(voice_blob::Model),
    /// 有人正在写，且 lease 未过期。退避重试。
    PendingElsewhere(voice_blob::Model),
    /// 有人写坏了，或者写到一半死了而 lease 已过期。调用方可以接管。
    Takeable(voice_blob::Model),
    /// 文件是坏的。不在采集路径上修——那是恢复器的事。
    Damaged(voice_blob::Model),
    /// 墓碑。用户刚要求删掉这段音频，此时再存一份是违背意图；
    /// 墓碑清完之后同样的音频再来会正常采集。
    Deleting(voice_blob::Model),
}

fn by_key(key: &BlobKey<'_>) -> Condition {
    Condition::all()
        .add(voice_blob::Column::BotSelfId.eq(key.bot_self_id))
        .add(voice_blob::Column::SourceType.eq(key.source_type))
        .add(voice_blob::Column::SourceId.eq(key.source_id))
        .add(voice_blob::Column::FileFormat.eq(key.file_format))
        .add(voice_blob::Column::Sha256.eq(key.sha256))
}

pub async fn find_blob(db: &impl Read, key: &BlobKey<'_>) -> Result<Option<voice_blob::Model>, DbErr> {
    voice_blob::Entity::find().filter(by_key(key)).one(db.conn()?).await
}

fn classify(blob: voice_blob::Model, now: EpochMs) -> ClaimOutcome {
    match blob.status {
        VoiceBlobStatus::Ready => ClaimOutcome::Ready(blob),
        VoiceBlobStatus::Damaged => ClaimOutcome::Damaged(blob),
        VoiceBlobStatus::Deleting => ClaimOutcome::Deleting(blob),
        VoiceBlobStatus::Pending => match blob.lease_expires_at {
            Some(expiry) if expiry <= now => ClaimOutcome::Takeable(blob),
            _ => ClaimOutcome::PendingElsewhere(blob),
        },
    }
}

/// 尝试成为这段音频的 owner。
///
/// 插入成功就是 owner；撞上去重键说明别人先到，读回那一行让调用方决定怎么办。
/// `id` 与 `token` 由调用方生成，因为它们要在失败重试时换新的。
///
/// 用 `INSERT … ON CONFLICT DO NOTHING` 再看影响行数，而不是插入后接住唯一
/// 索引的报错：一条失败的 INSERT 在 SQLite 上对事务无害，在 PostgreSQL 上会
/// 把整个事务打成 aborted，后面的每一条语句都跟着失败。靠报错分支是一个只在
/// 一种后端上成立的写法。
#[allow(clippy::too_many_arguments)]
pub async fn claim_blob(
    tx: &WriteTx,
    key: &BlobKey<'_>,
    id: &str,
    token: &str,
    file_name: &str,
    file_size: i64,
    now: EpochMs,
    lease_ms: i64,
) -> Result<ClaimOutcome, DbErr> {
    let row = voice_blob::ActiveModel {
        id: Set(id.to_owned()),
        bot_self_id: Set(key.bot_self_id),
        source_type: Set(key.source_type),
        source_id: Set(key.source_id.to_owned()),
        sha256: Set(key.sha256.to_owned()),
        file_format: Set(key.file_format.to_owned()),
        file_name: Set(file_name.to_owned()),
        file_size: Set(file_size),
        status: Set(VoiceBlobStatus::Pending),
        owner_token: Set(Some(token.to_owned())),
        fence_epoch: Set(0),
        lease_expires_at: Set(Some(now + lease_ms)),
        created_at: Set(now),
        updated_at: Set(now),
    };
    let inserted = voice_blob::Entity::insert(row)
        .on_conflict(
            OnConflict::columns([
                voice_blob::Column::BotSelfId,
                voice_blob::Column::SourceType,
                voice_blob::Column::SourceId,
                voice_blob::Column::FileFormat,
                voice_blob::Column::Sha256,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(tx.conn()?)
        .await?;
    if inserted > 0 {
        return Ok(ClaimOutcome::Owned {
            id: id.to_owned(),
            token: token.to_owned(),
            epoch: 0,
        });
    }
    match find_blob(tx, key).await? {
        Some(blob) => Ok(classify(blob, now)),
        // 插入撞了去重键，回读却没有——只可能是这中间被删掉了。
        // 当作可以重来，调用方会再转一圈。
        None => Ok(ClaimOutcome::Takeable(voice_blob::Model {
            id: id.to_owned(),
            bot_self_id: key.bot_self_id,
            source_type: key.source_type,
            source_id: key.source_id.to_owned(),
            sha256: key.sha256.to_owned(),
            file_format: key.file_format.to_owned(),
            file_name: file_name.to_owned(),
            file_size,
            status: VoiceBlobStatus::Pending,
            owner_token: None,
            fence_epoch: 0,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        })),
    }
}

/// 接管一个 lease 已过期的 pending。
///
/// 条件里带 `observed_*` 是全部的意义所在：旧 owner 醒来时带的是旧 token 和旧
/// epoch，这个 UPDATE 对它不成立，它写不进去。返回 `None` 就是没抢到——别人
/// 刚刚接管了，或者它自己活过来了。
pub async fn takeover_blob(
    tx: &WriteTx,
    blob_id: &str,
    observed_token: Option<&str>,
    observed_epoch: i64,
    new_token: &str,
    now: EpochMs,
    lease_ms: i64,
) -> Result<Option<i64>, DbErr> {
    let new_epoch = observed_epoch + 1;
    let owner = match observed_token {
        Some(token) => voice_blob::Column::OwnerToken.eq(token),
        None => voice_blob::Column::OwnerToken.is_null(),
    };
    let affected = voice_blob::Entity::update_many()
        .set(voice_blob::ActiveModel {
            owner_token: Set(Some(new_token.to_owned())),
            fence_epoch: Set(new_epoch),
            lease_expires_at: Set(Some(now + lease_ms)),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(voice_blob::Column::Id.eq(blob_id))
        .filter(voice_blob::Column::Status.eq(VoiceBlobStatus::Pending))
        .filter(voice_blob::Column::FenceEpoch.eq(observed_epoch))
        .filter(voice_blob::Column::LeaseExpiresAt.lte(now))
        .filter(owner)
        .exec(tx.conn()?)
        .await?
        .rows_affected;
    Ok((affected == 1).then_some(new_epoch))
}

/// 续租。长下载期间调用，条件与 publish 相同。
pub async fn renew_lease(
    tx: &WriteTx,
    blob_id: &str,
    token: &str,
    epoch: i64,
    now: EpochMs,
    lease_ms: i64,
) -> Result<bool, DbErr> {
    let affected = voice_blob::Entity::update_many()
        .set(voice_blob::ActiveModel {
            lease_expires_at: Set(Some(now + lease_ms)),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(voice_blob::Column::Id.eq(blob_id))
        .filter(voice_blob::Column::Status.eq(VoiceBlobStatus::Pending))
        .filter(voice_blob::Column::OwnerToken.eq(token))
        .filter(voice_blob::Column::FenceEpoch.eq(epoch))
        .exec(tx.conn()?)
        .await?
        .rows_affected;
    Ok(affected == 1)
}

/// 发布：pending -> ready。
///
/// **返回 false 就是丢了所有权**，那个任务不能发布、也不能写 clip；它只能重读
/// 那一行，等它变 ready 之后走复用的路。这是 fencing 的落点。
pub async fn publish_blob(tx: &WriteTx, blob_id: &str, token: &str, epoch: i64, now: EpochMs) -> Result<bool, DbErr> {
    let affected = voice_blob::Entity::update_many()
        .set(voice_blob::ActiveModel {
            status: Set(VoiceBlobStatus::Ready),
            owner_token: Set(None),
            lease_expires_at: Set(None),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(voice_blob::Column::Id.eq(blob_id))
        .filter(voice_blob::Column::Status.eq(VoiceBlobStatus::Pending))
        .filter(voice_blob::Column::OwnerToken.eq(token))
        .filter(voice_blob::Column::FenceEpoch.eq(epoch))
        .exec(tx.conn()?)
        .await?
        .rows_affected;
    Ok(affected == 1)
}

/// 标成坏的。文件缺失或校验不过时用，不静默当正常。
pub async fn mark_damaged(tx: &WriteTx, blob_id: &str, now: EpochMs) -> Result<u64, DbErr> {
    Ok(voice_blob::Entity::update_many()
        .set(voice_blob::ActiveModel {
            status: Set(VoiceBlobStatus::Damaged),
            owner_token: Set(None),
            lease_expires_at: Set(None),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(voice_blob::Column::Id.eq(blob_id))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
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
/// 幂等：同一个 OneBot 事件重投不会写出第二行，也不会失败——先查后插的两步
/// 在调用方的写事务里，中间插不进第二个同样的事件。
///
/// 冗余的账号/会话三列**从 blob 行读**而不是从参数传，这样两边不可能不一致。
#[allow(clippy::too_many_arguments)]
pub async fn record_clip(
    tx: &WriteTx,
    blob: &voice_blob::Model,
    id: &str,
    sender_id: &str,
    platform_message_id: Option<i64>,
    segment_index: i32,
    transcript: Option<&str>,
    transcript_source: Option<&str>,
    now: EpochMs,
) -> Result<ClipOutcome, DbErr> {
    // 没有 message id 就没有"同一次出现"可言（唯一索引也是部分索引，只覆盖
    // 非 NULL 的）。直接插。
    let existing = match platform_message_id {
        Some(message_id) => {
            voice_clip::Entity::find()
                .filter(voice_clip::Column::BotSelfId.eq(blob.bot_self_id))
                .filter(voice_clip::Column::SourceType.eq(blob.source_type))
                .filter(voice_clip::Column::SourceId.eq(blob.source_id.as_str()))
                .filter(voice_clip::Column::PlatformMessageId.eq(message_id))
                .filter(voice_clip::Column::SegmentIndex.eq(segment_index))
                .one(tx.conn()?)
                .await?
        }
        None => None,
    };

    if let Some(existing) = existing {
        if existing.blob_id != blob.id {
            return Ok(ClipOutcome::KeptExisting);
        }
        if existing.transcript.is_none() && transcript.is_some() {
            voice_clip::Entity::update(voice_clip::ActiveModel {
                id: Unchanged(existing.id),
                transcript: Set(transcript.map(str::to_owned)),
                transcript_source: Set(transcript_source.map(str::to_owned)),
                updated_at: Set(now),
                ..Default::default()
            })
            .exec_without_returning(tx.conn()?)
            .await?;
            return Ok(ClipOutcome::TranscriptFilled);
        }
        return Ok(ClipOutcome::AlreadyRecorded);
    }

    voice_clip::Entity::insert(voice_clip::ActiveModel {
        id: Set(id.to_owned()),
        blob_id: Set(blob.id.clone()),
        bot_self_id: Set(blob.bot_self_id),
        source_type: Set(blob.source_type),
        source_id: Set(blob.source_id.clone()),
        sender_id: Set(sender_id.to_owned()),
        platform_message_id: Set(platform_message_id),
        segment_index: Set(segment_index),
        transcript: Set(transcript.map(str::to_owned)),
        transcript_source: Set(transcript_source.map(str::to_owned)),
        created_at: Set(now),
        updated_at: Set(now),
    })
    .exec_without_returning(tx.conn()?)
    .await?;
    Ok(ClipOutcome::Inserted)
}

// ---------------------------------------------------------------------------
// 删除
// ---------------------------------------------------------------------------

/// `SELECT 1 FROM voice_clips WHERE voice_clips.blob_id = voice_blobs.id`，
/// 给 `NOT EXISTS` 用的关联子查询。
fn clips_of_this_blob() -> SelectStatement {
    Query::select()
        .expr(Expr::val(1))
        .from(voice_clip::Entity)
        .and_where(
            Expr::col((voice_clip::Entity, voice_clip::Column::BlobId))
                .equals((voice_blob::Entity, voice_blob::Column::Id)),
        )
        .to_owned()
}

/// 已发布或已标坏、且没有任何 clip 引用的行。
fn unreferenced() -> Condition {
    Condition::all()
        .add(voice_blob::Column::Status.is_in([VoiceBlobStatus::Ready, VoiceBlobStatus::Damaged]))
        .add(Expr::exists(clips_of_this_blob()).not())
}

/// 把没有任何 clip 引用的 blob 标成墓碑，返回它们。
///
/// **这是共享 blob 的安全阀。** 多个发送者的 clip 可以指向同一个 blob，按发送者
/// 删除时直接墓碑化会连带删掉别人的合法样本，所以只碰真正没人引用的那些。
/// 调用方拿着返回的行去删文件，删成功了才 [`delete_blob_rows`]。
///
/// 三件事让它成立，而它们都是被同一个场景逼出来的——**另一个会话的采集正在同时
/// 跑**：
///
/// - 整个在调用方的一个 `BEGIN IMMEDIATE` 里（`Db::write`），和删 clip 的那一步
///   同一个事务。查完再按 id 更新，中间那一瞬别人插进来的 clip 会连同它刚落盘
///   的 blob 一起被删掉（`ON DELETE CASCADE`）。
/// - UPDATE 自己也带 `NOT EXISTS`，回读只认真的转成墓碑的那些。写事务本该让上
///   一条足够，但一个只在注释里成立的前提，改天会被一次"顺手挪出事务"推翻。
/// - `damaged` 一并处理。它是"文件对不上"，不是"这行不算数"——留在外面，一个
///   没人引用的坏 blob 的文件永远不会被删掉。
pub async fn tombstone_unreferenced(tx: &WriteTx, now: EpochMs) -> Result<Vec<voice_blob::Model>, DbErr> {
    let doomed = collectable_ids(tx).await?;
    if doomed.is_empty() {
        return Ok(Vec::new());
    }
    tombstone_ids(tx, &doomed, now).await
}

/// 第一步：此刻没人引用的 id。
async fn collectable_ids(tx: &WriteTx) -> Result<Vec<String>, DbErr> {
    voice_blob::Entity::find()
        .select_only()
        .column(voice_blob::Column::Id)
        .filter(unreferenced())
        .into_tuple()
        .all(tx.conn()?)
        .await
}

/// 第二步：把这些 id 里**仍然**没人引用的转成墓碑，回读真的转成了的那些。
/// 条件里再带一次 `NOT EXISTS`，就是上面说的第二条。
async fn tombstone_ids(tx: &WriteTx, doomed: &[String], now: EpochMs) -> Result<Vec<voice_blob::Model>, DbErr> {
    voice_blob::Entity::update_many()
        .set(voice_blob::ActiveModel {
            status: Set(VoiceBlobStatus::Deleting),
            owner_token: Set(None),
            lease_expires_at: Set(None),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(voice_blob::Column::Id.is_in(doomed.iter().map(String::as_str)))
        .filter(unreferenced())
        .exec(tx.conn()?)
        .await?;
    voice_blob::Entity::find()
        .filter(voice_blob::Column::Id.is_in(doomed.iter().map(String::as_str)))
        .filter(voice_blob::Column::Status.eq(VoiceBlobStatus::Deleting))
        .all(tx.conn()?)
        .await
}

/// 文件已经删掉了，行才走。失败的留着 `deleting` 给恢复器重试。
pub async fn delete_blob_rows(tx: &WriteTx, ids: &[String]) -> Result<u64, DbErr> {
    if ids.is_empty() {
        return Ok(0);
    }
    Ok(voice_blob::Entity::delete_many()
        .filter(voice_blob::Column::Id.is_in(ids.iter().map(String::as_str)))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

pub async fn delete_clips_by_sender(tx: &WriteTx, sender_id: &str) -> Result<u64, DbErr> {
    Ok(voice_clip::Entity::delete_many()
        .filter(voice_clip::Column::SenderId.eq(sender_id))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

pub async fn delete_clips_by_session(
    tx: &WriteTx,
    bot_self_id: i64,
    source_type: VoiceCorpusSourceType,
    source_id: &str,
) -> Result<u64, DbErr> {
    Ok(voice_clip::Entity::delete_many()
        .filter(voice_clip::Column::BotSelfId.eq(bot_self_id))
        .filter(voice_clip::Column::SourceType.eq(source_type))
        .filter(voice_clip::Column::SourceId.eq(source_id))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

pub async fn delete_all_clips(tx: &WriteTx) -> Result<u64, DbErr> {
    Ok(voice_clip::Entity::delete_many().exec(tx.conn()?).await?.rows_affected)
}

/// 待重试的墓碑，恢复器用。
pub async fn tombstones(db: &impl Read) -> Result<Vec<voice_blob::Model>, DbErr> {
    voice_blob::Entity::find()
        .filter(voice_blob::Column::Status.eq(VoiceBlobStatus::Deleting))
        .all(db.conn()?)
        .await
}

/// 启动时还停在 `pending` 的行。
///
/// 只有拿到了语料目录独占锁才该调用这个：那时"还有 owner 活着"是不可能的，
/// 剩下的必然是上一次进程死掉留下的。
pub async fn stale_pending(db: &impl Read) -> Result<Vec<voice_blob::Model>, DbErr> {
    voice_blob::Entity::find()
        .filter(voice_blob::Column::Status.eq(VoiceBlobStatus::Pending))
        .all(db.conn()?)
        .await
}

/// 落了盘但没有任何 clip 指着它。
///
/// 采集在写完 blob 与写 clip 之间死掉就会留下一个。文件占着地方，而没有任何
/// 一次采集会承认它。`damaged` 也算：那是"文件对不上"，一个连 clip 都没有的
/// 坏 blob 没有任何东西还需要它。
pub async fn orphaned(db: &impl Read) -> Result<Vec<voice_blob::Model>, DbErr> {
    voice_blob::Entity::find().filter(unreferenced()).all(db.conn()?).await
}

/// 所有已发布的行，用来对着磁盘核一遍。
pub async fn all_ready(db: &impl Read) -> Result<Vec<voice_blob::Model>, DbErr> {
    voice_blob::Entity::find()
        .filter(voice_blob::Column::Status.eq(VoiceBlobStatus::Ready))
        .all(db.conn()?)
        .await
}

/// 每一行，不论状态。恢复器拿它算出"库里认领了哪些路径"，剩下的都是野文件。
pub async fn all_blobs(db: &impl Read) -> Result<Vec<voice_blob::Model>, DbErr> {
    voice_blob::Entity::find().all(db.conn()?).await
}

/// 每个 clip 连同它的 blob，只要 blob 的状态在 `statuses` 里，按 clip 的
/// `created_at` 升序。
///
/// **两个查询在 Rust 里拼，不用 `find_also_related`。** 那是 LEFT JOIN，右边的
/// 行用 `from_query_result_optional` 解码——任何解码失败都变成 `None`，包括一个
/// 认不出来的 `source_type`。于是一行坏数据读出来不是错误，而是"这条 clip 没有
/// blob"，正是这个 crate 到处拒绝的那种静默默认。分开读，blob 那一边的解码
/// 失败就是整个查询的失败。`blob_id` 非空且级联，所以一条没有 blob 的 clip
/// 是坏库，不是空答案。
///
/// **两个查询必须落在同一个快照上，所以参数是 [`Snapshot`] 而不是池。** 在池上
/// 各自自动提交时，后台采集在两次读之间提交一个新 blob 和它的 clip，第二次读
/// 看得见 clip、第一次读的 blob 表里却没有它——上面那个"坏库"报错就会在一个
/// 好好的库上冒出来，设置页的列表和导出随采集节奏间歇失败。
async fn clips_with_blobs(
    db: &impl Snapshot,
    statuses: &[VoiceBlobStatus],
) -> Result<Vec<(voice_clip::Model, voice_blob::Model)>, DbErr> {
    let blobs: std::collections::HashMap<String, voice_blob::Model> = all_blobs(db)
        .await?
        .into_iter()
        .map(|blob| (blob.id.clone(), blob))
        .collect();
    voice_clip::Entity::find()
        .order_by_asc(voice_clip::Column::CreatedAt)
        .all(db.conn()?)
        .await?
        .into_iter()
        .filter_map(|clip| match blobs.get(&clip.blob_id) {
            Some(blob) if statuses.contains(&blob.status) => Some(Ok((clip, blob.clone()))),
            Some(_) => None,
            None => Some(Err(DbErr::RecordNotFound(format!(
                "voice_blobs row `{}` of clip `{}`",
                clip.blob_id, clip.id
            )))),
        })
        .collect()
}

/// 导出要读的行：每个 clip 连同它的 blob。
///
/// manifest 的每一行两边都要——转写和发送者在 clip 上，文件名和格式在 blob 上。
/// 只读 `ready` 的：`damaged` 的文件对不上，`deleting` 的正在消失。
pub async fn export_rows(db: &impl Snapshot) -> Result<Vec<(voice_clip::Model, voice_blob::Model)>, DbErr> {
    clips_with_blobs(db, &[VoiceBlobStatus::Ready]).await
}

/// 每个会话有多少条还没有转写。设置页要能说"导出会跳过多少"。
///
/// 键是 `"<bot>|<source_type>|<source_id>"`，`source_type` 是它的存储拼写。
pub async fn untranscribed_by_session(db: &impl Read) -> Result<std::collections::HashMap<String, i64>, DbErr> {
    let rows: Vec<(i64, VoiceCorpusSourceType, String)> = voice_clip::Entity::find()
        .select_only()
        .column(voice_clip::Column::BotSelfId)
        .column(voice_clip::Column::SourceType)
        .column(voice_clip::Column::SourceId)
        .filter(voice_clip::Column::Transcript.is_null())
        .into_tuple()
        .all(db.conn()?)
        .await?;
    let mut out = std::collections::HashMap::new();
    for (bot, kind, id) in rows {
        *out.entry(format!("{bot}|{}|{id}", kind.as_str())).or_insert(0) += 1;
    }
    Ok(out)
}

/// 一个会话攒了多少语料。给设置页看的，所以**不含转写、不含发送者**——
/// 它回答的是"占了多少地方、要不要清"，不是"里面说了什么"。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionTotal {
    pub bot_self_id: i64,
    pub source_type: VoiceCorpusSourceType,
    pub source_id: String,
    pub clips: i64,
    pub bytes: i64,
    pub last_captured_at: i64,
}

/// 在 Rust 里聚合而不是写 `GROUP BY`：语料按会话最多几千行，一次读完比一段
/// ORM 的聚合类型体操便宜得多，而这个函数只在设置页打开时调用一次。
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
pub async fn session_totals(db: &impl Snapshot) -> Result<Vec<SessionTotal>, DbErr> {
    use std::collections::{HashMap, HashSet};

    let rows = clips_with_blobs(db, &[VoiceBlobStatus::Ready, VoiceBlobStatus::Damaged]).await?;

    let mut totals: HashMap<(i64, VoiceCorpusSourceType, String), SessionTotal> = HashMap::new();
    let mut counted_blobs: HashSet<(i64, VoiceCorpusSourceType, String, String)> = HashSet::new();
    for (clip, blob) in rows {
        let key = (blob.bot_self_id, blob.source_type, blob.source_id.clone());
        let entry = totals.entry(key.clone()).or_insert_with(|| SessionTotal {
            bot_self_id: blob.bot_self_id,
            source_type: blob.source_type,
            source_id: blob.source_id.clone(),
            clips: 0,
            bytes: 0,
            last_captured_at: 0,
        });
        entry.clips += 1;
        if counted_blobs.insert((key.0, key.1, key.2, blob.id.clone())) {
            entry.bytes += blob.file_size;
        }
        entry.last_captured_at = std::cmp::max(entry.last_captured_at, clip.created_at);
    }
    let mut out: Vec<SessionTotal> = totals.into_values().collect();
    out.sort_by_key(|b| std::cmp::Reverse(b.last_captured_at));
    Ok(out)
}

// ---------------------------------------------------------------------------
// opt-out
// ---------------------------------------------------------------------------

/// "以后别再录我"。全局，不按会话——一个人说了不录，不该要求他对每个群、
/// 每个 bot 账号再分别说一次。重复写不是错误。
pub async fn set_optout(tx: &WriteTx, sender_id: &str, now: EpochMs) -> Result<u64, DbErr> {
    voice_sender_optout::Entity::insert(voice_sender_optout::ActiveModel {
        sender_id: Set(sender_id.to_owned()),
        created_at: Set(now),
    })
    .on_conflict(
        OnConflict::column(voice_sender_optout::Column::SenderId)
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(tx.conn()?)
    .await
}

pub async fn clear_optout(tx: &WriteTx, sender_id: &str) -> Result<u64, DbErr> {
    Ok(voice_sender_optout::Entity::delete_by_id(sender_id)
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// 整份名单。采集路径每次都要查，而这张表只有拒绝过的人，通常是空的。
pub async fn optouts(db: &impl Read) -> Result<Vec<String>, DbErr> {
    voice_sender_optout::Entity::find()
        .select_only()
        .column(voice_sender_optout::Column::SenderId)
        .into_tuple()
        .all(db.conn()?)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    fn key(sha: &str) -> BlobKey<'_> {
        BlobKey {
            bot_self_id: 1,
            source_type: VoiceCorpusSourceType::OnebotGroup,
            source_id: "123",
            sha256: sha,
            file_format: "amr",
        }
    }

    async fn claim(db: &Db, key: BlobKey<'_>, id: &str, token: &str) -> ClaimOutcome {
        db.write(async |tx| claim_blob(tx, &key, id, token, "f.amr", 10, 1_000, 60_000).await)
            .await
            .unwrap()
    }

    async fn own(db: &Db, sha: &str, id: &str, token: &str) -> (String, i64) {
        match claim(db, key(sha), id, token).await {
            ClaimOutcome::Owned { id, epoch, .. } => (id, epoch),
            other => panic!("expected to own it: {other:?}"),
        }
    }

    async fn publish(db: &Db, id: &str, token: &str, epoch: i64, now: EpochMs) -> bool {
        db.write(async |tx| publish_blob(tx, id, token, epoch, now).await)
            .await
            .unwrap()
    }

    async fn ready_blob(db: &Db, sha: &str, id: &str) -> voice_blob::Model {
        let (_, epoch) = own(db, sha, id, "t").await;
        assert!(publish(db, id, "t", epoch, 1_000).await);
        find_blob(db, &key(sha)).await.unwrap().unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    async fn clip(
        db: &Db,
        blob: &voice_blob::Model,
        id: &str,
        sender: &str,
        message_id: Option<i64>,
        segment: i32,
        transcript: Option<&str>,
        now: EpochMs,
    ) -> ClipOutcome {
        db.write(async |tx| {
            record_clip(
                tx,
                blob,
                id,
                sender,
                message_id,
                segment,
                transcript,
                transcript.map(|_| "s"),
                now,
            )
            .await
        })
        .await
        .unwrap()
    }

    async fn clips(db: &Db) -> Vec<voice_clip::Model> {
        voice_clip::Entity::find()
            .order_by_asc(voice_clip::Column::CreatedAt)
            .all(db.conn().unwrap())
            .await
            .unwrap()
    }

    async fn damage(db: &Db, id: &str, now: EpochMs) {
        db.write(async |tx| mark_damaged(tx, id, now).await).await.unwrap();
    }

    async fn tombstone(db: &Db, now: EpochMs) -> Vec<voice_blob::Model> {
        db.write(async |tx| tombstone_unreferenced(tx, now).await)
            .await
            .unwrap()
    }

    /// The two-query reads take a [`Snapshot`]; on the pool they would not compile.
    async fn totals(db: &Db) -> Result<Vec<SessionTotal>, DbErr> {
        db.read(async |tx| session_totals(tx).await).await
    }

    async fn exported(db: &Db) -> Result<Vec<(voice_clip::Model, voice_blob::Model)>, DbErr> {
        db.read(async |tx| export_rows(tx).await).await
    }

    /// A blob and its clip, committed together, from a task of its own — the
    /// shape a background capture has. Spawned because a write started from a
    /// task that has a read open is refused (`cap::refuse_inside_transaction`):
    /// another task is exactly what a concurrent capture is.
    async fn capture_elsewhere(db: &Db, sha: &'static str, blob_id: &'static str, clip_id: &'static str) {
        let db = db.clone();
        tokio::spawn(async move {
            let blob = ready_blob(&db, sha, blob_id).await;
            clip(&db, &blob, clip_id, "alice", None, 0, None, 2_000).await;
        })
        .await
        .unwrap();
    }

    /// The race Codex review found on PR 36, reproduced deterministically: on
    /// the pool every statement is its own snapshot, so a capture committed
    /// between "read the blobs" and "read the clips" leaves a clip whose blob
    /// the first read never saw. This is what made the join report a broken
    /// database on a sound one. It is why the join takes a `Snapshot`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_the_pool_a_capture_between_two_reads_is_half_visible() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::sea::file_test_db(dir.path()).await;

        let blobs_before = all_blobs(&db).await.unwrap();
        capture_elsewhere(&db, "aa", "b1", "c1").await;
        let clips_after = clips(&db).await;

        assert!(blobs_before.is_empty());
        assert_eq!(clips_after.len(), 1, "the second read sees the new clip");
        assert!(
            !blobs_before.iter().any(|blob| blob.id == clips_after[0].blob_id),
            "and its blob is missing from the first read"
        );
    }

    /// The other half of the experiment: inside a read transaction the snapshot
    /// is fixed at the first statement, so the same interleaving is invisible
    /// to both reads, the join answers for one moment, and a fresh read
    /// afterwards sees the capture whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inside_a_read_transaction_both_reads_see_one_moment() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::sea::file_test_db(dir.path()).await;
        ready_blob(&db, "00", "b0").await;
        // The capture's own handle on the pool, standing for another task's.
        let capturer = db.clone();

        let (blobs, rows) = db
            .read(async |tx| {
                let blobs = all_blobs(tx).await?;
                capture_elsewhere(&capturer, "aa", "b1", "c1").await;
                Ok::<_, DbErr>((blobs, export_rows(tx).await?))
            })
            .await
            .unwrap();
        assert_eq!(blobs.len(), 1, "the snapshot was taken before the capture");
        assert!(rows.is_empty(), "the capture is invisible to the second read too");

        let rows = exported(&db).await.unwrap();
        assert_eq!(rows.len(), 1, "a fresh snapshot sees it, blob and clip together");
        assert_eq!(rows[0].1.id, "b1");
    }

    /// 同一段音频的第二个采集任务认出别人已经有了它，而不是写第二份。
    #[tokio::test]
    async fn a_second_claim_on_the_same_bytes_finds_the_first() {
        let db = sea_test_db().await;
        own(&db, "aa", "b1", "t1").await;

        match claim(&db, key("aa"), "b2", "t2").await {
            ClaimOutcome::PendingElsewhere(blob) => assert_eq!(blob.id, "b1"),
            other => panic!("{other:?}"),
        }
    }

    /// `source_type` 没有 CHECK 守着，所以库里一个认不出来的值只能在读的时候
    /// 被拒绝——而且是整个查询失败，不是把那一行读成某个默认的类型。
    /// `status` 有 CHECK，写不进去一个坏值，它的拼写由 entity 那边的测试钉住。
    #[tokio::test]
    async fn an_unknown_persisted_source_type_fails_the_read() {
        let db = sea_test_db().await;
        ready_blob(&db, "aa", "b1").await;
        execute_for_tests(&db, "UPDATE voice_blobs SET source_type = 'onebot_channel'")
            .await
            .unwrap();

        let error = all_ready(&db).await.unwrap_err().to_string();
        assert!(error.contains("onebot_channel"), "{error}");
        assert!(
            find_blob(&db, &key("aa")).await.unwrap().is_none(),
            "the key no longer matches"
        );

        // 经 clip 读 blob 的两条路也要失败，而且要说出那个值。`find_also_related`
        // 在这里会把坏行读成"没有 blob"——这两条断言钉住的就是不用它的理由。
        let error = totals(&db).await.unwrap_err().to_string();
        assert!(error.contains("onebot_channel"), "{error}");
        let error = exported(&db).await.unwrap_err().to_string();
        assert!(error.contains("onebot_channel"), "{error}");
    }

    /// 另一个群里的同一段音频是另一份语料：删掉这个群的不该动那个群的。
    #[tokio::test]
    async fn the_same_bytes_in_another_session_is_another_blob() {
        let db = sea_test_db().await;
        own(&db, "aa", "b1", "t1").await;

        let elsewhere = BlobKey {
            source_id: "456",
            ..key("aa")
        };
        match claim(&db, elsewhere, "b2", "t2").await {
            ClaimOutcome::Owned { id, .. } => assert_eq!(id, "b2"),
            other => panic!("{other:?}"),
        }
    }

    /// fencing 的落点：旧 owner 醒来时带的是旧 epoch，发布不进去。
    ///
    /// 没有这一条，同一个进程里两个任务会都以为自己是 owner——`owner_instance`
    /// 那版就是这么错的，进程 id 对同进程的两个任务是同一个值。
    #[tokio::test]
    async fn a_fenced_out_owner_cannot_publish() {
        let db = sea_test_db().await;
        let (id, old_epoch) = own(&db, "aa", "b1", "old").await;

        // lease 过期后被接管。
        let new_epoch = db
            .write(async |tx| takeover_blob(tx, &id, Some("old"), old_epoch, "new", 100_000, 60_000).await)
            .await
            .unwrap()
            .expect("takeover should win");
        assert_eq!(new_epoch, old_epoch + 1);

        assert!(
            !publish(&db, &id, "old", old_epoch, 200_000).await,
            "the old owner is fenced out"
        );
        assert!(publish(&db, &id, "new", new_epoch, 200_000).await);
    }

    /// lease 还没过期时接管不了。
    #[tokio::test]
    async fn a_live_lease_cannot_be_taken_over() {
        let db = sea_test_db().await;
        let (id, epoch) = own(&db, "aa", "b1", "t1").await;
        let taken = db
            .write(async |tx| takeover_blob(tx, &id, Some("t1"), epoch, "t2", 1_500, 60_000).await)
            .await
            .unwrap();
        assert!(taken.is_none());
        assert!(
            db.write(async |tx| renew_lease(tx, &id, "t1", epoch, 1_500, 60_000).await)
                .await
                .unwrap(),
            "the live owner can still renew"
        );
    }

    /// 同一段音频被两个人发出来是**两行** clip，各自带着自己的发送者。
    /// 单表按 sha 唯一的那版会把第二个人整个丢掉。
    #[tokio::test]
    async fn two_senders_of_one_recording_are_two_clips() {
        let db = sea_test_db().await;
        let blob = ready_blob(&db, "aa", "b1").await;

        let first = clip(&db, &blob, "c1", "alice", Some(10), 0, Some("你好"), 1).await;
        let second = clip(&db, &blob, "c2", "bob", Some(11), 0, Some("你好"), 2).await;
        assert_eq!(first, ClipOutcome::Inserted);
        assert_eq!(second, ClipOutcome::Inserted);

        let senders: Vec<String> = clips(&db).await.into_iter().map(|c| c.sender_id).collect();
        assert_eq!(senders, vec!["alice", "bob"]);
    }

    /// 同一个事件重投不写第二行，而当时缺的转写会被补上——成对补，不允许
    /// 只有文本没有来源。
    #[tokio::test]
    async fn a_replayed_event_fills_the_transcript_it_lacked() {
        let db = sea_test_db().await;
        let blob = ready_blob(&db, "aa", "b1").await;

        clip(&db, &blob, "c1", "alice", Some(10), 0, None, 1).await;
        let again = clip(&db, &blob, "c2", "alice", Some(10), 0, Some("你好"), 2).await;
        assert_eq!(again, ClipOutcome::TranscriptFilled);

        let rows = clips(&db).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].transcript.as_deref(), Some("你好"));
        assert_eq!(rows[0].transcript_source.as_deref(), Some("s"));

        // 第三次什么都不做。
        let third = clip(&db, &blob, "c3", "alice", Some(10), 0, Some("别的"), 3).await;
        assert_eq!(third, ClipOutcome::AlreadyRecorded);
        let rows = clips(&db).await;
        assert_eq!(rows[0].transcript.as_deref(), Some("你好"), "先到的那份不被覆盖");

        // 同一次出现指向另一个 blob：先到的留下，这次的不写。
        let other = ready_blob(&db, "bb", "b2").await;
        assert_eq!(
            clip(&db, &other, "c4", "alice", Some(10), 0, None, 4).await,
            ClipOutcome::KeptExisting
        );
        assert_eq!(clips(&db).await.len(), 1);
    }

    /// 一条消息里的两个 record 段是两行。
    #[tokio::test]
    async fn two_segments_of_one_message_are_two_clips() {
        let db = sea_test_db().await;
        let a = ready_blob(&db, "aa", "b1").await;
        let b = ready_blob(&db, "bb", "b2").await;

        assert_eq!(
            clip(&db, &a, "c1", "alice", Some(10), 0, None, 1).await,
            ClipOutcome::Inserted
        );
        assert_eq!(
            clip(&db, &b, "c2", "alice", Some(10), 1, None, 1).await,
            ClipOutcome::Inserted
        );
    }

    /// 按发送者删除只该带走没人再引用的文件。两个人发过同一段音频时，删掉
    /// 一个人不能让另一个人的样本消失。删 clip 和立墓碑在**同一个**写事务里，
    /// 和 `manage::delete` 一样。
    #[tokio::test]
    async fn deleting_one_sender_leaves_a_shared_recording_alone() {
        let db = sea_test_db().await;
        let blob = ready_blob(&db, "aa", "b1").await;
        clip(&db, &blob, "c1", "alice", Some(10), 0, None, 1).await;
        clip(&db, &blob, "c2", "bob", Some(11), 0, None, 1).await;

        let doomed = db
            .write(async |tx| {
                delete_clips_by_sender(tx, "alice").await?;
                tombstone_unreferenced(tx, 2).await
            })
            .await
            .unwrap();
        assert!(doomed.is_empty(), "bob 还引用着它");
        assert_eq!(
            find_blob(&db, &key("aa")).await.unwrap().unwrap().status,
            VoiceBlobStatus::Ready
        );

        let doomed = db
            .write(async |tx| {
                delete_clips_by_sender(tx, "bob").await?;
                tombstone_unreferenced(tx, 3).await
            })
            .await
            .unwrap();
        assert_eq!(doomed.len(), 1);
        assert_eq!(doomed[0].id, "b1");
        assert_eq!(doomed[0].status, VoiceBlobStatus::Deleting);
    }

    /// 墓碑收集器的 UPDATE 自己带着 `NOT EXISTS`：一条在 SELECT 之后、UPDATE
    /// 之前挂上来的 clip 让那一行留下。`tombstone_unreferenced` 的写事务本该挡住
    /// 这种交错，所以这里模拟的是"顺手挪出事务"的那一天——它的两半各在一个事务
    /// 里跑，SELECT 的结果拿在手上，clip 先落了地，然后才是**真正的**那条 UPDATE。
    #[tokio::test]
    async fn the_tombstone_update_rechecks_references_itself() {
        let db = sea_test_db().await;
        let blob = ready_blob(&db, "aa", "b1").await;

        // SELECT 看到它没人引用。
        let doomed = db.write(async |tx| collectable_ids(tx).await).await.unwrap();
        assert_eq!(doomed, vec!["b1".to_owned()]);

        // 一条 clip 在两步之间挂了上来。
        clip(&db, &blob, "c1", "alice", Some(10), 0, None, 2).await;

        // 拿着旧的 id 列表去 UPDATE，条件里的 NOT EXISTS 让它一行都改不动。
        let tombstoned = db.write(async |tx| tombstone_ids(tx, &doomed, 3).await).await.unwrap();
        assert!(tombstoned.is_empty(), "the clip that arrived in between keeps the blob");
        assert_eq!(
            find_blob(&db, &key("aa")).await.unwrap().unwrap().status,
            VoiceBlobStatus::Ready
        );
    }

    /// 坏掉又没人引用的 blob 也要被收走。留在外面的话，它的文件永远删不掉——
    /// 墓碑扫描只看 `ready`，而"坏"恰恰意味着这一行不会再变回 `ready`。
    #[tokio::test]
    async fn a_damaged_recording_nobody_references_is_collected_too() {
        let db = sea_test_db().await;
        let blob = ready_blob(&db, "aa", "b1").await;
        damage(&db, &blob.id, 2).await;

        let doomed = tombstone(&db, 3).await;
        assert_eq!(doomed.len(), 1);
        assert_eq!(doomed[0].id, "b1");
        assert_eq!(doomed[0].status, VoiceBlobStatus::Deleting);
        assert_eq!(tombstones(&db).await.unwrap().len(), 1);
    }

    /// 有 clip 指着的坏行**不动**：那份记录记着谁在什么时候说过话，
    /// 而它不该因为文件坏了就消失。
    #[tokio::test]
    async fn a_damaged_recording_someone_still_references_stays() {
        let db = sea_test_db().await;
        let blob = ready_blob(&db, "aa", "b1").await;
        clip(&db, &blob, "c1", "alice", Some(10), 0, None, 1).await;
        damage(&db, &blob.id, 2).await;

        assert!(tombstone(&db, 3).await.is_empty());
        assert!(orphaned(&db).await.unwrap().is_empty());
    }

    /// 统计数的是 clip，不是 blob。两个人发过同一段音频时两个数就分家了：
    /// 会话看起来少一条，而"删掉这个会话"实际会带走两条。字节反过来——
    /// 一份文件只占一次地方。
    #[tokio::test]
    async fn a_shared_recording_counts_twice_but_takes_up_room_once() {
        let db = sea_test_db().await;
        let blob = ready_blob(&db, "aa", "b1").await;
        clip(&db, &blob, "c1", "alice", Some(10), 0, None, 1).await;
        clip(&db, &blob, "c2", "bob", Some(11), 0, None, 5).await;

        let totals = totals(&db).await.unwrap();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].clips, 2, "两次出现是两条");
        assert_eq!(totals[0].bytes, 10, "一份文件只占一次地方");
        assert_eq!(totals[0].last_captured_at, 5, "最近一次来自 clip，不是 blob 建立的时刻");
        assert_eq!(totals[0].source_type, VoiceCorpusSourceType::OnebotGroup);

        let untranscribed = untranscribed_by_session(&db).await.unwrap();
        assert_eq!(untranscribed.get("1|onebot_group|123"), Some(&2));
    }

    /// 文件坏了，会话不能从管理列表上消失：`resolve_handle` 在这份统计上重算
    /// HMAC，不在列表上的会话就没有句柄，clips 连同发送者和转写却都还在库里。
    /// 按 `ready` 过滤正好把最该被删的那批数据变成删不掉的。
    #[tokio::test]
    async fn a_session_whose_files_all_went_bad_is_still_listed_for_deletion() {
        let db = sea_test_db().await;
        let blob = ready_blob(&db, "aa", "b1").await;
        clip(&db, &blob, "c1", "alice", Some(10), 0, None, 1).await;
        damage(&db, &blob.id, 2).await;

        let totals = totals(&db).await.unwrap();
        assert_eq!(totals.len(), 1, "damaged 只挡导出，不挡删除");
        assert_eq!(totals[0].clips, 1);

        // 导出这边照旧跳过它——两个调用者对 damaged 的答案相反，
        // 这正是统计不借用 export_rows 的原因。
        assert!(exported(&db).await.unwrap().is_empty());
    }

    /// 导出按 clip 的时间排，每一行都带着它的 blob。
    #[tokio::test]
    async fn export_rows_pair_each_clip_with_its_blob_in_capture_order() {
        let db = sea_test_db().await;
        let a = ready_blob(&db, "aa", "b1").await;
        let b = ready_blob(&db, "bb", "b2").await;
        clip(&db, &b, "c2", "bob", Some(11), 0, None, 5).await;
        clip(&db, &a, "c1", "alice", Some(10), 0, Some("你好"), 1).await;

        let rows = exported(&db).await.unwrap();
        let pairs: Vec<(&str, &str)> = rows.iter().map(|(c, b)| (c.id.as_str(), b.id.as_str())).collect();
        assert_eq!(pairs, vec![("c1", "b1"), ("c2", "b2")]);
    }

    #[tokio::test]
    async fn an_optout_is_global_and_idempotent() {
        let db = sea_test_db().await;
        db.write(async |tx| set_optout(tx, "alice", 1).await).await.unwrap();
        db.write(async |tx| set_optout(tx, "alice", 2).await).await.unwrap();
        assert_eq!(optouts(&db).await.unwrap(), vec!["alice"]);
        db.write(async |tx| clear_optout(tx, "alice").await).await.unwrap();
        assert!(optouts(&db).await.unwrap().is_empty());
    }

    /// 恢复器用的三份清单各看各的状态。
    #[tokio::test]
    async fn recovery_lists_are_split_by_status() {
        let db = sea_test_db().await;
        own(&db, "aa", "b1", "t1").await;
        let ready = ready_blob(&db, "bb", "b2").await;
        clip(&db, &ready, "c1", "alice", Some(10), 0, None, 1).await;
        ready_blob(&db, "cc", "b3").await;

        let ids = |rows: Vec<voice_blob::Model>| rows.into_iter().map(|b| b.id).collect::<Vec<_>>();
        assert_eq!(ids(stale_pending(&db).await.unwrap()), ["b1"]);
        assert_eq!(ids(orphaned(&db).await.unwrap()), ["b3"]);
        let mut all = ids(all_ready(&db).await.unwrap());
        all.sort();
        assert_eq!(all, ["b2", "b3"]);
        assert_eq!(all_blobs(&db).await.unwrap().len(), 3);

        let removed = db
            .write(async |tx| delete_blob_rows(tx, &["b1".to_owned(), "b3".to_owned()]).await)
            .await
            .unwrap();
        assert_eq!(removed, 2);
        assert_eq!(all_blobs(&db).await.unwrap().len(), 1);
        assert_eq!(db.write(async |tx| delete_blob_rows(tx, &[]).await).await.unwrap(), 0);
    }
}
