//! Session-scoped QQ tools exposed to the model during OneBot chats.
//!
//! Security: the target group/user is fixed to the current session at
//! construction time and is NOT part of the tool parameters, so the model
//! cannot read or act on other chats. Action tools are admin-only and go
//! through the Y/N chat approval flow — the approver is the message sender,
//! so handing them to non-admins would let users approve themselves.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use super::protocol::MessageSegment;
use super::protocol::OneBotAction;
use super::session::{SessionKey, SessionKind};
use super::{SharedState, call_api};

pub const QQ_HISTORY_TOOL: &str = "qq_get_chat_history";
const MAX_HISTORY_COUNT: i64 = 50;
const MAX_OUTPUT_CHARS: usize = 8000;
const MAX_LIKE_TIMES: i64 = 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Any,
    GroupOnly,
    PrivateOnly,
}

struct ToolSpec {
    name: &'static str,
    admin_only: bool,
    needs_approval: bool,
    scope: Scope,
}

const SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "list_stickers",
        admin_only: false,
        needs_approval: false,
        scope: Scope::Any,
    },
    ToolSpec {
        name: "send_sticker",
        admin_only: false,
        needs_approval: false,
        scope: Scope::Any,
    },
    ToolSpec {
        // 白名单已经是屋主做过的决定，再要求管理员身份等于开了功能而屋里没人
        // 用得上。也不停下来问：每条语音都等一个 Y，这个功能就不存在了——它做
        // 的事是往它已经在的那间屋子里发一条消息，和 send_sticker 同一类。
        name: SEND_VOICE_TOOL,
        admin_only: false,
        needs_approval: false,
        // 私聊/群的区别 `Scope` 表达不了：那是配置决定的，不是工具的性质。
        // 由 `send_policy` 在运行时回答，三处共用同一个判断。
        scope: Scope::Any,
    },
    ToolSpec {
        name: QQ_HISTORY_TOOL,
        admin_only: false,
        needs_approval: false,
        scope: Scope::Any,
    },
    ToolSpec {
        name: "qq_get_group_info",
        admin_only: false,
        needs_approval: false,
        scope: Scope::GroupOnly,
    },
    ToolSpec {
        name: "qq_get_group_member_list",
        admin_only: false,
        needs_approval: false,
        scope: Scope::GroupOnly,
    },
    ToolSpec {
        name: "qq_get_group_member_info",
        admin_only: false,
        needs_approval: false,
        scope: Scope::GroupOnly,
    },
    ToolSpec {
        name: "qq_get_user_info",
        admin_only: false,
        needs_approval: false,
        scope: Scope::PrivateOnly,
    },
    ToolSpec {
        name: "qq_get_friend_list",
        admin_only: true,
        needs_approval: false,
        scope: Scope::Any,
    },
    ToolSpec {
        name: "qq_get_group_list",
        admin_only: true,
        needs_approval: false,
        scope: Scope::Any,
    },
    ToolSpec {
        name: "qq_delete_msg",
        admin_only: true,
        needs_approval: true,
        scope: Scope::Any,
    },
    ToolSpec {
        name: "qq_send_poke",
        admin_only: true,
        needs_approval: true,
        scope: Scope::Any,
    },
    ToolSpec {
        name: "qq_send_like",
        admin_only: true,
        needs_approval: true,
        scope: Scope::Any,
    },
    ToolSpec {
        name: "qq_set_essence_msg",
        admin_only: true,
        needs_approval: true,
        scope: Scope::GroupOnly,
    },
    ToolSpec {
        name: "qq_set_group_ban",
        admin_only: true,
        needs_approval: true,
        scope: Scope::GroupOnly,
    },
    ToolSpec {
        name: "qq_set_group_kick",
        admin_only: true,
        needs_approval: true,
        scope: Scope::GroupOnly,
    },
    ToolSpec {
        name: "qq_set_group_card",
        admin_only: true,
        needs_approval: true,
        scope: Scope::GroupOnly,
    },
    ToolSpec {
        name: "qq_set_group_name",
        admin_only: true,
        needs_approval: true,
        scope: Scope::GroupOnly,
    },
    ToolSpec {
        name: "qq_send_group_notice",
        admin_only: true,
        needs_approval: true,
        scope: Scope::GroupOnly,
    },
];

pub struct QqToolExecutor {
    state: Arc<SharedState>,
    session: SessionKey,
    is_admin: bool,
    self_id: Option<i64>,
    turn_id: String,
    /// 这个会话现在能不能发语音。在构造时算一次并存下来，而不是每处各查一次：
    /// 三个用它的地方必须看到同一个答案，否则就会出现"工具数组里公告了、
    /// dispatch 又拒绝"——模型会当着一屋子人反复调用它。
    policy: SessionPolicy,
    /// 发语音要用的配置。`None` 就是这个会话发不了。
    send_readiness: Option<crate::voice_corpus::SendReadiness>,
    /// 出站策略的代。合成前和派发前各比一次——模型可能在看到旧描述之后、
    /// 配置已经换掉时才调用。
    send_generation: u64,
}

impl QqToolExecutor {
    pub fn new(
        state: Arc<SharedState>,
        session: SessionKey,
        is_admin: bool,
        self_id: Option<i64>,
        turn_id: String,
    ) -> Self {
        let send_readiness = self_id.and_then(|bot| state.services.corpus.send_policy(bot, &session.to_string()));
        let send_generation = state.services.corpus.send_generation();
        Self {
            state,
            session,
            is_admin,
            self_id,
            turn_id,
            policy: SessionPolicy {
                voice_send: send_readiness.is_some(),
            },
            send_readiness,
            send_generation,
        }
    }

    fn available(&self, spec: &ToolSpec) -> bool {
        spec_available(spec, &self.session.kind, self.is_admin, self.policy)
    }

    pub fn owns(&self, name: &str) -> bool {
        SPECS.iter().any(|s| s.name == name)
    }

    pub fn requires_approval(&self, name: &str) -> bool {
        SPECS.iter().find(|s| s.name == name).is_some_and(|s| s.needs_approval)
    }

