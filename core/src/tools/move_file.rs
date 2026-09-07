use super::{Permission, ResolvedTarget, Tool, ToolContext};
use async_trait::async_trait;

pub struct MoveFileTool;

#[async_trait]
impl Tool for MoveFileTool {
    fn name(&self) -> &str {
        "move_file"
    }

    fn description(&self) -> &str {
        "Move or rename a file or directory. The destination must not already exist. \
         Paths can be relative to project root or absolute."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "from": {
                    "type": "string",
                    "description": "Current path of the file or directory"
                },
                "to": {
                    "type": "string",
                    "description": "Destination path (including the new name)"
                },
                "description": super::description_property(),
            },
            "required": ["from", "to"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let from_str = args["from"].as_str().ok_or("missing 'from' argument")?;
        let to_str = args["to"].as_str().ok_or("missing 'to' argument")?;

        if context.is_access_root(from_str) {
            tracing::warn!(
                tool = "move_file",
                denied_path = %from_str,
                guard = "access_root",
                "refused a move of a protected path"
            );
            return Err(format!(
                "refusing to move '{from_str}': it is an authorized access root"
            ));
        }

        let from = context.resolve_and_validate(from_str)?;
        let to = context.resolve_and_validate(to_str)?;

        if let ResolvedTarget::Real(ref dst) = to
            && tokio::fs::symlink_metadata(dst).await.is_ok()
        {
            return Err(format!(
                "destination '{to_str}' already exists; delete it first or choose another name"
            ));
        }

        let journal = context.journal_record("move_file", crate::journal::capture::Op::RenameFrom);
        let observed = match (&journal, &from, &to) {
            // Files only: a directory move re-homes whole chains, which is a
            // per-file history rewrite the journal does not attempt — the
            // moved files surface as external on their next observation.
            (Some(j), ResolvedTarget::Real(src), ResolvedTarget::Real(dst))
                if tokio::fs::symlink_metadata(src)
                    .await
                    .map(|m| m.is_file())
                    .unwrap_or(false) =>
            {
                Some(j.observe_pair(src, dst).await)
            }
            _ => None,
        };

        super::backend::rename(&from, &to).await?;

        // Two rows, two chains: the old path records that its file left, the
        // new one records where the content came from — by the exact version,
        // so blame follows the move and not a later reincarnation.
        if let (Some(j), Some((src_obs, dst_obs))) = (&journal, &observed) {
            use crate::journal::capture::Op;
            let moved = src_obs.old.clone();
            let from_id = j.commit_as(Op::RenameFrom, src_obs, None, None).await;
            j.commit_as(Op::RenameTo, dst_obs, moved.as_deref(), from_id).await;
        }

        Ok(format!("Moved {from_str} to {to_str}"))
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

    /// A move writes two halves onto two chains, and the `rename_to` half
    /// names the exact `rename_from` version it came from — the pointer blame
    /// follows across the move.
    #[tokio::test]
    async fn a_move_links_its_two_halves_by_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "content").unwrap();

        let journal = crate::journal::capture::JournalCtx::new(
            crate::db::test_db(),
            tempfile::tempdir().unwrap().keep(),
            "conv".into(),
            "turn".into(),
            "desktop".into(),
            None,
            None,
            Some(dir.path().to_path_buf()),
            crate::journal::capture::JournalShared::new(),
        );
        let mut context = ctx(dir.path());
        context.journal = Some(journal.clone());

        // Canonicalised while it still exists: the tempdir spelling may be an
        // 8.3 short name, and the journal keys on the OS's own spelling.
        let real_a = crate::tools::verified::resolve_root(&dir.path().join("a.txt")).unwrap();

        MoveFileTool
            .execute(serde_json::json!({"from": "a.txt", "to": "b.txt"}), &context)
            .await
            .unwrap();

        let mut conn = journal.pool.get().unwrap();
        let real_b = crate::tools::verified::resolve_root(&dir.path().join("b.txt")).unwrap();
        let file_b = crate::db::ops::journal::file_by_path(&mut conn, &crate::journal::norm_path(&real_b).unwrap())
            .unwrap()
            .expect("destination chain");
        let chain_b = crate::db::ops::journal::chain(&mut conn, &file_b.id).unwrap();
        assert_eq!(chain_b.len(), 1);
        assert_eq!(chain_b[0].op, "rename_to");
        let from_id = chain_b[0]
            .moved_from_version_id
            .as_deref()
            .expect("linked to the source");

        // The pointer names the rename_from version on the old path's chain.
        let real_a_norm = crate::journal::norm_path(&real_a).unwrap();
        let file_a = crate::db::ops::journal::file_by_path(&mut conn, &real_a_norm)
            .unwrap()
            .expect("source chain");
        let chain_a = crate::db::ops::journal::chain(&mut conn, &file_a.id).unwrap();
        assert_eq!(chain_a.len(), 1);
        assert_eq!(chain_a[0].op, "rename_from");
        assert_eq!(chain_a[0].id, from_id);
        assert_eq!(chain_a[0].new_sha, None, "the old path records its file leaving");
    }

    #[tokio::test]
    async fn renames_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();

        let result = MoveFileTool
            .execute(serde_json::json!({"from": "a.txt", "to": "b.txt"}), &ctx(dir.path()))
            .await;
        assert!(result.is_ok());
        assert!(!dir.path().join("a.txt").exists());
        assert!(dir.path().join("b.txt").exists());
    }

    #[tokio::test]
    async fn refuses_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::fs::write(dir.path().join("b.txt"), "y").unwrap();

        let result = MoveFileTool
            .execute(serde_json::json!({"from": "a.txt", "to": "b.txt"}), &ctx(dir.path()))
            .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(dir.path().join("b.txt")).unwrap(), "y");
    }

    #[tokio::test]
    async fn refuses_outside_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();

        let result = MoveFileTool
            .execute(
                serde_json::json!({"from": "a.txt", "to": "../escaped.txt"}),
                &ctx(dir.path()),
            )
            .await;
        assert!(result.is_err());
        assert!(dir.path().join("a.txt").exists());
    }
}
