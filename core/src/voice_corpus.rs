//! 语料的目录、单 writer 锁，和采集授权。
//!
//! 三件事放在一起，因为它们回答的是同一个问题：这段音频**能不能**落盘，
//! 以及落到哪里。
//!
//! 这一层不认识 OneBot 的会话类型——它拿到的是 `(bot 账号, 会话字符串)`，
//! 也就是用户在设置里打出来的那个形式。白名单是用户写的，所以用户写的形式
//! 就是这里的键。

pub mod manage;
pub mod recover;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::sync::watch;

use crate::db::DbPool;

/// 语料根目录。**不放 `files/<conversation_id>/`**：删会话会把那个目录整个
/// `remove_dir_all` 掉（`commands/conversation.rs`），`/new` 会把一个群的语料
/// 切散到 N 个会话目录里，而且 `files/` 是 `resolve_attachment_uri` 与 remote
/// `/assets` 的信任根——声纹不该进那里。
pub fn corpus_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("voice_corpus")
}

/// 临时文件。只有一个 writer（见 [`CorpusLock`]），所以恢复器可以无条件清空
/// 这个目录：能拿到锁就说明里面剩下的必然是上次崩溃的残骸。
pub fn staging_dir(app_data_dir: &Path) -> PathBuf {
    corpus_dir(app_data_dir).join(".staging")
}

/// 一个会话的语料目录，名字是假名化过的——见 [`session_pseudonym`]。
pub fn session_dir(app_data_dir: &Path, pseudonym: &str) -> PathBuf {
    corpus_dir(app_data_dir).join(pseudonym)
}

/// 磁盘上那个文件是不是这一行说的那个。
///
/// **大小对上不等于内容对上。** 大小只挡得住截断——一次写到一半的崩溃、一块
/// 坏扇区、一次同名覆盖，长度可以分毫不差而字节已经不是原来的了。这份语料是
/// 要拿去训练的，一条内容错了的样本比一条缺失的样本贵得多：缺的那条不见了，
/// 错的那条会被当成真的用。
///
/// 值得为此读一遍整个文件：单条上限是 10 MiB，而这个判断只在两个地方问——
/// 启动时核一遍，以及复用一份已有音频之前。
pub fn file_matches(path: &Path, expected_size: i64, expected_sha256: &str) -> bool {
    use sha2::Digest;

    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if bytes.len() as i64 != expected_size {
        return false;
    }
    let digest = Sha256::digest(&bytes);
    let actual = digest.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    actual == expected_sha256
}

/// 语料目录的独占锁。
///
/// 拿不到就**整个采集功能停用**，而不是退化成"多个 writer 小心翼翼地协调"。
/// 后者要 DB-backed 的 grant epoch、实例心跳、跨进程删除屏障，外加一个没法
/// 回答的问题：磁盘上那个 `.part` 是别人正在写的，还是上次崩溃剩的。
///
/// Meridian 是桌面应用，正常部署就是一个实例；"同时开两个"本来也不是任何人
/// 想要的状态。用 OS 的 advisory lock 而不是自己写一个 PID 文件，就为了那个
/// 自己写不出来的性质：**进程崩溃时由内核释放**，所以不会留下需要人来判断的
/// stale lock。
///
/// `std::fs::File::try_lock` 自 Rust 1.89 稳定，所以这里不需要依赖。
pub struct CorpusLock {
    _file: std::fs::File,
}

impl CorpusLock {
    /// `Ok(None)` 是"别人拿着"，不是错误——调用方据此停用采集并告诉用户。
    pub fn acquire(app_data_dir: &Path) -> Result<Option<Self>, String> {
        let dir = corpus_dir(app_data_dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(".writer.lock"))
            .map_err(|e| e.to_string())?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e.to_string()),
        }
    }
}

