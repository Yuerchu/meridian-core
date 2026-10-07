//! 启动时把上一次死掉留下的东西收干净。
//!
//! 这一整件事之所以简单，是因为**只有一个 writer**（见 `CorpusLock`）。能拿到
//! 锁就说明没有别的进程在写，于是"这个 `.part` 是别人正在写的还是残骸"这个
//! 没法回答的问题根本不存在——它必然是残骸。多 writer 的版本要给每个对象带上
//! owner 实例和租约，还要一个跨进程的屏障，而那些全都是为了回答同一个问题。
//!
//! 数据库这一半在运行时上 `await`，文件那一半在 `spawn_blocking` 里：每一步先
//! 读出要处理的行，把哈希、删文件、扫目录整批交给一条阻塞线程，再把结果写回
//! 一个事务。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::db::entity::voice_blob;
use crate::db::sea::cap::Db;
use crate::db::sea::ops::voice_corpus as ops;

/// 收拾的结果，只用来记一行日志。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    pub staged_removed: usize,
    pub pending_dropped: usize,
    pub orphans_removed: usize,
    pub marked_damaged: usize,
    pub tombstones_cleared: usize,
    pub stray_files_removed: usize,
}

impl Recovered {
    fn is_quiet(&self) -> bool {
        *self == Self::default()
    }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(work).await.map_err(|e| e.to_string())
}

