//! 把一条语音留下来。
//!
//! 这条通道**在群门禁之前**，和贴纸采集并列。挂在 `process_media` 上只能抓到
//! @ 过 bot 的那些——群里绝大多数语音不会 @ bot，采到的会是一个被严重扭曲的
//! 子集，而那正是要拿去训练的数据。
//!
//! 它也不碰产品侧的任何东西：不 `get_or_create` 会话（不为录音在侧边栏凭空
//! 长出一个对话），不复用 `process_media`（那要 conversation_id、图片预算、
//! vision 判断，采集一个都不需要）。代价是 @ 过 bot 的语音会被转写两次，多一次
//! API 调用换一条干净的边界。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::format::MediaRef;
use super::protocol::OneBotAction;
use super::{DirectedCallOutcome, SharedState};
use crate::db::entity::voice_blob;
use crate::db::entity::voice_blob::VoiceCorpusSourceType;
use crate::db::sea::cap::{Db, WriteTx};
use crate::db::sea::ops::voice_corpus as ops;
use crate::voice_corpus::{self, CapturePermit, CaptureScope};

/// 单条语音的上限。一分钟的 SILK 是几十 KB，10 MiB 给的是"这显然不是语音"
/// 的边界，不是正常结果的余量。
const MAX_RECORD_BYTES: u64 = 10 * 1024 * 1024;
/// 并发采集数。比贴纸的 8 小：语音条数远少而单条更大，再高只是在抢同一根
/// websocket。
const CAPTURE_SLOTS: usize = 4;
/// lease 长度。下载超过它就要续租，见 `ops::renew_lease`。
const LEASE_MS: i64 = 60_000;
/// 同一段音频被别人握着时重试几次、隔多久。
///
/// 覆盖的是正常那一档：两个人几乎同时转发同一条语音，先到的那个下载几秒就发布
/// 了。乘起来远短于 [`LEASE_MS`]——超过 lease 的那种是 owner 死了，那时
/// `Takeable` 会接管，不走这条路。
const CLAIM_ATTEMPTS: usize = 5;
const CLAIM_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1500);
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const TRANSCRIBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 一条语音的来源，与产品侧的任何上下文无关。
#[derive(Debug, Clone)]
pub struct RecordSource {
    pub scope: CaptureScope,
    pub source_type: VoiceCorpusSourceType,
    pub source_id: String,
    /// 消息的发送者。**已经排除过所有本地 bot 账号**——只查当前连接的 self_id
    /// 不够：bot A 发的 TTS 会被同群的 bot B 当成真人语音采集。
    pub sender_id: i64,
    pub message_id: Option<i64>,
    /// 回应要走回它来的那条连接。广播会让另一个适配器抢答一个它没听说过的
    /// message_id。
    pub conn_id: u64,
}

/// 排在后台，不挡住本轮。
pub fn capture_in_background(state: Arc<SharedState>, source: RecordSource, records: Vec<MediaRef>) {
    if records.is_empty() {
        return;
    }
    let Some(permit) = state
        .services
        .corpus
        .acquire(&source.scope, &source.sender_id.to_string())
    else {
        return;
    };
    // 一条消息一个 permit，几段语音共享；提交任务各自再 clone 一份，所以
    // 撤权要等的是"最后一个提交任务结束"，不是"这个 future 还活着"。
    let permit = Arc::new(permit);
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let slots = SLOTS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(CAPTURE_SLOTS)))
        .clone();
    tokio::spawn(async move {
        let Ok(_slot) = slots.acquire_owned().await else { return };
        // permit 一路带进来：撤权要等它归还，而它归还之前这个 scope 不会被撤。
        capture_all(&state, &source, &records, &permit).await;
    });
}

async fn capture_all(
    state: &Arc<SharedState>,
    source: &RecordSource,
    records: &[MediaRef],
    permit: &Arc<CapturePermit>,
) {
    // 转写按**消息**作答，所以只有单段时它能被归给某一段。多段时一律留空:
    // 把一次结果复制到每一段，产出的是自信的错误标签。
    let transcript = match records.len() {
        1 => transcribe(state, source).await,
        _ => {
            tracing::debug!(
                segments = records.len(),
                "voice: several record segments in one message; transcripts left empty"
            );
            None
        }
    };
    for (index, record) in records.iter().enumerate() {
        if let Err(error) = capture_one(state, source, record, index as i32, transcript.as_deref(), permit).await {
            tracing::warn!(%error, session = %source.scope, "voice capture failed");
        }
    }
}