/// 目录与导出里代替真实 id 的名字。
///
/// **假名化，不是匿名化**——同一次安装内稳定（同一个人的样本能聚到一起），
/// 跨安装不可关联，也回不到 QQ 号。
///
/// 三件事都是必须的：`bot_self_id` 要进去，否则两个账号同群会指向同一个目录；
/// 长度前缀分隔各段，否则 `("ab","c")` 与 `("a","bc")` 撞成同一个名字；
/// domain 让"会话的假名"和"发送者的假名"即使 id 相同也不相等——私聊的
/// `source_id` **就是对方的 QQ 号**，只哈希发送者等于没做。
fn pseudonym(storage_key: &[u8], domain: &str, parts: &[&str]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(storage_key).expect("HMAC takes a key of any length");
    mac.update(domain.as_bytes());
    mac.update(&(domain.len() as u64).to_le_bytes());
    for part in parts {
        mac.update(part.as_bytes());
        mac.update(&(part.len() as u64).to_le_bytes());
    }
    mac.finalize().into_bytes()[..8]
        .iter()
        .fold(String::new(), |mut acc, b| {
            use std::fmt::Write;
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

pub fn session_pseudonym(storage_key: &[u8], bot_self_id: i64, source_type: &str, source_id: &str) -> String {
    pseudonym(
        storage_key,
        "voice-session",
        &[&bot_self_id.to_string(), source_type, source_id],
    )
}

pub fn sender_pseudonym(storage_key: &[u8], sender_id: &str) -> String {
    pseudonym(storage_key, "voice-sender", &[sender_id])
}

/// 假名化用的密钥，取出来或者建一个。
///
/// **必须在第一次采集之前存在**，因为目录名在那时就要算出来。而且**不可静默
/// 轮换**：换了 key，磁盘上已有的目录名全部对不上。轮换是一个要重写全部目录
/// 的显式操作，本次不提供。
pub const STORAGE_KEY_PREF: &str = "onebot.voice_storage_key";

/// **读与建必须在同一个写事务里。** 两个采集任务同时第一次跑，各自读到空、
/// 各自生成一把、后写的覆盖先写的——而先写的那个任务已经拿着它自己的 key 算出
/// 目录名开始落盘了。那个目录之后没有人解析得出来，还会被恢复器当野文件扫掉。
/// `BEGIN IMMEDIATE` 让第二个调用者堵在事务开头，醒来时读到的是已经提交的那把。
pub fn storage_key(pool: &DbPool) -> Result<Vec<u8>, String> {
    let mut conn = crate::util::get_conn(pool)?;
    let existing = conn
        .immediate_transaction(|conn| {
            if let Some(existing) =
                crate::db::ops::preference::get_preference(conn, STORAGE_KEY_PREF)?.filter(|v| !v.trim().is_empty())
            {
                return Ok(existing);
            }
            // uuid 的随机性来自 getrandom,这里要的就是"没人能猜到"。两个 v4
            // 拼起来是 256 位。
            let fresh = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
            crate::db::ops::preference::set_preference(conn, STORAGE_KEY_PREF, &fresh, crate::util::now_ms())?;
            Ok::<_, diesel::result::Error>(fresh)
        })
        .map_err(|e| e.to_string())?;
    hex_decode(&existing)
}

fn hex_decode(raw: &str) -> Result<Vec<u8>, String> {
    (0..raw.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(raw.get(i..i + 2).ok_or("odd-length key")?, 16).map_err(|e| e.to_string()))
        .collect()
}

/// 一个 bot 账号在一个会话里的采集授权范围。
///
/// 账号是其中一维而不是附注：两个 bot 各自被拉进同一个群是两次独立的同意。
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct CaptureScope {
    pub bot_self_id: i64,
    /// `SessionKey` 的字符串形式，`group:123` / `private:456`。
    pub session: String,
}

impl CaptureScope {
    pub fn new(bot_self_id: i64, session: impl Into<String>) -> Self {
        Self {
            bot_self_id,
            session: session.into(),
        }
    }

    /// 白名单里的写法：`<bot>@<session>`。用户在设置里打的就是这个。
    pub fn parse(raw: &str) -> Option<Self> {
        let (bot, session) = raw.split_once('@')?;
        let bot_self_id = bot.trim().parse().ok()?;
        let session = session.trim();
        (!session.is_empty()).then(|| Self::new(bot_self_id, session))
    }
}

impl std::fmt::Display for CaptureScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.bot_self_id, self.session)
    }
}