/// 五类对象，各有各的处理方式。
///
/// 拿不到锁就什么都不做：那时磁盘上的东西属于另一个进程。
/// `key` 是调用方先 `storage_key(db).await` 拿到的假名化密钥。
pub async fn run(db: &Db, key: &[u8], app_data_dir: &Path, writable: bool) -> Result<Recovered, String> {
    if !writable {
        return Ok(Recovered::default());
    }
    let now = crate::util::now_ms();
    let mut out = Recovered::default();
    let key: std::sync::Arc<[u8]> = key.into();
    let data_dir = app_data_dir.to_path_buf();

    // 1. staging 整个清空。
    let staging = super::staging_dir(app_data_dir);
    out.staged_removed = blocking(move || {
        let mut removed = 0;
        if let Ok(entries) = std::fs::read_dir(&staging) {
            for entry in entries.flatten() {
                if std::fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        removed
    })
    .await?;

    // 2. 还停在 pending 的行。它们的文件从没发布过（发布和转 ready 在同一个
    //    事务里，中间只隔一个 rename），所以删行就够。
    out.pending_dropped = db
        .write(async |tx| {
            let pending = ops::stale_pending(tx).await?;
            let ids: Vec<String> = pending.into_iter().map(|b| b.id).collect();
            ops::delete_blob_rows(tx, &ids).await
        })
        .await
        .map_err(|e: sea_orm::DbErr| e.to_string())? as usize;

    // 3. 说自己 ready、磁盘上却对不上的。**标出来而不是删掉**：那一行背后有真实
    //    的 clip，它们记着谁在什么时候说过话，而那份记录不该因为文件坏了就消失。
    //
    //    核的是 sha 而不只是大小，理由在 `file_matches` 上：这份语料要拿去训练，
    //    一条内容错了的样本会被当成真的用。整个目录哈希一遍是一条阻塞线程的活。
    // pool-read-before-write: recovery runs under the corpus lock, so no other
    // writer exists, and the hashing between these reads and their writes is
    // blocking work that must not hold the write lock.
    let ready = ops::all_ready(db).await.map_err(|e| e.to_string())?;
    let damaged_ids = {
        let key = key.clone();
        let data_dir = data_dir.clone();
        blocking(move || {
            ready
                .into_iter()
                .filter(|blob| !super::file_matches(&blob_path(&data_dir, &key, blob), blob.file_size, &blob.sha256))
                .map(|blob| blob.id)
                .collect::<Vec<String>>()
        })
        .await?
    };
    if !damaged_ids.is_empty() {
        out.marked_damaged = db
            .write(async |tx| {
                let mut marked = 0;
                for id in &damaged_ids {
                    marked += ops::mark_damaged(tx, id, now).await?;
                }
                Ok::<_, sea_orm::DbErr>(marked)
            })
            .await
            .map_err(|e| e.to_string())? as usize;
    }

    // 4. 没有任何 clip 指着的行——写完 blob、还没写 clip 就死了。**排在标坏
    //    之后**：一个刚被标坏又没人引用的行，排在前面就要再等一次启动才走得掉，
    //    而它的文件在那之前一直占着地方。
    // pool-read-before-write: under the corpus lock (see step 1).
    let orphans = ops::orphaned(db).await.map_err(|e| e.to_string())?;
    if !orphans.is_empty() {
        let ids = {
            let key = key.clone();
            let data_dir = data_dir.clone();
            blocking(move || {
                orphans
                    .into_iter()
                    .map(|blob| {
                        let _ = std::fs::remove_file(blob_path(&data_dir, &key, &blob));
                        blob.id
                    })
                    .collect::<Vec<String>>()
            })
            .await?
        };
        out.orphans_removed = db
            .write(async |tx| ops::delete_blob_rows(tx, &ids).await)
            .await
            .map_err(|e: sea_orm::DbErr| e.to_string())? as usize;
    }

    // 5. 墓碑：接着删。删成功了行才走。
    // pool-read-before-write: under the corpus lock (see step 1).
    let tombstones = ops::tombstones(db).await.map_err(|e| e.to_string())?;
    if !tombstones.is_empty() {
        let cleared = {
            let key = key.clone();
            let data_dir = data_dir.clone();
            blocking(move || {
                tombstones
                    .into_iter()
                    .filter(|blob| {
                        let path = blob_path(&data_dir, &key, blob);
                        !path.exists() || std::fs::remove_file(&path).is_ok()
                    })
                    .map(|blob| blob.id)
                    .collect::<Vec<String>>()
            })
            .await?
        };
        out.tombstones_cleared = db
            .write(async |tx| ops::delete_blob_rows(tx, &cleared).await)
            .await
            .map_err(|e: sea_orm::DbErr| e.to_string())? as usize;
    }

    // 6. 磁盘上有、库里没有的文件。上一步之后才做，否则会把刚标成 damaged 的
    //    那些误当成野文件——它们的行还在。
    let known: HashSet<PathBuf> = ops::all_blobs(db)
        .await
        .map_err(|e| e.to_string())?
        .iter()
        .map(|blob| blob_path(&data_dir, &key, blob))
        .collect();
    out.stray_files_removed = blocking(move || sweep_stray_files(&known, &data_dir)).await?;

    if !out.is_quiet() {
        tracing::info!(?out, "voice corpus: recovered after an unclean stop");
    }
    Ok(out)
}

fn blob_path(app_data_dir: &Path, key: &[u8], blob: &voice_blob::Model) -> PathBuf {
    let pseudonym = super::session_pseudonym(key, blob.bot_self_id, blob.source_type.as_str(), &blob.source_id);
    super::session_dir(app_data_dir, &pseudonym).join(&blob.file_name)
}

/// 库里没人认领的文件。
///
/// 按"目录 + 文件名"比对而不是只比文件名：两个会话可以有同一段音频的两份拷贝，
/// 那是有意为之（删一个不影响另一个），只比文件名会让其中一份看起来有人认领。
fn sweep_stray_files(known: &HashSet<PathBuf>, app_data_dir: &Path) -> usize {
    let root = super::corpus_dir(app_data_dir);
    let staging = super::staging_dir(app_data_dir);
    let mut removed = 0;
    let Ok(sessions) = std::fs::read_dir(&root) else {
        return 0;
    };
    for session in sessions.flatten() {
        let dir = session.path();
        // `.staging` 是我们自己的，锁文件也是。
        if !dir.is_dir() || dir == staging {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else { continue };
        for file in files.flatten() {
            let path = file.path();
            if path.is_file() && !known.contains(&path) && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::voice_blob::{VoiceBlobStatus, VoiceCorpusSourceType};
    use crate::db::sea::shared_test_db;

    /// 一个数据目录、一个库文件：行和 key 都走 SeaORM。
    async fn fixture() -> (tempfile::TempDir, Db, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let (_diesel, sea) = shared_test_db(dir.path()).await;
        let key = super::super::storage_key(&sea).await.unwrap();
        (dir, sea, key)
    }

    /// 行**照着字节来**：sha 和大小都从 `bytes` 算，所以 fixture 本身是自洽的,
    /// 一条测试要制造"对不上"就得明确地去改磁盘。连接只经 ops 可达，所以行是
    /// 走生产那几步到达目标状态的：claim，然后视状态 publish / 标坏 / 立墓碑。
    async fn blob(db: &Db, id: &str, status: VoiceBlobStatus, bytes: &[u8]) -> voice_blob::Model {
        let sha = sha_of(bytes);
        let key = ops::BlobKey {
            bot_self_id: 1,
            source_type: VoiceCorpusSourceType::OnebotGroup,
            source_id: "123",
            sha256: &sha,
            file_format: "amr",
        };
        let file_name = format!("{id}.amr");
        let size = bytes.len() as i64;
        db.write(async |tx| {
            let epoch = match ops::claim_blob(tx, &key, id, "t", &file_name, size, 1, 60_000).await? {
                ops::ClaimOutcome::Owned { epoch, .. } => epoch,
                other => panic!("expected to own a fresh row: {other:?}"),
            };
            if status != VoiceBlobStatus::Pending {
                assert!(ops::publish_blob(tx, id, "t", epoch, 1).await?);
            }
            match status {
                VoiceBlobStatus::Damaged => {
                    ops::mark_damaged(tx, id, 1).await?;
                }
                VoiceBlobStatus::Deleting => {
                    let doomed = ops::tombstone_unreferenced(tx, 1).await?;
                    assert!(doomed.iter().any(|b| b.id == id));
                }
                VoiceBlobStatus::Pending | VoiceBlobStatus::Ready => {}
            }
            Ok::<_, sea_orm::DbErr>(())
        })
        .await
        .unwrap();
        ops::find_blob(db, &key).await.unwrap().unwrap()
    }

    async fn clip(db: &Db, blob: &voice_blob::Model) {
        db.write(async |tx| ops::record_clip(tx, blob, "c1", "alice", Some(7), 0, None, None, 1).await)
            .await
            .unwrap();
    }

    async fn status_of(db: &Db, id: &str) -> VoiceBlobStatus {
        ops::all_blobs(db)
            .await
            .unwrap()
            .into_iter()
            .find(|b| b.id == id)
            .expect("the row is still there")
            .status
    }

    fn sha_of(bytes: &[u8]) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(bytes).iter().fold(String::new(), |mut acc, b| {
            use std::fmt::Write;
            let _ = write!(acc, "{b:02x}");
            acc
        })
    }

    fn write_blob_file(dir: &Path, key: &[u8], b: &voice_blob::Model, bytes: &[u8]) {
        let path = blob_path(dir, key, b);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// 拿不到锁时一个字节都不动——磁盘上的东西属于另一个进程。
    #[tokio::test]
    async fn a_reader_without_the_lock_touches_nothing() {
        let (dir, db, key) = fixture().await;
        blob(&db, "a", VoiceBlobStatus::Pending, b"abc").await;
        assert_eq!(run(&db, &key, dir.path(), false).await.unwrap(), Recovered::default());
        assert_eq!(status_of(&db, "a").await, VoiceBlobStatus::Pending);
    }

    /// pending 行是上次崩溃的残骸，staging 里的临时文件也是。
    #[tokio::test]
    async fn what_a_crash_left_behind_is_cleared() {
        let (dir, db, key) = fixture().await;
        blob(&db, "a", VoiceBlobStatus::Pending, b"abc").await;
        let staging = super::super::staging_dir(dir.path());
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("half.part"), b"xx").unwrap();

        let out = run(&db, &key, dir.path(), true).await.unwrap();
        assert_eq!(out.pending_dropped, 1);
        assert_eq!(out.staged_removed, 1);
        assert!(ops::all_blobs(&db).await.unwrap().is_empty());
    }

    /// 文件对不上的行被**标坏，不是删掉**：它背后的 clip 记着谁在什么时候
    /// 说过话，那份记录不该因为文件坏了就消失。
    #[tokio::test]
    async fn a_ready_row_whose_file_is_wrong_is_marked_not_dropped() {
        let (dir, db, key) = fixture().await;
        let b = blob(&db, "a", VoiceBlobStatus::Ready, b"the real bytes").await;
        clip(&db, &b).await;
        write_blob_file(dir.path(), &key, &b, b"short");

        let out = run(&db, &key, dir.path(), true).await.unwrap();
        assert_eq!(out.marked_damaged, 1);
        assert_eq!(
            status_of(&db, "a").await,
            VoiceBlobStatus::Damaged,
            "行还在，只是被标坏了"
        );
    }

    /// **同样长、内容不同**也要被认出来。
    ///
    /// 只比大小的那一版对这条一无所知：一次写到一半的崩溃、一块坏扇区、一次
    /// 同名覆盖，长度可以分毫不差。而这份语料是要拿去训练的——一条内容错了的
    /// 样本会被当成真的用，比一条缺失的贵得多。
    #[tokio::test]
    async fn a_file_of_the_right_length_but_the_wrong_bytes_is_still_wrong() {
        let (dir, db, key) = fixture().await;
        let b = blob(&db, "a", VoiceBlobStatus::Ready, b"aaaaa").await;
        clip(&db, &b).await;
        write_blob_file(dir.path(), &key, &b, b"bbbbb");

        assert_eq!(run(&db, &key, dir.path(), true).await.unwrap().marked_damaged, 1);
    }

    /// 没有任何 clip 指着的已发布行，连同它的文件一起走。
    #[tokio::test]
    async fn an_orphan_takes_its_file_with_it() {
        let (dir, db, key) = fixture().await;
        let b = blob(&db, "a", VoiceBlobStatus::Ready, b"hello").await;
        write_blob_file(dir.path(), &key, &b, b"hello");
        let path = blob_path(dir.path(), &key, &b);
        assert!(path.exists());

        let out = run(&db, &key, dir.path(), true).await.unwrap();
        assert_eq!(out.marked_damaged, 0, "文件是对的");
        assert_eq!(out.orphans_removed, 1);
        assert!(!path.exists(), "孤儿的文件不该留着占地方");
    }

    /// 墓碑接着删：文件删掉了行才走，文件本来就不在也算。
    #[tokio::test]
    async fn a_tombstone_is_cleared_once_its_file_is_gone() {
        let (dir, db, key) = fixture().await;
        let with_file = blob(&db, "a", VoiceBlobStatus::Deleting, b"hello").await;
        write_blob_file(dir.path(), &key, &with_file, b"hello");
        blob(&db, "b", VoiceBlobStatus::Deleting, b"gone").await;

        let out = run(&db, &key, dir.path(), true).await.unwrap();
        assert_eq!(out.tombstones_cleared, 2);
        assert!(!blob_path(dir.path(), &key, &with_file).exists());
        assert!(ops::all_blobs(&db).await.unwrap().is_empty());
    }

    /// 库里没人认领的文件也要清掉——那是删除删了一半留下的。
    #[tokio::test]
    async fn a_file_nobody_claims_is_swept() {
        let (dir, db, key) = fixture().await;
        let stray = super::super::session_dir(dir.path(), "deadbeefdeadbeef");
        std::fs::create_dir_all(&stray).unwrap();
        let path = stray.join("nobody.amr");
        std::fs::write(&path, b"x").unwrap();

        let out = run(&db, &key, dir.path(), true).await.unwrap();
        assert_eq!(out.stray_files_removed, 1);
        assert!(!path.exists());
    }
}
