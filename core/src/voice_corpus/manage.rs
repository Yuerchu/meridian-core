//! 看一眼、导出、删掉。
//!
//! 三件事共用一个前提：**语料是给人管的**，所以每一个都要能回答"这动了什么、
//! 哪半边没成功"。返回一个数字的删除说不出文件删了而行没删。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::db::DbPool;
use crate::db::models::voice_corpus::{VoiceBlobRow, VoiceCorpusSourceType};
use crate::db::ops::voice_corpus as ops;

/// 要动哪些语料。
///
/// 没有"缺省即全部"：调用方必须显式构造其中一个目标。shell 的
/// `VoiceCorpusDeleteSelector` 是严格的 tagged union，缺字段或未知字段会在进入
/// core 之前反序列化失败。
#[derive(Debug, Clone)]
pub enum CorpusSelector {
    /// 一个会话，用它的**假名**指认。
    ///
    /// 不是 `(bot_self_id, session)`：那一对就是 bot 的 QQ 号加群号，而私聊那一
    /// 档的"群号"是对方本人的号。列表和删除都要经远程接口送到另一台设备上，
    /// 所以两边说的都是这个不可逆的名字，真实的号从不出这台机器。
    Session { handle: String },
    /// 跨会话按人。这就是"把我的声音删掉"，也是 `idx_voice_clips_sender` 存在
    /// 的理由。这一档的 id 是**用户自己打进来的**，不是我们发出去的。
    Sender { id: String },
    /// 确认短语必须一字不差，和 `/forget all` 同一条纪律。
    All { confirmation: String },
}

pub const DELETE_ALL_CONFIRMATION: &str = "DELETE ALL VOICE";

/// 假名换回真身。
///
/// 没有反查表：假名是 HMAC，所以把还在库里的每个会话算一遍再比对就够了。
/// 认不出来是错误而不是"什么都不删"——一个说了删却什么都没删的按钮，比一个
/// 报错的按钮糟。
fn resolve_handle(pool: &DbPool, handle: &str) -> Result<(i64, VoiceCorpusSourceType, String), String> {
    let key = crate::voice_corpus::storage_key(pool)?;
    let mut conn = crate::util::get_conn(pool)?;
    for total in ops::session_totals(&mut conn).map_err(|e| e.to_string())? {
        let source_type = VoiceCorpusSourceType::parse(&total.source_type)?;
        let pseudonym =
            crate::voice_corpus::session_pseudonym(&key, total.bot_self_id, source_type.as_str(), &total.source_id);
        if pseudonym == handle {
            return Ok((total.bot_self_id, source_type, total.source_id));
        }
    }
    Err("no stored voice matches that session".into())
}

/// 这个进程能不能写语料。
///
/// 拿不到语料目录的独占锁就一律拒绝——**删除也一样**。第二个实例的内存屏障对
/// 持锁的那个实例毫无约束力，所以它一边删库删文件、另一边还在往同一个目录里
/// 录，删除会报告成功而磁盘上并没有干净。
fn require_writer(coordinator: &crate::voice_corpus::CorpusCoordinator) -> Result<(), String> {
    if coordinator.writable() {
        return Ok(());
    }
    Err("another Meridian instance holds the voice corpus; close it and try again".into())
}

/// 一个会话攒了多少，给设置页看。
pub type SessionTotal = ops::SessionTotal;

pub fn list_sessions(pool: &DbPool) -> Result<Vec<SessionTotal>, String> {
    let mut conn = crate::util::get_conn(pool)?;
    let totals = ops::session_totals(&mut conn).map_err(|e| e.to_string())?;
    for total in &totals {
        VoiceCorpusSourceType::parse(&total.source_type)?;
    }
    Ok(totals)
}

#[derive(Debug, Default)]
pub struct DeleteReport {
    pub clips: usize,
    pub files: usize,
    pub bytes: i64,
    /// 哪半边没成功。**不是一个数字**：文件与数据库可能部分失败，而一个总数
    /// 说不出是哪一半。
    pub failures: Vec<String>,
}

