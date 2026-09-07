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

use std::sync::Arc;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::format::MediaRef;
use super::protocol::OneBotAction;
use super::{DirectedCallOutcome, SharedState};
use crate::db::models::voice_corpus::VoiceBlobRow;
use crate::db::ops::voice_corpus as ops;
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
    pub source_type: &'static str,
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

async fn capture_all(state: &Arc<SharedState>, source: &RecordSource, records: &[MediaRef], permit: &CapturePermit) {
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
struct Staged {
    path: std::path::PathBuf,
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
    permit: &CapturePermit,
) -> Result<(), String> {
    let data_dir = state.services.paths.data_dir.clone();
    let staged = fetch_to_staging(record, &data_dir).await?;

    // 一次采集可能跑几十秒，而这中间用户可能把这个会话从白名单里拿掉。permit
    // 保证撤权会等我们结束，但不保证写下去的东西还是用户想要的。这里问一次是
    // 为了省掉后面的活；**决定性的那一次在写事务里面**，见 `commit`。
    if !permit.still_authorised() {
        tracing::info!(session = %source.scope, "voice capture dropped: the grant moved while it was running");
        return Ok(());
    }

    let key_bytes = voice_corpus::storage_key(&state.services.db)?;
    let pseudonym = voice_corpus::session_pseudonym(
        &key_bytes,
        source.scope.bot_self_id,
        source.source_type,
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
        let pool = state.services.db.clone();
        let auth = permit.authorisation();
        let source = source.clone();
        let transcript = transcript.map(str::to_string);
        let staged_path = staged.path.clone();
        let final_path = final_path.clone();
        let file_name = file_name.clone();
        let sha = staged.sha256.clone();
        let format = staged.format;
        let size = staged.size;

        let settled = tokio::task::spawn_blocking(move || {
            commit(CommitInput {
                pool,
                auth,
                source,
                transcript,
                staged_path,
                final_path,
                file_name,
                sha,
                format,
                size,
                segment_index,
            })
        })
        .await
        .map_err(|e| e.to_string())??;

        if settled == Settled::Done {
            return Ok(());
        }
    }
    tracing::warn!(
        session = %source.scope,
        "voice capture gave up: another task held these bytes for the whole window"
    );
    Ok(())
}

/// [`commit`] 的参数。一个结构体而不是十一个位置参数——顺序相同类型相同的
/// `String` 太多，写反了编译器不会说话。
struct CommitInput {
    pool: crate::db::DbPool,
    auth: voice_corpus::Authorisation,
    source: RecordSource,
    transcript: Option<String>,
    staged_path: std::path::PathBuf,
    final_path: std::path::PathBuf,
    file_name: String,
    sha: String,
    format: &'static str,
    size: i64,
    segment_index: i32,
}

#[derive(PartialEq, Eq)]
enum Settled {
    Done,
    /// 有人正握着这段音频。退一步再来。
    Retry,
}

/// 事务里的失败。
///
/// 要一个自己的类型，是因为 diesel 的 `immediate_transaction` 要求错误能从
/// `diesel::result::Error` 转过来，而这段代码有一半的失败来自文件系统。全都
/// 压成 `String` 就得让文件错误冒充数据库错误，回滚的原因在日志里会对不上号。
#[derive(Debug)]
enum CommitError {
    Db(diesel::result::Error),
    File(String),
}

impl From<diesel::result::Error> for CommitError {
    fn from(error: diesel::result::Error) -> Self {
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
fn commit(input: CommitInput) -> Result<Settled, String> {
    let CommitInput {
        pool,
        auth,
        source,
        transcript,
        staged_path,
        final_path,
        file_name,
        sha,
        format,
        size,
        segment_index,
    } = input;
    let mut conn = crate::util::get_conn(&pool)?;
    let now = crate::util::now_ms();
    let key = ops::BlobKey {
        bot_self_id: source.scope.bot_self_id,
        source_type: source.source_type,
        source_id: &source.source_id,
        sha256: &sha,
        file_format: format,
    };

    conn.immediate_transaction(|conn| {
        if !auth.still_authorised() {
            tracing::info!(session = %source.scope, "voice capture dropped: the grant moved while it was running");
            return Ok(Settled::Done);
        }

        let blob = match settle_blob(conn, &key, &file_name, size, &staged_path, &final_path, now)? {
            Claimed::Blob(blob) => blob,
            Claimed::Retry => return Ok(Settled::Retry),
            // 它正在被删除，或者别人已经把它标坏了。两种情况下这次都不写。
            Claimed::Skip => return Ok(Settled::Done),
        };

        ops::record_clip(
            conn,
            &blob,
            &uuid::Uuid::new_v4().to_string(),
            &source.sender_id.to_string(),
            source.message_id,
            segment_index,
            transcript.as_deref(),
            transcript.as_deref().map(|_| "llonebot.voice_msg_to_text"),
            now,
        )?;
        Ok(Settled::Done)
    })
    .map_err(|error: CommitError| error.to_string())
}

/// [`settle_blob`] 的三种答案。
enum Claimed {
    /// 可以挂 clip 的那一行。
    Blob(VoiceBlobRow),
    /// 有人正握着它，退一步再来。
    Retry,
    /// 这次不写：墓碑，或者已经被标坏了。
    Skip,
}

/// 抢所有权、发布文件，返回可以挂 clip 的那一行。
#[allow(clippy::too_many_arguments)]
fn settle_blob(
    conn: &mut diesel::SqliteConnection,
    key: &ops::BlobKey<'_>,
    file_name: &str,
    size: i64,
    staged_path: &std::path::Path,
    final_path: &std::path::Path,
    now: i64,
) -> Result<Claimed, CommitError> {
    let id = uuid::Uuid::new_v4().to_string();
    let token = uuid::Uuid::new_v4().to_string();
    match ops::claim_blob(conn, key, &id, &token, file_name, size, now, LEASE_MS)? {
        ops::ClaimOutcome::Owned { id, token, epoch } => {
            publish_file(staged_path, final_path).map_err(CommitError::File)?;
            // fencing:返回 false 就是所有权在下载期间被接管了。这个任务既不
            // 发布也不写 clip——文件已经在位,让接管者去 publish。
            if !ops::publish_blob(conn, &id, &token, epoch, now)? {
                return Ok(Claimed::Retry);
            }
            read_blob(conn, key)
        }
        ops::ClaimOutcome::Ready(blob) => {
            // 已经有一份。校验磁盘上那个确实对得上——对不上就是它坏了,
            // 标出来而不是把新 clip 挂到一个坏文件上。
            if voice_corpus::file_matches(final_path, blob.file_size, &blob.sha256) {
                return Ok(Claimed::Blob(blob));
            }
            ops::mark_damaged(conn, &blob.id, now)?;
            Ok(Claimed::Skip)
        }
        ops::ClaimOutcome::Takeable(blob) => {
            let token = uuid::Uuid::new_v4().to_string();
            let taken = ops::takeover_blob(
                conn,
                &blob.id,
                blob.owner_token.as_deref(),
                blob.fence_epoch,
                &token,
                now,
                LEASE_MS,
            )?;
            let Some(epoch) = taken else { return Ok(Claimed::Retry) };
            publish_file(staged_path, final_path).map_err(CommitError::File)?;
            if !ops::publish_blob(conn, &blob.id, &token, epoch, now)? {
                return Ok(Claimed::Retry);
            }
            read_blob(conn, key)
        }
        ops::ClaimOutcome::PendingElsewhere(_) => Ok(Claimed::Retry),
        ops::ClaimOutcome::Damaged(_) => Ok(Claimed::Skip),
        ops::ClaimOutcome::Deleting(_) => {
            tracing::debug!("voice capture skipped: these bytes are being deleted");
            Ok(Claimed::Skip)
        }
    }
}

fn read_blob(conn: &mut diesel::SqliteConnection, key: &ops::BlobKey<'_>) -> Result<Claimed, CommitError> {
    use crate::db::schema::voice_blobs;
    use diesel::prelude::*;
    let blob = voice_blobs::table
        .filter(voice_blobs::bot_self_id.eq(key.bot_self_id))
        .filter(voice_blobs::source_type.eq(key.source_type))
        .filter(voice_blobs::source_id.eq(key.source_id))
        .filter(voice_blobs::file_format.eq(key.file_format))
        .filter(voice_blobs::sha256.eq(key.sha256))
        .select(VoiceBlobRow::as_select())
        .first(conn)
        .optional()?;
    // 刚刚 publish 过，所以它必然在。真读不到就当作没抢到，让上面再转一圈。
    Ok(blob.map_or(Claimed::Retry, Claimed::Blob))
}

/// 把临时文件挪到最终位置。
///
/// **Windows 上 `rename` 在目标已存在时会失败**（Unix 是覆盖），所以先看目标
/// 在不在：在就直接用它（内容寻址保证字节相同，这里的 TOCTOU 无害）。
/// 剩下的失败——权限、磁盘满、父目录缺失——要报出来，不能伪装成"别人赢了"。
fn publish_file(staged: &std::path::Path, final_path: &std::path::Path) -> Result<(), String> {
    if final_path.exists() {
        let _ = std::fs::remove_file(staged);
        return Ok(());
    }
    match std::fs::rename(staged, final_path) {
        Ok(()) => Ok(()),
        Err(_) if final_path.exists() => {
            let _ = std::fs::remove_file(staged);
            Ok(())
        }
        Err(e) => Err(format!("could not publish the audio: {e}")),
    }
}

/// 两档获取，边下边算 sha。
///
/// **`get_record` 不在其中**：它的 `out_format` 是必填的（必然转码，与"原样
/// 落盘"矛盾），返回的是**运行适配器那台机器**上的路径（反向 WS 在另一台机器
/// 时读不到），而"是否同机"没有可靠依据——`onebot.host` 是监听地址不是 peer。
/// 做对它要引入"授权根目录"让 WS 对端指定 Meridian 去读本机文件，那是一个新的
/// 攻击面，换一档必然转码的数据。
async fn fetch_to_staging(record: &MediaRef, data_dir: &std::path::Path) -> Result<Staged, String> {
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
            let digest = Sha256::digest(&bytes);
            let sha = digest.iter().fold(String::new(), |mut acc, b| {
                use std::fmt::Write;
                let _ = write!(acc, "{b:02x}");
                acc
            });
            Ok((sha, magic_format(&bytes)))
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

async fn stream_to_file(url: &str, path: &std::path::Path) -> Result<u64, String> {
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

async fn write_base64(payload: &str, path: &std::path::Path) -> Result<u64, String> {
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
    use super::*;

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
}
