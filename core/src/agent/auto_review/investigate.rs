//! The second look, for the calls the cheap one could not settle.
//!
//! `rm -rf $TARGET` cannot be judged from the command. Neither can a write to a
//! path that may or may not be a symlink into somewhere else, nor a script
//! whose body nobody has read. A classifier answering from the transcript alone
//! has to guess, and the direction it guesses in is what decides whether this
//! mode is unusable (too many false denials) or pointless (too many false
//! allows). So when the first pass is not sure, it gets to go and look.
//!
//! Deliberately **not** built on `engine::run_turn`. That loop writes rows: an
//! assistant message, a tool result for every call, an audit copy of each. A
//! review is a decision *about* a message rather than a message of its own, and
//! its trace belongs in that message's `auto_review` column. So this is its own
//! small loop, and being small is what makes the rest of its properties cheap:
//!
//! * Four read-only tools and no others. Not a filter over a wider set — the
//!   set is the whitelist, so a tool added to the registry tomorrow is not
//!   silently handed to the reviewer.
//! * Tools run directly, never through `Approvals`. Nothing here may ask for
//!   permission: the whole call stack is inside an approval already, and a
//!   reviewer that could trigger a review would recurse.
//! * A hard round and time budget, because this runs while a person is waiting
//!   for a tool call that has not started yet.

use std::collections::HashMap;

use tokio_util::sync::CancellationToken;

use super::assessment::{self, Read};
use crate::db::models::message::MessageUsage;
use crate::events::AutoReviewEvidence;
use crate::provider::{ChatMessage, ChatParams, ChatProvider, ToolDefinition};
use crate::services::Services;
use crate::tools::{FileAccess, ShellType, ToolContext};

/// Enough to resolve a path, read the file it points at and check one caller.
/// Codex allows more, but its reviewer is not standing between a user and a
/// tool call they are waiting on.
const MAX_ROUNDS: usize = 6;
const TIMEOUT_SECS: u64 = 60;
/// Per tool result. A reviewer deciding whether a deletion is authorised needs
/// to know what is in a directory, not to read every file in it.
const MAX_TOOL_OUTPUT_TOKENS: usize = 1_500;

/// What the escalating pass was given.
pub(super) struct Job<'a> {
    pub services: &'a Services,
    pub provider: &'a dyn ChatProvider,
    pub params: ChatParams,
    pub system_prompt: String,
    /// The same transcript the first pass saw. Shared verbatim so the provider
    /// can serve most of this request from its cache.
    pub scene: &'a str,
    pub working_directory: Option<&'a str>,
    /// The reviewed turn's own file boundary, passed through unchanged. See
    /// `context` for what happened when this was assumed instead.
    pub file_access: &'a FileAccess,
    pub turn_id: &'a str,
    pub conversation_id: &'a str,
}

pub(super) struct Findings {
    pub read: Read,
    pub usage: MessageUsage,
    /// The largest single round's prompt. Summed usage cannot answer "which
    /// price tier did this reach" — see `ReviewCost::peak_prompt_tokens`.
    pub peak_prompt: Option<i32>,
    /// What it actually looked at, filed beside the verdict. This is the part a
    /// user reads when they want to know whether a denial was informed.
    pub evidence: Vec<AutoReviewEvidence>,
}

/// What is appended to the shared prefix instead of the first pass's answer.
///
/// The first pass's own text is deliberately *not* carried over. It is the same
/// model's earlier guess, and showing it here would anchor this pass to a
/// conclusion it is supposed to re-reach independently — the same reason the
/// two hook gates do not share a conversation.
const ESCALATION: &str = "\
这个动作不能只凭对话记录判断。你现在有四个只读工具：read_file、search_files、glob、
list_directory。

先去核实再裁决：命令里的路径展开之后指向什么？目标存在吗，是文件还是目录，大不大？
通配符会匹配到哪些东西？要写入的位置是不是符号链接？要执行的脚本内容是什么？

查证之后按同样的格式给出裁决。查不到的东西就当作查不到 —— 缺失的信息应当让你更谨慎，
但不要凭想象补上它。";

fn definitions(services: &Services) -> Vec<ToolDefinition> {
    services
        .tools
        .definitions()
        .into_iter()
        .filter(|d| crate::tools::READ_ONLY_TOOLS.contains(&d.name.as_str()))
        .collect()
}

