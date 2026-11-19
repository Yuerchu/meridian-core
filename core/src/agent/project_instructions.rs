#[cfg(not(target_os = "android"))]
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "android"))]
use std::sync::Mutex;
#[cfg(not(target_os = "android"))]
use std::sync::OnceLock;

use super::context::estimate_tokens;

pub(crate) const MAX_FILE_SIZE: u64 = 256 * 1024;
#[cfg(not(target_os = "android"))]
const MAX_RULES_FILES: usize = 50;

#[cfg(not(target_os = "android"))]
struct CachedInstructions {
    content: String,
}

#[cfg(not(target_os = "android"))]
static CACHE: OnceLock<Mutex<HashMap<PathBuf, (u64, CachedInstructions)>>> = OnceLock::new();

#[cfg(not(target_os = "android"))]
fn cache() -> &'static Mutex<HashMap<PathBuf, (u64, CachedInstructions)>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn instruction_budget(context_limit: usize) -> usize {
    match context_limit {
        0..16_000 => 0,
        16_000..64_000 => 4_000,
        64_000..128_000 => 16_000,
        _ => 32_000,
    }
}

#[cfg(not(target_os = "android"))]
pub async fn load_project_instructions(project_path: Option<&str>, max_tokens: usize) -> Option<String> {
    let root = project_path?;
    let root_path = PathBuf::from(root);
    let max_tokens = if max_tokens == 0 { return None } else { max_tokens };

    let root_owned = root_path.clone();
    tokio::task::spawn_blocking(move || load_sync(&root_owned, max_tokens))
        .await
        .ok()
        .flatten()
}

#[cfg(target_os = "android")]
pub async fn load_project_instructions(_project_path: Option<&str>, _max_tokens: usize) -> Option<String> {
    None
}

#[cfg(not(target_os = "android"))]
#[derive(Hash)]
struct InstructionFile {
    path: PathBuf,
    content: String,
}

#[cfg(not(target_os = "android"))]
fn load_sync(root: &Path, max_tokens: usize) -> Option<String> {
    if max_tokens == 0 {
        return None;
    }
    let (claude_md, claude_local_md, rule_files) = discover_files(root);
    let mut hasher = DefaultHasher::new();
    max_tokens.hash(&mut hasher);
    claude_md.hash(&mut hasher);
    claude_local_md.hash(&mut hasher);
    rule_files.hash(&mut hasher);
    let snapshot_hash = hasher.finish();
    {
        let guard = cache().lock().ok()?;
        if let Some((cached_hash, cached)) = guard.get(root)
            && *cached_hash == snapshot_hash
        {
            return Some(cached.content.clone());
        }
    }
    let content = format_instructions(
        root,
        claude_md.as_ref(),
        claude_local_md.as_ref(),
        &rule_files,
        max_tokens,
    );
    if content.is_empty() {
        return None;
    }
    let result = format!(
        "

<project_instructions>
{}
</project_instructions>",
        content.trim()
    );
    if let Ok(mut guard) = cache().lock() {
        guard.insert(
            root.to_path_buf(),
            (
                snapshot_hash,
                CachedInstructions {
                    content: result.clone(),
                },
            ),
        );
    }
    Some(result)
}

#[cfg(not(target_os = "android"))]
const MAX_RULE_DEPTH: usize = 16;
#[cfg(not(target_os = "android"))]
const MAX_RULE_ENTRIES: usize = 2048;

