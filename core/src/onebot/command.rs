use strum::{EnumIter, IntoEnumIterator};

/// Who may run a command. No command claims `AdminOnly` today; the variant is
/// the domain, not a code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAccess {
    Everyone,
    #[allow(dead_code)]
    AdminOnly,
    /// Fine in a private chat, restricted in a group — for commands that act on
    /// state the whole room shares.
    AdminOnlyInGroup,
}

/// Where a command makes sense. Every current command answers `Any`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    Any,
    /// Output would expose things learned elsewhere, so it must not be run
    /// where everyone can read it.
    #[allow(dead_code)]
    PrivateOnly,
    #[allow(dead_code)]
    GroupOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter)]
pub enum SlashCommand {
    New,
    Compact,
    Model,
    Status,
    Memory,
    Help,
}

impl SlashCommand {
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "new" | "reset" => Some(Self::New),
            "compact" => Some(Self::Compact),
            "model" => Some(Self::Model),
            "status" => Some(Self::Status),
            "memory" | "mem" | "记忆" => Some(Self::Memory),
            "help" => Some(Self::Help),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Compact => "compact",
            Self::Model => "model",
            Self::Status => "status",
            Self::Memory => "memory",
            Self::Help => "help",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::New => "重置对话，创建新会话",
            Self::Compact => "压缩上下文",
            Self::Model => "查看或切换模型",
            Self::Status => "查看当前会话状态",
            Self::Memory => "查看或删除关于你的记忆",
            Self::Help => "显示此帮助",
        }
    }

    pub fn args_hint(&self) -> &'static str {
        match self {
            Self::Compact => " [说明]",
            Self::Model => " [模型名]",
            Self::Memory => " [me|forget N|undo|optout|group|...]",
            _ => "",
        }
    }

    pub fn access(&self) -> CommandAccess {
        match self {
            // A group's conversation is shared, so wiping or compacting it is
            // not one member's call to make.
            Self::New | Self::Compact => CommandAccess::AdminOnlyInGroup,
            _ => CommandAccess::Everyone,
        }
    }

    pub fn scope(&self) -> CommandScope {
        CommandScope::Any
    }

    pub fn available(&self, is_admin: bool, is_group: bool) -> bool {
        let access_ok = match self.access() {
            CommandAccess::Everyone => true,
            CommandAccess::AdminOnly => is_admin,
            CommandAccess::AdminOnlyInGroup => is_admin || !is_group,
        };
        let scope_ok = match self.scope() {
            CommandScope::Any => true,
            CommandScope::PrivateOnly => !is_group,
            CommandScope::GroupOnly => is_group,
        };
        access_ok && scope_ok
    }

    /// Filtered by what the caller can actually run: listing commands they will
    /// only be refused for is both noise and a map of the admin surface.
    pub fn help_text(is_admin: bool, is_group: bool) -> String {
        let mut lines = vec!["可用指令:".to_string()];
        for cmd in Self::iter().filter(|c| c.available(is_admin, is_group)) {
            lines.push(format!("/{}{} — {}", cmd.name(), cmd.args_hint(), cmd.description()));
        }
        lines.join("\n")
    }
}

/// `/memory` subcommands. Parsed separately from the top-level command so the
/// permission rules for each can be stated on their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemorySub {
    /// Counts only — safe anywhere.
    Overview,
    /// What the bot knows about you, here.
    Me,
    /// Forget entries from the last listing, by number.
    Forget(Vec<usize>),
    ForgetAll {
        confirmed: bool,
    },
    Undo,
    OptOut,
    OptIn,
    /// This room's own memories.
    Group,
    GroupForget(Vec<usize>),
    /// Operator views: everything about someone, or the bot-wide layer.
    User(i64),
    UserAdd {
        user_id: i64,
        content: String,
    },
    Global,
    GlobalAdd {
        key: String,
        content: String,
    },
    Pending,
    Help,
}

impl MemorySub {
    /// Operator-only subcommands are also private-only: their output mixes what
    /// was learned in other rooms, in private chats, and the operator's own
    /// notes. None of that belongs in a group.
    pub fn is_operator_only(&self) -> bool {
        matches!(
            self,
            MemorySub::User(_)
                | MemorySub::UserAdd { .. }
                | MemorySub::Global
                | MemorySub::GlobalAdd { .. }
                | MemorySub::Pending
        )
    }
}

