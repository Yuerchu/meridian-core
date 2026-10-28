//! Chinese labels for QQ approval cards, gated against `src/i18n/locales/zh-CN.json`.
//!
//! QQ must not show wire names (`run_command`, `qq_set_group_ban`). The
//! front end already i18n-izes them as `chat.tool.name.*`; this map is that
//! same table for the handler, which has no locale loader. Known tools use
//! the Chinese label as the heading and the model's `description` as the
//! body. Unknown / MCP tools use the description as the heading when one is
//! present, otherwise the configured name. Arguments are never dumped as JSON.

/// Every `chat.tool.name.*` key in zh-CN, plus the two reply-bank tools.
/// Bidirectional with the locale file: a label that drifts, or a locale key
/// this map does not know, fails the gate below.
const LABELS: &[(&str, &str)] = &[
    ("run_agent", "子 Agent"),
    ("read_file", "读取文件"),
    ("write_file", "写入文件"),
    ("edit_file", "编辑文件"),
    ("delete_file", "删除文件"),
    ("move_file", "移动文件"),
    ("list_directory", "列出目录"),
    ("search_files", "搜索文件"),
    ("glob", "匹配文件"),
    ("run_command", "运行命令"),
    ("apply_patch", "应用补丁"),
    ("save_memory", "保存记忆"),
    ("recall_memory", "回忆"),
    ("list_memories", "列出记忆"),
    ("delete_memory", "删除记忆"),
    ("ask_user", "提问"),
    ("web_search", "网络搜索"),
    ("google_search", "Google 搜索"),
    ("url_context", "网页获取"),
    ("update_todos", "任务清单"),
    ("enter_plan", "请求规划"),
    ("exit_plan", "提交方案"),
    ("load_skill", "加载技能"),
    ("read_app_logs", "查看应用日志"),
    ("qq_get_chat_history", "查询聊天记录"),
    ("qq_get_group_info", "查询群信息"),
    ("qq_get_group_member_list", "查询群成员列表"),
    ("qq_get_group_member_info", "查询群成员资料"),
    ("qq_get_user_info", "查询用户资料"),
    ("qq_get_friend_list", "查询好友列表"),
    ("qq_get_group_list", "查询群列表"),
    ("qq_delete_msg", "撤回消息"),
    ("qq_send_poke", "戳一戳"),
    ("qq_send_like", "点赞"),
    ("qq_set_essence_msg", "设置精华消息"),
    ("qq_set_group_ban", "群禁言"),
    ("qq_set_group_kick", "移出群聊"),
    ("qq_set_group_card", "设置群名片"),
    ("qq_set_group_name", "修改群名"),
    ("qq_send_group_notice", "发布群公告"),
    ("qq_list_replies", "查看回复措辞"),
    ("qq_manage_replies", "维护回复措辞"),
    ("list_stickers", "查看可用表情"),
    ("send_sticker", "发送表情"),
    ("Agent", "子 Agent"),
    ("Bash", "运行命令"),
    ("Read", "读取文件"),
    ("Write", "写入文件"),
    ("Edit", "编辑文件"),
    ("NotebookEdit", "编辑 Notebook"),
    ("Glob", "匹配文件"),
    ("Grep", "搜索内容"),
    ("WebFetch", "抓取网页"),
    ("WebSearch", "网络搜索"),
    ("TodoWrite", "任务清单"),
    ("EnterPlanMode", "请求规划"),
    ("ExitPlanMode", "提交方案"),
    ("AskUserQuestion", "提问"),
    ("Skill", "加载技能"),
    ("ToolSearch", "检索工具"),
    ("Workflow", "工作流"),
    ("ReportFindings", "提交审查结论"),
    ("TaskCreate", "创建后台任务"),
    ("TaskGet", "查询后台任务"),
    ("TaskList", "列出后台任务"),
    ("TaskUpdate", "更新后台任务"),
    ("TaskOutput", "读取任务输出"),
    ("TaskStop", "停止任务"),
    ("LSP", "语言服务"),
    ("add_redaction_rule", "添加脱敏规则"),
    ("list_redaction_rules", "列出脱敏规则"),
    ("remove_redaction_rule", "删除脱敏规则"),
    ("conversation_usage", "查看会话用量"),
    ("read_conversation", "读取会话"),
    ("read_plan", "读取方案"),
    ("update_plan", "更新方案"),
    ("send_voice", "发送语音"),
    ("PowerShell", "运行命令"),
    ("Monitor", "监视输出"),
    ("read_background_output", "读取后台输出"),
    ("stop_background_task", "停止后台命令"),
    ("list_background_tasks", "列出后台命令"),
];

pub fn known_label(name: &str) -> Option<&'static str> {
    LABELS.iter().find(|(tool, _)| *tool == name).map(|(_, label)| *label)
}