/// drain 最多等这么久。
///
/// 一次采集的上限是下载超时加落盘，正常远快于此。给它一个上限是因为另一端是
/// 一个人：设置页按下保存之后无限转，比"撤权生效了，但有一个在途任务还在写完
/// 它那一条"更糟——而后者恰恰是 permit 语义本来就承诺的。
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 出站语音要凑齐的四项。
///
/// 缺任何一项，`send_voice` 就要从 `definitions()` / `ordinary_names()` /
/// `execute()` **三处同时**消失——把一个必然失败的工具端上去，只会让模型反复
/// 调用它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReadiness {
    pub model: String,
    pub reference_id: String,
    /// 不是 key 本身，是它的指纹。策略要能回答"key 换过没有"，而把密钥抄一份
    /// 放进一个会被日志和 Debug 打印的结构里没有必要。
    pub key_fingerprint: String,
}

#[derive(Default)]
struct Grants {
    generation: u64,
    capture: HashSet<CaptureScope>,
    optouts: HashSet<String>,
    /// 哪些群允许 bot 发语音。私聊默认允许，所以这里只列群。
    ///
    /// 与采集白名单**分开**：一个授权保存真人声纹，一个授权 bot 说话，两件事
    /// 的风险和授权人都不一样。
    send_groups: HashSet<CaptureScope>,
    /// `None` = 四项没凑齐，或者总开关是关的。
    readiness: Option<SendReadiness>,
    /// 出站策略自己的代。与采集那一代分开：改了音色不该让采集的 permit 作废。
    send_generation: u64,
    in_flight: HashMap<CaptureScope, usize>,
    /// 撤权/删除期间挡住新 permit。与"不在白名单里"分开，因为删除结束后白名单
    /// 可能仍然有效——屏障是临时的，撤权是永久的。
    barriers: HashSet<CaptureScope>,
    /// 每个范围上"发生过一次屏障"的次数。
    ///
    /// **屏障立起来就 +1，而不是撤掉时才动。** 一次删除是"立屏障 → drain → 删
    /// → 撤屏障"，drain 是有上限的（见 [`DRAIN_TIMEOUT`]），而一次采集可以比它
    /// 久得多。超时之后删除照常走完，屏障也撤了——只看白名单和 `generation`
    /// 的话，那个熬过整场删除的旧任务会发现自己"仍然被授权"，然后把用户刚要求
    /// 删掉的那段音频重新写回去。
    scope_epochs: HashMap<CaptureScope, u64>,
}

impl Grants {
    fn epoch_of(&self, scope: &CaptureScope) -> u64 {
        self.scope_epochs.get(scope).copied().unwrap_or(0)
    }
}

/// 谁现在可以往语料里写。
///
/// 放在 `Services` 上而不是 OneBot 的 `SharedState` 里，因为 `start_onebot`
/// 会整个重建那个 state：一个跟着重建的协调器，会把正在进行的采集和刚刚做出
/// 的撤权一起忘掉。
pub struct CorpusCoordinator {
    grants: Mutex<Grants>,
    /// permit 释放的计数。drain 等它变化，而不是轮询——在循环外 clone 一个
    /// receiver 就不会丢掉通知。
    release_tx: watch::Sender<u64>,
    release_rx: watch::Receiver<u64>,
    /// `None` = 锁在别人手里，采集整个停用。
    lock: Option<CorpusLock>,
}

impl CorpusCoordinator {
    /// 拿不到锁不是错误：返回的协调器一律拒绝发 permit，调用方据此告诉用户
    /// 采集停用了。
    pub fn new(app_data_dir: &Path) -> Self {
        let lock = match CorpusLock::acquire(app_data_dir) {
            Ok(Some(lock)) => Some(lock),
            Ok(None) => {
                tracing::warn!("voice corpus directory is locked by another instance; capture is off");
                None
            }
            Err(error) => {
                tracing::warn!(%error, "could not lock the voice corpus directory; capture is off");
                None
            }
        };
        let (release_tx, release_rx) = watch::channel(0);
        Self {
            grants: Mutex::new(Grants::default()),
            release_tx,
            release_rx,
            lock: None.or(lock),
        }
    }

    /// 这个进程能不能采集。
    pub fn writable(&self) -> bool {
        self.lock.is_some()
    }