async fn transcribe(state: &Arc<SharedState>, source: &RecordSource) -> Option<String> {
    let message_id = source.message_id?;
    let action = OneBotAction::voice_msg_to_text(message_id, String::new());
    match super::call_api_to_conn(state, source.conn_id, action, TRANSCRIBE_TIMEOUT).await {
        DirectedCallOutcome::AdapterAccepted(data) => data
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
        _ => None,
    }
}

/// 落盘的字节，以及它们的身份。
///
/// 临时文件的清理挂在 `Drop` 上，因为**放行的路径比放弃的路径多**：命中一份
/// 已经存在的音频是最常见的结果（同一段语音被转发、被重发），而那条路只是把
/// 既有的 blob 拿来挂一条 clip，谁也不会想起本次下载还留着一个 `.part`。留下
/// 的是一段真人录音，在数据库之外，删除找不到它，导出也看不见它。
#[derive(Debug)]
struct Staged {
    path: PathBuf,
    sha256: String,
    size: i64,
    format: &'static str,
}

impl Drop for Staged {
    /// 只删自己写的那个临时文件。**绝不碰最终文件**——那可能正被另一个任务
    /// 或既有行引用着。发布过的话这里删的是一个已经改了名的路径，失败即无事。
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn capture_one(
    state: &Arc<SharedState>,
    source: &RecordSource,
    record: &MediaRef,
    segment_index: i32,
    transcript: Option<&str>,
    permit: &Arc<CapturePermit>,
) -> Result<(), String> {
    let data_dir = state.services.paths.data_dir.clone();
    let mut staged = fetch_to_staging(record, &data_dir).await?;

    // 一次采集可能跑几十秒，而这中间用户可能把这个会话从白名单里拿掉。permit
    // 保证撤权会等我们结束，但不保证写下去的东西还是用户想要的。这里问一次是
    // 为了省掉后面的活；**决定性的那一次在写事务里面**，见 `commit`。
    if !permit.still_authorised() {
        tracing::info!(session = %source.scope, "voice capture dropped: the grant moved while it was running");
        return Ok(());
    }

    let key_bytes = voice_corpus::storage_key(&state.services.sea).await?;
    let pseudonym = voice_corpus::session_pseudonym(
        &key_bytes,
        source.scope.bot_self_id,
        source.source_type.as_str(),
        &source.source_id,
    );
    let dir = voice_corpus::session_dir(&data_dir, &pseudonym);
    let file_name = format!("{}.{}", staged.sha256, staged.format);
    let final_path = dir.join(&file_name);
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;

    // 同一段音频正被另一个任务写着时**等它写完**，而不是把这次出现丢掉：
    // 两个人几乎同时转发同一条语音是常事，丢掉的那一条会让第二个人从这份语料
    // 里消失。blob 会去重，clip 不该。
    for attempt in 0..CLAIM_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(CLAIM_BACKOFF).await;
        }
        let input = CommitInput {
            sea: state.services.sea.clone(),
            auth: permit.authorisation(),
            permit: Arc::clone(permit),
            source: source.clone(),
            transcript: transcript.map(str::to_string),
            staged,
            final_path: final_path.clone(),
            file_name: file_name.clone(),
            segment_index,
            #[cfg(test)]
            pauses: None,
        };
        // 提交在自己的任务里跑，这个 future 只等它的结果：这里被丢掉（连接
        // 断了、服务停了）不会把一次提交掐在半路。
        match spawn_commit(input).await.map_err(|e| e.to_string())?? {
            Settled::Done => return Ok(()),
            Settled::Retry(back) => staged = back,
        }
    }
    tracing::warn!(
        session = %source.scope,
        "voice capture gave up: another task held these bytes for the whole window"
    );
    Ok(())
}

