use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Permission, Tool, ToolContext};
use crate::agent::skills;

/// Second and third stages of skill disclosure. The first stage — the list of
/// available skills and what each one is for — rides in this tool's description
/// and the `skill_name` enum, both filled in per request while tool definitions
/// are assembled. Bodies arrive only when the model asks, and land in the
/// conversation history where compaction can reclaim them later.
pub struct LoadSkillTool {
    root: PathBuf,
}

impl LoadSkillTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        "load_skill"
    }

    fn description(&self) -> &str {
        "Load the full instructions for one of the available skills. Call this before acting on a \
         task a skill covers. Without 'path' you get the skill's instructions plus a list of its \
         bundled reference files; pass one of those paths to read that file."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "Name of the skill to load"
                },
                "path": {
                    "type": "string",
                    "description": "Optional bundled resource path, exactly as listed by a previous \
                                    call without 'path' (e.g. 'references/forms.md')"
                }
            },
            "required": ["skill_name"]
        })
    }

    /// Reading a skill has no side effects and interrupting the user for it
    /// would make progressive disclosure more expensive than just injecting
    /// everything up front.
    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let skill_name = args
            .get("skill_name")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: skill_name")?
            .trim()
            .to_string();
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let pool = context
            .db_pool
            .clone()
            .ok_or("Skills are unavailable without a database")?;
        let project_id = context.project_id.clone();
        let assistant_id = context.assistant_id.clone();

        let wanted = skill_name.clone();
        let available = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            crate::db::ops::skill_binding::resolve_available(&mut conn, project_id.as_deref(), assistant_id.as_deref())
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;

        let matches: Vec<_> = available.iter().filter(|s| s.llm_name == wanted).collect();
        let skill = match matches.len() {
            1 => matches[0],
            0 => {
                let mut names: Vec<&str> = available.iter().map(|s| s.llm_name.as_str()).collect();
                names.sort_unstable();
                names.dedup();
                return Err(if names.is_empty() {
                    format!("No skill named '{wanted}' is available; none are bound to this assistant.")
                } else {
                    format!(
                        "No skill named '{wanted}' is available. Available skills: {}",
                        names.join(", ")
                    )
                });
            }
            // Two directories claiming one name makes the name meaningless, so
            // refuse rather than silently picking one.
            _ => {
                let dirs: Vec<&str> = matches.iter().map(|s| s.dir_name.as_str()).collect();
                return Err(format!(
                    "Skill name '{wanted}' is ambiguous — it is claimed by {} directories ({}). \
                     Ask the user to rename or unbind one of them.",
                    matches.len(),
                    dirs.join(", ")
                ));
            }
        };

        if let Some(rel) = path {
            let body = skills::read_skill_resource(&self.root, &skill.dir_name, &rel)?;
            return Ok(format!("# {} — {}\n\n{}", skill.llm_name, rel, body));
        }

        let body = skills::read_skill_body(&self.root, &skill.dir_name)
            .ok_or_else(|| format!("Skill '{wanted}' has no readable SKILL.md on disk"))?;
        let resources = skills::list_skill_resources(&self.root, &skill.dir_name);

        let mut out = format!("# Skill: {}\n\n{}", skill.llm_name, body.trim());
        if !resources.is_empty() {
            out.push_str("\n\n## Bundled resources\n\nRead one with load_skill(skill_name, path):\n");
            for r in resources {
                out.push_str(&format!("- {r}\n"));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::skill::SkillInsert;
    use crate::db::models::skill_binding::SkillLayer;
    use crate::db::{DbPool, test_db};
    use crate::tools::{FileAccess, ShellType};
    use std::path::Path;

    fn write_skill(root: &Path, dir: &str, name: &str, body: &str) {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join(skills::SKILL_FILE),
            format!("---\nname: {name}\ndescription: Test skill\n---\n{body}"),
        )
        .unwrap();
    }

    fn index_and_bind(pool: &DbPool, dir: &str, name: &str) {
        let mut conn = pool.get().unwrap();
        crate::db::ops::skill::upsert_skill(
            &mut conn,
            &SkillInsert {
                dir_name: dir,
                llm_name: name,
                llm_description: "Test skill",
                display_name: dir,
                display_description: None,
                source: "user",
                is_enabled: 1,
                is_builtin: 0,
                mtime_hash: None,
                created_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();
        crate::db::ops::skill_binding::bind(&mut conn, SkillLayer::Global, None, dir).unwrap();
    }

    fn ctx(pool: DbPool) -> ToolContext {
        ToolContext {
            working_directory: None,
            shell: ShellType::Bash,
            file_access: FileAccess::Unrestricted,
            project_id: None,
            conversation_id: None,
            turn_id: None,
            assistant_id: None,
            db_pool: Some(pool),
            #[cfg(not(target_os = "android"))]
            sandbox_policy: None,
            tool_secrets: std::collections::HashMap::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
            journal: None,
        }
    }

    #[tokio::test]
    async fn loads_body_and_lists_resources() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "pdf-tools", "pdf-tools", "Use pdftk for forms.");
        let refs = dir.path().join("pdf-tools").join(skills::REFERENCES_DIR);
        std::fs::create_dir_all(&refs).unwrap();
        std::fs::write(refs.join("forms.md"), "Form details").unwrap();

        let pool = test_db();
        index_and_bind(&pool, "pdf-tools", "pdf-tools");

        let out = LoadSkillTool::new(dir.path().to_path_buf())
            .execute(json!({"skill_name": "pdf-tools"}), &ctx(pool))
            .await
            .unwrap();

        assert!(out.contains("Use pdftk for forms."));
        assert!(out.contains("references/forms.md"));
        assert!(!out.contains("description: Test skill"), "frontmatter must be stripped");
    }

    #[tokio::test]
    async fn loads_a_single_resource_by_path() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "s", "s", "Overview");
        let refs = dir.path().join("s").join(skills::REFERENCES_DIR);
        std::fs::create_dir_all(&refs).unwrap();
        std::fs::write(refs.join("deep.md"), "Deep content").unwrap();

        let pool = test_db();
        index_and_bind(&pool, "s", "s");

        let out = LoadSkillTool::new(dir.path().to_path_buf())
            .execute(json!({"skill_name": "s", "path": "references/deep.md"}), &ctx(pool))
            .await
            .unwrap();

        assert!(out.contains("Deep content"));
        assert!(!out.contains("Overview"));
    }

    #[tokio::test]
    async fn unbound_skill_is_refused_even_though_it_exists_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "bound", "bound", "Visible");
        write_skill(dir.path(), "unbound", "unbound", "Hidden");

        let pool = test_db();
        index_and_bind(&pool, "bound", "bound");
        // Indexed but never bound.
        let mut conn = pool.get().unwrap();
        crate::db::ops::skill::upsert_skill(
            &mut conn,
            &SkillInsert {
                dir_name: "unbound",
                llm_name: "unbound",
                llm_description: "d",
                display_name: "unbound",
                display_description: None,
                source: "user",
                is_enabled: 1,
                is_builtin: 0,
                mtime_hash: None,
                created_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();
        drop(conn);

        let err = LoadSkillTool::new(dir.path().to_path_buf())
            .execute(json!({"skill_name": "unbound"}), &ctx(pool))
            .await
            .unwrap_err();

        assert!(err.contains("No skill named 'unbound'"));
        assert!(err.contains("bound"), "error should list what is available");
    }

    #[tokio::test]
    async fn ambiguous_name_is_refused_rather_than_guessed() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "mine-pdf", "pdf", "Mine");
        write_skill(dir.path(), "theirs-pdf", "pdf", "Theirs");

        let pool = test_db();
        index_and_bind(&pool, "mine-pdf", "pdf");
        index_and_bind(&pool, "theirs-pdf", "pdf");

        let err = LoadSkillTool::new(dir.path().to_path_buf())
            .execute(json!({"skill_name": "pdf"}), &ctx(pool))
            .await
            .unwrap_err();

        assert!(err.contains("ambiguous"));
        assert!(err.contains("mine-pdf") && err.contains("theirs-pdf"));
    }

    #[tokio::test]
    async fn resource_path_cannot_escape_the_skill_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "s", "s", "Body");
        std::fs::write(dir.path().join("secret.txt"), "TOP SECRET").unwrap();

        let pool = test_db();
        index_and_bind(&pool, "s", "s");

        let err = LoadSkillTool::new(dir.path().to_path_buf())
            .execute(json!({"skill_name": "s", "path": "../secret.txt"}), &ctx(pool))
            .await
            .unwrap_err();

        assert!(!err.contains("TOP SECRET"));
        assert!(err.contains("invalid resource path"));
    }
}