    /// What the model is shown, which has to be a property of the *session*
    /// rather than of whoever spoke this turn: the tool array is part of the
    /// prefix a provider caches, and `is_admin` is decided per message
    /// (`handler.rs` reads it off the sender). Filtering this by the speaker
    /// made an admin's turn and an ordinary member's turn two different
    /// prefixes, and in a group where both speak every alternation threw the
    /// whole cache away — the system prompt with it, `base_prompt` being
    /// derived from the tool set.
    ///
    /// So a group shows every QQ tool its scope has and refuses at dispatch
    /// instead. What authorises a call is the loop's `offered` set, which it
    /// checks before every path and which follows the round's own speakers;
    /// `execute` re-checks against this executor. Widening what is *shown* is
    /// only acceptable because these descriptions are our own fixed prose about
    /// the chat the reader is already in. It is not acceptable for the registry
    /// and MCP tools, which carry user-configured server names and schemas —
    /// see `exposes_full_toolset`.
    ///
    /// A private chat has one counterpart, so `is_admin` is already constant
    /// for the life of the session and narrowing by it costs no cache.
    pub fn definitions(&self) -> Vec<crate::provider::ToolDefinition> {
        let shown = shown_as_admin(&self.session.kind, self.is_admin);
        SPECS
            .iter()
            .filter(|s| spec_available(s, &self.session.kind, shown, self.policy))
            .map(|s| self.definition_for(s.name))
            .collect()
    }

    pub fn session_kind(&self) -> &SessionKind {
        &self.session.kind
    }

    /// What this session comes down to for someone with no admin standing: the
    /// set a round runs on when such a person opened it, and the one a turn
    /// falls back to when they speak into one an admin opened.
    ///
    /// Independent of this executor's own `is_admin` on purpose. That is the
    /// authority the round was *built* with, and the whole point here is to
    /// answer for somebody else.
    pub fn ordinary_names(&self) -> Vec<String> {
        SPECS
            .iter()
            .filter(|s| spec_available(s, &self.session.kind, false, self.policy))
            .map(|s| s.name.to_string())
            .collect()
    }