/// The model's one-line `description` argument, when present and non-empty.
pub fn call_description(arguments: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    value
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Heading shown on the card. Known tools use the i18n label; unknown tools
/// use the description when they have one, otherwise the configured name.
pub fn display_heading(name: &str, description: Option<&str>) -> String {
    if let Some(label) = known_label(name) {
        return label.to_string();
    }
    if let Some(desc) = description.filter(|s| !s.is_empty()) {
        return desc.to_string();
    }
    name.to_string()
}

pub fn approval_prompt(name: &str, arguments: &str, sandbox_reason: Option<&str>) -> String {
    let description = call_description(arguments);
    let known = known_label(name).is_some();
    let heading = display_heading(name, description.as_deref());
    // Known: label heading, description as body. Unknown: description already
    // is the heading, so it is not repeated.
    let body = if known { description } else { None };

    let mut lines = vec![heading];
    if let Some(body) = body {
        lines.push(String::new());
        lines.push(body);
    }
    lines.push(String::new());
    if let Some(reason) = sandbox_reason {
        lines.push(format!("拦截输出: {}", truncate_chars(reason, 300)));
        lines.push(String::new());
        lines.push("引用本条消息回复 Y 在沙箱外重试，其他内容拒绝并作为理由转达（60秒超时）".into());
    } else {
        lines.push("引用本条消息回复 Y 批准，其他内容拒绝并作为理由转达（60秒超时）".into());
    }
    lines.join("\n")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let taken: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{taken}...")
    } else {
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn a_known_tool_uses_the_label_and_keeps_the_description_as_body() {
        let prompt = approval_prompt(
            "run_command",
            r#"{"command":"rm -rf /","description":"清一下缓存"}"#,
            None,
        );
        assert!(prompt.starts_with("运行命令\n\n清一下缓存\n\n"), "{prompt}");
        assert!(!prompt.contains("run_command"), "{prompt}");
        assert!(!prompt.contains("rm -rf"), "{prompt}");
        assert!(!prompt.contains("🔧"), "{prompt}");
        assert!(prompt.contains("引用本条消息回复 Y 批准"));
    }

    #[test]
    fn an_unknown_tool_with_a_description_uses_it_as_the_heading_once() {
        let prompt = approval_prompt(
            "mcp__weather__forecast",
            r#"{"city":"Shanghai","description":"查明天的天气"}"#,
            None,
        );
        assert!(prompt.starts_with("查明天的天气\n\n引用"), "{prompt}");
        assert_eq!(prompt.matches("查明天的天气").count(), 1, "{prompt}");
        assert!(!prompt.contains("mcp__weather__forecast"), "{prompt}");
        assert!(!prompt.contains("Shanghai"), "{prompt}");
    }

    #[test]
    fn an_unknown_tool_without_a_description_uses_the_configured_name() {
        let prompt = approval_prompt("mcp__weather__forecast", r#"{"city":"Shanghai"}"#, None);
        assert!(prompt.starts_with("mcp__weather__forecast\n\n引用"), "{prompt}");
        assert!(!prompt.contains("Shanghai"), "{prompt}");
    }

    #[test]
    fn a_sandbox_retry_names_the_intercept_and_asks_to_retry_outside() {
        let prompt = approval_prompt("run_command", "{}", Some("Access denied"));
        assert!(prompt.contains("拦截输出: Access denied"), "{prompt}");
        assert!(prompt.contains("在沙箱外重试"), "{prompt}");
        assert!(!prompt.contains("🔧") && !prompt.contains("⚠️"), "{prompt}");
    }

    #[test]
    fn labels_match_zh_cn_chat_tool_names_bidirectionally() {
        let Some(path) = locale_path() else {
            return;
        };
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let map: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));

        let mut locale: BTreeMap<&str, &str> = BTreeMap::new();
        for (key, value) in &map {
            let Some(name) = key.strip_prefix("chat.tool.name.") else {
                continue;
            };
            let Some(label) = value.as_str() else {
                panic!("{key} is not a string");
            };
            locale.insert(name, label);
        }

        for (name, label) in LABELS {
            match locale.get(name) {
                Some(got) if *got == *label => {}
                Some(got) => panic!("zh-CN chat.tool.name.{name} is {got:?}, labels.rs has {label:?}"),
                None => panic!("labels.rs has {name} but zh-CN.json has no chat.tool.name.{name}"),
            }
        }
        for (name, label) in &locale {
            match LABELS.iter().find(|(n, _)| n == name) {
                Some((_, got)) if got == label => {}
                Some((_, got)) => panic!("labels.rs {name} is {got:?}, zh-CN has {label:?}"),
                None => panic!("zh-CN has chat.tool.name.{name} but labels.rs does not"),
            }
        }
    }

    fn locale_path() -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../src/i18n/locales/zh-CN.json");
        path.exists().then_some(path)
    }
}
