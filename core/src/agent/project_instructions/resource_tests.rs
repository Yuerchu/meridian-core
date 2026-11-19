use super::*;

fn rules_root(root: &Path) -> PathBuf {
    let path = root.join(".claude/rules");
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn file_link(source: &Path, dest: &Path) {
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(source, dest).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, dest).unwrap();
}

fn dir_link(source: &Path, dest: &Path) {
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(source, dest).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, dest).unwrap();
}

#[test]
fn escaping_top_local_fallback_and_rule_files_are_not_read() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.md");
    std::fs::write(&secret, "outside sentinel").unwrap();
    let rules = rules_root(root.path());
    for path in [
        root.path().join("CLAUDE.md"),
        root.path().join("CLAUDE.local.md"),
        root.path().join(".claude/CLAUDE.md"),
        rules.join("escape.md"),
    ] {
        file_link(&secret, &path);
    }
    assert!(load_sync(root.path(), 32_000).is_none());
}

#[test]
fn directory_links_including_internal_loops_are_not_walked() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.md"), "outside sentinel").unwrap();
    let rules = rules_root(root.path());
    std::fs::write(rules.join("good.md"), "safe rule").unwrap();
    dir_link(outside.path(), &rules.join("escape"));
    dir_link(&rules, &rules.join("loop"));
    let (_, _, files) = discover_files(root.path());
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].content, "safe rule");
}

#[test]
fn linked_claude_and_rules_directories_are_rejected() {
    for linked_claude in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("CLAUDE.md"), "outside sentinel").unwrap();
        std::fs::write(outside.path().join("rule.md"), "outside sentinel").unwrap();
        let link = if linked_claude {
            root.path().join(".claude")
        } else {
            std::fs::create_dir(root.path().join(".claude")).unwrap();
            root.path().join(".claude/rules")
        };
        dir_link(outside.path(), &link);
        assert!(load_sync(root.path(), 32_000).is_none());
    }
}

#[cfg(windows)]
#[test]
fn windows_junction_is_not_walked() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.md"), "outside sentinel").unwrap();
    let rules = rules_root(root.path());
    let junction = rules.join("junction");
    let result = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(junction.to_string_lossy().replace('/', "\\"))
        .arg(outside.path().to_string_lossy().replace('/', "\\"))
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(discover_files(root.path()).2.is_empty());
    std::fs::remove_dir(junction).unwrap();
}

#[test]
fn rule_limit_is_global_and_selection_is_stable() {
    let root = tempfile::tempdir().unwrap();
    let rules = rules_root(root.path());
    for sub in ["b", "a"] {
        std::fs::create_dir(rules.join(sub)).unwrap();
        for i in (0..40).rev() {
            std::fs::write(rules.join(sub).join(format!("{i:02}.md")), "rule").unwrap();
        }
    }
    let (_, _, first) = discover_files(root.path());
    let (_, _, second) = discover_files(root.path());
    assert_eq!(first.len(), MAX_RULES_FILES);
    let paths: Vec<_> = first.iter().map(|f| &f.path).collect();
    assert_eq!(paths, second.iter().map(|f| &f.path).collect::<Vec<_>>());
    assert!(paths.windows(2).all(|p| p[0] < p[1]));
    assert!(paths[49].ends_with("b/09.md"));
}

#[test]
fn entry_budget_counts_non_markdown_entries_and_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let rules = rules_root(root.path());
    std::fs::write(root.path().join("CLAUDE.md"), "top survives").unwrap();
    std::fs::write(rules.join("a.md"), "must not depend on enumeration order").unwrap();
    for i in 0..MAX_RULE_ENTRIES {
        std::fs::write(rules.join(format!("{i:04}.txt")), "").unwrap();
    }
    assert!(discover_files(root.path()).2.is_empty());
    let result = load_sync(root.path(), 32_000).unwrap();
    assert!(result.contains("top survives"));
    assert!(!result.contains("enumeration order"));
}

#[test]
fn depth_budget_stops_before_level_seventeen() {
    let root = tempfile::tempdir().unwrap();
    let mut path = rules_root(root.path());
    for depth in 0..=MAX_RULE_DEPTH + 1 {
        std::fs::write(path.join(format!("level-{depth}.md")), "rule").unwrap();
        path = path.join("d");
        std::fs::create_dir(&path).unwrap();
    }
    let (_, _, files) = discover_files(root.path());
    assert_eq!(files.len(), MAX_RULE_DEPTH + 1);
    assert!(!files.iter().any(|f| f.path.ends_with("level-17.md")));
}

#[test]
fn formatting_uses_opened_snapshot_not_a_swapped_path() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = root.path().join("CLAUDE.md");
    let secret = outside.path().join("secret.md");
    std::fs::write(&path, "safe snapshot").unwrap();
    std::fs::write(&secret, "outside sentinel").unwrap();
    let (claude, local, rules) = discover_files(root.path());
    std::fs::remove_file(&path).unwrap();
    file_link(&secret, &path);
    let result = format_instructions(root.path(), claude.as_ref(), local.as_ref(), &rules, 32_000);
    assert!(result.contains("safe snapshot"));
    assert!(!result.contains("outside sentinel"));
    assert!(load_sync(root.path(), 32_000).is_none());
}

#[test]
fn cache_accounts_for_token_budget_and_current_content() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("CLAUDE.md");
    std::fs::write(&path, "word ".repeat(5000)).unwrap();
    let large = load_sync(root.path(), 32_000).unwrap();
    let small = load_sync(root.path(), 100).unwrap();
    assert!(small.len() < large.len());
    assert!(load_sync(root.path(), 0).is_none());
    std::fs::write(&path, "fresh content").unwrap();
    assert!(load_sync(root.path(), 32_000).unwrap().contains("fresh content"));
}

#[test]
fn oversized_and_non_utf8_instructions_are_skipped() {
    let root = tempfile::tempdir().unwrap();
    let rules = rules_root(root.path());
    std::fs::File::create(root.path().join("CLAUDE.md"))
        .unwrap()
        .set_len(MAX_FILE_SIZE + 1)
        .unwrap();
    std::fs::write(root.path().join("CLAUDE.local.md"), [255]).unwrap();
    std::fs::File::create(rules.join("big.md"))
        .unwrap()
        .set_len(MAX_FILE_SIZE + 1)
        .unwrap();
    assert!(load_sync(root.path(), 32_000).is_none());
}