    fn definition_for(&self, name: &str) -> crate::provider::ToolDefinition {
        let scope = match self.session.kind {
            SessionKind::Group => "本群",
            SessionKind::Private => "本私聊",
        };
        let no_params = serde_json::json!({ "type": "object", "properties": {} });
        let (description, parameters) = match name {
            "list_stickers" => (
                "列出当前 QQ 机器人账号已确认语义、可发送的表情。返回的 sticker_id 只用于 send_sticker。".to_string(),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "可选的名称或标签筛选" }
                    }
                }),
            ),
            SEND_VOICE_TOOL => (
                format!(
                    "把一段话合成为语音，作为独立消息发到当前 QQ 会话。每个助手回合最多尝试一次，可同时回复文字。\
                     文本 {} 字以内；句首可用 [{}] 这类标记控制语气，只认这些词，其他会被拒绝。\
                     链接、代码、表格不要放进来。",
                    crate::tts::MAX_TTS_CHARS,
                    crate::tts::CUES.join("]、[")
                ),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": format!("要念出来的话，{} 字以内", crate::tts::MAX_TTS_CHARS),
                        }
                    },
                    "required": ["text"]
                }),
            ),
            "send_sticker" => (
                "把一个已确认表情作为独立消息发到当前 QQ 会话。每个助手回合最多成功一次，可同时回复文字。".to_string(),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "sticker_id": { "type": "string", "description": "list_stickers 返回的精确 id" }
                    },
                    "required": ["sticker_id"]
                }),
            ),
            QQ_HISTORY_TOOL => (
                format!(
                    "获取当前 QQ 会话({scope})的历史消息记录。用于了解最近的聊天上下文,\
                     例如回答\"刚才聊了什么\"之类的问题。输出末尾会给出继续向前翻页用的 message_seq。"
                ),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "count": {
                            "type": "integer",
                            "description": "获取多少条消息,1-50,默认 20",
                        },
                        "message_seq": {
                            "type": "integer",
                            "description": "从该消息序号继续向前翻页;省略则取最新消息",
                        },
                    },
                }),
            ),
            "qq_get_group_info" => ("获取本群的基本信息(群名、人数等)。".to_string(), no_params),
            "qq_get_group_member_list" => ("获取本群成员列表(名称、QQ号、角色)。".to_string(), no_params),
            "qq_get_group_member_info" => (
                "获取本群某个成员的详细信息(名片、角色、头衔、入群时间等)。".to_string(),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "user_id": { "type": "integer", "description": "成员 QQ 号" },
                    },
                    "required": ["user_id"],
                }),
            ),
            "qq_get_user_info" => ("获取当前私聊对象的资料(昵称等)。".to_string(), no_params),
            "qq_get_friend_list" => ("获取机器人的好友列表。".to_string(), no_params),
            "qq_get_group_list" => ("获取机器人加入的群列表。".to_string(), no_params),
            "qq_delete_msg" => (
                "撤回一条消息(自己发出的,或作为群管理员撤回他人的)。".to_string(),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message_id": { "type": "integer", "description": "要撤回的消息 ID" },
                    },
                    "required": ["message_id"],
                }),
            ),
            "qq_send_poke" => (
                match self.session.kind {
                    SessionKind::Group => "戳一戳本群的某个成员。".to_string(),
                    SessionKind::Private => "戳一戳当前私聊对象。".to_string(),
                },
                match self.session.kind {
                    SessionKind::Group => serde_json::json!({
                        "type": "object",
                        "properties": {
                            "user_id": { "type": "integer", "description": "要戳的成员 QQ 号" },
                        },
                        "required": ["user_id"],
                    }),
                    SessionKind::Private => no_params,
                },
            ),
            "qq_send_like" => (
                "给某人的资料卡点赞。".to_string(),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "user_id": {
                            "type": "integer",
                            "description": "点赞对象 QQ 号;私聊中省略则默认当前对象",
                        },
                        "times": { "type": "integer", "description": "点赞次数,1-20,默认 10" },
                    },
                }),
            ),
            "qq_set_essence_msg" => (
                "将本群的一条消息设为精华消息。".to_string(),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message_id": { "type": "integer", "description": "消息 ID" },
                    },
                    "required": ["message_id"],
                }),
            ),
            "qq_set_group_ban" => (
                "禁言本群成员。duration 为秒数,0 表示解除禁言。".to_string(),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "user_id": { "type": "integer", "description": "成员 QQ 号" },
                        "duration": { "type": "integer", "description": "禁言秒数,0 解除,默认 600" },
                    },
                    "required": ["user_id"],
                }),
            ),
            "qq_set_group_kick" => (
                "将成员移出本群。".to_string(),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "user_id": { "type": "integer", "description": "成员 QQ 号" },
                    },
                    "required": ["user_id"],
                }),
            ),
            "qq_set_group_card" => (
                "设置本群成员的群名片。".to_string(),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "user_id": { "type": "integer", "description": "成员 QQ 号" },
                        "card": { "type": "string", "description": "新的群名片,空字符串表示清除" },
                    },
                    "required": ["user_id", "card"],
                }),
            ),
            "qq_set_group_name" => (
                "修改本群的群名。".to_string(),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "新的群名" },
                    },
                    "required": ["name"],
                }),
            ),
            "qq_send_group_notice" => (
                "发布本群的群公告。".to_string(),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "content": { "type": "string", "description": "公告内容" },
                    },
                    "required": ["content"],
                }),
            ),
            _ => (String::new(), no_params),
        };
        crate::provider::ToolDefinition {
            name: name.into(),
            description,
            parameters: with_description(name, parameters),
        }
    }

    pub async fn execute(&self, name: &str, arguments: &str) -> Result<String, String> {
        let spec = SPECS
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("Unknown QQ tool: {name}"))?;
        // The offered-tools gate in the agent loop already blocks unavailable
        // tools; this re-check is defense in depth.
        if !self.available(spec) {
            return Err(format!("Tool {name} is not available in this session"));
        }
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|error| format!("invalid JSON arguments for {name}: {error}"))?;
        if !args.is_object() {
            return Err(format!("arguments for {name} must be a JSON object"));
        }

        match name {
            "list_stickers" => self.list_stickers(&args).await,
            "send_sticker" => self.send_sticker(&args).await,
            SEND_VOICE_TOOL => self.send_voice(&args).await,
            QQ_HISTORY_TOOL => self.get_chat_history(&args).await,
            "qq_get_group_info" => self.get_group_info().await,
            "qq_get_group_member_list" => self.get_group_member_list().await,
            "qq_get_group_member_info" => {
                let user_id = require_i64(&args, "user_id")?;
                self.get_group_member_info(user_id).await
            }
            "qq_get_user_info" => self.get_user_info().await,
            "qq_get_friend_list" => self.get_friend_list().await,
            "qq_get_group_list" => self.get_group_list().await,
            "qq_delete_msg" => {
                let message_id = require_i64(&args, "message_id")?;
                call_api(&self.state, OneBotAction::delete_msg(message_id, echo())).await?;
                Ok(format!("已撤回消息 {message_id}"))
            }
            "qq_send_poke" => {
                let (user_id, group_id) = match self.session.kind {
                    SessionKind::Group => (require_i64(&args, "user_id")?, Some(self.session.id)),
                    SessionKind::Private => (self.session.id, None),
                };
                call_api(&self.state, OneBotAction::send_poke(user_id, group_id, echo())).await?;
                Ok("已发送戳一戳".into())
            }
            "qq_send_like" => {
                let user_id = match get_i64(&args, "user_id") {
                    Some(id) => id,
                    None if self.session.kind == SessionKind::Private => self.session.id,
                    None => return Err("missing required parameter: user_id".into()),
                };
                let times = get_i64(&args, "times").unwrap_or(10).clamp(1, MAX_LIKE_TIMES);
                call_api(&self.state, OneBotAction::send_like(user_id, times, echo())).await?;
                Ok(format!("已给 {user_id} 点赞 {times} 次"))
            }
            "qq_set_essence_msg" => {
                let message_id = require_i64(&args, "message_id")?;
                call_api(&self.state, OneBotAction::set_essence_msg(message_id, echo())).await?;
                Ok("已设为精华消息".into())
            }
            "qq_set_group_ban" => {
                let user_id = require_i64(&args, "user_id")?;
                let duration = get_i64(&args, "duration").unwrap_or(600).max(0);
                call_api(
                    &self.state,
                    OneBotAction::set_group_ban(self.session.id, user_id, duration, echo()),
                )
                .await?;
                Ok(if duration > 0 {
                    format!("已禁言 {user_id} {duration} 秒")
                } else {
                    format!("已解除 {user_id} 的禁言")
                })
            }
            "qq_set_group_kick" => {
                let user_id = require_i64(&args, "user_id")?;
                call_api(
                    &self.state,
                    OneBotAction::set_group_kick(self.session.id, user_id, echo()),
                )
                .await?;
                Ok(format!("已将 {user_id} 移出本群"))
            }
            "qq_set_group_card" => {
                let user_id = require_i64(&args, "user_id")?;
                let card = args.get("card").and_then(|v| v.as_str()).unwrap_or("");
                call_api(
                    &self.state,
                    OneBotAction::set_group_card(self.session.id, user_id, card, echo()),
                )
                .await?;
                Ok(format!("已设置 {user_id} 的群名片"))
            }
            "qq_set_group_name" => {
                let name_arg = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or("missing required parameter: name")?;
                call_api(
                    &self.state,
                    OneBotAction::set_group_name(self.session.id, name_arg, echo()),
                )
                .await?;
                Ok("已修改群名".into())
            }
            "qq_send_group_notice" => {
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or("missing required parameter: content")?;
                call_api(
                    &self.state,
                    OneBotAction::send_group_notice(self.session.id, content, echo()),
                )
                .await?;
                Ok("已发布群公告".into())
            }
            _ => Err(format!("Unknown QQ tool: {name}")),
        }
    }

    async fn list_stickers(&self, args: &serde_json::Value) -> Result<String, String> {
        let self_id = self.self_id.ok_or("OneBot event did not include self_id")?.to_string();
        let query = args
            .get("query")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let pool = self.state.services.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            let Some(pack) =
                crate::db::ops::emoji_pack::get_by_source_account(&mut conn, &self_id).map_err(|e| e.to_string())?
            else {
                return Ok("[]".to_string());
            };
            let stickers =
                crate::db::ops::emoji::list_confirmed_for_packs(&mut conn, &[pack.id]).map_err(|e| e.to_string())?;
            let values: Vec<_> = stickers
                .into_iter()
                .filter(|sticker| {
                    query.is_empty()
                        || format!("{} {}", sticker.name, sticker.tags.as_deref().unwrap_or(""))
                            .to_lowercase()
                            .contains(&query)
                })
                .take(100)
                .map(|sticker| {
                    serde_json::json!({
                        "sticker_id": sticker.id,
                        "name": sticker.name,
                        "tags": sticker.tags,
                    })
                })
                .collect();
            serde_json::to_string(&values).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// 把一段话念出来，发到这个会话。
    ///
    /// 时序是三段串行，各有各的预算，**合成必须在 `call_api` 那 10 秒之外**：
    /// 读 keyring（阻塞 IO，`spawn_blocking`）→ `tts::synthesize`（自己的超时）
    /// → base64 + 发送（`call_api_to_conn` 的超时）。把合成塞进最后那一段，
    /// 长文本上会随机超时。
    ///
    /// 失败一律 `Err`，模型照常用文字回答——那是已经有的路径，不花一分钱。
    /// **工具内部绝不自己补发一条文字消息**：模型自己的回复本来就要来，补发就是
    /// 两条。
    async fn send_voice(&self, args: &serde_json::Value) -> Result<String, String> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("missing required parameter: text")?;

        let Some(readiness) = self.send_readiness.clone() else {
            return Err("voice replies are not configured for this session".into());
        };

        // 额度在**尝试**时就占掉，不等结果——这一次要不要花钱，在请求发出去的
        // 那一刻就定了。permit 一直持到消息成功入队为止。
        let _permit = self.state.services.voice_limiter.try_acquire(
            &self.session.to_string(),
            &self.turn_id,
            crate::util::now_ms(),
        )?;

        // 请求前校验一次：模型可能在看到旧描述之后、配置已经换掉时才调用。
        self.check_send_generation()?;

        let secrets = self.state.services.secrets.clone();
        let api_key = tokio::task::spawn_blocking(move || {
            let name = crate::secrets::SecretName::new(super::FISH_KEY_SECRET).ok()?;
            secrets
                .get(&crate::secrets::SecretScope::Global, &name)
                .ok()
                .flatten()
                .filter(|k| !k.trim().is_empty())
        })
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the Fish Audio API key is not set")?;

        let speech = crate::tts::synthesize(
            &api_key,
            crate::tts::SpeechRequest {
                text,
                model: &readiness.model,
                reference_id: &readiness.reference_id,
            },
        )
        .await?;

        // 派发前再校验一次：合成期间音色也可能被换掉，而那时念出来的已经不是
        // 用户配置的那个嗓音了。
        self.check_send_generation()?;

        let encoded = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(&speech.bytes)
        };
        let segment = MessageSegment::record(&format!("base64://{encoded}"));
        let action = match self.session.kind {
            SessionKind::Group => OneBotAction::send_group_msg(self.session.id, vec![segment]),
            SessionKind::Private => OneBotAction::send_private_msg(self.session.id, vec![segment]),
        };

        // 定向而不是广播：两个账号连着时，广播会让两个号各发一遍同一条语音。
        //
        // 入队单独有一个短得多的上限。上面那次校验只在帧真的发出去之前才算数,
        // 而队列满着的时候它可以在里面躺满整个 20 秒——那期间撤权、换音色、
        // 关掉开关全都可能发生,而这条语音照发。何况一条迟到 20 秒的语音回复
        // 落在群里本来就是错的。
        let conn_id = self.state.conn_for_self_id(self.self_id);
        let outcome = match conn_id {
            Some(conn) => {
                super::call_api_to_conn_within(
                    &self.state,
                    conn,
                    action,
                    std::time::Duration::from_secs(2),
                    std::time::Duration::from_secs(20),
                )
                .await
            }
            None => super::DirectedCallOutcome::NotDispatched("no adapter connection for this account".into()),
        };

        let chars = text.chars().count();
        match outcome {
            super::DirectedCallOutcome::AdapterAccepted(_) => {
                tracing::info!(chars, "voice reply sent");
                Ok(serde_json::json!({ "sent": true, "chars": chars }).to_string())
            }
            // **不是错误**：帧已经在连接队列里，很可能已经发出去了。报成错误
            // 会让模型再发一遍同一句话。
            super::DirectedCallOutcome::DeliveryUnknown => {
                tracing::warn!(chars, "voice reply dispatched but unacknowledged");
                Ok(serde_json::json!({
                    "sent": "unknown",
                    "note": "The voice message was dispatched but not acknowledged. It may well have arrived — do not send it again.",
                })
                .to_string())
            }
            super::DirectedCallOutcome::Refused { retcode, message } => {
                Err(format!("the adapter refused the voice message ({retcode}): {message}"))
            }
            super::DirectedCallOutcome::NotDispatched(why) => Err(format!("could not send the voice message: {why}")),
        }
    }

    /// 出站配置有没有在这一轮中间被换掉。
    fn check_send_generation(&self) -> Result<(), String> {
        if self.state.services.corpus.send_generation() == self.send_generation {
            return Ok(());
        }
        Err("the voice settings changed while this was being prepared; try again".into())
    }

    async fn send_sticker(&self, args: &serde_json::Value) -> Result<String, String> {
        static SENT_TURNS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        let sticker_id = args
            .get("sticker_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .ok_or("missing required parameter: sticker_id")?
            .to_string();
        {
            let sent = SENT_TURNS
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if sent.contains(&self.turn_id) {
                return Err("A sticker has already been sent in this turn".into());
            }
        }
        let self_id = self.self_id.ok_or("OneBot event did not include self_id")?.to_string();
        let pool = self.state.services.db.clone();
        let data_dir = self.state.services.paths.data_dir.clone();
        let sticker = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            let pack = crate::db::ops::emoji_pack::get_by_source_account(&mut conn, &self_id)
                .map_err(|e| e.to_string())?
                .ok_or("No sticker pool exists for this bot account")?;
            let sticker = crate::db::ops::emoji::get_emoji(&mut conn, &sticker_id)
                .map_err(|_| "Unknown sticker id".to_string())?;
            if sticker.pack_id != pack.id || sticker.semantic_status != "confirmed" {
                return Err("That sticker is not in this bot account's confirmed roster".into());
            }
            Ok::<_, String>(sticker)
        })
        .await
        .map_err(|e| e.to_string())??;

        let payload = crate::emoji::parse_native_payload(&sticker.id, sticker.native_payload.as_deref())?;
        let cached_image = || -> Result<MessageSegment, String> {
            use base64::Engine;
            if sticker.file_name.is_empty() {
                return Err("Sticker has no cached image fallback".into());
            }
            let path = crate::emoji::emoji_path(&data_dir, &sticker.pack_id, &sticker.file_name);
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            Ok(MessageSegment::image(&format!("base64://{encoded}")))
        };
        let send = |segment: MessageSegment, echo: String| match self.session.kind {
            SessionKind::Group => OneBotAction::send_group_msg(self.session.id, vec![segment]).with_echo(echo),
            SessionKind::Private => OneBotAction::send_private_msg(self.session.id, vec![segment]).with_echo(echo),
        };

        let first = match sticker.source.as_str() {
            "onebot_face" => MessageSegment::raw("face", payload),
            "onebot_mface" => MessageSegment::raw("mface", payload),
            _ => cached_image()?,
        };
        let direct = call_api(&self.state, send(first, echo())).await;
        if let Err(error) = direct {
            if sticker.source != "onebot_mface" {
                return Err(error);
            }
            call_api(&self.state, send(cached_image()?, echo())).await?;
        }

        let mut sent = SENT_TURNS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if sent.len() >= 4096 {
            sent.clear();
        }
        sent.insert(self.turn_id.clone());
        serde_json::to_string(&serde_json::json!({
            "sticker_id": sticker.id,
            "name": sticker.name,
        }))
        .map_err(|e| e.to_string())
    }

    async fn get_chat_history(&self, args: &serde_json::Value) -> Result<String, String> {
        let count = get_i64(args, "count").unwrap_or(20).clamp(1, MAX_HISTORY_COUNT);
        let message_seq = get_i64(args, "message_seq").filter(|s| *s > 0);

        let action = match self.session.kind {
            SessionKind::Group => OneBotAction::get_group_msg_history(self.session.id, message_seq, count, echo()),
            SessionKind::Private => OneBotAction::get_friend_msg_history(self.session.id, message_seq, count, echo()),
        };

        let data = call_api(&self.state, action).await?;
        let messages = data
            .get("messages")
            .and_then(|m| m.as_array())
            .ok_or("no messages in response")?;
        if messages.is_empty() {
            return Ok("没有获取到历史消息".into());
        }
        Ok(format_history(messages))
    }

    async fn get_group_info(&self) -> Result<String, String> {
        let data = call_api(&self.state, OneBotAction::get_group_info(self.session.id, echo())).await?;
        let name = data.get("group_name").and_then(|v| v.as_str()).unwrap_or("?");
        let mut out = format!("群名: {name}\n群号: {}", self.session.id);
        if let Some(n) = data.get("member_count").and_then(|v| v.as_i64()) {
            let max = data
                .get("max_member_count")
                .and_then(|v| v.as_i64())
                .map(|m| format!("/{m}"))
                .unwrap_or_default();
            out.push_str(&format!("\n成员数: {n}{max}"));
        }
        Ok(out)
    }

    async fn get_group_member_list(&self) -> Result<String, String> {
        let data = call_api(
            &self.state,
            OneBotAction::get_group_member_list(self.session.id, echo()),
        )
        .await?;
        let members = data.as_array().ok_or("unexpected member list response")?;
        let mut lines = Vec::with_capacity(members.len() + 1);
        lines.push(format!("本群共 {} 人:", members.len()));
        for m in members {
            let name = m
                .get("card")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| m.get("nickname").and_then(|v| v.as_str()))
                .unwrap_or("?");
            let id = m.get("user_id").and_then(|v| v.as_i64()).unwrap_or(0);
            let role = match m.get("role").and_then(|v| v.as_str()) {
                Some("owner") => " [群主]",
                Some("admin") => " [管理员]",
                _ => "",
            };
            lines.push(format!("{name}({id}){role}"));
        }
        Ok(truncate_head(lines.join("\n")))
    }

    async fn get_group_member_info(&self, user_id: i64) -> Result<String, String> {
        let data = call_api(
            &self.state,
            OneBotAction::get_group_member_info(self.session.id, user_id, echo()),
        )
        .await?;
        let mut out = Vec::new();
        let nickname = data.get("nickname").and_then(|v| v.as_str()).unwrap_or("?");
        out.push(format!("昵称: {nickname}"));
        if let Some(card) = data.get("card").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            out.push(format!("群名片: {card}"));
        }
        out.push(format!("QQ号: {user_id}"));
        let role = match data.get("role").and_then(|v| v.as_str()) {
            Some("owner") => "群主",
            Some("admin") => "管理员",
            _ => "成员",
        };
        out.push(format!("角色: {role}"));
        if let Some(title) = data.get("title").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            out.push(format!("头衔: {title}"));
        }
        if let Some(ts) = data.get("join_time").and_then(|v| v.as_i64()).filter(|t| *t > 0)
            && let Some(dt) = chrono::DateTime::from_timestamp(ts, 0)
        {
            out.push(format!(
                "入群时间: {}",
                dt.with_timezone(&chrono::Local).format("%Y-%m-%d")
            ));
        }
        Ok(out.join("\n"))
    }

    async fn get_user_info(&self) -> Result<String, String> {
        let data = call_api(&self.state, OneBotAction::get_stranger_info(self.session.id, echo())).await?;
        let nickname = data.get("nickname").and_then(|v| v.as_str()).unwrap_or("?");
        let mut out = format!("昵称: {nickname}\nQQ号: {}", self.session.id);
        if let Some(sign) = data.get("sign").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            out.push_str(&format!("\n签名: {sign}"));
        }
        Ok(out)
    }

    async fn get_friend_list(&self) -> Result<String, String> {
        let data = call_api(&self.state, OneBotAction::get_friend_list(echo())).await?;
        let friends = data.as_array().ok_or("unexpected friend list response")?;
        let mut lines = Vec::with_capacity(friends.len() + 1);
        lines.push(format!("共 {} 位好友:", friends.len()));
        for f in friends {
            let name = f
                .get("remark")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| f.get("nickname").and_then(|v| v.as_str()))
                .unwrap_or("?");
            let id = f.get("user_id").and_then(|v| v.as_i64()).unwrap_or(0);
            lines.push(format!("{name}({id})"));
        }
        Ok(truncate_head(lines.join("\n")))
    }

    async fn get_group_list(&self) -> Result<String, String> {
        let data = call_api(&self.state, OneBotAction::get_group_list(echo())).await?;
        let groups = data.as_array().ok_or("unexpected group list response")?;
        let mut lines = Vec::with_capacity(groups.len() + 1);
        lines.push(format!("共加入 {} 个群:", groups.len()));
        for g in groups {
            let name = g.get("group_name").and_then(|v| v.as_str()).unwrap_or("?");
            let id = g.get("group_id").and_then(|v| v.as_i64()).unwrap_or(0);
            lines.push(format!("{name}({id})"));
        }
        Ok(truncate_head(lines.join("\n")))
    }
}