#[cfg(not(target_os = "android"))]
fn is_link(meta: &cap_std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt;
        // Includes junctions and other reparse points, not just symlinks.
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

#[cfg(not(target_os = "android"))]
fn open_directory(parent: &cap_std::fs::Dir, name: &Path) -> Option<cap_std::fs::Dir> {
    use cap_fs_ext::DirExt;
    // Each call takes exactly one child component. No intermediate component
    // can be a followed link; the opened handle is checked, not a later path.
    let dir = parent.open_dir_nofollow(name).ok()?;
    let meta = dir.dir_metadata().ok()?;
    (!is_link(&meta)).then_some(dir)
}

#[cfg(not(target_os = "android"))]
fn read_instruction(dir: &cap_std::fs::Dir, name: &Path, display: PathBuf) -> Option<InstructionFile> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    use std::io::Read;
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    // Avoid blocking on a substituted FIFO before checking its opened metadata.
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = dir.open_with(name, &options).ok()?;
    let meta = file.metadata().ok()?;
    if is_link(&meta) || !meta.is_file() || meta.len() > MAX_FILE_SIZE {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_SIZE + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_FILE_SIZE {
        return None;
    }
    Some(InstructionFile {
        path: display,
        content: String::from_utf8(bytes).ok()?,
    })
}

#[cfg(not(target_os = "android"))]
fn discover_files(root: &Path) -> (Option<InstructionFile>, Option<InstructionFile>, Vec<InstructionFile>) {
    // The user-selected root is the sole ambient open. Every subsequent open,
    // metadata lookup and directory iteration is relative to an owned handle.
    let Ok(dir) = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority()) else {
        return (None, None, Vec::new());
    };
    let claude_dir = open_directory(&dir, Path::new(".claude"));
    let claude_md = read_instruction(&dir, Path::new("CLAUDE.md"), root.join("CLAUDE.md")).or_else(|| {
        read_instruction(
            claude_dir.as_ref()?,
            Path::new("CLAUDE.md"),
            root.join(".claude/CLAUDE.md"),
        )
    });
    let local = read_instruction(&dir, Path::new("CLAUDE.local.md"), root.join("CLAUDE.local.md"));
    let mut rules = Vec::new();
    if let Some(rules_dir) = claude_dir
        .as_ref()
        .and_then(|dir| open_directory(dir, Path::new("rules")))
    {
        let mut remaining = MAX_RULE_ENTRIES;
        // Fail closed if enumeration exhausts the work budget: selecting an
        // arbitrary OS-order prefix and sorting it afterwards is not stable.
        if collect_md_files(&rules_dir, &root.join(".claude/rules"), 0, &mut remaining, &mut rules).is_err() {
            rules.clear();
        }
    }
    (claude_md, local, rules)
}

#[cfg(not(target_os = "android"))]
fn collect_md_files(
    dir: &cap_std::fs::Dir,
    display: &Path,
    depth: usize,
    remaining: &mut usize,
    out: &mut Vec<InstructionFile>,
) -> Result<(), ()> {
    if out.len() == MAX_RULES_FILES {
        return Ok(());
    }
    let entries = dir.entries().map_err(|_| ())?;
    let mut names = Vec::new();
    for entry in entries {
        *remaining = remaining.checked_sub(1).ok_or(())?;
        names.push(entry.map_err(|_| ())?.file_name());
        if *remaining == 0 {
            return Err(());
        }
    }
    names.sort();
    for name in names {
        if out.len() == MAX_RULES_FILES {
            break;
        }
        let name = Path::new(&name);
        let Ok(meta) = dir.symlink_metadata(name) else { continue };
        if is_link(&meta) {
            continue;
        }
        if meta.is_dir() && depth < MAX_RULE_DEPTH {
            if let Some(child) = open_directory(dir, name) {
                collect_md_files(&child, &display.join(name), depth + 1, remaining, out)?;
            }
        } else if meta.is_file()
            && name.extension().is_some_and(|ext| ext == "md")
            && let Some(file) = read_instruction(dir, name, display.join(name))
        {
            out.push(file);
        }
    }
    Ok(())
}

pub(crate) fn compute_mtime_hash(paths: &[&PathBuf]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for p in paths {
        if let Ok(meta) = std::fs::metadata(p)
            && let Ok(mtime) = meta.modified()
        {
            mtime.hash(&mut hasher);
        }
        p.hash(&mut hasher);
    }
    hasher.finish()
}

