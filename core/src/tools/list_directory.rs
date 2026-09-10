use super::{Permission, Tool, ToolContext};
use async_trait::async_trait;

pub struct ListDirectoryTool;

#[async_trait]
impl Tool for ListDirectoryTool {
    fn name(&self) -> &str {
        "list_directory"
    }

    fn description(&self) -> &str {
        "List the contents of a directory, showing file names, types, and sizes. Path can be relative to project root or absolute."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the directory to list"
                }
            },
            "required": ["path"]
        })
    }

    /// Ask rather than Always. This reads the filesystem, and an absolute path
    /// walks out of the project as easily as a relative one — as Always it was
    /// a way to enumerate the machine without a prompt while `read_file` was
    /// busy asking about files inside the project. `reach` is what keeps
    /// listing inside the project from prompting.
    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    fn reach(&self, args: &serde_json::Value, context: &ToolContext) -> super::reach::Reach {
        match args["path"].as_str() {
            Some(p) => super::reach::locate(context, p, false),
            None => super::reach::Reach::Outside,
        }
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let path_str = args["path"].as_str().ok_or("missing 'path' argument")?;

        let target = context.resolve_and_validate(path_str)?;
        let entries = super::backend::list_dir(&target).await?;

        let mut lines = Vec::new();
        for entry in entries {
            let kind = if entry.is_dir {
                "dir "
            } else if entry.is_symlink {
                "link"
            } else {
                "file"
            };

            let size = match entry.size {
                Some(bytes) => format_size(bytes),
                None => "-".to_string(),
            };

            lines.push(format!("{kind}  {size:>8}  {}", entry.name));
        }

        lines.sort();

        if lines.is_empty() {
            return Ok("(empty directory)".to_string());
        }

        Ok(lines.join("\n"))
    }
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}