/// Static catalog for the settings UI: tool names plus the flags that drive
/// the availability badges. Display names/descriptions are localized on the
/// frontend by tool name.
pub fn catalog() -> Vec<serde_json::Value> {
    SPECS
        .iter()
        .map(|s| {
            let scope = match s.scope {
                Scope::Any => "any",
                Scope::GroupOnly => "group",
                Scope::PrivateOnly => "private",
            };
            serde_json::json!({
                "name": s.name,
                "description": "",
                "source": "onebot",
                "admin_only": s.admin_only,
                "needs_approval": s.needs_approval,
                "scope": scope,
            })
        })
        .collect()
}

/// Whose view of the QQ tool set this session shows. A group's has to be one
/// view for everyone in it, because the speaker changes between turns and the
/// tool array is cached prefix; a private chat's counterpart never changes, so
/// its view can stay as narrow as it always was.
fn shown_as_admin(kind: &SessionKind, is_admin: bool) -> bool {
    match kind {
        SessionKind::Group => true,
        SessionKind::Private => is_admin,
    }
}

/// Whether this session may be sent the registry and MCP tool definitions at
/// all.
///
/// Only a private chat with an admin. Those definitions carry names, prose and
/// argument schemas that come from the user's own configuration — an MCP server
/// is routinely pointed at internal systems — and a group cannot show them to
/// one member without showing them to everyone present, because one tool array
/// is all a session gets. The narrower QQ set stays available to a group and is
/// safe to (see `definitions`).
///
/// An admin who needs the full set in a group has the same conversation with
/// the bot in private.
pub(super) fn exposes_full_toolset(kind: &SessionKind, is_admin: bool) -> bool {
    is_admin && *kind == SessionKind::Private
}