/// 删除历史。
///
/// 顺序是设计的一部分：**先撤权并等在途采集结束**（否则刚删完就有一条新的落
/// 盘），**再删 clips**，**然后只把没有任何 clip 指着的 blob 标成墓碑**——多个
/// 发送者的 clip 可以指向同一份音频，按发送者直接删会连带抹掉别人的合法样本。
/// 最后才删文件，删成功了行才走。
///
/// 这个函数**只处理已有数据**。"以后别再录我"是 [`set_optout`]，两件事分开是
/// 因为合并意味着一次手滑要么删掉几个月的数据，要么把一次删除变成永久停录。
pub async fn delete(
    pool: &DbPool,
    app_data_dir: &Path,
    coordinator: &crate::voice_corpus::CorpusCoordinator,
    selector: CorpusSelector,
) -> Result<DeleteReport, String> {
    require_writer(coordinator)?;
    if let CorpusSelector::All { confirmation } = &selector
        && confirmation != DELETE_ALL_CONFIRMATION
    {
        return Err(format!("confirmation must be exactly `{DELETE_ALL_CONFIRMATION}`"));
    }

    // 假名在立屏障之前就要换回真身：屏障要挂在真实的会话上，而且认不出来的
    // 句柄该在什么都没动之前就报错。
    let resolved = match &selector {
        CorpusSelector::Session { handle } => {
            let pool = pool.clone();
            let handle = handle.clone();
            Some(
                tokio::task::spawn_blocking(move || resolve_handle(&pool, &handle))
                    .await
                    .map_err(|e| e.to_string())??,
            )
        }
        _ => None,
    };

    // 屏障覆盖到哪，取决于删的是什么。按人删跨会话，所以那一档挡住全部——
    // 删除期间少采几秒，比删完发现刚又录进来一条好。
    let scopes = match &resolved {
        Some((bot_self_id, source_type, source_id)) => {
            vec![crate::voice_corpus::CaptureScope::new(
                *bot_self_id,
                session_key(*source_type, source_id),
            )]
        }
        None => coordinator.granted_scopes(),
    };
    coordinator.revoke_and_drain_temporarily(&scopes).await;

    let pool2 = pool.clone();
    let data_dir = app_data_dir.to_path_buf();
    let out = tokio::task::spawn_blocking(move || delete_blocking(&pool2, &data_dir, selector, resolved))
        .await
        .map_err(|e| e.to_string())?;

    coordinator.lift_barriers(&scopes);
    out
}

fn delete_blocking(
    pool: &DbPool,
    app_data_dir: &Path,
    selector: CorpusSelector,
    resolved: Option<(i64, VoiceCorpusSourceType, String)>,
) -> Result<DeleteReport, String> {
    let key = crate::voice_corpus::storage_key(pool)?;
    let mut conn = crate::util::get_conn(pool)?;
    let now = crate::util::now_ms();

    let clips = match &selector {
        CorpusSelector::Session { .. } => {
            let (bot_self_id, source_type, source_id) = resolved.ok_or("the session was never resolved")?;
            ops::delete_clips_by_session(&mut conn, bot_self_id, source_type.as_str(), &source_id)
        }
        CorpusSelector::Sender { id } => ops::delete_clips_by_sender(&mut conn, id),
        CorpusSelector::All { .. } => ops::delete_all_clips(&mut conn),
    }
    .map_err(|e| e.to_string())?;
    let mut report = DeleteReport {
        clips,
        ..Default::default()
    };

    for blob in ops::tombstone_unreferenced(&mut conn, now).map_err(|e| e.to_string())? {
        let path = blob_path(app_data_dir, &key, &blob);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                report.files += 1;
                report.bytes += blob.file_size;
                ops::delete_blob_rows(&mut conn, &[blob.id]).map_err(|e| e.to_string())?;
            }
            // 文件不在了也算成功——目标是它不存在。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                ops::delete_blob_rows(&mut conn, &[blob.id]).map_err(|e| e.to_string())?;
            }
            Err(e) => {
                // 行留着 `deleting`，恢复器下次接着删。**不含绝对路径**：
                // 这条消息会经远程接口送到另一台设备上。
                report.failures.push(format!("could not delete one file: {}", e.kind()));
            }
        }
    }
    Ok(report)
}