fn context(job: &Job<'_>, cancel: &CancellationToken) -> ToolContext {
    build_context(
        job.working_directory,
        job.file_access,
        job.conversation_id,
        job.turn_id,
        job.services.db.clone(),
        cancel,
    )
}

/// Split out from `context` so it can be reached without a `Services`: what it
/// decides is a security boundary, and a test that needed the whole application
/// standing up to check it is a test nobody writes.
fn build_context(
    working_directory: Option<&str>,
    file_access: &FileAccess,
    conversation_id: &str,
    turn_id: &str,
    db_pool: crate::db::DbPool,
    cancel: &CancellationToken,
) -> ToolContext {
    ToolContext {
        // The one field everything else depends on: without it `reach` calls
        // every path `Outside`, and the reviewer would be judging a repository
        // it has no way to read.
        working_directory: working_directory.map(str::to_string),
        shell: ShellType::default_for_platform(),
        // **The turn's own boundary, never a wider one.** Hardcoding
        // `Unrestricted` here was a hole rather than a simplification: a QQ turn
        // runs with `FileAccess::Roots(vec![])` (`onebot/agent.rs`) and no
        // working directory, and `Unrestricted` with no root resolves against
        // nothing — `verify_path(&resolved, None)` skips the prefix check
        // entirely, so the reviewer could read the whole machine. Its
        // `rationale` goes back into the chat, which makes that an exfiltration
        // path a group member can trigger by asking for anything that needs
        // approval. Android is the same story with its SAF whitelist.
        file_access: file_access.clone(),
        project_id: None,
        conversation_id: Some(conversation_id.to_string()),
        turn_id: Some(turn_id.to_string()),
        assistant_id: None,
        db_pool: Some(db_pool),
        #[cfg(not(target_os = "android"))]
        sandbox_policy: None,
        // None of the four read a credential. An empty map is the honest
        // description of that rather than an oversight.
        tool_secrets: HashMap::new(),
        cancel: cancel.clone(),
        journal: None,
    }
}

/// The budget, spent from the inside.
///
/// This used to be a `select!` against a `sleep`, which could not work: the
/// branch that fires cancels a token and then *awaits the same future*, and the
/// only thing that reads that token is a check at the top of the loop. What the
/// loop is actually blocked on is `chat_with_tools`, which has never heard of
/// it — so a wedged endpoint held the deadline open for as long as it liked,
/// with a person waiting on a tool call the whole time. Dropping the future
/// instead would have been safe here (everything below is read-only) but would
/// have thrown away the evidence already gathered.
///
/// So the deadline is enforced at each await that can block, and `run` is
/// simply `drive` — there is no outer race left to get wrong.
pub(super) async fn run(job: Job<'_>) -> Findings {
    let cancel = CancellationToken::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);
    let findings = drive(&job, &cancel, deadline).await;
    // Whatever else happened, the tools are told the run is over — one may have
    // spawned something that outlives the await it was reached through.
    cancel.cancel();
    findings
}