    /// 换掉出站语音的策略。设置页保存和换 key 都走这里——**只监听 preference
    /// 变化会漏掉换 key**，而 key 是四项之一。
    pub fn apply_send_policy(&self, groups: HashSet<CaptureScope>, readiness: Option<SendReadiness>) {
        if let Ok(mut grants) = self.grants.lock() {
            grants.send_groups = groups;
            grants.readiness = readiness;
            grants.send_generation += 1;
        }
    }

    /// 这个会话现在能不能发语音，能的话用什么配置。
    ///
    /// 一个函数回答，因为 `definitions()`、`ordinary_names()` 和 `execute()`
    /// **必须用同一个判断**。只过滤第一个不构成权限边界：非 admin 的 `offered`
    /// 来自 `ordinary_names()`，而 `execute` 只查 `Scope`——模型凭名字就能调到
    /// 一个没被展示的工具。
    pub fn send_policy(&self, bot_self_id: i64, session: &str) -> Option<SendReadiness> {
        let grants = self.grants.lock().ok()?;
        let readiness = grants.readiness.clone()?;
        // 私聊默认允许：一个对手方，屋主就是听的人。群是一间屋子，发不发语音
        // 是屋主的决定。
        if session.starts_with("private:") {
            return Some(readiness);
        }
        let scope = CaptureScope::new(bot_self_id, session);
        grants.send_groups.contains(&scope).then_some(readiness)
    }

    /// 出站策略的代。请求前和派发前各读一次——**模型可能在看到 S1 的工具描述
    /// 之后、配置已经切到 S2 时才调用**，而合成期间音色也可能被换掉。
    pub fn send_generation(&self) -> u64 {
        self.grants.lock().map(|g| g.send_generation).unwrap_or(0)
    }

    pub fn generation(&self) -> u64 {
        self.grants.lock().map(|g| g.generation).unwrap_or(0)
    }

    /// 换掉白名单与 opt-out 名单，并让 generation 前进一格。
    ///
    /// **先 drain 再换**：见 [`Self::revoke_and_drain`]。这个入口用于设置页保存，
    /// 它可能同时新增和移除，所以移除的那些要走屏障。
    pub async fn apply(&self, capture: HashSet<CaptureScope>, optouts: HashSet<String>) {
        let removed: Vec<CaptureScope> = {
            let Ok(grants) = self.grants.lock() else { return };
            grants.capture.difference(&capture).cloned().collect()
        };
        if !removed.is_empty() {
            self.revoke_and_drain(&removed).await;
        }
        if let Ok(mut grants) = self.grants.lock() {
            grants.capture = capture;
            grants.optouts = optouts;
            grants.generation += 1;
        }
    }

