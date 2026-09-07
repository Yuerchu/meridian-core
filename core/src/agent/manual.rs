//! The official `meridian-manual` skill.
//!
//! Written to disk at startup rather than shipped as a static file, because the
//! volatile half is generated from the same registries the app actually uses:
//! the tool list comes from `ToolRegistry`, the variable list from
//! `template::available_variables()`. Delete a tool or a variable and it leaves
//! the manual on the next launch — there is no hand-maintained copy to forget.

use std::path::Path;

use crate::provider::ToolDefinition;

pub const MANUAL_DIR: &str = "meridian-manual";

/// The stable half: concepts that change far more slowly than the registries.
const PROSE: &str = include_str!("manual.md");

const FRONTMATTER: &str = "---\nname: meridian-manual\ndescription: How Meridian itself works — assistants, projects, memories, tools and approval, skills, context compaction. Read this before answering questions about the app.\n---\n\n";

pub fn render(tool_defs: &[ToolDefinition]) -> String {
    let mut out = String::with_capacity(PROSE.len() + 2048);
    out.push_str(FRONTMATTER);
    // Identifies the file as generated, so a launch can tell it from a skill a
    // user happened to put under the same directory name.
    out.push_str(&format!(
        "<!-- {}; edits are overwritten on launch -->\n\n",
        super::skills::GENERATED_MARKER
    ));
    out.push_str(PROSE.trim_end());

    out.push_str("\n\n## Tools in this build\n\n");
    if tool_defs.is_empty() {
        out.push_str("No tools are currently registered.\n");
    } else {
        // Sorted so the file only changes when the tool set actually changes.
        let mut names: Vec<(&str, String)> = tool_defs
            .iter()
            .map(|d| (d.name.as_str(), first_sentence(&d.description)))
            .collect();
        names.sort_by(|a, b| a.0.cmp(b.0));
        for (name, summary) in names {
            out.push_str(&format!("- `{name}` — {summary}\n"));
        }
    }

    out.push_str("\n## System prompt variables\n\n");
    for v in crate::template::available_variables() {
        out.push_str(&format!("- `{{{{{}}}}}` — {}\n", v.name, v.description_en));
    }

    out
}

/// Tool descriptions are written for models and can run long; the manual only
/// needs enough to know the tool exists and roughly what it is for.
fn first_sentence(description: &str) -> String {
    let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.find(". ") {
        Some(i) if i < 160 => flat[..=i].trim().to_string(),
        _ if flat.chars().count() > 160 => {
            format!("{}…", flat.chars().take(159).collect::<String>().trim_end())
        }
        _ => flat,
    }
}

/// Overwrite the manual on every launch so it tracks the running build. Failure
/// is not fatal: a missing manual costs the model some knowledge, it does not
/// stop the app.
pub fn write_manual(skills_root: &Path, tool_defs: &[ToolDefinition]) -> std::io::Result<()> {
    let dir = skills_root.join(MANUAL_DIR);
    super::skills::preserve_user_directory(&dir)?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(super::skills::SKILL_FILE), render(tool_defs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({}),
        }
    }

    #[test]
    fn manual_parses_as_a_skill() {
        let rendered = render(&[def("read_file", "Read a file.")]);
        let (name, description) = crate::agent::skills::parse_frontmatter(&rendered).unwrap();
        assert_eq!(name, "meridian-manual");
        assert!(crate::agent::skills::is_valid_slug(&name));
        assert!(!description.is_empty());
    }

    #[test]
    fn tools_come_from_the_passed_definitions_only() {
        let rendered = render(&[def("read_file", "Read a file."), def("run_command", "Run it.")]);
        assert!(rendered.contains("`read_file`"));
        assert!(rendered.contains("`run_command`"));

        // The point of generating this: drop a tool and it leaves the manual.
        let without = render(&[def("read_file", "Read a file.")]);
        assert!(without.contains("`read_file`"));
        assert!(!without.contains("`run_command`"));
    }

    #[test]
    fn variables_come_from_the_template_registry() {
        let rendered = render(&[]);
        for v in crate::template::available_variables() {
            assert!(
                rendered.contains(&format!("{{{{{}}}}}", v.name)),
                "variable {} missing from manual",
                v.name
            );
        }
    }

    #[test]
    fn output_is_stable_across_renders_and_input_order() {
        // The manual is written on every launch; churn would show up as a
        // spurious file change, and the tool list feeds prompt-cached payloads.
        let a = render(&[def("b-tool", "B."), def("a-tool", "A.")]);
        let b = render(&[def("a-tool", "A."), def("b-tool", "B.")]);
        assert_eq!(a, b);
    }

    #[test]
    fn long_tool_descriptions_are_shortened() {
        let long = "First sentence here. ".to_string() + &"padding ".repeat(200);
        let rendered = render(&[def("verbose", &long)]);
        let line = rendered.lines().find(|l| l.contains("`verbose`")).unwrap();
        assert!(line.len() < 200, "line was {} chars", line.len());
        assert!(line.contains("First sentence here."));
    }

    #[test]
    fn empty_tool_list_still_renders() {
        let rendered = render(&[]);
        assert!(rendered.contains("No tools are currently registered."));
        assert!(crate::agent::skills::parse_frontmatter(&rendered).is_some());
    }

    #[test]
    fn write_manual_lands_where_scanning_finds_it() {
        let dir = tempfile::tempdir().unwrap();
        write_manual(dir.path(), &[def("read_file", "Read a file.")]).unwrap();

        let found = crate::agent::skills::scan_skills(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].dir_name, MANUAL_DIR);
        assert_eq!(found[0].llm_name, "meridian-manual");
    }

    #[test]
    fn rewriting_replaces_rather_than_appends() {
        let dir = tempfile::tempdir().unwrap();
        write_manual(dir.path(), &[def("read_file", "Read a file.")]).unwrap();
        write_manual(dir.path(), &[def("write_file", "Write a file.")]).unwrap();

        let body = std::fs::read_to_string(dir.path().join(MANUAL_DIR).join(crate::agent::skills::SKILL_FILE)).unwrap();
        assert!(body.contains("`write_file`"));
        assert!(!body.contains("`read_file`"));
    }
}