/// Registry tools a QQ session may offer whoever is in it, `exposes_full_toolset`
/// having said no to the rest.
///
/// That refusal is about what a *definition reveals*: an MCP tool carries the
/// user's own server names and argument schemas, a file tool names paths on this
/// machine, and a group cannot show one member any of it without showing
/// everyone. `web_search` reveals none of that — its description is our own
/// fixed prose, it reads nothing here, and a QQ session's file access is an
/// empty root set regardless. On that test it belongs with the QQ tools rather
/// than with the registry it happens to live in, and being swept up with them
/// was the accident.
///
/// Fixed per session and not per speaker, like the rest of the tool array: it
/// goes to everyone in a session or to nobody, so it cannot be the thing that
/// makes an admin's turn and a member's turn two different cached prefixes.
///
/// Narrowing only. An assistant that has `web_search` switched off still does
/// not get it — `ToolExposure::Only` filters what `enabled_tools` already
/// allowed — and its `Permission::Ask` is unchanged, so a search still asks
/// before it runs.
pub(super) const OPEN_REGISTRY_TOOLS: &[&str] = &["web_search"];

/// What an ordinary member may run: the session-locked QQ reads, plus any
/// open registry tool that is actually in this turn's array.
///
/// Shared by the opening `offered` and by `InboxSteering` after a demotion, so
/// a member joining an admin's turn does not lose a tool the array still
/// advertises — which is how the model ends up calling `web_search` in front
/// of the group and being refused.
pub(super) fn ordinary_offered(
    names: impl IntoIterator<Item = String>,
    tool_defs: &[crate::provider::ToolDefinition],
) -> std::collections::HashSet<String> {
    names
        .into_iter()
        .chain(
            tool_defs
                .iter()
                .map(|t| t.name.clone())
                .filter(|name| OPEN_REGISTRY_TOOLS.contains(&name.as_str())),
        )
        .collect()
}

