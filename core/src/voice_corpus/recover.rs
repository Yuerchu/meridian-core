//! 启动时把上一次死掉留下的东西收干净。
//!
//! 这一整件事之所以简单，是因为**只有一个 writer**（见 `CorpusLock`）。能拿到
//! 锁就说明没有别的进程在写，于是"这个 `.part` 是别人正在写的还是残骸"这个
//! 没法回答的问题根本不存在——它必然是残骸。多 writer 的版本要给每个对象带上
//! owner 实例和租约，还要一个跨进程的屏障，而那些全都是为了回答同一个问题。

use std::collections::HashSet;
use std::path::Path;

use crate::db::DbPool;
use crate::db::models::voice_corpus::VoiceBlobRow;
use crate::db::ops::voice_corpus as ops;

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

/// 五类对象，各有各的处理方式。
///
/// 拿不到锁就什么都不做：那时磁盘上的东西属于另一个进程。
pub fn run(pool: &DbPool, app_data_dir: &Path, writable: bool) -> Result<Recovered, String> {
    if !writable {
        return Ok(Recovered::default());
    }
    // key 要在拿连接**之前**取。`storage_key` 自己要一个连接，而这个函数会一直
    // 持着它的那个直到结束——两者叠在一起，一个只有一条连接的池就死等到超时。
    let key = super::storage_key(pool)?;
    let mut conn = crate::util::get_conn(pool)?;
    let now = crate::util::now_ms();
    let mut out = Recovered::default();

    // 1. staging 整个清空。
    let staging = super::staging_dir(app_data_dir);
    if let Ok(entries) = std::fs::read_dir(&staging) {
        for entry in entries.flatten() {
            if std::fs::remove_file(entry.path()).is_ok() {
                out.staged_removed += 1;
            }
        }
    }

    // 2. 还停在 pending 的行。它们的文件从没发布过（发布和转 ready 在同一个
    //    调用里，中间只隔一个 rename），所以删行就够。
    let pending = ops::stale_pending(&mut conn).map_err(|e| e.to_string())?;
    let pending_ids: Vec<String> = pending.iter().map(|b| b.id.clone()).collect();
    out.pending_dropped = ops::delete_blob_rows(&mut conn, &pending_ids).map_err(|e| e.to_string())?;

    // 3. 说自己 ready、磁盘上却对不上的。**标出来而不是删掉**：那一行背后有真实
    //    的 clip，它们记着谁在什么时候说过话，而那份记录不该因为文件坏了就消失。
    //
    //    核的是 sha 而不只是大小，理由在 `file_matches` 上：这份语料要拿去训练，
    //    一条内容错了的样本会被当成真的用。
    for blob in ops::all_ready(&mut conn).map_err(|e| e.to_string())? {
        let path = blob_path(app_data_dir, &key, &blob);
        if !super::file_matches(&path, blob.file_size, &blob.sha256) {
            ops::mark_damaged(&mut conn, &blob.id, now).map_err(|e| e.to_string())?;
            out.marked_damaged += 1;
        }
    }

    // 4. 没有任何 clip 指着的行——写完 blob、还没写 clip 就死了。**排在标坏
    //    之后**：一个刚被标坏又没人引用的行，排在前面就要再等一次启动才走得掉，
    //    而它的文件在那之前一直占着地方。
    for blob in ops::orphaned(&mut conn).map_err(|e| e.to_string())? {
        let _ = std::fs::remove_file(blob_path(app_data_dir, &key, &blob));
        out.orphans_removed += ops::delete_blob_rows(&mut conn, &[blob.id]).map_err(|e| e.to_string())?;
    }

    // 5. 墓碑：接着删。删成功了行才走。
    for blob in ops::tombstones(&mut conn).map_err(|e| e.to_string())? {
        let path = blob_path(app_data_dir, &key, &blob);
        if !path.exists() || std::fs::remove_file(&path).is_ok() {
            out.tombstones_cleared += ops::delete_blob_rows(&mut conn, &[blob.id]).map_err(|e| e.to_string())?;
        }
    }

    // 6. 磁盘上有、库里没有的文件。上一步之后才做，否则会把刚标成 damaged 的
    //    那些误当成野文件——它们的行还在。
    out.stray_files_removed = sweep_stray_files(&mut conn, app_data_dir, &key)?;

    if !out.is_quiet() {
        tracing::info!(?out, "voice corpus: recovered after an unclean stop");
    }
    Ok(out)
}

