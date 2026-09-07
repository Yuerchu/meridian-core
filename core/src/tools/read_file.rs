use super::{Permission, Tool, ToolContext};
use async_trait::async_trait;

pub struct ReadFileTool;

const MAX_OUTPUT_BYTES: usize = 256 * 1024;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Path can be relative to project root or absolute. Output truncated at 256KB for large files."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read"
                }
            },
            "required": ["path"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    fn reach(&self, args: &serde_json::Value, context: &ToolContext) -> super::reach::Reach {
        match args["path"].as_str() {
            Some(p) => super::reach::locate(context, p, false),
            None => super::reach::Reach::Outside,
        }
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let path_str = args["path"].as_str().ok_or("missing 'path' argument")?;
        // Opened rather than merely resolved: reads are the one thing that runs
        // without asking once the path is inside the project, so the check has
        // to apply to the handle that does the reading.
        let target = context.open_read(path_str)?;

        let capped = super::backend::read_capped_opened(target, MAX_OUTPUT_BYTES).await?;

        if capped.truncated {
            let size_info = capped
                .total_size
                .map(|s| format!(", total {} bytes", s))
                .unwrap_or_default();
            return Ok(format!(
                "{}...\n\n(file truncated at 256KB{})",
                capped.content, size_info
            ));
        }

        Ok(capped.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{FileAccess, ShellType};

    fn ctx(wd: &std::path::Path) -> ToolContext {
        ToolContext {
            working_directory: Some(wd.to_string_lossy().to_string()),
            shell: ShellType::Bash,
            file_access: FileAccess::Unrestricted,
            project_id: None,
            conversation_id: None,
            turn_id: None,
            assistant_id: None,
            db_pool: None,
            #[cfg(not(target_os = "android"))]
            sandbox_policy: None,
            tool_secrets: std::collections::HashMap::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
            journal: None,
        }
    }

    #[tokio::test]
    async fn reads_a_file_inside_the_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), "contents here").unwrap();

        let out = ReadFileTool
            .execute(serde_json::json!({"path": "note.txt"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(out, "contents here");
    }

    #[tokio::test]
    async fn reports_truncation_for_a_file_over_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(MAX_OUTPUT_BYTES + 1024);
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();

        let out = ReadFileTool
            .execute(serde_json::json!({"path": "big.txt"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert!(
            out.contains("file truncated"),
            "got tail: {}",
            &out[out.len().saturating_sub(80)..]
        );
    }

    #[tokio::test]
    async fn refuses_to_read_outside_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("id_rsa");
        std::fs::write(&secret, "PRIVATE KEY").unwrap();

        let err = ReadFileTool
            .execute(serde_json::json!({"path": secret.to_string_lossy()}), &ctx(dir.path()))
            .await
            .unwrap_err();
        assert!(err.contains("Access denied"), "got {err}");
    }

    /// Junctions need no privilege on Windows, so this is the redirection that
    /// is actually reachable. The refusal must come from what the handle turned
    /// out to be, since the path string alone looks entirely local.
    #[cfg(windows)]
    #[tokio::test]
    async fn refuses_a_read_redirected_out_by_a_junction() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("id_rsa"), "PRIVATE KEY").unwrap();

        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(dir.path().join("vendor"))
            .arg(outside.path())
            .status()
            .unwrap();
        assert!(status.success());

        let err = ReadFileTool
            .execute(serde_json::json!({"path": "vendor/id_rsa"}), &ctx(dir.path()))
            .await
            .unwrap_err();
        assert!(err.contains("Access denied"), "got {err}");
    }
}
