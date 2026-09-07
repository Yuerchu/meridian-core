use super::{Permission, Tool, ToolContext};
use async_trait::async_trait;

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Make a targeted edit to a file by replacing an exact string with a new string. Much safer than write_file for modifying existing code. The old_string must match exactly (including whitespace and indentation)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file to edit (relative to project root or absolute)"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact string to find and replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement string"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "If true, replace all occurrences. Default: false (replace first only)."
                },
                "description": super::description_property(),
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    fn reach(&self, args: &serde_json::Value, context: &ToolContext) -> super::reach::Reach {
        match args["file_path"].as_str() {
            Some(p) => super::reach::locate(context, p, true),
            None => super::reach::Reach::Outside,
        }
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let file_path = args["file_path"].as_str().ok_or("missing 'file_path' argument")?;
        let old_string = args["old_string"].as_str().ok_or("missing 'old_string' argument")?;
        let new_string = args["new_string"].as_str().ok_or("missing 'new_string' argument")?;
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);

        if old_string.is_empty() {
            return Err("old_string cannot be empty".to_string());
        }
        if old_string == new_string {
            return Err("old_string and new_string are identical".to_string());
        }

        // Read and write ride the same handle, so the text that matched
        // `old_string` is the text being replaced. `open_edit` also refuses
        // to create the file: a failed match must not leave an empty one.
        let target = context.open_edit(file_path)?;
        let journal = context.journal_record("edit_file", crate::journal::capture::Op::Edit);
        let mut replaced = 0usize;
        super::backend::edit_opened(
            target,
            |content| {
                let count = content.matches(old_string).count();
                if count == 0 {
                    return Err(format!(
                        "old_string not found in '{}'. File has {} bytes.",
                        file_path,
                        content.len()
                    ));
                }
                replaced = if replace_all { count } else { 1 };
                Ok(if replace_all {
                    content.replace(old_string, new_string)
                } else {
                    content.replacen(old_string, new_string, 1)
                })
            },
            journal,
        )
        .await?;
        Ok(format!("Replaced {} occurrence(s) in {}", replaced, file_path))
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

    /// The whole native capture path: a tool edit lands in the journal as one
    /// version whose old and new snapshots are the bytes that were actually
    /// read and written — through the same handle, under the same lock.
    #[tokio::test]
    async fn an_edit_is_journalled_with_the_observed_contents() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one two").unwrap();

        let blob_root = tempfile::tempdir().unwrap().keep();
        let journal = crate::journal::capture::JournalCtx::new(
            crate::db::test_db(),
            blob_root,
            "conv".into(),
            "turn".into(),
            "desktop".into(),
            Some("test-model".into()),
            None,
            Some(dir.path().to_path_buf()),
            crate::journal::capture::JournalShared::new(),
        );
        let mut context = ctx(dir.path());
        context.journal = Some(journal.clone());

        EditFileTool
            .execute(
                serde_json::json!({"file_path": "a.txt", "old_string": "two", "new_string": "2"}),
                &context,
            )
            .await
            .unwrap();

        let real = crate::tools::verified::resolve_root(&file).unwrap();
        let mut conn = journal.pool.get().unwrap();
        let row = crate::db::ops::journal::file_by_path(&mut conn, &crate::journal::norm_path(&real).unwrap())
            .unwrap()
            .expect("the edit should be journalled");
        let chain = crate::db::ops::journal::chain(&mut conn, &row.id).unwrap();
        assert_eq!(chain.len(), 1);
        let v = &chain[0];
        assert_eq!((v.op.as_str(), v.source.as_str()), ("edit", "native"));
        assert_eq!(v.conversation_id.as_deref(), Some("conv"));
        assert_eq!(v.tool_name.as_deref(), Some("edit_file"));
        assert_eq!(
            crate::journal::blobs::load(&journal.blob_root, v.observed_old_sha.as_deref().unwrap()).unwrap(),
            "one two"
        );
        assert_eq!(
            crate::journal::blobs::load(&journal.blob_root, v.new_sha.as_deref().unwrap()).unwrap(),
            "one 2"
        );
    }

    /// And a journal that cannot write must not fail the edit: the file still
    /// changes, the chain just misses an entry — under-attribution, on purpose.
    #[tokio::test]
    async fn a_broken_journal_does_not_fail_the_edit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one two").unwrap();

        // A blob root that is a *file* makes every snapshot store fail.
        let bad_root = dir.path().join("not-a-dir");
        std::fs::write(&bad_root, "x").unwrap();
        let journal = crate::journal::capture::JournalCtx::new(
            crate::db::test_db(),
            bad_root,
            "conv".into(),
            "turn".into(),
            "desktop".into(),
            None,
            None,
            None,
            crate::journal::capture::JournalShared::new(),
        );
        let mut context = ctx(dir.path());
        context.journal = Some(journal);

        EditFileTool
            .execute(
                serde_json::json!({"file_path": "a.txt", "old_string": "two", "new_string": "2"}),
                &context,
            )
            .await
            .expect("the edit must succeed regardless of the journal");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one 2");
    }

    #[tokio::test]
    async fn replaces_a_single_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one two one").unwrap();

        EditFileTool
            .execute(
                serde_json::json!({"file_path": "a.txt", "old_string": "one", "new_string": "1"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "1 two one");
    }

    #[tokio::test]
    async fn replace_all_shortens_the_file_without_leaving_a_tail() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "aaaa aaaa aaaa").unwrap();

        EditFileTool
            .execute(
                serde_json::json!({
                    "file_path": "a.txt", "old_string": "aaaa", "new_string": "b",
                    "replace_all": true
                }),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "b b b");
    }

    /// A failed match must change nothing at all — in particular it must not
    /// have created the file on the way to failing.
    #[tokio::test]
    async fn a_missing_file_is_not_created_by_a_failed_edit() {
        let dir = tempfile::tempdir().unwrap();
        let err = EditFileTool
            .execute(
                serde_json::json!({"file_path": "nope.txt", "old_string": "x", "new_string": "y"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(!dir.path().join("nope.txt").exists(), "edit created the file: {err}");
    }

    #[tokio::test]
    async fn an_unmatched_old_string_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "original").unwrap();

        let err = EditFileTool
            .execute(
                serde_json::json!({"file_path": "a.txt", "old_string": "absent", "new_string": "z"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(err.contains("not found"), "got {err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
    }

    #[tokio::test]
    async fn refuses_to_edit_outside_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("important.txt");
        std::fs::write(&victim, "must survive").unwrap();

        let err = EditFileTool
            .execute(
                serde_json::json!({
                    "file_path": victim.to_string_lossy(),
                    "old_string": "must", "new_string": "did not"
                }),
                &ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(err.contains("Access denied"), "got {err}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "must survive");
    }
}