fn blob_path(app_data_dir: &Path, key: &[u8], blob: &VoiceBlobRow) -> std::path::PathBuf {
    let pseudonym = super::session_pseudonym(key, blob.bot_self_id, &blob.source_type, &blob.source_id);
    super::session_dir(app_data_dir, &pseudonym).join(&blob.file_name)
}

/// 库里没人认领的文件。
///
/// 按"目录 + 文件名"比对而不是只比文件名：两个会话可以有同一段音频的两份拷贝，
/// 那是有意为之（删一个不影响另一个），只比文件名会让其中一份看起来有人认领。
fn sweep_stray_files(conn: &mut diesel::SqliteConnection, app_data_dir: &Path, key: &[u8]) -> Result<usize, String> {
    use diesel::prelude::*;

    let known: HashSet<std::path::PathBuf> = crate::db::schema::voice_blobs::table
        .select(crate::db::models::voice_corpus::VoiceBlobRow::as_select())
        .load(conn)
        .map_err(|e| e.to_string())?
        .iter()
        .map(|blob| blob_path(app_data_dir, key, blob))
        .collect();

    let root = super::corpus_dir(app_data_dir);
    let staging = super::staging_dir(app_data_dir);
    let mut removed = 0;
    let Ok(sessions) = std::fs::read_dir(&root) else {
        return Ok(0);
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
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::voice_corpus::blob_status;
    use crate::db::test_db;

    /// 行**照着字节来**：sha 和大小都从 `bytes` 算，所以 fixture 本身是自洽的,
    /// 一条测试要制造"对不上"就得明确地去改磁盘。
    fn blob(conn: &mut diesel::SqliteConnection, id: &str, status: &str, bytes: &[u8]) -> VoiceBlobRow {
        use crate::db::models::voice_corpus::VoiceBlobInsert;
        use crate::db::schema::voice_blobs;
        use diesel::prelude::*;
        let pending = status == blob_status::PENDING;
        diesel::insert_into(voice_blobs::table)
            .values(&VoiceBlobInsert {
                id,
                bot_self_id: 1,
                source_type: "onebot_group",
                source_id: "123",
                sha256: &sha_of(bytes),
                file_format: "amr",
                file_name: &format!("{id}.amr"),
                file_size: bytes.len() as i64,
                status,
                owner_token: pending.then_some("t"),
                fence_epoch: 0,
                lease_expires_at: pending.then_some(0),
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

    fn sha_of(bytes: &[u8]) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(bytes).iter().fold(String::new(), |mut acc, b| {
            use std::fmt::Write;
            let _ = write!(acc, "{b:02x}");
            acc
        })
    }

    fn write_blob_file(dir: &Path, pool: &DbPool, b: &VoiceBlobRow, bytes: &[u8]) {
        let key = super::super::storage_key(pool).unwrap();
        let path = blob_path(dir, &key, b);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// 拿不到锁时一个字节都不动——磁盘上的东西属于另一个进程。
    #[test]
    fn a_reader_without_the_lock_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let pool = test_db();
        {
            let mut conn = pool.get().unwrap();
            blob(&mut conn, "a", blob_status::PENDING, b"abc");
        }
        assert_eq!(run(&pool, dir.path(), false).unwrap(), Recovered::default());
    }

    /// pending 行是上次崩溃的残骸，staging 里的临时文件也是。
    #[test]
    fn what_a_crash_left_behind_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let pool = test_db();
        {
            let mut conn = pool.get().unwrap();
            blob(&mut conn, "a", blob_status::PENDING, b"abc");
        }
        let staging = super::super::staging_dir(dir.path());
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("half.part"), b"xx").unwrap();

        let out = run(&pool, dir.path(), true).unwrap();
        assert_eq!(out.pending_dropped, 1);
        assert_eq!(out.staged_removed, 1);
    }

    /// 文件对不上的行被**标坏，不是删掉**：它背后的 clip 记着谁在什么时候
    /// 说过话，那份记录不该因为文件坏了就消失。
    #[test]
    fn a_ready_row_whose_file_is_wrong_is_marked_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let pool = test_db();
        let b = {
            let mut conn = pool.get().unwrap();
            let b = blob(&mut conn, "a", blob_status::READY, b"the real bytes");
            crate::db::ops::voice_corpus::record_clip(&mut conn, &b, "c1", "alice", Some(7), 0, None, None, 1).unwrap();
            b
        };
        write_blob_file(dir.path(), &pool, &b, b"short");

        let out = run(&pool, dir.path(), true).unwrap();
        assert_eq!(out.marked_damaged, 1);

        use crate::db::schema::voice_blobs;
        use diesel::prelude::*;
        let mut conn = pool.get().unwrap();
        let status: String = voice_blobs::table
            .find("a")
            .select(voice_blobs::status)
            .first(&mut conn)
            .unwrap();
        assert_eq!(status, blob_status::DAMAGED, "行还在，只是被标坏了");
    }

    /// **同样长、内容不同**也要被认出来。
    ///
    /// 只比大小的那一版对这条一无所知：一次写到一半的崩溃、一块坏扇区、一次
    /// 同名覆盖，长度可以分毫不差。而这份语料是要拿去训练的——一条内容错了的
    /// 样本会被当成真的用，比一条缺失的贵得多。
    #[test]
    fn a_file_of_the_right_length_but_the_wrong_bytes_is_still_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let pool = test_db();
        let b = {
            let mut conn = pool.get().unwrap();
            let b = blob(&mut conn, "a", blob_status::READY, b"aaaaa");
            crate::db::ops::voice_corpus::record_clip(&mut conn, &b, "c1", "alice", Some(7), 0, None, None, 1).unwrap();
            b
        };
        write_blob_file(dir.path(), &pool, &b, b"bbbbb");

        assert_eq!(run(&pool, dir.path(), true).unwrap().marked_damaged, 1);
    }

    /// 没有任何 clip 指着的已发布行，连同它的文件一起走。
    #[test]
    fn an_orphan_takes_its_file_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let pool = test_db();
        let b = {
            let mut conn = pool.get().unwrap();
            blob(&mut conn, "a", blob_status::READY, b"hello")
        };
        write_blob_file(dir.path(), &pool, &b, b"hello");
        let key = super::super::storage_key(&pool).unwrap();
        let path = blob_path(dir.path(), &key, &b);
        assert!(path.exists());

        let out = run(&pool, dir.path(), true).unwrap();
        assert_eq!(out.marked_damaged, 0, "文件是对的");
        assert_eq!(out.orphans_removed, 1);
        assert!(!path.exists(), "孤儿的文件不该留着占地方");
    }

    /// 库里没人认领的文件也要清掉——那是删除删了一半留下的。
    #[test]
    fn a_file_nobody_claims_is_swept() {
        let dir = tempfile::tempdir().unwrap();
        let pool = test_db();
        let stray = super::super::session_dir(dir.path(), "deadbeefdeadbeef");
        std::fs::create_dir_all(&stray).unwrap();
        let path = stray.join("nobody.amr");
        std::fs::write(&path, b"x").unwrap();

        let out = run(&pool, dir.path(), true).unwrap();
        assert_eq!(out.stray_files_removed, 1);
        assert!(!path.exists());
    }
}