/// [`commit`] 的参数。一个结构体而不是十个位置参数——顺序相同类型相同的
/// `String` 太多，写反了编译器不会说话。
///
/// **提交任务按值拥有它需要的一切。** `staged` 的 `Drop` 删 `.part`，所以它
/// 必须活到改名之后；`permit` 让撤权的 drain 等到这个任务返回，而不是等到
/// `capture_one` 的 future 被丢掉；`sea` 是池的一个 clone。这三样一起进任务，
/// 任务就是提交的所有者，丢掉等它的那个 future 改变不了任何事。
struct CommitInput {
    sea: Db,
    auth: voice_corpus::Authorisation,
    permit: Arc<CapturePermit>,
    source: RecordSource,
    transcript: Option<String>,
    staged: Staged,
    final_path: PathBuf,
    file_name: String,
    segment_index: i32,
    /// 测试用：让任务停在某个点上，好在它停着的时候丢掉等它的 future、或者
    /// 发起一次撤权。生产一律 `None`。
    #[cfg(test)]
    pauses: CommitPauses,
}

#[derive(Debug)]
enum Settled {
    Done,
    /// 有人正握着这段音频。退一步再来——临时文件还给调用方，下一轮还要用。
    Retry(Staged),
}

/// 事务闭包里的答案；`staged` 留在闭包外面，它要在 `Retry` 时原样交回去。
enum Committed {
    Done,
    Retry,
}

/// 事务里的失败。
///
/// 要一个自己的类型，是因为 `Db::write` 要求错误能从 `DbErr` 转过来，而这段
/// 代码有一半的失败来自文件系统。全都压成 `String` 就得让文件错误冒充数据库
/// 错误，回滚的原因在日志里会对不上号。
#[derive(Debug)]
enum CommitError {
    Db(sea_orm::DbErr),
    File(String),
}

impl From<sea_orm::DbErr> for CommitError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Db(error)
    }
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(error) => write!(f, "{error}"),
            Self::File(message) => f.write_str(message),
        }
    }
}

/// 把一次提交放到它自己的任务上。
///
/// 返回的 `JoinHandle` 被丢掉只是不再等——任务照跑到底。`capture_one` 等它，
/// 而 `capture_one` 的 future 被丢掉时（连接断开、服务停止），`Staged` 和
/// permit 都在任务手里：`.part` 不会在 rename 中途被 `Drop` 抢走，撤权的 drain
/// 也要等到这个事务提交或回滚之后才放行。
fn spawn_commit(input: CommitInput) -> tokio::task::JoinHandle<Result<Settled, String>> {
    tokio::spawn(commit(input))
}

/// 一个写事务：查授权、抢所有权、发布文件、记一次采集。
///
/// **整个在 `BEGIN IMMEDIATE` 里面**，而这两头各有一个理由。
///
/// 前头是授权：删除是"立屏障 → drain → 删 → 撤屏障"，而 drain 有上限、下载
/// 没有。在事务外面问，答案在拿到写锁之前就可能过期；在里面问，一场同时开始的
/// 删除只能堵在写锁上等这次提交完，然后把这条 clip 一起删掉——那正是它该赢的。
///
/// 后头是 clip 的幂等：`record_clip` 先查后插，两步之间同一个事件的重投会撞上
/// 唯一索引，把一次本该无声的重复变成一个失败。
///
/// rename 也在事务里，和 Diesel 那版一样：它很小，而发布状态和磁盘上的文件要
/// 一起成立——行说 `ready` 的时候文件已经在位。
async fn commit(input: CommitInput) -> Result<Settled, String> {
    let CommitInput {
        sea,
        auth,
        permit,
        source,
        transcript,
        staged,
        final_path,
        file_name,
        segment_index,
        #[cfg(test)]
        pauses,
    } = input;
    let now = crate::util::now_ms();
    let key = ops::BlobKey {
        bot_self_id: source.scope.bot_self_id,
        source_type: source.source_type,
        source_id: &source.source_id,
        sha256: &staged.sha256,
        file_format: staged.format,
    };

    let outcome = sea
        .write(async |tx| {
            #[cfg(test)]
            wait_at(&pauses, PausePoint::InsideTransaction).await;
            if !auth.still_authorised() {
                tracing::info!(session = %source.scope, "voice capture dropped: the grant moved while it was running");
                return Ok(Committed::Done);
            }

            let blob = match settle_blob(
                tx,
                &key,
                &file_name,
                staged.size,
                &staged.path,
                &final_path,
                now,
                #[cfg(test)]
                &pauses,
            )
            .await?
            {
                Claimed::Blob(blob) => blob,
                Claimed::Retry => return Ok(Committed::Retry),
                // 它正在被删除，或者别人已经把它标坏了。两种情况下这次都不写。
                Claimed::Skip => return Ok(Committed::Done),
            };

            #[cfg(test)]
            wait_at(&pauses, PausePoint::BeforeClip).await;
            ops::record_clip(
                tx,
                &blob,
                &uuid::Uuid::new_v4().to_string(),
                &source.sender_id.to_string(),
                source.message_id,
                segment_index,
                transcript.as_deref(),
                transcript.as_deref().map(|_| "llonebot.voice_msg_to_text"),
                now,
            )
            .await?;
            Ok::<_, CommitError>(Committed::Done)
        })
        .await
        .map_err(|error| error.to_string())?;

    // 事务已经提交或回滚，permit 这时才归还：撤权的 drain 等的就是这一刻。
    drop(permit);
    Ok(match outcome {
        Committed::Done => Settled::Done,
        Committed::Retry => Settled::Retry(staged),
    })
}

