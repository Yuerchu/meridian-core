use std::collections::HashMap;

use regex::Regex;

pub struct TemplateContext {
    vars: HashMap<String, String>,
}

impl TemplateContext {
    pub fn new() -> Self {
        Self { vars: HashMap::new() }
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(key.into(), value.into());
    }
}

pub fn resolve(template: &str, ctx: &TemplateContext) -> String {
    let re = Regex::new(r"\{\{(\w+)\}\}").unwrap();
    re.replace_all(template, |caps: &regex::Captures| {
        let key = &caps[1];
        ctx.vars.get(key).cloned().unwrap_or_else(|| caps[0].to_string())
    })
    .into_owned()
}

pub fn build_context(assistant_name: Option<&str>, user_name: Option<&str>) -> TemplateContext {
    let mut ctx = TemplateContext::new();

    if let Some(name) = assistant_name {
        ctx.set("assistant_name", name);
    }
    if let Some(name) = user_name {
        ctx.set("user_name", name);
    }

    let now = chrono::Local::now();
    ctx.set("current_time", now.format("%H:%M").to_string());
    ctx.set("current_date", now.format("%Y-%m-%d").to_string());
    ctx.set("current_datetime", now.format("%Y-%m-%d %H:%M").to_string());
    ctx.set("day_of_week", now.format("%A").to_string());
    ctx.set("chat_style_hint", "To send multiple short messages instead of one long reply, separate them with a line containing only --- (three dashes). Each segment will be shown as a separate chat bubble. Use this for casual, human-like conversation flow.");

    ctx
}

pub struct TemplateVariable {
    pub name: &'static str,
    pub description_en: &'static str,
    pub description_zh: &'static str,
}

pub fn available_variables() -> Vec<TemplateVariable> {
    vec![
        TemplateVariable {
            name: "assistant_name",
            description_en: "Name of the current assistant",
            description_zh: "当前助手名称",
        },
        TemplateVariable {
            name: "user_name",
            description_en: "User's display name",
            description_zh: "用户显示名称",
        },
        TemplateVariable {
            name: "current_time",
            description_en: "Current time (HH:MM)",
            description_zh: "当前时间 (HH:MM)",
        },
        TemplateVariable {
            name: "current_date",
            description_en: "Current date (YYYY-MM-DD)",
            description_zh: "当前日期 (YYYY-MM-DD)",
        },
        TemplateVariable {
            name: "current_datetime",
            description_en: "Current date and time",
            description_zh: "当前日期和时间",
        },
        TemplateVariable {
            name: "day_of_week",
            description_en: "Day of the week (e.g. Monday)",
            description_zh: "星期几 (如 Monday)",
        },
        TemplateVariable {
            name: "emoji_list",
            description_en: "List of available emoji from assigned packs",
            description_zh: "已分配表情包中的可用表情列表",
        },
        TemplateVariable {
            name: "chat_style_hint",
            description_en: "Hint for segmented chat-style responses (use --- to split)",
            description_zh: "分句消息风格提示（用 --- 分隔多条消息）",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_basic() {
        let mut ctx = TemplateContext::new();
        ctx.set("name", "Alice");
        assert_eq!(resolve("Hello {{name}}!", &ctx), "Hello Alice!");
    }

    #[test]
    fn test_resolve_unknown_var_preserved() {
        let ctx = TemplateContext::new();
        assert_eq!(resolve("Hi {{unknown}}", &ctx), "Hi {{unknown}}");
    }

    #[test]
    fn test_resolve_multiple() {
        let mut ctx = TemplateContext::new();
        ctx.set("a", "1");
        ctx.set("b", "2");
        assert_eq!(resolve("{{a}} + {{b}} = 3", &ctx), "1 + 2 = 3");
    }

    #[test]
    fn test_no_placeholders() {
        let ctx = TemplateContext::new();
        assert_eq!(resolve("plain text", &ctx), "plain text");
    }

    #[test]
    fn test_build_context_sets_time() {
        let ctx = build_context(Some("Bot"), Some("User"));
        assert_eq!(ctx.vars.get("assistant_name").unwrap(), "Bot");
        assert_eq!(ctx.vars.get("user_name").unwrap(), "User");
        assert!(ctx.vars.contains_key("current_time"));
        assert!(ctx.vars.contains_key("current_date"));
    }
}