/// Parse "2", "2 3 5" and "2-5" into indices. Returns `None` when nothing
/// parses, so a typo is reported rather than silently deleting the wrong row.
fn parse_indices(input: &str) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for token in input.split_whitespace() {
        if let Some((a, b)) = token.split_once('-') {
            let (a, b) = (a.trim().parse::<usize>().ok()?, b.trim().parse::<usize>().ok()?);
            if a == 0 || b < a || b - a > 100 {
                return None;
            }
            out.extend(a..=b);
        } else {
            let n = token.parse::<usize>().ok()?;
            if n == 0 {
                return None;
            }
            out.push(n);
        }
    }
    (!out.is_empty()).then(|| {
        out.sort_unstable();
        out.dedup();
        out
    })
}

pub fn parse_memory_sub(args: &str) -> Option<MemorySub> {
    let args = args.trim();
    if args.is_empty() {
        return Some(MemorySub::Overview);
    }
    let (head, rest) = match args.find(char::is_whitespace) {
        Some(pos) => (&args[..pos], args[pos..].trim()),
        None => (args, ""),
    };

    match head {
        "me" => Some(MemorySub::Me),
        "forget" => {
            // Matched as exact token sequences. `trim_start_matches("all")`
            // succeeds on input that never contained "all", which made a bare
            // `forget yes` — a plausible typo, or an answer to some earlier
            // prompt — parse as a confirmed wipe of everything.
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            match tokens.as_slice() {
                ["all"] => Some(MemorySub::ForgetAll { confirmed: false }),
                ["all", "yes"] => Some(MemorySub::ForgetAll { confirmed: true }),
                _ => parse_indices(rest).map(MemorySub::Forget),
            }
        }
        "undo" => Some(MemorySub::Undo),
        "optout" => Some(MemorySub::OptOut),
        "optin" => Some(MemorySub::OptIn),
        "group" => {
            let (sub, tail) = match rest.find(char::is_whitespace) {
                Some(pos) => (&rest[..pos], rest[pos..].trim()),
                None => (rest, ""),
            };
            if sub == "forget" {
                parse_indices(tail).map(MemorySub::GroupForget)
            } else if rest.is_empty() {
                Some(MemorySub::Group)
            } else {
                None
            }
        }
        "user" => {
            let (id_str, tail) = match rest.find(char::is_whitespace) {
                Some(pos) => (&rest[..pos], rest[pos..].trim()),
                None => (rest, ""),
            };
            let user_id = id_str.parse::<i64>().ok()?;
            if let Some(content) = tail.strip_prefix("add") {
                let content = content.trim();
                (!content.is_empty()).then(|| MemorySub::UserAdd {
                    user_id,
                    content: content.to_string(),
                })
            } else if tail.is_empty() {
                Some(MemorySub::User(user_id))
            } else {
                None
            }
        }
        "global" => {
            if let Some(tail) = rest.strip_prefix("add") {
                let tail = tail.trim();
                let (key, content) = tail.split_once(char::is_whitespace)?;
                let content = content.trim();
                (!content.is_empty()).then(|| MemorySub::GlobalAdd {
                    key: key.to_string(),
                    content: content.to_string(),
                })
            } else if rest.is_empty() {
                Some(MemorySub::Global)
            } else {
                None
            }
        }
        "pending" => Some(MemorySub::Pending),
        "help" => Some(MemorySub::Help),
        _ => None,
    }
}

/// An admin decision on a pending friend/group request, e.g. "同意 3" / "拒绝3 广告".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestDecision {
    pub approve: bool,
    pub target: DecisionTarget,
    pub reason: Option<String>,
}

/// Which queue a numbered decision refers to.
///
/// The two queues number themselves independently — friend requests from an
/// in-memory counter, memory proposals from a database AUTOINCREMENT — so a bare
/// number is ambiguous and resolving it by "whichever queue we check first"
/// means an operator approving a memory can silently accept a stranger's friend
/// request instead. Memory proposals therefore carry an `M` prefix everywhere
/// they are displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionTarget {
    Request(u32),
    MemoryProposal(i32),
}

impl DecisionTarget {
    /// How the id is shown to the operator, and how they must type it back.
    pub fn label(&self) -> String {
        match self {
            DecisionTarget::Request(id) => id.to_string(),
            DecisionTarget::MemoryProposal(id) => format!("M{id}"),
        }
    }
}

