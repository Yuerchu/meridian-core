use crate::provider::ToolDefinition;
use crate::tools::command_shell::CommandShell;

/// Built-in agent baseline injected ahead of the user-configurable assistant
/// prompt, keyed on the tool set. The assistant prompt is a persona layer on
/// top of this; agent discipline must not depend on the user writing (or
/// keeping) it. Only lines for tools that are actually enabled are emitted, so
/// the model is never pointed at a tool it cannot call.
///
/// Everything here is static for a session: it changes when a tool is switched
/// on or a machine-wide setting changes, and never between two turns of the
/// same conversation, because this is the front of the cached prefix. Anything
/// that does change per turn goes after the message instead (`roster_block`),
/// or is frozen into the history (`memory_context`, `todo_context`).
///
/// `command_shell` is what `run_command` would run under, from the same
/// decision the executor makes (`CommandShell::select`); `None` where there is
/// no command executor at all, in which case `run_command` is not in the set
/// either.
pub fn base_prompt(tool_defs: &[ToolDefinition], command_shell: Option<&CommandShell>) -> Option<String> {
    let has = |name: &str| tool_defs.iter().any(|t| t.name == name);
    let has_editing = has("write_file") || has("edit_file") || has("apply_patch");
    let has_todos = has("update_todos");
    let shell = command_shell.filter(|_| has("run_command"));
    let has_stickers = has("send_sticker");

    if !has_editing && !has_todos && shell.is_none() && !has_stickers {
        return None;
    }

    let mut sections: Vec<String> = Vec::new();
    if has_editing {
        sections.push(file_editing_section(&has));
    }
    if has_todos {
        sections.push(checklist_section());
    }
    if let Some(shell) = shell {
        sections.push(format!("# Shell\n\n{}", shell.summary()));
    }
    if has_stickers {
        sections.push(sticker_section());
    }
    Some(sections.join("\n\n"))
}

/// The one thing the sticker tools' own descriptions do not say: what *not* to
/// do. The mechanics (pick an id with `list_stickers`, send it with
/// `send_sticker`) are on the definitions themselves; the invented inline tag
/// was the failure this line exists for.
fn sticker_section() -> String {
    [
        "# Stickers",
        "",
        "Stickers are sent with `send_sticker` after picking an id from `list_stickers`; never \
         write `[emoji:...]` tags or sticker names into your text.",
    ]
    .join("\n")
}

fn file_editing_section(has: &dyn Fn(&str) -> bool) -> String {
    let mut lines = vec![
        "# Agent guidelines".to_string(),
        String::new(),
        "You have tools to inspect and modify the user's files. Follow this discipline:".to_string(),
        String::new(),
    ];
    if has("read_file") {
        lines.push(
            "- Read a file before modifying it; base every edit on its actual current content, \
             never on assumptions."
                .to_string(),
        );
    }
    if has("edit_file") {
        lines.push(
            "- Prefer `edit_file` for targeted changes; `old_string` must match the file content \
             exactly, including whitespace and indentation."
                .to_string(),
        );
    }
    if has("apply_patch") {
        lines.push(
            "- `apply_patch` accepts a standard unified diff or a Codex-style patch \
             (`*** Begin Patch` envelope)."
                .to_string(),
        );
    }
    if has("write_file") {
        lines.push(
            "- Use `write_file` only to create new files or fully rewrite a file on purpose; it \
             overwrites the entire file."
                .to_string(),
        );
    }
    lines.push(
        "- If a tool call fails, read the error message and change your approach; never repeat \
         the same call with identical arguments."
            .to_string(),
    );
    lines.push("- Make the smallest change that fulfills the request; do not refactor unrelated code.".to_string());
    lines.join("\n")
}