/// [`settle_blob`] 的三种答案。
enum Claimed {
    /// 可以挂 clip 的那一行。
    Blob(voice_blob::Model),
    /// 有人正握着它，退一步再来。
    Retry,
    /// 这次不写：墓碑，或者已经被标坏了。
    Skip,
}

/// 抢所有权、发布文件，返回可以挂 clip 的那一行。
#[allow(clippy::too_many_arguments)]
async fn settle_blob(
    tx: &WriteTx,
    key: &ops::BlobKey<'_>,
    file_name: &str,
    size: i64,
    staged_path: &Path,
    final_path: &Path,
    now: i64,
    #[cfg(test)] pauses: &CommitPauses,
) -> Result<Claimed, CommitError> {
    let id = uuid::Uuid::new_v4().to_string();
    let token = uuid::Uuid::new_v4().to_string();
    match ops::claim_blob(tx, key, &id, &token, file_name, size, now, LEASE_MS).await? {
        ops::ClaimOutcome::Owned { id, token, epoch } => {
            #[cfg(test)]
            wait_at(pauses, PausePoint::BeforeRename).await;
            publish_file(staged_path, final_path).await.map_err(CommitError::File)?;
            // fencing:返回 false 就是所有权在下载期间被接管了。这个任务既不
            // 发布也不写 clip——文件已经在位,让接管者去 publish。
            if !ops::publish_blob(tx, &id, &token, epoch, now).await? {
                return Ok(Claimed::Retry);
            }
            read_blob(tx, key).await
        }
        ops::ClaimOutcome::Ready(blob) => {
            // 已经有一份。校验磁盘上那个确实对得上——对不上就是它坏了,
            // 标出来而不是把新 clip 挂到一个坏文件上。整个文件要读一遍算 sha，
            // 那是一条阻塞线程的活，不是运行时的。
            let (path, expected_size, expected_sha) = (final_path.to_path_buf(), blob.file_size, blob.sha256.clone());
            let matches =
                tokio::task::spawn_blocking(move || voice_corpus::file_matches(&path, expected_size, &expected_sha))
                    .await
                    .map_err(|e| CommitError::File(e.to_string()))?;
            if matches {
                return Ok(Claimed::Blob(blob));
            }
            ops::mark_damaged(tx, &blob.id, now).await?;
            Ok(Claimed::Skip)
        }
        ops::ClaimOutcome::Takeable(blob) => {
            let token = uuid::Uuid::new_v4().to_string();
            let taken = ops::takeover_blob(
                tx,
                &blob.id,
                blob.owner_token.as_deref(),
                blob.fence_epoch,
                &token,
                now,
                LEASE_MS,
            )
            .await?;
            let Some(epoch) = taken else { return Ok(Claimed::Retry) };
            #[cfg(test)]
            wait_at(pauses, PausePoint::BeforeRename).await;
            publish_file(staged_path, final_path).await.map_err(CommitError::File)?;
            if !ops::publish_blob(tx, &blob.id, &token, epoch, now).await? {
                return Ok(Claimed::Retry);
            }
            read_blob(tx, key).await
        }
        ops::ClaimOutcome::PendingElsewhere(_) => Ok(Claimed::Retry),
        ops::ClaimOutcome::Damaged(_) => Ok(Claimed::Skip),
        ops::ClaimOutcome::Deleting(_) => {
            tracing::debug!("voice capture skipped: these bytes are being deleted");
            Ok(Claimed::Skip)
        }
    }
}