pub(super) const SEND_VOICE_TOOL: &str = "send_voice";

/// 超出 `Scope` 之外、由会话本身决定的可用性。
///
/// `Scope` 是工具的性质（`qq_set_group_ban` 在私聊里永远没意义）；"这间屋子准
/// 不准发语音"是屋子的属性，而且还取决于四项配置有没有配齐。两者分开是因为
/// 放进 const 数组就会有人顺手按发言人去算它，而那正是"一个群一套工具数组"
/// 要挡的事。
///
/// **必须三处共用**：`definitions()`（模型看到什么）、`ordinary_names()`
/// （非 admin 的 `offered`）、`available()`（`execute` 的前置检查）。只改第一处
/// 不构成权限边界——`execute` 只查 `Scope`，模型凭名字就能调到。
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct SessionPolicy {
    pub voice_send: bool,
}

fn spec_available(spec: &ToolSpec, kind: &SessionKind, is_admin: bool, policy: SessionPolicy) -> bool {
    if spec.admin_only && !is_admin {
        return false;
    }
    if spec.name == SEND_VOICE_TOOL && !policy.voice_send {
        return false;
    }
    match spec.scope {
        Scope::Any => true,
        Scope::GroupOnly => *kind == SessionKind::Group,
        Scope::PrivateOnly => *kind == SessionKind::Private,
    }
}

/// A call that changes something outside this app, and so takes a
/// `description` — see [`crate::tools::description_property`].
///
/// Read off `needs_approval` rather than listed a second time, because the two
/// are the same set: a call worth stopping a person for is a call worth one line
/// saying what it is. `send_sticker` is the only addition — it stops for nobody
/// and it still lands in somebody's chat window.
fn has_effects(name: &str) -> bool {
    // `send_sticker` 与 `send_voice` 是仅有的两个加法——它们不为任何人停下，
    // 而结果都落在别人的聊天窗口里。
    name == "send_sticker" || name == SEND_VOICE_TOOL || SPECS.iter().any(|s| s.name == name && s.needs_approval)
}

/// Fold that property into a spec's parameters.
///
/// Done here rather than written into eighteen match arms, so it cannot drift
/// from the set it is keyed on. It reaches further in QQ than it does on the
/// desktop: `handler::make_approval_fn` prints the arguments verbatim into the
/// approval message, so for a group admin being asked about a ten-minute mute
/// this is the only part of that prompt written for a person.
fn with_description(name: &str, mut parameters: serde_json::Value) -> serde_json::Value {
    if has_effects(name)
        && let Some(props) = parameters.get_mut("properties").and_then(|p| p.as_object_mut())
    {
        props.insert("description".into(), crate::tools::description_property());
    }
    parameters
}