/// "以后别再录我"。与删除历史分开，见 [`delete`]。
pub fn set_optout(
    pool: &DbPool,
    coordinator: &crate::voice_corpus::CorpusCoordinator,
    sender_id: &str,
    on: bool,
) -> Result<(), String> {
    require_writer(coordinator)?;
    let mut conn = crate::util::get_conn(pool)?;
    let now = crate::util::now_ms();
    if on {
        ops::set_optout(&mut conn, sender_id, now).map_err(|e| e.to_string())?;
    } else {
        ops::clear_optout(&mut conn, sender_id).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// "把我的声音删掉，以后也别再录。"
///
/// **一个操作，不是两个。** 拆成"删除"加"拒绝将来"两次调用时，中间那一段
/// 屏障已经撤了、名单还没生效——这中间落盘的录音谁都不会再回头删。所以顺序
/// 是反的：**先让拒绝生效**（写库、热刷新，此后 `acquire` 一律不发 permit），
/// 再 drain 并删除。这样删除跑的时候，能产生新录音的路已经堵死了。
///
/// `refresh` 是调用方给的"让名单立刻生效"，因为热刷新要读 OneBot 的配置，
/// 而那是上一层的事。
pub async fn forget_sender<F, Fut>(
    pool: &DbPool,
    app_data_dir: &Path,
    coordinator: &crate::voice_corpus::CorpusCoordinator,
    sender_id: &str,
    refresh: F,
) -> Result<DeleteReport, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    require_writer(coordinator)?;
    {
        let pool = pool.clone();
        let sender_id = sender_id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut conn = crate::util::get_conn(&pool)?;
            ops::set_optout(&mut conn, &sender_id, crate::util::now_ms()).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;
    }
    refresh().await?;
    delete(
        pool,
        app_data_dir,
        coordinator,
        CorpusSelector::Sender {
            id: sender_id.to_string(),
        },
    )
    .await
}

#[derive(Debug)]
pub struct ExportReport {
    pub clips: usize,
    /// 没有转写而被跳过的条数。**要露出来**：默认不导出它们是一个决定，
    /// 而一个看不见的决定读起来像是数据本来就只有这些。
    pub skipped: usize,
    pub bytes: i64,
    pub path: String,
}

/// 导出成 `manifest.jsonl` + `audio/` 的 bundle。
///
/// 固定这个形状，不给"音频拷到哪"之类的参数：那些参数没有 UI，语义也没定义过，
/// 而 recipe 那边要的就是一个自洽的目录。
pub fn export(
    pool: &DbPool,
    app_data_dir: &Path,
    output_dir: &Path,
    include_sender: bool,
    include_untranscribed: bool,
) -> Result<ExportReport, String> {
    use std::io::Write;

    // 目标必须是空的（或者还不存在）。上一次导出的 `audio/` 里可能躺着这次已经
    // 删掉的录音，只重写 manifest 就是让"已被要求删除的音频"以旧文件的身份继续
    // 存在，而 manifest 还声称这个 bundle 是自洽的。也不替用户清场：这是一个
    // 用户随手指定的目录，在里面递归删除的代价比拒绝高得多。
    match std::fs::read_dir(output_dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err("export destination is not empty; pick an empty or new directory".into());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }

    let key = crate::voice_corpus::storage_key(pool)?;
    let mut conn = crate::util::get_conn(pool)?;
    let clips = ops::export_rows(&mut conn).map_err(|e| e.to_string())?;

    let audio_dir = output_dir.join("audio");
    std::fs::create_dir_all(&audio_dir).map_err(|e| e.to_string())?;
    let mut manifest = std::fs::File::create(output_dir.join("manifest.jsonl")).map_err(|e| e.to_string())?;

    let mut report = ExportReport {
        clips: 0,
        skipped: 0,
        bytes: 0,
        path: output_dir.display().to_string(),
    };

    for (clip, blob) in clips {
        if clip.transcript.is_none() && !include_untranscribed {
            // 当初同意留下的是"音频 + 转写"这一对，不是一段没有配对文本的录音。
            report.skipped += 1;
            continue;
        }
        let session =
            crate::voice_corpus::session_pseudonym(&key, blob.bot_self_id, &blob.source_type, &blob.source_id);
        let relative = format!("audio/{session}/{}", blob.file_name);
        let dest = output_dir.join(&relative);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if std::fs::copy(blob_path(app_data_dir, &key, &blob), &dest).is_err() {
            report.skipped += 1;
            continue;
        }

        // 每一行都是**手写的字段清单**，不是把行序列化出去。`exportable()` 那条
        // 教训的一般化:一个 catch-all 分支曾把注入的记忆块写成了训练样本。
        // 这里没有会话文本、没有消息体、没有昵称、没有群名。
        let line = serde_json::json!({
            "audio_filepath": relative,
            "text": clip.transcript,
            "session": session,
            "speaker": if include_sender {
                serde_json::Value::String(clip.sender_id.clone())
            } else {
                serde_json::Value::String(crate::voice_corpus::sender_pseudonym(&key, &clip.sender_id))
            },
            "format": blob.file_format,
            "bytes": blob.file_size,
            "transcript_source": clip.transcript_source,
            "captured_at": clip.created_at,
        });
        writeln!(manifest, "{line}").map_err(|e| e.to_string())?;
        report.clips += 1;
        report.bytes += blob.file_size;
    }

    if include_sender {
        // 留一条可审计的记录。假名化是默认，真实号是另一条路。
        tracing::info!(clips = report.clips, "voice corpus exported with raw sender ids");
    }
    Ok(report)
}

fn blob_path(app_data_dir: &Path, key: &[u8], blob: &VoiceBlobRow) -> PathBuf {
    let pseudonym = crate::voice_corpus::session_pseudonym(key, blob.bot_self_id, &blob.source_type, &blob.source_id);
    crate::voice_corpus::session_dir(app_data_dir, &pseudonym).join(&blob.file_name)
}

/// `(OnebotGroup, "123")` -> `group:123`，白名单里写的那个形式。
fn session_key(source_type: VoiceCorpusSourceType, source_id: &str) -> String {
    let kind = match source_type {
        VoiceCorpusSourceType::OnebotGroup => "group",
        VoiceCorpusSourceType::OnebotPrivate => "private",
    };
    format!("{kind}:{source_id}")
}

/// 一次安装内稳定的假名，跨安装不可关联。给设置页显示用。
pub fn session_label(pool: &DbPool, total: &SessionTotal) -> Result<String, String> {
    let key = crate::voice_corpus::storage_key(pool)?;
    let source_type = VoiceCorpusSourceType::parse(&total.source_type)?;
    Ok(crate::voice_corpus::session_pseudonym(
        &key,
        total.bot_self_id,
        source_type.as_str(),
        &total.source_id,
    ))
}

/// 每个会话有多少条没有转写——设置页要能说明"导出会跳过多少"。
pub fn untranscribed_counts(pool: &DbPool) -> Result<HashMap<String, i64>, String> {
    let mut conn = crate::util::get_conn(pool)?;
    ops::untranscribed_by_session(&mut conn).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::voice_corpus::{VoiceBlobInsert, blob_status};
    use crate::db::test_db;
    use diesel::prelude::*;

    fn ready(conn: &mut diesel::SqliteConnection, id: &str, session: &str) -> VoiceBlobRow {
        use crate::db::schema::voice_blobs;
        diesel::insert_into(voice_blobs::table)
            .values(&VoiceBlobInsert {
                id,
                bot_self_id: 1,
                source_type: "onebot_group",
                source_id: session,
                sha256: id,
                file_format: "amr",
                file_name: &format!("{id}.amr"),
                file_size: 4,
                status: blob_status::READY,
                owner_token: None,
                fence_epoch: 0,
                lease_expires_at: None,
                created_at: 1,
                updated_at: 1,
            })
            .execute(conn)
            .unwrap();
        voice_blobs::table
            .find(id)
            .select(VoiceBlobRow::as_select())
            .first(conn)
            .unwrap()
    }

    #[test]
    fn session_keys_only_accept_closed_source_types() {
        assert_eq!(session_key(VoiceCorpusSourceType::OnebotGroup, "123"), "group:123");
        assert_eq!(session_key(VoiceCorpusSourceType::OnebotPrivate, "456"), "private:456");

        let error = VoiceCorpusSourceType::parse("onebot_channel").unwrap_err();
        assert!(error.contains("unknown voice corpus source_type"), "{error}");
    }

    #[test]
    fn management_rejects_an_unknown_persisted_source_type() {
        let pool = test_db();
        {
            let mut conn = pool.get().unwrap();
            let blob = ready(&mut conn, "a", "123");
            ops::record_clip(&mut conn, &blob, "c1", "alice", Some(1), 0, None, None, 1).unwrap();

            use crate::db::schema::voice_blobs;
            diesel::update(voice_blobs::table.find("a"))
                .set(voice_blobs::source_type.eq("onebot_channel"))
                .execute(&mut conn)
                .unwrap();
        }

        let error = list_sessions(&pool).unwrap_err();
        assert!(error.contains("unknown voice corpus source_type"), "{error}");

        let key = crate::voice_corpus::storage_key(&pool).unwrap();
        let handle = crate::voice_corpus::session_pseudonym(&key, 1, "onebot_channel", "123");
        let error = resolve_handle(&pool, &handle).unwrap_err();
        assert!(error.contains("unknown voice corpus source_type"), "{error}");
    }

    /// 导出默认跳过没有转写的，而且**把跳过的条数说出来**。
    #[test]
    fn an_export_says_how_much_it_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let pool = test_db();
        {
            let mut conn = pool.get().unwrap();
            let a = ready(&mut conn, "a", "123");
            let b = ready(&mut conn, "b", "123");
            ops::record_clip(&mut conn, &a, "c1", "alice", Some(1), 0, Some("你好"), Some("s"), 1).unwrap();
            ops::record_clip(&mut conn, &b, "c2", "bob", Some(2), 0, None, None, 1).unwrap();
        }
        let key = crate::voice_corpus::storage_key(&pool).unwrap();
        for id in ["a", "b"] {
            let mut conn = pool.get().unwrap();
            use crate::db::schema::voice_blobs;
            let blob: VoiceBlobRow = voice_blobs::table
                .find(id)
                .select(VoiceBlobRow::as_select())
                .first(&mut conn)
                .unwrap();
            let path = blob_path(dir.path(), &key, &blob);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"abcd").unwrap();
        }

        let report = export(&pool, dir.path(), out.path(), false, false).unwrap();
        assert_eq!(report.clips, 1);
        assert_eq!(report.skipped, 1, "没有转写的那条被跳过，并且说了出来");

        let manifest = std::fs::read_to_string(out.path().join("manifest.jsonl")).unwrap();
        assert!(manifest.contains("你好"));
        assert!(!manifest.contains("alice"), "默认写假名，不是 QQ 号");
        assert!(manifest.contains("audio/"));
    }

    /// 非空目录拒绝导出。上一次 bundle 的 `audio/` 里可能躺着这次已经删掉的
    /// 录音，只重写 manifest 会让它们以旧文件的身份继续存在；也不替用户清场，
    /// 这是一个用户随手指定的目录。
    #[test]
    fn an_export_refuses_a_directory_that_already_has_content() {
        let dir = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("leftover.txt"), b"old").unwrap();
        let pool = test_db();

        let err = export(&pool, dir.path(), out.path(), false, false).unwrap_err();
        assert!(err.contains("not empty"), "{err}");
        // 而一个还不存在的目录是可以的——由导出自己创建。
        assert!(export(&pool, dir.path(), &out.path().join("fresh"), false, false).is_ok());
    }

    /// 带上真实发送者是另一条路，要显式要求。
    #[test]
    fn asking_for_real_sender_ids_is_a_separate_decision() {
        let dir = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let pool = test_db();
        {
            let mut conn = pool.get().unwrap();
            let a = ready(&mut conn, "a", "123");
            ops::record_clip(&mut conn, &a, "c1", "alice", Some(1), 0, Some("你好"), Some("s"), 1).unwrap();
        }
        let key = crate::voice_corpus::storage_key(&pool).unwrap();
        let mut conn = pool.get().unwrap();
        use crate::db::schema::voice_blobs;
        let blob: VoiceBlobRow = voice_blobs::table
            .find("a")
            .select(VoiceBlobRow::as_select())
            .first(&mut conn)
            .unwrap();
        drop(conn);
        let path = blob_path(dir.path(), &key, &blob);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"abcd").unwrap();

        export(&pool, dir.path(), out.path(), true, false).unwrap();
        let manifest = std::fs::read_to_string(out.path().join("manifest.jsonl")).unwrap();
        assert!(manifest.contains("alice"));
    }
}