/// Discipline for `update_todos`. The checklist is only worth anything if it
/// tracks reality as the work happens; a list written once and never touched
/// again is worse than none, because the interface keeps showing it.
fn checklist_section() -> String {
    [
        "# Task checklist",
        "",
        "You have `update_todos` for tracking multi-step work. Follow this discipline:",
        "",
        "- Open a checklist when a request needs several distinct steps. Skip it for anything \
         you can finish in one or two — tracking trivial work is noise.",
        "- Mark a step `in_progress` before you start it, not after. Exactly one step at a time.",
        "- Mark a step `completed` as soon as it is actually done. Do not save up several \
         completions and send them in one call.",
        "- A step that ran into an error stays `in_progress`; add a step for whatever has to be \
         resolved first rather than marking it done.",
        "- Send the entire list on every call — steps you leave out are deleted.",
        "- Do not recite the checklist back to the user; they can already see it. Say what \
         changed and carry on.",
        "- The checklist's current state is the most recent `<todo_list>` block in the \
         conversation; earlier ones are history.",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    #[test]
    fn no_file_editing_tools_means_no_prompt() {
        assert!(base_prompt(&[], None).is_none());
        assert!(base_prompt(&[def("read_file"), def("web_search"), def("qq_send_poke")], None).is_none());
    }

    #[test]
    fn any_editing_tool_enables_the_prompt() {
        let p = base_prompt(&[def("edit_file")], None).unwrap();
        assert!(p.contains("# Agent guidelines"));
        assert!(p.contains("edit_file"));
        // Lines for tools that are not enabled must not appear.
        assert!(!p.contains("apply_patch"));
        assert!(!p.contains("write_file"));
        assert!(!p.contains("Read a file before modifying"));
    }

    #[test]
    fn checklist_discipline_stands_on_its_own() {
        // An assistant with the checklist but no editing tools still needs the rules.
        let p = base_prompt(&[def("update_todos")], None).unwrap();
        assert!(p.contains("# Task checklist"));
        assert!(!p.contains("# Agent guidelines"));

        let both = base_prompt(&[def("edit_file"), def("update_todos")], None).unwrap();
        assert!(both.contains("# Agent guidelines"));
        assert!(both.contains("# Task checklist"));
    }

    #[test]
    fn editing_tools_alone_leave_out_the_checklist() {
        let p = base_prompt(&[def("edit_file")], None).unwrap();
        assert!(!p.contains("# Task checklist"));
    }

    /// The checklist is frozen into the history when it changes, so a long
    /// conversation shows the model several `<todo_list>` blocks. It has to be
    /// told which one is current, or it may act on a step it already finished.
    #[test]
    fn the_checklist_section_points_at_the_latest_block() {
        let p = base_prompt(&[def("update_todos")], None).unwrap();
        assert!(p.contains("most recent `<todo_list>` block"), "{p}");
    }

    /// The shell line needs both halves: a `run_command` to describe, and a
    /// decision about what it runs under. One without the other says nothing.
    #[test]
    fn run_command_gets_the_shell_line_only_with_a_shell() {
        let shell = CommandShell::Cmd;
        let p = base_prompt(&[def("run_command")], Some(&shell)).unwrap();
        assert!(p.contains("# Shell"), "{p}");
        assert!(p.contains("cmd.exe"), "{p}");

        assert!(base_prompt(&[def("run_command")], None).is_none());
        assert!(base_prompt(&[def("read_file")], Some(&shell)).is_none());
    }

    /// Gated on the tool that sends, not the one that lists: a session that can
    /// only look at the roster has nothing to be told not to do.
    #[test]
    fn send_sticker_adds_the_sticker_guidance() {
        let p = base_prompt(&[def("send_sticker")], None).unwrap();
        assert!(p.contains("# Stickers"), "{p}");
        assert!(p.contains("`[emoji:...]`"), "{p}");
        assert!(base_prompt(&[def("list_stickers")], None).is_none());
    }

    #[test]
    fn full_toolset_includes_all_guidelines() {
        let defs = [
            def("read_file"),
            def("write_file"),
            def("edit_file"),
            def("apply_patch"),
        ];
        let p = base_prompt(&defs, None).unwrap();
        assert!(p.contains("Read a file before modifying"));
        assert!(p.contains("old_string"));
        assert!(p.contains("*** Begin Patch"));
        assert!(p.contains("overwrites the entire file"));
        assert!(p.contains("identical arguments"));
    }
}