pub fn parse_request_decision(input: &str) -> Option<RequestDecision> {
    let trimmed = input.trim();
    let (approve, rest) = if let Some(r) = trimmed.strip_prefix("同意") {
        (true, r)
    } else if let Some(r) = trimmed.strip_prefix("拒绝") {
        (false, r)
    } else {
        return None;
    };

    let rest = rest.trim_start();
    // An `M`/`m` prefix selects the memory-proposal queue.
    let (is_memory, rest) = match rest.strip_prefix(['M', 'm']) {
        Some(r) => (true, r),
        None => (false, rest),
    };

    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    // The id must be followed by whitespace or end-of-input; otherwise this is
    // ordinary chat like "同意3楼" / "拒绝3个方案", not a request decision.
    let after = &rest[digits.len()..];
    if !after.is_empty() && !after.starts_with(char::is_whitespace) {
        return None;
    }
    let target = if is_memory {
        DecisionTarget::MemoryProposal(digits.parse().ok()?)
    } else {
        DecisionTarget::Request(digits.parse().ok()?)
    };
    let reason = after.trim();
    let reason = (!reason.is_empty()).then(|| reason.to_string());

    Some(RequestDecision {
        approve,
        target,
        reason,
    })
}

pub fn parse_command(input: &str) -> Option<(SlashCommand, &str)> {
    let trimmed = input.trim();
    let without_slash = trimmed.strip_prefix('/')?;
    let (name, args) = match without_slash.find(char::is_whitespace) {
        Some(pos) => (&without_slash[..pos], without_slash[pos..].trim_start()),
        None => (without_slash, ""),
    };
    SlashCommand::from_name(name).map(|cmd| (cmd, args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_request_decision_approve() {
        assert_eq!(
            parse_request_decision("同意 3"),
            Some(RequestDecision {
                approve: true,
                target: DecisionTarget::Request(3),
                reason: None
            })
        );
        assert_eq!(
            parse_request_decision("同意3"),
            Some(RequestDecision {
                approve: true,
                target: DecisionTarget::Request(3),
                reason: None
            })
        );
    }

    #[test]
    fn test_parse_request_decision_reject_with_reason() {
        assert_eq!(
            parse_request_decision("拒绝3 广告"),
            Some(RequestDecision {
                approve: false,
                target: DecisionTarget::Request(3),
                reason: Some("广告".into())
            })
        );
    }

    #[test]
    fn test_parse_request_decision_invalid() {
        assert_eq!(parse_request_decision("同意 abc"), None);
        assert_eq!(parse_request_decision("同意"), None);
        assert_eq!(parse_request_decision("hello"), None);
    }

    #[test]
    fn test_parse_request_decision_boundary() {
        // Digits glued to non-space text are ordinary chat, not a decision.
        assert_eq!(parse_request_decision("同意3楼"), None);
        assert_eq!(parse_request_decision("拒绝3个方案"), None);
        // A space before the reason keeps it a valid decision.
        assert_eq!(
            parse_request_decision("同意 3 广告"),
            Some(RequestDecision {
                approve: true,
                target: DecisionTarget::Request(3),
                reason: Some("广告".into())
            })
        );
        // Bare id with no trailing text is still valid.
        assert_eq!(
            parse_request_decision("同意3"),
            Some(RequestDecision {
                approve: true,
                target: DecisionTarget::Request(3),
                reason: None
            })
        );
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    /// The hole this closes: any member could reset a conversation the whole
    /// group shares.
    #[test]
    fn resetting_a_group_conversation_is_restricted() {
        assert!(!SlashCommand::New.available(false, true));
        assert!(SlashCommand::New.available(true, true));
        // A private chat is the caller's own, so no restriction there.
        assert!(SlashCommand::New.available(false, false));
    }

    #[test]
    fn everyday_commands_stay_open() {
        for cmd in [SlashCommand::Memory, SlashCommand::Status, SlashCommand::Help] {
            assert!(cmd.available(false, true), "{} should be open", cmd.name());
        }
    }

    /// Listing commands a caller will only be refused for is noise, and it hands
    /// non-admins a map of the admin surface.
    #[test]
    fn help_lists_only_what_the_caller_can_run() {
        let member = SlashCommand::help_text(false, true);
        assert!(!member.contains("/new"));
        assert!(member.contains("/memory"));

        let admin = SlashCommand::help_text(true, true);
        assert!(admin.contains("/new"));
    }

    #[test]
    fn memory_subcommands_parse() {
        assert_eq!(parse_memory_sub(""), Some(MemorySub::Overview));
        assert_eq!(parse_memory_sub("me"), Some(MemorySub::Me));
        assert_eq!(parse_memory_sub("forget 2"), Some(MemorySub::Forget(vec![2])));
        assert_eq!(parse_memory_sub("forget 2 3 5"), Some(MemorySub::Forget(vec![2, 3, 5])));
        assert_eq!(parse_memory_sub("forget 2-4"), Some(MemorySub::Forget(vec![2, 3, 4])));
        assert_eq!(parse_memory_sub("undo"), Some(MemorySub::Undo));
        assert_eq!(parse_memory_sub("optout"), Some(MemorySub::OptOut));
        assert_eq!(parse_memory_sub("group"), Some(MemorySub::Group));
        assert_eq!(
            parse_memory_sub("group forget 1"),
            Some(MemorySub::GroupForget(vec![1]))
        );
        assert_eq!(parse_memory_sub("user 123"), Some(MemorySub::User(123)));
    }

    /// Deleting all of someone's memories should not be one keystroke away.
    #[test]
    fn forget_all_needs_confirming() {
        assert_eq!(
            parse_memory_sub("forget all"),
            Some(MemorySub::ForgetAll { confirmed: false })
        );
        assert_eq!(
            parse_memory_sub("forget all yes"),
            Some(MemorySub::ForgetAll { confirmed: true })
        );
    }

    /// Only the exact `all yes` sequence confirms. A prefix-stripping check let
    /// `forget yes` and `forget allyes` through as confirmed, wiping everything
    /// with no prompt.
    #[test]
    fn confirmation_requires_the_exact_phrase() {
        for input in ["forget yes", "forget allyes", "forget all yes please", "forget yes all"] {
            assert_ne!(
                parse_memory_sub(input),
                Some(MemorySub::ForgetAll { confirmed: true }),
                "{input} must not count as confirmation"
            );
        }
    }

    /// A mistyped index must be reported, not rounded into some other row.
    #[test]
    fn malformed_indices_are_refused() {
        assert_eq!(parse_memory_sub("forget abc"), None);
        assert_eq!(parse_memory_sub("forget 0"), None);
        assert_eq!(parse_memory_sub("forget 5-2"), None);
        assert_eq!(parse_memory_sub("forget"), None);
    }

    /// These print things learned in other rooms and the operator's own notes.
    #[test]
    fn operator_views_are_marked_operator_only() {
        assert!(MemorySub::User(1).is_operator_only());
        assert!(MemorySub::Global.is_operator_only());
        assert!(MemorySub::Pending.is_operator_only());
        assert!(!MemorySub::Me.is_operator_only());
        assert!(!MemorySub::Group.is_operator_only());
    }

    /// The two queues number themselves independently, so a bare number must
    /// never reach the memory queue and an M-prefixed one must never reach the
    /// request queue. Without the prefix, approving a memory could accept a
    /// stranger's friend request that happened to share the id.
    #[test]
    fn decision_targets_are_unambiguous() {
        assert_eq!(
            parse_request_decision("同意 1").map(|d| d.target),
            Some(DecisionTarget::Request(1))
        );
        assert_eq!(
            parse_request_decision("同意 M1").map(|d| d.target),
            Some(DecisionTarget::MemoryProposal(1))
        );
        assert_eq!(
            parse_request_decision("拒绝 m7 太私密").map(|d| d.target),
            Some(DecisionTarget::MemoryProposal(7))
        );
        // Ordinary chat still must not be read as a decision.
        assert_eq!(parse_request_decision("同意M3楼"), None);
        assert_eq!(parse_request_decision("同意 M"), None);
    }

    #[test]
    fn decision_labels_round_trip() {
        for input in ["同意 42", "同意 M42"] {
            let d = parse_request_decision(input).unwrap();
            assert!(input.ends_with(&d.target.label()));
        }
    }

    #[test]
    fn memory_command_has_aliases() {
        assert_eq!(SlashCommand::from_name("mem"), Some(SlashCommand::Memory));
        assert_eq!(SlashCommand::from_name("记忆"), Some(SlashCommand::Memory));
    }
}
