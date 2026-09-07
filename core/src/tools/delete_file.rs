use super::{Permission, ResolvedTarget, Tool, ToolContext};
use async_trait::async_trait;

pub struct DeleteFileTool;

#[async_trait]
impl Tool for DeleteFileTool {
    fn name(&self) -> &str {
        "delete_file"
    }

    fn description(&self) -> &str {
        "Delete a file or directory. This is permanent and cannot be undone. \
         Deleting a non-empty directory requires recursive: true. \
         Path can be relative to project root or absolute."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file or directory to delete"
                },
                "recursive": {
                    "type": "boolean",
                    "description": "If true, delete a directory and all its contents. Default: false."
                },
                "description": super::description_property(),
            },
            "required": ["path"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let path_str = args["path"].as_str().ok_or("missing 'path' argument")?;
        let recursive = args["recursive"].as_bool().unwrap_or(false);

        // A guard firing is a near miss worth recording: it says what the model
        // tried to remove, and it answers the user's "why won't it delete this".
        let refused = |guard: &'static str| {
            tracing::warn!(
                tool = "delete_file",
                denied_path = %path_str,
                guard,
                recursive,
                "refused a delete on a protected path"
            );
        };

        if context.is_access_root(path_str) {
            refused("access_root");
            return Err(format!(
                "refusing to delete '{path_str}': it is an authorized access root"
            ));
        }

        let target = context.resolve_and_validate(path_str)?;

        if let ResolvedTarget::Real(ref p) = target {
            if p.parent().is_none() {
                refused("filesystem_root");
                return Err(format!("refusing to delete '{path_str}': it is a filesystem root"));
            }
            if let Some(ref wd) = context.working_directory {
                let wd_canonical = std::fs::canonicalize(wd).unwrap_or_else(|_| std::path::PathBuf::from(wd));
                let target_canonical = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                if target_canonical == wd_canonical {
                    refused("project_directory");
                    return Err(format!("refusing to delete '{path_str}': it is the project directory"));
                }
            }
        }

        // What the journal can still observe, gathered before the bytes go.
        // A single file is one observation; a recursive directory delete
        // tombstones only the files the journal already tracks — an untracked
        // file's disappearance is the external-labelling path's to notice,
        // never something to guess at.
        let journal = context.journal_record("delete_file", crate::journal::capture::Op::Delete);
        let mut observations = Vec::new();
        if let (Some(j), ResolvedTarget::Real(p)) = (&journal, &target) {
            let is_dir = tokio::fs::symlink_metadata(p)
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if is_dir {
                if recursive {
                    for tracked in j.ctx.tracked_under(p).await {
                        observations.push(j.observe(&tracked).await);
                    }
                }
            } else {
                observations.push(j.observe(p).await);
            }
        }

        super::backend::delete(&target, recursive).await?;

        if let Some(j) = &journal {
            for obs in &observations {
                j.commit(obs, None).await;
            }
        }

        Ok(format!("Deleted {path_str}"))
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
    async fn deletes_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("junk.txt");
        std::fs::write(&file, "x").unwrap();

        let result = DeleteFileTool
            .execute(serde_json::json!({"path": "junk.txt"}), &ctx(dir.path()))
            .await;
        assert!(result.is_ok());
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn non_recursive_rejects_non_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "x").unwrap();

        let result = DeleteFileTool
            .execute(serde_json::json!({"path": "sub"}), &ctx(dir.path()))
            .await;
        assert!(result.is_err());
        assert!(sub.exists());

        let result = DeleteFileTool
            .execute(serde_json::json!({"path": "sub", "recursive": true}), &ctx(dir.path()))
            .await;
        assert!(result.is_ok());
        assert!(!sub.exists());
    }

    #[tokio::test]
    async fn refuses_project_directory_and_outside_paths() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());

        let result = DeleteFileTool
            .execute(
                serde_json::json!({"path": dir.path().to_string_lossy(), "recursive": true}),
                &c,
            )
            .await;
        assert!(result.is_err());
        assert!(dir.path().exists());

        let result = DeleteFileTool
            .execute(serde_json::json!({"path": "../outside.txt"}), &c)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn refuses_access_root() {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolContext {
            working_directory: None,
            shell: ShellType::Bash,
            file_access: FileAccess::Roots(vec![crate::tools::AccessRoot {
                virtual_prefix: "/sdcard".to_string(),
                kind: crate::tools::RootKind::RealPath(dir.path().to_path_buf()),
            }]),
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
        };
        let result = DeleteFileTool
            .execute(serde_json::json!({"path": "/sdcard", "recursive": true}), &c)
            .await;
        assert!(result.is_err());
        assert!(dir.path().exists());
    }
}
