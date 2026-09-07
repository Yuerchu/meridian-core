use crate::provider::ToolDefinition;

/// Built-in agent baseline injected ahead of the user-configurable assistant
/// prompt whenever file-editing tools are enabled for the session. The
/// assistant prompt is a persona layer on top of this; agent discipline must
/// not depend on the user writing (or keeping) it. Only lines for tools that
/// are actually enabled are emitted, so the model is never pointed at a tool
/// it cannot call.
pub fn base_prompt(tool_defs: &[ToolDefinition]) -> Option<String> {
    let has = |name: &str| tool_defs.iter().any(|t| t.name == name);
    let has_editing = has("write_file") || has("edit_file") || has("apply_patch");
    let has_todos = has("update_todos");

    if !has_editing && !has_todos {
        return None;
    }

    let mut sections: Vec<String> = Vec::new();
    if has_editing {
        sections.push(file_editing_section(&has));
    }
    if has_todos {
        sections.push(checklist_section());
    }
    Some(sections.join("\n\n"))
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
        assert!(base_prompt(&[]).is_none());
        assert!(base_prompt(&[def("read_file"), def("web_search"), def("qq_send_poke")]).is_none());
    }

    #[test]
    fn any_editing_tool_enables_the_prompt() {
        let p = base_prompt(&[def("edit_file")]).unwrap();
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
        let p = base_prompt(&[def("update_todos")]).unwrap();
        assert!(p.contains("# Task checklist"));
        assert!(!p.contains("# Agent guidelines"));

        let both = base_prompt(&[def("edit_file"), def("update_todos")]).unwrap();
        assert!(both.contains("# Agent guidelines"));
        assert!(both.contains("# Task checklist"));
    }

    #[test]
    fn editing_tools_alone_leave_out_the_checklist() {
        let p = base_prompt(&[def("edit_file")]).unwrap();
        assert!(!p.contains("# Task checklist"));
    }

    #[test]
    fn full_toolset_includes_all_guidelines() {
        let defs = [
            def("read_file"),
            def("write_file"),
            def("edit_file"),
            def("apply_patch"),
        ];
        let p = base_prompt(&defs).unwrap();
        assert!(p.contains("Read a file before modifying"));
        assert!(p.contains("old_string"));
        assert!(p.contains("*** Begin Patch"));
        assert!(p.contains("overwrites the entire file"));
        assert!(p.contains("identical arguments"));
    }
}