pub(crate) fn read_file_utf8(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_SIZE {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

pub(crate) fn strip_frontmatter(content: &str) -> &str {
    if !content.starts_with("---") {
        return content;
    }
    let after_first = &content[3..];
    if let Some(end) = after_first.find("\n---") {
        let rest = &after_first[end + 4..];
        rest.strip_prefix('\n').unwrap_or(rest)
    } else {
        content
    }
}

pub(crate) fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    let current = estimate_tokens(text);
    if current <= max_tokens {
        return text.to_string();
    }
    let ratio = max_tokens as f64 / current as f64;
    let char_limit = (text.chars().count() as f64 * ratio) as usize;
    let mut result: String = text.chars().take(char_limit.saturating_sub(20)).collect();
    result.push_str("\n[... truncated]");
    result
}

#[cfg(not(target_os = "android"))]
fn format_instructions(
    root: &Path,
    claude_md: Option<&InstructionFile>,
    claude_local_md: Option<&InstructionFile>,
    rule_files: &[InstructionFile],
    max_tokens: usize,
) -> String {
    let mut parts = Vec::new();
    let mut tokens_used: usize = 0;

    let claude_budget = max_tokens * 70 / 100;
    let local_budget = max_tokens * 10 / 100;

    if let Some(file) = claude_md {
        let content = &file.content;
        let truncated = truncate_to_tokens(content, claude_budget);
        tokens_used += estimate_tokens(&truncated);
        parts.push(format!("# Project Instructions (CLAUDE.md)\n\n{}", truncated));
    }

    if let Some(file) = claude_local_md {
        let content = &file.content;
        let truncated = truncate_to_tokens(content, local_budget);
        tokens_used += estimate_tokens(&truncated);
        parts.push(format!("# Personal Overrides (CLAUDE.local.md)\n\n{}", truncated));
    }

    if !rule_files.is_empty() {
        let rules_budget = max_tokens.saturating_sub(tokens_used);
        if rules_budget > 100 {
            let per_rule = rules_budget / rule_files.len();
            let mut rule_parts = Vec::new();
            let rules_dir = root.join(".claude").join("rules");

            for file in rule_files {
                let path = &file.path;
                let content = &file.content;
                let body = strip_frontmatter(content);
                if body.trim().is_empty() {
                    continue;
                }
                let truncated = truncate_to_tokens(body, per_rule);
                let display_name = path
                    .strip_prefix(&rules_dir)
                    .unwrap_or(path.as_path())
                    .to_string_lossy()
                    .replace('\\', "/");
                rule_parts.push(format!("## {}\n{}", display_name, truncated));
            }

            if !rule_parts.is_empty() {
                parts.push(format!("# Project Rules\n\n{}", rule_parts.join("\n\n")));
            }
        }
    }

    parts.join("\n\n")
}

#[cfg(all(test, not(target_os = "android")))]
#[path = "project_instructions/resource_tests.rs"]
mod resource_tests;

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::*;

    #[test]
    fn test_instruction_budget_tiers() {
        assert_eq!(instruction_budget(8_000), 0);
        assert_eq!(instruction_budget(15_999), 0);
        assert_eq!(instruction_budget(16_000), 4_000);
        assert_eq!(instruction_budget(32_000), 4_000);
        assert_eq!(instruction_budget(63_999), 4_000);
        assert_eq!(instruction_budget(64_000), 16_000);
        assert_eq!(instruction_budget(127_999), 16_000);
        assert_eq!(instruction_budget(128_000), 32_000);
        assert_eq!(instruction_budget(1_000_000), 32_000);
    }

    #[test]
    fn test_strip_frontmatter() {
        let with_fm = "---\npaths:\n  - \"*.ts\"\n---\nActual content here";
        assert_eq!(strip_frontmatter(with_fm), "Actual content here");

        let without_fm = "Just normal content";
        assert_eq!(strip_frontmatter(without_fm), "Just normal content");

        let incomplete = "---\nno closing";
        assert_eq!(strip_frontmatter(incomplete), "---\nno closing");
    }

    #[test]
    fn test_truncate_to_tokens() {
        let short = "hello world";
        assert_eq!(truncate_to_tokens(short, 1000), "hello world");

        let long = "x".repeat(10000);
        let result = truncate_to_tokens(&long, 100);
        assert!(result.ends_with("[... truncated]"));
        assert!(result.len() < long.len());
    }

    #[test]
    fn test_discover_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let (claude, local, rules) = discover_files(dir.path());
        assert!(claude.is_none());
        assert!(local.is_none());
        assert!(rules.is_empty());
    }

    #[test]
    fn test_discover_claude_md_priority() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join("CLAUDE.md"), "top level").unwrap();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(root.join(".claude").join("CLAUDE.md"), "nested").unwrap();

        let (claude, _, _) = discover_files(root);
        let claude = claude.unwrap().path;
        assert!(claude.ends_with("CLAUDE.md"));
        assert!(!claude.to_string_lossy().contains(".claude"));
    }

    #[test]
    fn test_discover_fallback_to_nested() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(root.join(".claude").join("CLAUDE.md"), "nested").unwrap();

        let (claude, _, _) = discover_files(root);
        let claude = claude.unwrap().path;
        assert!(claude.to_string_lossy().contains(".claude"));
    }

    #[test]
    fn test_discover_claude_local() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join("CLAUDE.local.md"), "local overrides").unwrap();

        let (_, local, _) = discover_files(root);
        assert!(local.is_some());
    }

    #[test]
    fn test_discover_rules_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let rules_dir = root.join(".claude").join("rules");
        std::fs::create_dir_all(rules_dir.join("sub")).unwrap();
        std::fs::write(rules_dir.join("a.md"), "rule a").unwrap();
        std::fs::write(rules_dir.join("b.md"), "rule b").unwrap();
        std::fs::write(rules_dir.join("sub").join("c.md"), "rule c").unwrap();
        std::fs::write(rules_dir.join("not-md.txt"), "skip me").unwrap();

        let (_, _, rules) = discover_files(root);
        assert_eq!(rules.len(), 3);
    }

    #[test]
    fn test_format_instructions_full() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join("CLAUDE.md"), "# My Project\nDo things.").unwrap();
        std::fs::write(root.join("CLAUDE.local.md"), "My local stuff.").unwrap();
        let rules_dir = root.join(".claude").join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("style.md"), "---\npaths:\n  - \"*.ts\"\n---\nUse tabs.").unwrap();

        let (claude, local, rules) = discover_files(root);
        let result = format_instructions(root, claude.as_ref(), local.as_ref(), &rules, 32_000);

        assert!(result.contains("# Project Instructions (CLAUDE.md)"));
        assert!(result.contains("# My Project"));
        assert!(result.contains("# Personal Overrides (CLAUDE.local.md)"));
        assert!(result.contains("My local stuff."));
        assert!(result.contains("# Project Rules"));
        assert!(result.contains("Use tabs."));
        assert!(!result.contains("paths:"));
    }

    #[test]
    fn test_format_instructions_no_files() {
        let result = format_instructions(Path::new("/tmp"), None, None, &[], 32_000);
        assert!(result.is_empty());
    }

    #[test]
    fn test_non_utf8_file_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let rules_dir = root.join(".claude").join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("binary.md"), [0xFF, 0xFE, 0x00, 0x01]).unwrap();
        std::fs::write(rules_dir.join("valid.md"), "valid rule").unwrap();

        let (_, _, rules) = discover_files(root);
        let result = format_instructions(root, None, None, &rules, 32_000);
        assert!(result.contains("valid rule"));
    }

    #[test]
    fn test_load_sync_returns_none_for_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_sync(dir.path(), 32_000).is_none());
    }

    #[test]
    fn test_load_sync_wraps_in_tags() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "test content").unwrap();
        let result = load_sync(dir.path(), 32_000).unwrap();
        assert!(result.starts_with("\n\n<project_instructions>"));
        assert!(result.ends_with("</project_instructions>"));
    }

    #[test]
    fn test_cache_hit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "cached content").unwrap();

        let r1 = load_sync(dir.path(), 32_000).unwrap();
        let r2 = load_sync(dir.path(), 32_000).unwrap();
        assert_eq!(r1, r2);
    }
}
