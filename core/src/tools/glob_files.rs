use super::{Permission, Tool, ToolContext};
use async_trait::async_trait;

pub struct GlobFilesTool;

#[async_trait]
impl Tool for GlobFilesTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern (e.g. '**/*.rs', 'src/**/*.ts'). Respects .gitignore rules automatically. Returns file paths relative to the search directory."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files (e.g. '**/*.rs', 'src/**/*.tsx')"
                },
                "path": {
                    "type": "string",
                    "description": "Base directory to search in (defaults to working directory)"
                }
            },
            "required": ["pattern"]
        })
    }

    /// Ask rather than Always. This one followed symlinks out of its base
    /// directory as well, so as Always it was the widest unprompted read in the
    /// tool set. Globbing inside the project still does not prompt.
    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    fn reach(&self, args: &serde_json::Value, context: &ToolContext) -> super::reach::Reach {
        // Absent path means the project root, which is inside by definition.
        match args["path"].as_str() {
            Some(p) => super::reach::locate(context, p, false),
            None if context.working_directory.is_some() => super::reach::Reach::ReadsProject,
            None => super::reach::Reach::Outside,
        }
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or("missing 'pattern' argument")?
            .to_string();
        let base_str = args["path"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| context.working_dir_or_current().to_string_lossy().to_string());
        let base = match context.resolve_and_validate(&base_str)? {
            super::ResolvedTarget::Real(p) => p,
            super::ResolvedTarget::Saf { .. } => {
                return Err("glob is not supported in SAF-authorized directories; \
                     enable 'All files access' in Settings to search there"
                    .to_string());
            }
        };

        tokio::task::spawn_blocking(move || glob_search(&base, &pattern))
            .await
            .map_err(|e| format!("task failed: {e}"))?
    }
}

const MAX_RESULTS: usize = 1000;

fn glob_search(base: &std::path::Path, pattern: &str) -> Result<String, String> {
    use ignore::WalkBuilder;

    let full_pattern = base.join(pattern).to_string_lossy().to_string();
    let glob_matcher =
        glob::Pattern::new(&full_pattern).map_err(|e| format!("invalid glob pattern '{}': {}", pattern, e))?;

    let walker = WalkBuilder::new(base)
        .hidden(false)
        .follow_links(true)
        .git_ignore(true)
        .git_global(false)
        .build();

    let mut matches = Vec::new();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let path_str = path.to_string_lossy();
        if glob_matcher.matches(&path_str) || glob_matcher.matches(&path_str.replace('\\', "/")) {
            let relative = path
                .strip_prefix(base)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| path_str.to_string());
            matches.push(relative);

            if matches.len() >= MAX_RESULTS {
                break;
            }
        }
    }

    matches.sort();

    if matches.is_empty() {
        return Ok(format!("No files matching '{}' found.", pattern));
    }

    let total = matches.len();
    let mut result = matches.join("\n");
    if total >= MAX_RESULTS {
        result.push_str(&format!("\n\n(showing first {} matches)", MAX_RESULTS));
    }
    Ok(result)
}