async fn drive(job: &Job<'_>, cancel: &CancellationToken, deadline: tokio::time::Instant) -> Findings {
    let tools = definitions(job.services);
    let tool_context = context(job, cancel);
    let mut messages = vec![
        super::system(&job.system_prompt),
        ChatMessage::user(job.scene),
        ChatMessage::user(ESCALATION),
    ];
    let mut usage = MessageUsage::default();
    let mut peak_prompt: Option<i32> = None;
    let mut evidence: Vec<AutoReviewEvidence> = Vec::new();

    for round in 0..MAX_ROUNDS {
        if cancel.is_cancelled() {
            break;
        }
        let asked = job
            .provider
            .chat_with_tools(messages.clone(), tools.clone(), job.params.clone());
        let answer = match tokio::time::timeout_at(deadline, asked).await {
            // The deadline, enforced where the blocking actually happens.
            // Whatever was gathered before it is kept: an unreadable verdict
            // with evidence attached still tells the user their reviewer is
            // timing out, and where it got to.
            Err(_) => {
                tracing::warn!(round, "the escalating review pass ran out of time");
                return Findings {
                    read: Read::Unreadable("深入审查超时"),
                    usage,
                    peak_prompt,
                    evidence,
                };
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, round, "the escalating review pass failed mid-flight");
                return Findings {
                    read: Read::Unreadable("深入审查中断"),
                    usage,
                    peak_prompt,
                    evidence,
                };
            }
            Ok(Ok(a)) => a,
        };
        let round = super::usage_of(answer.usage.as_ref());
        peak_prompt = super::peak(peak_prompt, round.input_tokens);
        usage = super::add(usage, round);

        if answer.tool_calls.is_empty() {
            return Findings {
                read: assessment::parse(&answer.text),
                usage,
                peak_prompt,
                evidence,
            };
        }

        messages.push(ChatMessage::assistant_with_tools(
            &answer.text,
            None,
            answer.tool_calls.clone(),
        ));
        for call in &answer.tool_calls {
            // Not a filter over what the registry has — the whitelist *is* the
            // set. A model naming `run_command` gets told no rather than
            // getting a shell.
            let allowed = crate::tools::READ_ONLY_TOOLS.contains(&call.name.as_str());
            let output = match (allowed, job.services.tools.get(&call.name)) {
                (true, Some(tool)) => {
                    match serde_json::from_str::<serde_json::Value>(&call.arguments) {
                        Err(error) => format!("Error: invalid tool arguments JSON: {error}"),
                        Ok(args) if !args.is_object() => "Error: tool arguments must be a JSON object".to_string(),
                        Ok(args) => {
                            // A `search_files` over a huge tree blocks as readily as a
                            // wedged endpoint does, and this is the other await a round
                            // can be lost in. The tool is told through `cancel` as well,
                            // for the ones that watch it.
                            match tokio::time::timeout_at(deadline, tool.execute(args, &tool_context)).await {
                                Err(_) => {
                                    cancel.cancel();
                                    "Error: 工具执行超出了审查的时间预算。".to_string()
                                }
                                Ok(Ok(out)) => {
                                    crate::agent::truncate::truncate_middle_with_token_budget(
                                        &out,
                                        MAX_TOOL_OUTPUT_TOKENS,
                                    )
                                    .0
                                }
                                Ok(Err(e)) => format!("Error: {e}"),
                            }
                        }
                    }
                }
                _ => "Error: 审查员只能使用 read_file、search_files、glob、list_directory。".to_string(),
            };
            evidence.push(AutoReviewEvidence {
                tool: call.name.clone(),
                arguments: call.arguments.clone(),
            });
            messages.push(ChatMessage::tool_result(&call.id, &output));
        }
    }

    // Out of rounds with tool calls still coming. Whatever it was doing, it was
    // not converging on an answer.
    Findings {
        read: Read::Unreadable("深入审查用尽了轮数仍未给出裁决"),
        usage,
        peak_prompt,
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn context_under(access: FileAccess, cwd: Option<&str>) -> ToolContext {
        build_context(cwd, &access, "c1", "t1", test_db(), &CancellationToken::new())
    }

    /// The escalating pass must not be able to read anything the turn could
    /// not. This was `FileAccess::Unrestricted` hardcoded, which with no
    /// working directory means no root to check against — `verify_path` skips
    /// the prefix test entirely and the whole disk is readable. On QQ that is
    /// reachable by any group member: ask for something that needs approval,
    /// and the reviewer's `rationale` comes back into the chat carrying
    /// whatever it read.
    #[test]
    fn the_reviewer_inherits_the_turns_boundary_rather_than_widening_it() {
        // What a QQ turn runs under (`onebot/agent.rs`): nothing is readable.
        let qq = context_under(FileAccess::Roots(vec![]), None);
        assert!(
            qq.resolve_and_validate("C:/Windows/System32/drivers/etc/hosts")
                .is_err()
        );
        assert!(qq.resolve_and_validate("/etc/passwd").is_err());
        assert!(qq.resolve_and_validate("secrets.txt").is_err());
    }

    /// And the desktop keeps working: its boundary is the project, so paths
    /// inside it resolve and paths outside do not.
    #[test]
    fn a_desktop_boundary_still_reaches_its_own_project() {
        let dir = std::env::temp_dir().join(format!("meridian-review-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("inside.txt"), "x").unwrap();
        let root = dir.to_string_lossy().to_string();

        let desktop = context_under(FileAccess::Unrestricted, Some(&root));
        assert!(desktop.resolve_and_validate("inside.txt").is_ok());
        assert!(desktop.resolve_and_validate("../outside.txt").is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