async fn read_blob(tx: &WriteTx, key: &ops::BlobKey<'_>) -> Result<Claimed, CommitError> {
    // 刚刚 publish 过，所以它必然在。真读不到就当作没抢到，让上面再转一圈。
    Ok(ops::find_blob(tx, key).await?.map_or(Claimed::Retry, Claimed::Blob))
}

/// 把临时文件挪到最终位置。
///
/// **Windows 上 `rename` 在目标已存在时会失败**（Unix 是覆盖），所以先看目标
/// 在不在：在就直接用它（内容寻址保证字节相同，这里的 TOCTOU 无害）。
/// 剩下的失败——权限、磁盘满、父目录缺失——要报出来，不能伪装成"别人赢了"。
async fn publish_file(staged: &Path, final_path: &Path) -> Result<(), String> {
    if tokio::fs::try_exists(final_path).await.unwrap_or(false) {
        let _ = tokio::fs::remove_file(staged).await;
        return Ok(());
    }
    match tokio::fs::rename(staged, final_path).await {
        Ok(()) => Ok(()),
        Err(_) if tokio::fs::try_exists(final_path).await.unwrap_or(false) => {
            let _ = tokio::fs::remove_file(staged).await;
            Ok(())
        }
        Err(e) => Err(format!("could not publish the audio: {e}")),
    }
}

/// 测试用的停靠点。每个都在写事务里面：第一个在授权复查之前，后两个夹着
/// rename 和 clip 的写入，正好是"丢掉 future"和"发起撤权"最想撞上的三个时刻。
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PausePoint {
    /// 事务已开，授权还没复查。
    InsideTransaction,
    /// 已经抢到所有权，`.part` 还没改名。
    BeforeRename,
    /// 文件已经发布、行已经 `ready`，clip 还没写。
    BeforeClip,
}

/// 一个停靠点：任务到了就说一声，然后等放行。`Notify` 存得住一次通知，所以
/// 两边谁先到都不丢。
#[cfg(test)]
struct Pause {
    at: PausePoint,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(test)]
type CommitPauses = Option<Arc<Pause>>;

#[cfg(test)]
async fn wait_at(pauses: &CommitPauses, point: PausePoint) {
    if let Some(pause) = pauses
        && pause.at == point
    {
        pause.reached.notify_one();
        pause.release.notified().await;
    }
}