    /// 现在被授权的全部范围。删除要用它——按人删跨会话，屏障得覆盖所有地方。
    pub fn granted_scopes(&self) -> Vec<CaptureScope> {
        self.grants
            .lock()
            .map(|grants| grants.capture.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// 只立屏障并等 drain，**不动白名单**。
    ///
    /// 删除历史用这个：删掉已有数据不等于撤销以后的授权，那是两件事（一个处理
    /// 已经存在的，一个拒绝将来的）。合并它们意味着一次删除会顺手把这个群永久
    /// 停录，而没人要求过那个。
    pub async fn revoke_and_drain_temporarily(&self, scopes: &[CaptureScope]) {
        self.raise_barriers(scopes);
        self.drain(scopes).await;
    }

    /// 撤掉屏障。删除结束时调用——授权本身从没被动过。
    pub fn lift_barriers(&self, scopes: &[CaptureScope]) {
        if let Ok(mut grants) = self.grants.lock() {
            for scope in scopes {
                grants.barriers.remove(scope);
            }
        }
    }

    fn raise_barriers(&self, scopes: &[CaptureScope]) {
        if let Ok(mut grants) = self.grants.lock() {
            for scope in scopes {
                grants.barriers.insert(scope.clone());
                // 立起来的这一刻就作废这个范围上所有已经发出的 permit。撤掉屏障
                // 不会把它们还回来，那正是要的：drain 超时之后跑完的那个任务，
                // 写的是用户刚刚要求删掉的东西。
                *grants.scope_epochs.entry(scope.clone()).or_insert(0) += 1;
            }
        }
    }

    /// 这一代服务结束了：挡住全部、等在途归还、然后把授权整个清空。
    ///
    /// `stop()` 用它。少了这一步，恢复器会在下一次 `start()` 里跑起来，而上一代
    /// 那些还握着 permit 的采集任务正在写——恢复器无条件清空 `.staging` 和所有
    /// `pending` 行，靠的正是"能拿到锁就说明没有 writer"这个前提。
    pub async fn quiesce(&self) {
        let scopes = self.granted_scopes();
        if !scopes.is_empty() {
            self.raise_barriers(&scopes);
            self.drain(&scopes).await;
        }
        if let Ok(mut grants) = self.grants.lock() {
            grants.capture.clear();
            grants.barriers.clear();
            grants.generation += 1;
        }
    }

    /// 立屏障、等在途采集结束、然后真正撤销。
    ///
    /// 已经拿到 permit 的任务**允许跑完**——那是 permit 的正常语义，也是唯一
    /// 能简单推理的。所以这个函数返回之后的保证是"不再新增"，不是"磁盘上没有
    /// 刚写的东西"。
    pub async fn revoke_and_drain(&self, scopes: &[CaptureScope]) {
        self.raise_barriers(scopes);
        self.drain(scopes).await;
        if let Ok(mut grants) = self.grants.lock() {
            for scope in scopes {
                grants.capture.remove(scope);
                grants.barriers.remove(scope);
            }
            grants.generation += 1;
        }
    }

    /// 等这些范围上的在途采集全部归还 permit。
    async fn drain(&self, scopes: &[CaptureScope]) {
        // receiver 在循环外 clone：它记着自己见过的版本，所以两次检查之间的
        // 释放不会被漏掉。
        let mut release = self.release_rx.clone();
        let drained = tokio::time::timeout(DRAIN_TIMEOUT, async {
            loop {
                let busy = {
                    let Ok(grants) = self.grants.lock() else { break };
                    scopes
                        .iter()
                        .any(|scope| grants.in_flight.get(scope).copied().unwrap_or(0) > 0)
                };
                if !busy {
                    break;
                }
                if release.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        if drained.is_err() {
            // 屏障已经立着，所以"不再新增"仍然成立——超时只意味着有一个在途
            // 任务比预期慢。等下去的代价是设置页那个按下保存的人无限等，而他
            // 等的那件事已经保证了。
            tracing::warn!(
                scopes = scopes.len(),
                "voice capture drain timed out; the revocation stands and one capture may still be finishing"
            );
        }
    }

    /// 要采集就得先拿到这个。`None` 表示不许——锁没拿到、屏障中、不在白名单里，
    /// 或者这个人拒绝过留存。
    pub fn acquire(self: &Arc<Self>, scope: &CaptureScope, sender_id: &str) -> Option<CapturePermit> {
        self.lock.as_ref()?;
        let mut grants = self.grants.lock().ok()?;
        if grants.barriers.contains(scope) || !grants.capture.contains(scope) || grants.optouts.contains(sender_id) {
            return None;
        }
        let generation = grants.generation;
        let scope_epoch = grants.epoch_of(scope);
        *grants.in_flight.entry(scope.clone()).or_insert(0) += 1;
        Some(CapturePermit {
            authorisation: Authorisation {
                coordinator: Arc::clone(self),
                scope: scope.clone(),
                generation,
                scope_epoch,
            },
        })
    }
}

/// permit 那一刻的授权，脱离 permit 单独带走。
///
/// 存在的理由是**最终提交发生在另一条线程上**：写库是 `spawn_blocking`，而
/// permit 要留在异步这一侧继续挡住撤权。这个句柄是 `'static` 的，所以可以进到
/// 那个事务里再问一次——**在提交之前**，而不是在下载之前。
#[derive(Clone)]
pub struct Authorisation {
    coordinator: Arc<CorpusCoordinator>,
    scope: CaptureScope,
    generation: u64,
    scope_epoch: u64,
}

impl Authorisation {
    /// 授权自这个 permit 发出以来没有变过，这个 scope 现在仍在白名单里，
    /// 而且这中间没有人对它立过屏障。
    ///
    /// 三个条件缺一不可。前两个漏掉的是删除：删除**不动白名单也不推进
    /// generation**，它只立一个临时屏障；drain 有上限而下载没有，所以熬过整场
    /// 删除的那个任务只看前两个条件会认为自己仍然被授权。
    pub fn still_authorised(&self) -> bool {
        let Ok(grants) = self.coordinator.grants.lock() else {
            return false;
        };
        grants.generation == self.generation
            && grants.epoch_of(&self.scope) == self.scope_epoch
            && !grants.barriers.contains(&self.scope)
            && grants.capture.contains(&self.scope)
    }
}

/// 一次采集的许可。持有期间它的 scope 不会被撤销——撤销要等所有 permit 归还。
///
/// **在最终提交之前要问一次 [`Self::still_authorised`]**：一次采集可能跑几十
/// 秒，而这中间用户可能把这个会话从白名单里拿掉。permit 保证撤权会等它结束，
/// 但不保证它写下去的东西还是用户想要的。
pub struct CapturePermit {
    authorisation: Authorisation,
}

impl CapturePermit {
    pub fn scope(&self) -> &CaptureScope {
        &self.authorisation.scope
    }

    /// 带进 `spawn_blocking` 的那一份。permit 本身留在异步侧继续挡住撤权。
    pub fn authorisation(&self) -> Authorisation {
        self.authorisation.clone()
    }

    pub fn still_authorised(&self) -> bool {
        self.authorisation.still_authorised()
    }
}

impl Drop for CapturePermit {
    fn drop(&mut self) {
        let auth = &self.authorisation;
        let Ok(mut grants) = auth.coordinator.grants.lock() else {
            return;
        };
        if let Some(count) = grants.in_flight.get_mut(&auth.scope) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                grants.in_flight.remove(&auth.scope);
            }
        }
        drop(grants);
        // 唤醒 drain。send_modify 保证版本号一定前进，哪怕没有接收者。
        auth.coordinator.release_tx.send_modify(|n| *n += 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coordinator() -> (tempfile::TempDir, Arc<CorpusCoordinator>) {
        let dir = tempfile::tempdir().unwrap();
        let c = Arc::new(CorpusCoordinator::new(dir.path()));
        (dir, c)
    }

    fn scope() -> CaptureScope {
        CaptureScope::new(1, "group:123")
    }

    #[tokio::test]
    async fn nothing_is_captured_without_an_allowlist_entry() {
        let (_dir, c) = coordinator();
        assert!(c.acquire(&scope(), "alice").is_none());

        c.apply(HashSet::from([scope()]), HashSet::new()).await;
        assert!(c.acquire(&scope(), "alice").is_some());
    }

    /// 另一个账号在同一个群里是另一份授权。少了这一条，把 bot A 加进白名单
    /// 会顺手让 bot B 也开始录同一个群。
    #[tokio::test]
    async fn another_account_in_the_same_group_is_not_covered() {
        let (_dir, c) = coordinator();
        c.apply(HashSet::from([scope()]), HashSet::new()).await;
        assert!(c.acquire(&CaptureScope::new(2, "group:123"), "alice").is_none());
    }

    #[tokio::test]
    async fn someone_who_opted_out_is_never_captured() {
        let (_dir, c) = coordinator();
        c.apply(HashSet::from([scope()]), HashSet::from(["bob".to_string()]))
            .await;
        assert!(c.acquire(&scope(), "alice").is_some());
        assert!(c.acquire(&scope(), "bob").is_none());
    }

    /// 撤权返回之后不再发新的 permit。已经发出去的那个允许跑完——所以这里
    /// 断言的是"不再新增"，而不是"磁盘上什么都没有"。
    #[tokio::test]
    async fn revoking_waits_for_what_is_already_running() {
        let (_dir, c) = coordinator();
        c.apply(HashSet::from([scope()]), HashSet::new()).await;
        let permit = c.acquire(&scope(), "alice").expect("granted");

        let revoker = {
            let c = Arc::clone(&c);
            tokio::spawn(async move { c.revoke_and_drain(&[scope()]).await })
        };

        // permit 还在手上，撤权应该还没结束。
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!revoker.is_finished(), "撤权不该在 permit 归还前返回");

        drop(permit);
        revoker.await.unwrap();
        assert!(c.acquire(&scope(), "alice").is_none(), "撤权之后不再发 permit");
    }

    /// 一次长采集跨过了一次撤权：permit 让它跑完，但它在提交前问一次就知道
    /// 自己写下去的东西已经不是用户想要的了。
    #[tokio::test]
    async fn a_permit_can_tell_that_its_grant_moved_underneath_it() {
        let (_dir, c) = coordinator();
        c.apply(HashSet::from([scope()]), HashSet::new()).await;
        let permit = c.acquire(&scope(), "alice").expect("granted");
        assert!(permit.still_authorised());

        // 用**新增**另一个会话来推进 generation，不是移除这一个：移除会等这个
        // permit 归还，而它正握在手上——那是在等自己。这也是这个断言想说的事，
        // 授权换过一版，permit 看到的那一版就不再是现在这一版了。
        c.apply(
            HashSet::from([scope(), CaptureScope::new(1, "group:999")]),
            HashSet::new(),
        )
        .await;
        assert!(!permit.still_authorised());
    }

    /// 删除**不动白名单也不推进 generation**，所以它只能靠 scope 自己那一格。
    ///
    /// 这是 drain 超时之后那条路：删除照常走完并撤掉屏障，而那个熬过整场删除的
    /// 采集任务醒来时，白名单和 generation 都和它出发时一模一样。少了这一格，
    /// 它会把用户刚要求删掉的音频重新写回去，而删除已经报告成功了。
    #[tokio::test]
    async fn a_permit_that_outlived_a_deletion_may_not_commit() {
        let (_dir, c) = coordinator();
        c.apply(HashSet::from([scope()]), HashSet::new()).await;
        let permit = c.acquire(&scope(), "alice").expect("granted");

        // 删除那一套，但不等 drain——超时的那条路走的就是这个顺序。
        c.raise_barriers(&[scope()]);
        assert!(!permit.still_authorised(), "屏障立着的时候当然不行");
        c.lift_barriers(&[scope()]);

        assert!(c.acquire(&scope(), "bob").is_some(), "删完之后照常采集");
        assert!(
            !permit.still_authorised(),
            "撤掉屏障不该把跨过这场删除的旧 permit 还回来"
        );
    }

    #[test]
    fn a_second_holder_of_the_directory_lock_gets_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let first = CorpusCoordinator::new(dir.path());
        assert!(first.writable());

        let second = CorpusCoordinator::new(dir.path());
        assert!(!second.writable(), "同一个目录只允许一个 writer");
        assert!(
            Arc::new(second).acquire(&scope(), "alice").is_none(),
            "拿不到锁就一律不采集"
        );
    }

    /// 建一次，之后每次都是同一把。
    ///
    /// 测试池只有一条连接，所以这里跑的是顺序那一路——真正的并发靠的是
    /// `BEGIN IMMEDIATE`，第二个调用者堵在事务开头，醒来时读到的是已提交的值。
    /// 不是同一把的话，先动手的那个任务已经用它自己的 key 算出目录名开始落盘了，
    /// 而那个目录名之后没有人解析得出来——恢复器会把它当野文件扫掉。
    #[test]
    fn the_storage_key_is_created_once_and_then_read() {
        let pool = crate::db::test_db();
        let first = storage_key(&pool).unwrap();
        assert_eq!(first.len(), 32);
        assert_eq!(storage_key(&pool).unwrap(), first);
    }

    /// 假名化要把账号算进去，否则两个 bot 同群会共用一个目录；
    /// 而会话和发送者即使 id 相同也必须不同——私聊的 source_id 就是对方的号。
    #[test]
    fn a_pseudonym_separates_accounts_and_domains() {
        let key = b"k";
        let a = session_pseudonym(key, 1, "onebot_private", "999");
        let b = session_pseudonym(key, 2, "onebot_private", "999");
        assert_ne!(a, b, "两个账号同一个对手方不能撞到一起");
        assert_ne!(a, sender_pseudonym(key, "999"), "会话与发送者要分开");

        // 长度前缀：拼接歧义不能变成同一个名字。
        assert_ne!(
            session_pseudonym(key, 1, "ab", "c"),
            session_pseudonym(key, 1, "a", "bc")
        );
    }
}
