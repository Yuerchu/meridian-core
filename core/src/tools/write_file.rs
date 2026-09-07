use super::{Permission, Tool, ToolContext};
use async_trait::async_trait;

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file at the given path. Creates the file if it doesn't exist, overwrites if it does. Path can be relative to project root or absolute."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                },
                "description": super::description_property(),
            },
            "required": ["path", "content"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    fn reach(&self, args: &serde_json::Value, context: &ToolContext) -> super::reach::Reach {
        match args["path"].as_str() {
            Some(p) => super::reach::locate(context, p, true),
            None => super::reach::Reach::Outside,
        }
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let path_str = args["path"].as_str().ok_or("missing 'path' argument")?;
        let content = args["content"].as_str().ok_or("missing 'content' argument")?;

        // The write may run without a prompt, so it goes through the handle it
        // verified rather than resolving the name again.
        let target = context.open_write(path_str)?;
        let journal = context.journal_record("write_file", crate::journal::capture::Op::Write);
        super::backend::write_opened(target, content, journal).await?;
        Ok(format!("Successfully wrote {} bytes to {}", content.len(), path_str))
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
    async fn writes_a_new_file_and_creates_its_parents() {
        let dir = tempfile::tempdir().unwrap();
        let out = WriteFileTool
            .execute(
                serde_json::json!({"path": "nested/deep/note.txt", "content": "hello"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.contains("Successfully wrote"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("nested/deep/note.txt")).unwrap(),
            "hello"
        );
    }

    /// Overwriting has to leave the file at the new length, not the new content
    /// padded with whatever the old one was — the handle is reused, so the
    /// truncate is ours to get right.
    #[tokio::test]
    async fn overwriting_a_longer_file_leaves_no_tail() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "aaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();

        WriteFileTool
            .execute(serde_json::json!({"path": "a.txt", "content": "bb"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "bb");
    }

    #[tokio::test]
    async fn refuses_to_write_outside_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("important.txt");
        std::fs::write(&victim, "must survive").unwrap();

        let err = WriteFileTool
            .execute(
                serde_json::json!({"path": victim.to_string_lossy(), "content": "clobbered"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(err.contains("Access denied"), "got {err}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "must survive");
    }

    /// The escape a lexical check gets wrong: nothing exists to canonicalize,
    /// so the `..` survives into the comparison and the write lands outside.
    #[tokio::test]
    async fn refuses_a_traversal_through_a_directory_that_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let err = WriteFileTool
            .execute(
                serde_json::json!({
                    "path": "ghost/../../../escaped.txt",
                    "content": "x"
                }),
                &ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(err.contains("Access denied"), "got {err}");
    }

    fn ctx_with_journal(wd: &std::path::Path) -> (ToolContext, std::sync::Arc<crate::journal::capture::JournalCtx>) {
        let journal = crate::journal::capture::JournalCtx::new(
            crate::db::test_db(),
            tempfile::tempdir().unwrap().keep(),
            "conv".into(),
            "turn".into(),
            "desktop".into(),
            None,
            None,
            Some(wd.to_path_buf()),
            crate::journal::capture::JournalShared::new(),
        );
        let mut context = ctx(wd);
        context.journal = Some(journal.clone());
        (context, journal)
    }

    /// Journaling an existing non-UTF-8 file used to `read_to_string` and fail
    /// the write. The file must still be overwritten; the journal just skips.
    #[tokio::test]
    async fn overwriting_non_utf8_succeeds_even_with_a_journal() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, [0xff, 0xfe, 0xfd]).unwrap();
        let (context, journal) = ctx_with_journal(dir.path());

        WriteFileTool
            .execute(serde_json::json!({"path": "a.bin", "content": "ok"}), &context)
            .await
            .expect("write must succeed; journal failure is skip, not abort");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ok");

        let real = crate::tools::verified::resolve_root(&file).unwrap();
        let mut conn = journal.pool.get().unwrap();
        let row = crate::db::ops::journal::file_by_path(&mut conn, &crate::journal::norm_path(&real).unwrap()).unwrap();
        assert!(row.is_none(), "unreadable old bytes are not journalled");
    }

    #[tokio::test]
    async fn overwriting_an_oversized_file_does_not_fail_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        std::fs::write(&file, vec![b'x'; crate::journal::capture::MAX_SNAPSHOT_BYTES + 1]).unwrap();
        let (context, _) = ctx_with_journal(dir.path());

        WriteFileTool
            .execute(serde_json::json!({"path": "big.txt", "content": "tiny"}), &context)
            .await
            .expect("a large existing file must still be overwritable");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "tiny");
    }
}