/// 两档获取，边下边算 sha。
///
/// **`get_record` 不在其中**：它的 `out_format` 是必填的（必然转码，与"原样
/// 落盘"矛盾），返回的是**运行适配器那台机器**上的路径（反向 WS 在另一台机器
/// 时读不到），而"是否同机"没有可靠依据——`onebot.host` 是监听地址不是 peer。
/// 做对它要引入"授权根目录"让 WS 对端指定 Meridian 去读本机文件，那是一个新的
/// 攻击面，换一档必然转码的数据。
async fn fetch_to_staging(record: &MediaRef, data_dir: &Path) -> Result<Staged, String> {
    let staging = voice_corpus::staging_dir(data_dir);
    tokio::fs::create_dir_all(&staging).await.map_err(|e| e.to_string())?;
    let path = staging.join(format!("{}.part", uuid::Uuid::new_v4()));

    let url = record
        .url
        .as_deref()
        .filter(|u| u.starts_with("http"))
        .or_else(|| record.file.as_deref().filter(|f| f.starts_with("http")));
    let inline = record
        .file
        .as_deref()
        .and_then(|f| f.strip_prefix("base64://"))
        .filter(|s| !s.is_empty());

    let bytes_written = if let Some(url) = url {
        match stream_to_file(url, &path).await {
            Ok(written) => written,
            // 第 1 档失败回退第 2 档，两档都不成立才放弃。
            Err(error) => match inline {
                Some(payload) => {
                    tracing::debug!(%error, "voice: url fetch failed, falling back to the inline payload");
                    write_base64(payload, &path).await?
                }
                None => {
                    let _ = tokio::fs::remove_file(&path).await;
                    return Err(error);
                }
            },
        }
    } else if let Some(payload) = inline {
        write_base64(payload, &path).await?
    } else {
        let _ = tokio::fs::remove_file(&path).await;
        // 计数而不是静默：如果某个适配器全落在这里，这个功能对它就是不可用的，
        // 那要早点知道，而不是等着看空空如也的语料目录。
        tracing::warn!("voice capture skipped: the segment carries neither a url nor an inline payload");
        return Err("no fetchable audio in the record segment".into());
    };

    let (sha256, format) = tokio::task::spawn_blocking({
        let path = path.clone();
        move || -> Result<(String, &'static str), String> {
            let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
            Ok((sha_hex(&bytes), magic_format(&bytes)))
        }
    })
    .await
    .map_err(|e| e.to_string())??;

    Ok(Staged {
        path,
        sha256,
        size: bytes_written as i64,
        format,
    })
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

async fn stream_to_file(url: &str, path: &Path) -> Result<u64, String> {
    let response = super::media::http_client()?
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let mut response = response;
    let mut file = tokio::fs::File::create(path).await.map_err(|e| e.to_string())?;
    let mut written: u64 = 0;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        written += chunk.len() as u64;
        if written > MAX_RECORD_BYTES {
            let _ = tokio::fs::remove_file(path).await;
            return Err("the audio is too large".into());
        }
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    Ok(written)
}

async fn write_base64(payload: &str, path: &Path) -> Result<u64, String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err("the audio is too large".into());
    }
    tokio::fs::write(path, &bytes).await.map_err(|e| e.to_string())?;
    Ok(bytes.len() as u64)
}