fn echo() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn get_i64(args: &serde_json::Value, key: &str) -> Option<i64> {
    args.get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}

fn require_i64(args: &serde_json::Value, key: &str) -> Result<i64, String> {
    get_i64(args, key).ok_or_else(|| format!("missing required parameter: {key}"))
}

/// Keep the head of an over-long listing (the interesting part for lists).
fn truncate_head(text: String) -> String {
    if text.chars().count() <= MAX_OUTPUT_CHARS {
        return text;
    }
    let cut = text
        .char_indices()
        .nth(MAX_OUTPUT_CHARS)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    format!("{}\n(更多条目已截断)", &text[..cut])
}

fn format_history(messages: &[serde_json::Value]) -> String {
    let mut lines = Vec::with_capacity(messages.len());
    let mut min_seq: Option<i64> = None;
    for msg in messages {
        if let Some(seq) = msg.get("message_seq").and_then(|v| v.as_i64()).filter(|s| *s > 0) {
            min_seq = Some(min_seq.map_or(seq, |m: i64| m.min(seq)));
        }
        let time = msg
            .get("time")
            .and_then(|v| v.as_i64())
            .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
            .map(|dt| dt.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
            .unwrap_or_default();
        let sender = msg
            .get("sender")
            .map(|s| {
                s.get("card")
                    .and_then(|v| v.as_str())
                    .filter(|c| !c.is_empty())
                    .or_else(|| s.get("nickname").and_then(|v| v.as_str()))
                    .unwrap_or("?")
            })
            .unwrap_or("?");
        let text = msg
            .get("message")
            .map(|m| super::format::segments_to_text(m, None))
            .filter(|t| !t.is_empty())
            .or_else(|| msg.get("raw_message").and_then(|v| v.as_str()).map(String::from))
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        lines.push(format!("[{time}] {sender}: {text}"));
    }

    let mut out = lines.join("\n");
    let char_count = out.chars().count();
    if char_count > MAX_OUTPUT_CHARS {
        let skip = char_count - MAX_OUTPUT_CHARS;
        let cut = out.char_indices().nth(skip).map(|(i, _)| i).unwrap_or(0);
        out = format!("(更早的消息已截断)\n{}", &out[cut..]);
    }
    if let Some(seq) = min_seq {
        out.push_str(&format!("\n(如需更早的消息,传入 message_seq={seq} 继续向前翻页)"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(kind: SessionKind, is_admin: bool) -> Vec<&'static str> {
        SPECS
            .iter()
            .filter(|s| spec_available(s, &kind, is_admin, SessionPolicy { voice_send: true }))
            .map(|s| s.name)
            .collect()
    }

    /// What a session shows and what a speaker may run are two questions, and
    /// only the second one is about who spoke. A group gets one tool array
    /// however many people talk into it, because that array is part of the
    /// prefix the provider caches — it used to be rebuilt per message, so an
    /// admin and an ordinary member alternating threw the cache away on every
    /// turn.
    #[test]
    fn a_group_shows_one_tool_set_whoever_spoke() {
        let shown_to_admin = names(SessionKind::Group, shown_as_admin(&SessionKind::Group, true));
        let shown_to_member = names(SessionKind::Group, shown_as_admin(&SessionKind::Group, false));
        assert_eq!(shown_to_admin, shown_to_member);

        // And the widening stops at what is shown: an ordinary member may still
        // only run the read-only ones, which is what `ordinary_names` returns
        // and the loop checks before dispatch.
        let permitted = names(SessionKind::Group, false);
        assert!(permitted.len() < shown_to_member.len());
        assert!(permitted.iter().all(|n| shown_to_member.contains(n)));
        assert!(!permitted.contains(&"qq_set_group_ban"));
    }

    /// A private chat has one counterpart, so `is_admin` cannot change under it
    /// and narrowing costs no cache — which leaves no reason to show somebody
    /// tools they will only be refused.
    #[test]
    fn a_private_chat_shows_nothing_its_counterpart_cannot_run() {
        let shown = names(SessionKind::Private, shown_as_admin(&SessionKind::Private, false));
        assert_eq!(shown, names(SessionKind::Private, false));
        assert!(!shown.contains(&"qq_delete_msg"));
    }

    /// The registry and MCP definitions carry the user's own server names,
    /// prose and argument schemas. A group is one tool array shared by everyone
    /// present, so showing them to its admin would be showing them to the room.
    #[test]
    fn only_an_admin_in_private_is_sent_the_registry_and_mcp_tools() {
        assert!(exposes_full_toolset(&SessionKind::Private, true));
        assert!(!exposes_full_toolset(&SessionKind::Private, false));
        assert!(
            !exposes_full_toolset(&SessionKind::Group, true),
            "a group cannot show these to one member alone",
        );
        assert!(!exposes_full_toolset(&SessionKind::Group, false));
    }

    #[test]
    fn demoting_keeps_open_registry_tools_still_in_the_array() {
        let names = ["list_stickers".to_string()];
        let with_search = vec![crate::provider::ToolDefinition {
            name: "web_search".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }];
        let offered = ordinary_offered(names.clone(), &with_search);
        assert!(offered.contains("list_stickers"));
        assert!(offered.contains("web_search"));

        let offered = ordinary_offered(names, &[]);
        assert!(offered.contains("list_stickers"));
        assert!(!offered.contains("web_search"));
    }

    #[test]
    fn test_non_admin_group_gets_query_tools_only() {
        let n = names(SessionKind::Group, false);
        assert!(n.contains(&"list_stickers"));
        assert!(n.contains(&"send_sticker"));
        assert!(n.contains(&QQ_HISTORY_TOOL));
        assert!(n.contains(&"qq_get_group_member_list"));
        assert!(!n.contains(&"qq_get_user_info"), "private-only tool absent in groups");
        assert!(!n.contains(&"qq_set_group_ban"), "action tools are admin-only");
        assert!(!n.contains(&"qq_get_friend_list"), "bot-global reads are admin-only");
    }

    #[test]
    fn test_non_admin_private_scope() {
        let n = names(SessionKind::Private, false);
        assert!(n.contains(&QQ_HISTORY_TOOL));
        assert!(n.contains(&"qq_get_user_info"));
        assert!(!n.contains(&"qq_get_group_member_list"));
        assert!(!n.contains(&"qq_delete_msg"));
    }

    #[test]
    fn test_admin_group_gets_action_tools() {
        let n = names(SessionKind::Group, true);
        assert!(n.contains(&"qq_set_group_ban"));
        assert!(n.contains(&"qq_send_group_notice"));
        assert!(n.contains(&"qq_get_friend_list"));
        assert!(!n.contains(&"qq_get_user_info"), "still group-scoped");
    }

    #[test]
    fn test_admin_private_excludes_group_actions() {
        let n = names(SessionKind::Private, true);
        assert!(n.contains(&"qq_delete_msg"));
        assert!(n.contains(&"qq_send_poke"));
        assert!(!n.contains(&"qq_set_group_kick"));
    }

    #[test]
    fn test_action_tools_require_approval_queries_do_not() {
        let approval_needed: Vec<_> = SPECS.iter().filter(|s| s.needs_approval).map(|s| s.name).collect();
        assert!(approval_needed.contains(&"qq_set_group_ban"));
        assert!(approval_needed.contains(&"qq_delete_msg"));
        assert!(!approval_needed.contains(&QQ_HISTORY_TOOL));
        assert!(!approval_needed.contains(&"send_sticker"));
        assert!(!approval_needed.contains(&"qq_get_group_member_list"));
        // Every approval-gated tool is also admin-only.
        assert!(SPECS.iter().filter(|s| s.needs_approval).all(|s| s.admin_only));
    }

    /// 白名单关掉时，`send_voice` 从**两边同时**消失。
    ///
    /// 只从一边拿掉就是让模型拿着一个会被 dispatch 拒绝的工具，在一屋子人面前
    /// 反复调用——`definitions()` 决定它看见什么，`ordinary_names()` 决定非
    /// admin 的 `offered`，而 `execute` 只查 `Scope`。
    #[test]
    fn a_session_without_voice_neither_shows_nor_offers_it() {
        let off = SessionPolicy { voice_send: false };
        let on = SessionPolicy { voice_send: true };
        let spec = SPECS.iter().find(|s| s.name == SEND_VOICE_TOOL).unwrap();

        for kind in [SessionKind::Group, SessionKind::Private] {
            for is_admin in [true, false] {
                assert!(
                    !spec_available(spec, &kind, is_admin, off),
                    "关掉时对任何人、任何会话都不可用"
                );
                assert!(spec_available(spec, &kind, is_admin, on));
            }
        }
    }

    /// 而且策略是**屋子的属性，不是人的**：同一个群里两个不同身份的发言人
    /// 看到的是同一套。否则工具数组会随说话人变，把 prompt cache 前缀甩掉。
    #[test]
    fn the_voice_policy_does_not_depend_on_who_spoke() {
        let policy = SessionPolicy { voice_send: true };
        let spec = SPECS.iter().find(|s| s.name == SEND_VOICE_TOOL).unwrap();
        assert_eq!(
            spec_available(spec, &SessionKind::Group, true, policy),
            spec_available(spec, &SessionKind::Group, false, policy),
        );
    }

    /// The `description` parameter goes on the calls that change something and
    /// nowhere else. A read already says what it is in its path or its pattern,
    /// and one on every query is output tokens spent restating an argument the
    /// card is showing anyway.
    #[test]
    fn only_a_call_with_effects_is_asked_to_describe_itself() {
        let takes_one = |name: &str| {
            let params = with_description(name, serde_json::json!({ "type": "object", "properties": {} }));
            params["properties"].get("description").is_some()
        };

        assert!(takes_one("qq_set_group_ban"));
        assert!(takes_one("qq_send_group_notice"));
        assert!(takes_one("qq_delete_msg"));
        // Sends a message and never asks first, which is why it is named in
        // `has_effects` rather than derived from `needs_approval`.
        assert!(takes_one("send_sticker"));
        assert!(takes_one(SEND_VOICE_TOOL), "同理：不问人，但落在别人的聊天窗口里");

        assert!(!takes_one(QQ_HISTORY_TOOL));
        assert!(!takes_one("qq_get_group_member_list"));
        assert!(!takes_one("list_stickers"));

        // The rule, not the list: anything that stops for a person describes
        // itself, so adding a spec cannot quietly leave one out.
        for spec in SPECS.iter().filter(|s| s.needs_approval) {
            assert!(
                takes_one(spec.name),
                "{} stops for a person and says nothing",
                spec.name
            );
        }
    }

    /// A tool with real parameters keeps them. The injection writes into the
    /// existing `properties` object, and an early version that replaced it would
    /// have passed every assertion above while dropping `user_id`.
    #[test]
    fn describing_a_call_does_not_cost_it_its_own_arguments() {
        let params = with_description(
            "qq_set_group_ban",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "user_id": { "type": "integer" },
                    "duration": { "type": "integer" },
                },
                "required": ["user_id"],
            }),
        );
        let props = params["properties"].as_object().unwrap();
        assert_eq!(props.len(), 3);
        assert!(props.contains_key("user_id"));
        assert!(props.contains_key("duration"));
        // Never required: a model that forgets it should produce a card with a
        // plainer summary, not a call that fails validation mid-turn.
        assert_eq!(params["required"], serde_json::json!(["user_id"]));
    }

    #[test]
    fn test_format_history_paging_footer() {
        let messages = vec![
            serde_json::json!({
                "time": 1700000000, "message_seq": 120,
                "sender": {"nickname": "甲"},
                "message": [{"type": "text", "data": {"text": "hello"}}],
            }),
            serde_json::json!({
                "time": 1700000060, "message_seq": 121,
                "sender": {"nickname": "乙"},
                "message": [{"type": "text", "data": {"text": "world"}}],
            }),
        ];
        let out = format_history(&messages);
        assert!(out.contains("甲: hello"));
        assert!(
            out.contains("message_seq=120"),
            "footer points at the oldest seq: {out}"
        );
    }

    #[test]
    fn test_format_history_no_footer_without_seq() {
        let messages = vec![serde_json::json!({
            "time": 1700000000,
            "sender": {"nickname": "甲"},
            "message": [{"type": "text", "data": {"text": "hi"}}],
        })];
        let out = format_history(&messages);
        assert!(!out.contains("message_seq="));
    }
}