/// 格式**由字节判定**，不信 URL、文件名或 Content-Type——三者都由对端控制，
/// 而这个值决定文件落进哪个去重桶。认不出来就叫 `bin`：存下来总比丢掉好，
/// 而一个诚实的"不知道"比一个猜错的扩展名有用。
fn magic_format(bytes: &[u8]) -> &'static str {
    const SILK: &[u8] = b"#!SILK";
    if bytes.starts_with(SILK) || (bytes.len() > 1 && bytes[1..].starts_with(SILK)) {
        return "silk";
    }
    if bytes.starts_with(b"#!AMR") {
        return "amr";
    }
    if bytes.starts_with(b"OggS") {
        return "ogg";
    }
    if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WAVE" {
        return "wav";
    }
    if bytes.starts_with(b"ID3") || (bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0) {
        return "mp3";
    }
    "bin"
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::db::entity::voice_blob::VoiceBlobStatus;
    use crate::db::entity::voice_clip;
    use crate::db::sea::shared_test_db;
    use crate::voice_corpus::CorpusCoordinator;

    /// 格式认的是字节。QQ 的 SILK 常带一个前导字节，所以第二个位置也要看。
    #[test]
    fn the_format_comes_from_the_bytes() {
        assert_eq!(magic_format(b"#!SILK_V3xxxx"), "silk");
        assert_eq!(magic_format(b"\x02#!SILK_V3xxx"), "silk");
        assert_eq!(magic_format(b"#!AMR\n\x00\x00"), "amr");
        assert_eq!(magic_format(b"OggS\x00\x02\x00\x00"), "ogg");
        assert_eq!(magic_format(b"RIFF\x24\x08\x00\x00WAVEfmt "), "wav");
        assert_eq!(magic_format(b"ID3\x03\x00\x00\x00"), "mp3");
        assert_eq!(magic_format(b"\xff\xfb\x90\x00"), "mp3");
    }

    /// 认不出来不等于丢掉。
    #[test]
    fn an_unknown_container_is_still_kept() {
        assert_eq!(magic_format(b"whatever this is"), "bin");
        assert_eq!(magic_format(b""), "bin");
    }

    /// 一个骗人的扩展名改变不了它落进哪个桶。
    #[test]
    fn a_lying_file_name_does_not_decide_the_format() {
        assert_eq!(magic_format(b"#!AMR\n\x00\x00"), "amr", "叫 .mp3 也还是 amr");
    }

    const BYTES: &[u8] = b"#!AMR\n\x00\x00some audio";

    /// 一个数据目录、一个库文件、一个持锁的协调器，和一条已经下载好的语音。
    struct Fixture {
        dir: tempfile::TempDir,
        db: Db,
        coordinator: Arc<CorpusCoordinator>,
        key: Vec<u8>,
    }

    fn scope() -> CaptureScope {
        CaptureScope::new(1, "group:123")
    }

    impl Fixture {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let (_diesel, db) = shared_test_db(dir.path()).await;
            let coordinator = Arc::new(CorpusCoordinator::new(dir.path()));
            coordinator.apply(HashSet::from([scope()]), HashSet::new()).await;
            let key = voice_corpus::storage_key(&db).await.unwrap();
            Self {
                dir,
                db,
                coordinator,
                key,
            }
        }

        fn permit(&self) -> Arc<CapturePermit> {
            Arc::new(self.coordinator.acquire(&scope(), "alice").expect("granted"))
        }

        /// 下载完的 `.part`，和它要去的地方。
        fn stage(&self) -> (Staged, PathBuf) {
            let staging = voice_corpus::staging_dir(self.dir.path());
            std::fs::create_dir_all(&staging).unwrap();
            let part = staging.join("x.part");
            std::fs::write(&part, BYTES).unwrap();
            let staged = Staged {
                path: part,
                sha256: sha_hex(BYTES),
                size: BYTES.len() as i64,
                format: magic_format(BYTES),
            };
            let pseudonym = voice_corpus::session_pseudonym(&self.key, 1, "onebot_group", "123");
            let dir = voice_corpus::session_dir(self.dir.path(), &pseudonym);
            std::fs::create_dir_all(&dir).unwrap();
            let final_path = dir.join(format!("{}.{}", staged.sha256, staged.format));
            (staged, final_path)
        }

        fn input(&self, permit: &Arc<CapturePermit>, pauses: CommitPauses) -> (CommitInput, PathBuf, PathBuf) {
            let (staged, final_path) = self.stage();
            let part = staged.path.clone();
            let file_name = final_path.file_name().unwrap().to_string_lossy().into_owned();
            let input = CommitInput {
                sea: self.db.clone(),
                auth: permit.authorisation(),
                permit: Arc::clone(permit),
                source: RecordSource {
                    scope: scope(),
                    source_type: VoiceCorpusSourceType::OnebotGroup,
                    source_id: "123".into(),
                    sender_id: 42,
                    message_id: Some(7),
                    conn_id: 1,
                },
                transcript: Some("你好".into()),
                staged,
                final_path: final_path.clone(),
                file_name,
                segment_index: 0,
                pauses,
            };
            (input, final_path, part)
        }

        /// 等这个 scope 上的 permit 全部归还——也就是等提交任务结束。
        async fn wait_idle(&self) {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            while self.coordinator.in_flight(&scope()) > 0 {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the commit task never returned its permit"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        /// 每一行，不论状态——连接只经 ops 可达，这里没有别的读法。
        async fn blobs(&self) -> Vec<voice_blob::Model> {
            ops::all_blobs(&self.db).await.unwrap()
        }

        /// 挂在 `ready` blob 上的 clip；这些测试里要么 blob 是 `ready`，要么
        /// 根本没有 blob，所以这就是全部的 clip。
        async fn clips(&self) -> Vec<voice_clip::Model> {
            self.db
                .read(async |tx| ops::export_rows(tx).await)
                .await
                .unwrap()
                .into_iter()
                .map(|(clip, _)| clip)
                .collect()
        }
    }

    fn pause_at(at: PausePoint) -> Arc<Pause> {
        Arc::new(Pause {
            at,
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        })
    }

    /// 等任务的那个 future 在停靠点上被丢掉；返回时它确实已经丢了。
    async fn drop_the_waiter_at(pause: &Arc<Pause>, input: CommitInput) {
        let waiter = tokio::spawn(async move { spawn_commit(input).await.unwrap() });
        pause.reached.notified().await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
    }

    /// 等提交的 future 在 rename 之前被丢掉，提交照样走完：行 `ready`，文件在
    /// 最终位置，`.part` 没了，permit 归还。
    #[tokio::test]
    async fn a_commit_dropped_before_the_rename_still_publishes_whole() {
        let f = Fixture::new().await;
        let pause = pause_at(PausePoint::BeforeRename);
        let permit = f.permit();
        let (input, final_path, part) = f.input(&permit, Some(pause.clone()));

        drop_the_waiter_at(&pause, input).await;
        drop(permit);
        assert_eq!(
            f.coordinator.in_flight(&scope()),
            1,
            "the task's own clone keeps the permit alive"
        );
        assert!(part.exists(), "nothing has been renamed yet");
        pause.release.notify_one();
        f.wait_idle().await;

        let blobs = f.blobs().await;
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].status, VoiceBlobStatus::Ready);
        assert!(final_path.exists(), "the file reached its final path");
        assert!(!part.exists(), "no .part left behind");
        assert_eq!(f.clips().await.len(), 1);
        assert_eq!(f.coordinator.in_flight(&scope()), 0);
    }

    /// 在 rename 之后、写 clip 之前被丢掉也一样：事务整个提交——blob `ready`
    /// **而且** clip 在。
    #[tokio::test]
    async fn a_commit_dropped_after_the_rename_still_records_the_clip() {
        let f = Fixture::new().await;
        let pause = pause_at(PausePoint::BeforeClip);
        let permit = f.permit();
        let (input, final_path, part) = f.input(&permit, Some(pause.clone()));

        drop_the_waiter_at(&pause, input).await;
        drop(permit);
        assert!(final_path.exists(), "renamed already");
        assert!(f.clips().await.is_empty(), "the clip is not written yet");
        pause.release.notify_one();
        f.wait_idle().await;

        let blobs = f.blobs().await;
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].status, VoiceBlobStatus::Ready);
        let clips = f.clips().await;
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].blob_id, blobs[0].id);
        assert_eq!(clips[0].transcript.as_deref(), Some("你好"));
        assert!(final_path.exists());
        assert!(!part.exists());
        assert_eq!(f.coordinator.in_flight(&scope()), 0);
    }

    /// 撤权在任务停在事务里的时候开始：drain 不会在任务之前返回；放行之后，
    /// 事务里的那次复查说"不"，于是什么都不写——没有 clip、没有 blob 行、
    /// `.part` 也删了。
    #[tokio::test]
    async fn a_revocation_during_the_commit_waits_for_it_and_the_commit_writes_nothing() {
        let f = Fixture::new().await;
        let pause = pause_at(PausePoint::InsideTransaction);
        let permit = f.permit();
        let (input, _final_path, part) = f.input(&permit, Some(pause.clone()));

        let handle = spawn_commit(input);
        pause.reached.notified().await;
        drop(permit);

        let revoker = {
            let c = Arc::clone(&f.coordinator);
            tokio::spawn(async move { c.revoke_and_drain(&[scope()]).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!revoker.is_finished(), "the drain must wait for the commit task");

        pause.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(3), revoker)
            .await
            .expect("the drain returns once the task does")
            .unwrap();
        assert!(matches!(handle.await.unwrap().unwrap(), Settled::Done));

        assert!(f.blobs().await.is_empty(), "the claim never happened");
        assert!(f.clips().await.is_empty());
        assert!(!part.exists(), "the staged file is cleaned up");
        assert_eq!(f.coordinator.in_flight(&scope()), 0);
        assert!(f.coordinator.acquire(&scope(), "alice").is_none(), "revoked");
    }

    /// 没有停靠点的一次提交：从 `.part` 到 `ready` 行加 clip，再来一次同一条
    /// 消息什么都不重复。
    #[tokio::test]
    async fn a_plain_commit_publishes_once_and_a_replay_adds_nothing() {
        let f = Fixture::new().await;
        let permit = f.permit();
        let (input, final_path, _part) = f.input(&permit, None);
        assert!(matches!(spawn_commit(input).await.unwrap().unwrap(), Settled::Done));
        let (input, _, part) = f.input(&permit, None);
        assert!(matches!(spawn_commit(input).await.unwrap().unwrap(), Settled::Done));

        assert_eq!(f.blobs().await.len(), 1);
        assert_eq!(f.clips().await.len(), 1);
        assert!(final_path.exists());
        assert!(
            !part.exists(),
            "the second download is dropped, not kept beside the first"
        );
    }
}
