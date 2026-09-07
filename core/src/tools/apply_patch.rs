use super::{Permission, ResolvedTarget, Tool, ToolContext};
use async_trait::async_trait;

pub struct ApplyPatchTool;

/// Resolve a patch's path against the patch's base directory.
///
/// Shared by `execute` and `reach` on purpose: if the two disagreed about which
/// file a patch line names, the approval prompt would be describing a different
/// file from the one that gets written.
fn join_base(p: &str, base: Option<&str>) -> String {
    let is_absolute = std::path::Path::new(p).is_absolute() || p.starts_with('/');
    match base {
        Some(base) if !is_absolute => format!("{base}/{p}"),
        _ => p.to_string(),
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a patch to one or more files. Two formats are accepted: \
         (1) a standard unified diff, as produced by `diff -u` or `git diff`; \
         (2) a Codex-style patch: `*** Begin Patch`, then for each file `*** Add File: <path>` \
         (content lines prefixed with '+'), `*** Update File: <path>` (optionally followed by \
         `*** Move to: <newpath>`; hunks start with `@@` or `@@ <context>` and contain \
         ' '/'+'/'-' prefixed lines), or `*** Delete File: <path>`, ending with `*** End Patch`."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "The patch content, in unified diff or Codex patch format"
                },
                "base_path": {
                    "type": "string",
                    "description": "Base directory path to resolve relative file paths in the patch (optional)"
                },
                "description": super::description_property(),
            },
            "required": ["patch"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    /// The widest reach of everything the patch touches.
    ///
    /// A patch is several operations at once, so one hook file among ten
    /// ordinary edits still has to be asked about. Deletes and moves end the
    /// question immediately: they are not reversible, and no width of mode
    /// covers them.
    fn reach(&self, args: &serde_json::Value, context: &ToolContext) -> super::reach::Reach {
        use super::reach::Reach;
        let Some(patch) = args["patch"].as_str() else {
            return Reach::Outside;
        };
        let Ok(ops) = parse_patch(patch) else {
            return Reach::Outside;
        };
        let base = args["base_path"]
            .as_str()
            .map(str::to_string)
            .or_else(|| context.working_directory.clone());

        let mut seen = Vec::new();
        for op in &ops {
            let path = match op {
                FileOp::Delete { .. } => return Reach::Outside,
                FileOp::Update(u) if u.move_to.is_some() => return Reach::Outside,
                FileOp::Add { path, .. } => path,
                FileOp::Update(u) => &u.path,
            };
            seen.push(super::reach::locate(context, &join_base(path, base.as_deref()), true));
        }
        super::reach::widest(seen)
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let patch = args["patch"].as_str().ok_or("missing 'patch' argument")?;
        let base_str: Option<String> = args["base_path"]
            .as_str()
            .map(str::to_string)
            .or_else(|| context.working_directory.clone());

        let ops = parse_patch(patch)?;

        let join = |p: &str| join_base(p, base_str.as_deref());
        let resolve = |p: &str| -> Result<ResolvedTarget, String> { context.resolve_and_validate(&join(p)) };

        let mut added: Vec<String> = Vec::new();
        let mut updated: Vec<String> = Vec::new();
        let mut moved_count = 0usize;
        let mut deleted: Vec<String> = Vec::new();

        for op in &ops {
            match op {
                FileOp::Add { path, content } => {
                    // The exclusive create both refuses an existing file and
                    // hands back the handle that will be written, so nothing
                    // is re-resolved between the two.
                    let target = context.open_create_new(&join(path)).map_err(|e| {
                        if e.contains("exists") {
                            format!("Add File: '{path}' already exists; use '*** Update File:' to modify it")
                        } else {
                            e
                        }
                    })?;
                    let journal = context.journal_record("apply_patch", crate::journal::capture::Op::Patch);
                    super::backend::write_opened(target, content, journal).await?;
                    added.push(path.clone());
                }
                FileOp::Delete { path } => {
                    if context.is_access_root(path) {
                        return Err(format!("refusing to delete '{path}': it is an authorized access root"));
                    }
                    let target = resolve(path)?;
                    let journal = context.journal_record("apply_patch", crate::journal::capture::Op::Delete);
                    let observed = match (&journal, &target) {
                        (Some(j), ResolvedTarget::Real(p)) => Some(j.observe(p).await),
                        _ => None,
                    };
                    super::backend::delete(&target, false).await?;
                    if let (Some(j), Some(obs)) = (&journal, &observed) {
                        j.commit(obs, None).await;
                    }
                    deleted.push(path.clone());
                }
                FileOp::Update(update) => {
                    let apply = |original: &str| apply_file_update(original, update);

                    // An in-place update is the one shape that can run on a
                    // single handle: what the hunks matched against is what
                    // gets rewritten. A move has to rename before it writes,
                    // and a new file has nothing to read, so both of those
                    // resolve by path instead.
                    if update.move_to.is_none() && !update.is_new_file {
                        let target = context.open_edit(&join(&update.path))?;
                        let journal = context.journal_record("apply_patch", crate::journal::capture::Op::Patch);
                        super::backend::edit_opened(target, apply, journal).await?;
                        updated.push(update.path.clone());
                        continue;
                    }

                    let target = resolve(&update.path)?;
                    let original = if update.is_new_file {
                        String::new()
                    } else {
                        super::backend::read_to_string(&target).await?
                    };
                    let result = apply(&original)?;
                    match &update.move_to {
                        None => {
                            let journal = context.journal_record("apply_patch", crate::journal::capture::Op::Patch);
                            let observed = match (&journal, &target) {
                                (Some(j), ResolvedTarget::Real(p)) => Some(j.observe(p).await),
                                _ => None,
                            };
                            super::backend::write_string(&target, &result).await?;
                            if let (Some(j), Some(obs)) = (&journal, &observed) {
                                j.commit(obs, Some(&result)).await;
                            }
                            updated.push(update.path.clone());
                        }
                        Some(dst) => {
                            let dst_target = resolve(dst)?;
                            if let ResolvedTarget::Real(ref p) = dst_target
                                && tokio::fs::symlink_metadata(p).await.is_ok()
                            {
                                return Err(format!("Move to: '{dst}' already exists"));
                            }
                            let journal = context.journal_record("apply_patch", crate::journal::capture::Op::Patch);
                            let observed = match (&journal, &target, &dst_target) {
                                (Some(j), ResolvedTarget::Real(src), ResolvedTarget::Real(dstp)) => {
                                    Some(j.observe_pair(src, dstp).await)
                                }
                                _ => None,
                            };
                            super::backend::rename(&target, &dst_target).await?;
                            super::backend::write_string(&dst_target, &result).await.map_err(|e| {
                                format!("file was moved to '{dst}' but updating its content failed: {e}")
                            })?;
                            if let (Some(j), Some((src_obs, dst_obs))) = (&journal, &observed) {
                                use crate::journal::capture::Op;
                                let from = j.commit_as(Op::RenameFrom, src_obs, None, None).await;
                                j.commit_as(Op::RenameTo, dst_obs, Some(&result), from).await;
                            }
                            updated.push(format!("{} -> {}", update.path, dst));
                            moved_count += 1;
                        }
                    }
                }
            }
        }

        let mut parts = Vec::new();
        if !added.is_empty() {
            parts.push(format!("{} added", added.len()));
        }
        if !updated.is_empty() {
            if moved_count > 0 {
                parts.push(format!("{} updated ({moved_count} moved)", updated.len()));
            } else {
                parts.push(format!("{} updated", updated.len()));
            }
        }
        if !deleted.is_empty() {
            parts.push(format!("{} deleted", deleted.len()));
        }
        let files: Vec<String> = added.into_iter().chain(updated).chain(deleted).collect();
        Ok(format!("Applied patch: {} — {}", parts.join(", "), files.join(", ")))
    }
}

// ---- Intermediate representation ----
//
// Both parsers produce a list of file-level operations. Update bodies stay
// format-specific: numbered hunks (unified diff) locate by declared line
// numbers with strict verification, contextual chunks (Codex format) locate
// by content search. Unifying them would silently turn the strict unified
// path into fuzzy matching.

#[derive(Debug)]
enum FileOp {
    Add { path: String, content: String },
    Delete { path: String },
    Update(FileUpdate),
}

#[derive(Debug)]
struct FileUpdate {
    path: String,
    move_to: Option<String>,
    is_new_file: bool,
    body: UpdateBody,
}

#[derive(Debug)]
enum UpdateBody {
    Numbered(Vec<Hunk>),
    Contextual(Vec<ContextChunk>),
}

#[derive(Debug)]
struct ContextChunk {
    // Text after "@@ " (e.g. a function signature) used to anchor the search.
    change_context: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    is_end_of_file: bool,
}

#[derive(Debug)]
struct Hunk {
    old_start: usize,
    lines: Vec<PatchLine>,
}

#[derive(Debug)]
enum PatchLine {
    Context(String),
    Add(String),
    Remove(String),
}

// ---- Format detection & dispatch ----

enum PatchFormat {
    Codex,
    Unified,
}

fn detect_format(patch: &str) -> PatchFormat {
    for line in patch.lines() {
        let t = line.trim();
        if t == "*** Begin Patch"
            || t.starts_with("*** Update File:")
            || t.starts_with("*** Add File:")
            || t.starts_with("*** Delete File:")
        {
            return PatchFormat::Codex;
        }
    }
    PatchFormat::Unified
}

fn parse_patch(patch: &str) -> Result<Vec<FileOp>, String> {
    let ops = match detect_format(patch) {
        PatchFormat::Codex => parse_codex_patch(patch)?,
        PatchFormat::Unified => parse_unified_diff(patch)?,
    };
    if ops.is_empty() {
        let first_line = patch.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!(
            "no file changes found in patch. Received input starting with: \"{}\". Expected either \
             a unified diff (\"--- a/file\" / \"+++ b/file\" / \"@@ -1,3 +1,4 @@\" hunks) or a \
             Codex-style patch (\"*** Begin Patch\" / \"*** Update File: path\" / \"@@\" hunks / \
             \"*** End Patch\")",
            preview80(first_line)
        ));
    }
    Ok(ops)
}

/// Apply the restricted patch language used by plan mode to an in-memory
/// `plan.md`. This shares both parsers and both applicators with the ordinary
/// filesystem tool, but admits exactly one operation against exactly one
/// logical path. The caller persists the returned snapshot and materializes
/// the private file atomically.
pub fn apply_plan_patch(current: Option<&str>, patch: &str) -> Result<String, String> {
    let mut ops = parse_patch(patch)?;
    if ops.len() != 1 {
        return Err("a plan update must contain exactly one file operation for plan.md".into());
    }

    let next = match ops.remove(0) {
        FileOp::Add { path, content } => {
            require_plan_path(&path)?;
            if current.is_some() {
                return Err("plan.md already exists; revise it with an Update File patch".into());
            }
            content
        }
        FileOp::Delete { .. } => return Err("plan.md cannot be deleted".into()),
        FileOp::Update(update) => {
            require_plan_path(&update.path)?;
            if update.move_to.is_some() {
                return Err("plan.md cannot be moved".into());
            }
            if update.is_new_file {
                if current.is_some() {
                    return Err("plan.md already exists; the /dev/null form is only valid for the first draft".into());
                }
                apply_file_update("", &update)?
            } else {
                let original = current.ok_or(
                    "plan.md does not exist; create the first draft with Add File: plan.md or a /dev/null diff",
                )?;
                apply_file_update(original, &update)?
            }
        }
    };

    if next.trim().is_empty() {
        return Err("the resulting plan.md is empty".into());
    }
    if current == Some(next.as_str()) {
        return Err("the patch makes no changes to plan.md".into());
    }
    Ok(next)
}

fn require_plan_path(path: &str) -> Result<(), String> {
    if path.replace('\\', "/") == "plan.md" {
        Ok(())
    } else {
        Err(format!("plan mode patches may only target plan.md, not {path:?}"))
    }
}

fn apply_file_update(original: &str, update: &FileUpdate) -> Result<String, String> {
    match &update.body {
        UpdateBody::Numbered(hunks) => apply_hunks(original, hunks),
        UpdateBody::Contextual(chunks) => apply_context_chunks(original, chunks, &update.path),
    }
}

fn preview80(s: &str) -> String {
    let t = s.trim();
    if t.chars().count() > 80 {
        let cut: String = t.chars().take(80).collect();
        format!("{cut}…")
    } else {
        t.to_string()
    }
}

// ---- Codex-style patch parsing ----

fn header_path(trimmed: &str, marker: &str) -> Option<String> {
    trimmed.strip_prefix(marker).map(|rest| rest.trim().to_string())
}

fn register_path(seen: &mut std::collections::HashSet<String>, path: &str, marker: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err(format!("'{marker}' is missing a file path"));
    }
    if !seen.insert(path.to_string()) {
        return Err(format!("duplicate entry for '{path}' in patch"));
    }
    Ok(())
}

/// Recognize "@@" / "@@ <context>" chunk headers. Returns the optional context text.
/// A unified-diff style "@@ -1,3 +1,4 @@" header is treated as a plain separator.
fn chunk_header(trimmed: &str) -> Option<Option<String>> {
    if trimmed == "@@" {
        return Some(None);
    }
    let rest = trimmed.strip_prefix("@@ ")?;
    let ctx = rest.strip_suffix(" @@").unwrap_or(rest).trim();
    let line_numbers = regex::Regex::new(r"^-\d+(,\d+)? \+\d+(,\d+)?$").unwrap();
    if ctx.is_empty() || line_numbers.is_match(ctx) {
        Some(None)
    } else {
        Some(Some(ctx.to_string()))
    }
}

fn parse_codex_patch(patch: &str) -> Result<Vec<FileOp>, String> {
    let all_lines: Vec<&str> = patch.lines().collect();

    // Locate the envelope. Junk outside it (prose, markdown fences) is ignored.
    let begin = all_lines.iter().position(|l| l.trim() == "*** Begin Patch");
    let end = all_lines.iter().rposition(|l| l.trim() == "*** End Patch");
    let (body, offset, strict) = match (begin, end) {
        (Some(b), Some(e)) if e > b => (&all_lines[b + 1..e], b + 1, true),
        (Some(_), _) => {
            return Err(
                "found '*** Begin Patch' but no '*** End Patch' — the patch may be truncated; \
                 re-emit the complete patch"
                    .to_string(),
            );
        }
        // No envelope but bare "*** Update File:" headers: accept, tolerate surrounding junk.
        (None, _) => (&all_lines[..], 0, false),
    };

    let mut ops: Vec<FileOp> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut i = 0;
    while i < body.len() {
        let trimmed = body[i].trim();
        if trimmed == "*** End Patch" {
            break;
        }
        if let Some(path) = header_path(trimmed, "*** Add File:") {
            register_path(&mut seen, &path, "*** Add File:")?;
            let (content, next) = parse_add_file_body(body, i + 1, &path)?;
            ops.push(FileOp::Add { path, content });
            i = next;
        } else if let Some(path) = header_path(trimmed, "*** Delete File:") {
            register_path(&mut seen, &path, "*** Delete File:")?;
            ops.push(FileOp::Delete { path });
            i += 1;
        } else if let Some(path) = header_path(trimmed, "*** Update File:") {
            register_path(&mut seen, &path, "*** Update File:")?;
            let mut move_to = None;
            let mut j = i + 1;
            if j < body.len()
                && let Some(dst) = header_path(body[j].trim(), "*** Move to:")
            {
                if dst.is_empty() {
                    return Err("'*** Move to:' is missing a file path".to_string());
                }
                move_to = Some(dst);
                j += 1;
            }
            let (chunks, next) = parse_update_chunks(body, j, &path, offset)?;
            ops.push(FileOp::Update(FileUpdate {
                path,
                move_to,
                is_new_file: false,
                body: UpdateBody::Contextual(chunks),
            }));
            i = next;
        } else if trimmed.is_empty() || !strict {
            // Blank lines between file sections; in implicit-envelope mode also
            // tolerate arbitrary junk around the headers.
            i += 1;
        } else {
            return Err(format!(
                "patch line {}: expected '*** Add File:', '*** Update File:', '*** Delete File:' \
                 or '*** End Patch', got: \"{}\"",
                offset + i + 1,
                preview80(trimmed)
            ));
        }
    }
    Ok(ops)
}

fn parse_add_file_body(lines: &[&str], start: usize, path: &str) -> Result<(String, usize), String> {
    let mut content_lines: Vec<&str> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().starts_with("*** ") {
            break;
        }
        if let Some(rest) = line.strip_prefix('+') {
            content_lines.push(rest);
        } else if line.trim().is_empty() {
            content_lines.push("");
        } else {
            return Err(format!(
                "in 'Add File: {path}': expected '+'-prefixed content lines, got: \"{}\"",
                preview80(line)
            ));
        }
        i += 1;
    }
    let content = if content_lines.is_empty() {
        String::new()
    } else {
        let mut c = content_lines.join("\n");
        c.push('\n');
        c
    };
    Ok((content, i))
}

fn parse_update_chunks(
    lines: &[&str],
    start: usize,
    path: &str,
    offset: usize,
) -> Result<(Vec<ContextChunk>, usize), String> {
    let mut chunks: Vec<ContextChunk> = Vec::new();
    let mut cur: Option<ContextChunk> = None;
    let mut sealed = false; // saw "*** End of File"
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if trimmed == "*** End of File" {
            if let Some(mut c) = cur.take() {
                c.is_end_of_file = true;
                chunks.push(c);
            } else if let Some(last) = chunks.last_mut() {
                last.is_end_of_file = true;
            }
            sealed = true;
            i += 1;
            continue;
        }
        if trimmed.starts_with("*** ") {
            break; // next file header or "*** End Patch", handled by the caller
        }
        if let Some(ctx) = chunk_header(trimmed) {
            if sealed {
                return Err(format!(
                    "in 'Update File: {path}': found a chunk after '*** End of File'"
                ));
            }
            if let Some(c) = cur.take() {
                chunks.push(c);
            }
            cur = Some(ContextChunk {
                change_context: ctx,
                old_lines: Vec::new(),
                new_lines: Vec::new(),
                is_end_of_file: false,
            });
            i += 1;
            continue;
        }
        if sealed {
            return Err(format!(
                "in 'Update File: {path}': found content after '*** End of File'"
            ));
        }
        // The first chunk may start without a "@@" header.
        let c = cur.get_or_insert_with(|| ContextChunk {
            change_context: None,
            old_lines: Vec::new(),
            new_lines: Vec::new(),
            is_end_of_file: false,
        });
        if line.is_empty() {
            c.old_lines.push(String::new());
            c.new_lines.push(String::new());
        } else if let Some(rest) = line.strip_prefix('+') {
            c.new_lines.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix('-') {
            c.old_lines.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix(' ') {
            c.old_lines.push(rest.to_string());
            c.new_lines.push(rest.to_string());
        } else {
            return Err(format!(
                "patch line {}: expected '+', '-', ' ', '@@' or '*** ' marker inside \
                 'Update File: {path}', got: \"{}\"",
                offset + i + 1,
                preview80(line)
            ));
        }
        i += 1;
    }
    if let Some(c) = cur.take() {
        chunks.push(c);
    }
    if chunks.is_empty() {
        return Err(format!("'Update File: {path}' contains no change hunks"));
    }
    Ok((chunks, i))
}

// ---- Context-based application (Codex chunks) ----

/// Content search with four progressively more lenient passes:
/// exact -> trailing-whitespace-insensitive -> whitespace-insensitive ->
/// Unicode punctuation-normalized. Each pass scans the whole window before
/// falling through, so an early fuzzy match never wins over a later exact one.
/// With `eof` the pattern is only tried anchored at the end of the file.
fn seek_lines(haystack: &[&str], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > haystack.len() {
        return None;
    }
    let levels: [fn(&str, &str) -> bool; 4] = [
        |a, b| a == b,
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
        |a, b| normalize_punct(a) == normalize_punct(b),
    ];
    let last = haystack.len() - pattern.len();
    for eq in levels {
        let candidates: Box<dyn Iterator<Item = usize>> = if eof {
            Box::new(std::iter::once(last))
        } else {
            Box::new(start..=last)
        };
        for idx in candidates {
            if pattern.iter().enumerate().all(|(k, p)| eq(haystack[idx + k], p)) {
                return Some(idx);
            }
        }
    }
    None
}

/// Fold common typographic characters to ASCII so a plain-ASCII patch still
/// matches context containing smart punctuation (mirrors `git apply` tolerance).
fn normalize_punct(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{2018}'..='\u{201B}' => '\'',
            '\u{201C}'..='\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            _ => c,
        })
        .collect()
}

fn apply_context_chunks(original: &str, chunks: &[ContextChunk], path: &str) -> Result<String, String> {
    let line_ending = if original.contains("\r\n") { "\r\n" } else { "\n" };
    let orig_lines: Vec<&str> = if original.is_empty() {
        Vec::new()
    } else {
        original.lines().collect()
    };

    // (start, old_len, new_lines), collected first, applied in reverse.
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index: usize = 0;

    for chunk in chunks {
        if let Some(ref ctx) = chunk.change_context {
            let anchor = seek_lines(&orig_lines, std::slice::from_ref(ctx), line_index, false).ok_or_else(|| {
                format!(
                    "could not find context line '@@ {ctx}' in {path} (searched from line {})",
                    line_index + 1
                )
            })?;
            line_index = anchor + 1;
        }

        if chunk.old_lines.is_empty() {
            // Pure insertion: append at end of file.
            replacements.push((orig_lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }

        let mut old = chunk.old_lines.clone();
        let mut new = chunk.new_lines.clone();
        let mut found = seek_lines(&orig_lines, &old, line_index, chunk.is_end_of_file);
        if found.is_none() && old.last().is_some_and(|l| l.is_empty()) {
            // The file's final newline appears as an empty line in the patch but
            // not in orig_lines; drop the sentinel and retry.
            old.pop();
            if new.last().is_some_and(|l| l.is_empty()) {
                new.pop();
            }
            if !old.is_empty() {
                found = seek_lines(&orig_lines, &old, line_index, chunk.is_end_of_file);
            }
        }
        let idx = found.ok_or_else(|| {
            let shown: Vec<String> = old.iter().take(3).map(|l| preview80(l)).collect();
            format!(
                "could not find these lines in {path} (searched from line {}):\n{}\nThe file \
                 content may differ from what the patch expects — re-read the file and regenerate \
                 the patch",
                line_index + 1,
                shown.join("\n")
            )
        })?;
        replacements.push((idx, old.len(), new));
        line_index = idx + old.len();
    }

    // line_index advances monotonically, but an EOF-anchored chunk can jump
    // backwards; sort and reject overlaps instead of corrupting the file.
    replacements.sort_by_key(|r| r.0);
    for w in replacements.windows(2) {
        if w[0].0 + w[0].1 > w[1].0 {
            return Err(format!("patch chunks overlap in {path}; regenerate the patch"));
        }
    }

    let mut result: Vec<String> = orig_lines.iter().map(|s| s.to_string()).collect();
    for (start, old_len, new) in replacements.iter().rev() {
        result.splice(*start..*start + *old_len, new.iter().cloned());
    }

    let mut out = result.join(line_ending);
    if original.ends_with('\n') || original.is_empty() {
        out.push_str(line_ending);
    }
    Ok(out)
}

// ---- Unified diff parsing ----

fn parse_unified_diff(patch: &str) -> Result<Vec<FileOp>, String> {
    let mut files = Vec::new();
    let lines: Vec<&str> = patch.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        // Look for --- line
        if lines[i].starts_with("--- ") && i + 1 < lines.len() && lines[i + 1].starts_with("+++ ") {
            let old_path = strip_prefix(lines[i].trim_start_matches("--- "));
            let new_path = strip_prefix(lines[i + 1].trim_start_matches("+++ "));
            let is_new_file = old_path == "/dev/null";
            let path = if is_new_file { new_path } else { old_path };
            i += 2;

            let mut hunks = Vec::new();
            while i < lines.len() && lines[i].starts_with("@@ ") {
                let (hunk, next_i) = parse_hunk(&lines, i)?;
                hunks.push(hunk);
                i = next_i;
            }

            if !hunks.is_empty() {
                files.push(FileOp::Update(FileUpdate {
                    path,
                    move_to: None,
                    is_new_file,
                    body: UpdateBody::Numbered(hunks),
                }));
            }
        } else {
            i += 1;
        }
    }

    Ok(files)
}

fn strip_prefix(path: &str) -> String {
    let path = path.trim();
    // Strip a/ or b/ prefix from git diffs
    if path.starts_with("a/") || path.starts_with("b/") {
        path[2..].to_string()
    } else {
        path.to_string()
    }
}

fn parse_hunk(lines: &[&str], start: usize) -> Result<(Hunk, usize), String> {
    let header = lines[start];
    let old_start = parse_hunk_header(header)?;

    let mut hunk_lines = Vec::new();
    let mut i = start + 1;

    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("@@ ") || line.starts_with("--- ") || line.starts_with("+++ ") {
            break;
        }
        if let Some(rest) = line.strip_prefix('+') {
            hunk_lines.push(PatchLine::Add(rest.to_string()));
        } else if let Some(rest) = line.strip_prefix('-') {
            hunk_lines.push(PatchLine::Remove(rest.to_string()));
        } else if let Some(rest) = line.strip_prefix(' ') {
            hunk_lines.push(PatchLine::Context(rest.to_string()));
        } else if line.starts_with('\\') {
            // "\ No newline at end of file" — skip
        } else if line.is_empty() {
            // Empty context line
            hunk_lines.push(PatchLine::Context(String::new()));
        } else {
            break;
        }
        i += 1;
    }

    Ok((
        Hunk {
            old_start,
            lines: hunk_lines,
        },
        i,
    ))
}

fn parse_hunk_header(header: &str) -> Result<usize, String> {
    // @@ -old_start,old_count +new_start,new_count @@
    let re = regex::Regex::new(r"@@ -(\d+)").unwrap();
    let caps = re
        .captures(header)
        .ok_or_else(|| format!("invalid hunk header: {header}"))?;
    caps[1]
        .parse::<usize>()
        .map_err(|e| format!("invalid line number in hunk header: {e}"))
}

fn apply_hunks(original: &str, hunks: &[Hunk]) -> Result<String, String> {
    let line_ending = if original.contains("\r\n") { "\r\n" } else { "\n" };
    let orig_lines: Vec<&str> = if original.is_empty() {
        Vec::new()
    } else {
        original.lines().collect()
    };

    let mut result = Vec::new();
    let mut pos: usize = 0; // 0-indexed position in original

    for hunk in hunks {
        let start = if hunk.old_start == 0 { 0 } else { hunk.old_start - 1 };

        // Copy unchanged lines before this hunk
        while pos < start && pos < orig_lines.len() {
            result.push(orig_lines[pos].to_string());
            pos += 1;
        }

        for line in &hunk.lines {
            match line {
                PatchLine::Context(s) => {
                    let actual = orig_lines.get(pos).copied().unwrap_or("");
                    if pos >= orig_lines.len() || actual != s.as_str() {
                        return Err(format!(
                            "patch context mismatch at line {}: expected {s:?} but found {actual:?}",
                            pos + 1
                        ));
                    }
                    result.push(orig_lines[pos].to_string());
                    pos += 1;
                }
                PatchLine::Add(s) => {
                    result.push(s.clone());
                }
                PatchLine::Remove(s) => {
                    let actual = orig_lines.get(pos).copied().unwrap_or("");
                    if pos >= orig_lines.len() || actual != s.as_str() {
                        return Err(format!(
                            "patch delete mismatch at line {}: expected to remove {s:?} but found {actual:?}",
                            pos + 1
                        ));
                    }
                    pos += 1;
                }
            }
        }
    }

    // Copy remaining lines after last hunk
    while pos < orig_lines.len() {
        result.push(orig_lines[pos].to_string());
        pos += 1;
    }

    let mut out = result.join(line_ending);
    if original.ends_with('\n') || original.is_empty() {
        out.push_str(line_ending);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_patch_requires_an_add_then_updates_the_same_virtual_file() {
        let first = apply_plan_patch(
            None,
            "*** Begin Patch\n*** Add File: plan.md\n+# 计划\n+\n+处理 😀\n*** End Patch\n",
        )
        .unwrap();
        assert_eq!(first, "# 计划\n\n处理 😀\n");

        let second = apply_plan_patch(
            Some(&first),
            "*** Begin Patch\n*** Update File: plan.md\n@@\n-处理 😀\n+处理 😀 并测试\n*** End Patch\n",
        )
        .unwrap();
        assert_eq!(second, "# 计划\n\n处理 😀 并测试\n");
    }

    #[test]
    fn plan_patch_rejects_other_files_destructive_ops_and_noops() {
        let current = "# Plan\n";
        for patch in [
            "*** Begin Patch\n*** Update File: other.md\n@@\n-x\n+y\n*** End Patch\n",
            "*** Begin Patch\n*** Delete File: plan.md\n*** End Patch\n",
            "*** Begin Patch\n*** Update File: plan.md\n@@\n # Plan\n*** End Patch\n",
        ] {
            assert!(apply_plan_patch(Some(current), patch).is_err(), "accepted {patch}");
        }
    }

    #[test]
    fn plan_patch_preserves_crlf() {
        let current = "# Plan\r\n\r\nOne\r\n";
        let patch = "--- a/plan.md\n+++ b/plan.md\n@@ -1,3 +1,3 @@\n # Plan\n \n-One\n+Two\n";
        assert_eq!(apply_plan_patch(Some(current), patch).unwrap(), "# Plan\r\n\r\nTwo\r\n");
    }

    #[test]
    fn test_apply_hunks_context_and_add() {
        let original = "line1\nline2\nline3\n";
        let hunks = vec![Hunk {
            old_start: 1,
            lines: vec![
                PatchLine::Context("line1".into()),
                PatchLine::Add("inserted".into()),
                PatchLine::Context("line2".into()),
            ],
        }];
        let out = apply_hunks(original, &hunks).unwrap();
        assert_eq!(out, "line1\ninserted\nline2\nline3\n");
    }

    #[test]
    fn test_apply_hunks_context_mismatch_rejected() {
        let original = "alpha\nbeta\ngamma\n";
        let hunks = vec![Hunk {
            old_start: 1,
            lines: vec![PatchLine::Context("WRONG".into()), PatchLine::Add("x".into())],
        }];
        assert!(apply_hunks(original, &hunks).is_err());
    }

    #[test]
    fn test_apply_hunks_remove_mismatch_rejected() {
        let original = "a\nb\nc\n";
        let hunks = vec![Hunk {
            old_start: 1,
            lines: vec![PatchLine::Remove("NOT_A".into())],
        }];
        assert!(apply_hunks(original, &hunks).is_err());
    }

    #[test]
    fn test_apply_hunks_preserves_crlf() {
        let original = "one\r\ntwo\r\nthree\r\n";
        let hunks = vec![Hunk {
            old_start: 2,
            lines: vec![PatchLine::Context("two".into()), PatchLine::Add("added".into())],
        }];
        let out = apply_hunks(original, &hunks).unwrap();
        assert_eq!(out, "one\r\ntwo\r\nadded\r\nthree\r\n");
    }

    // ---- parse_patch dispatch ----

    #[test]
    fn unified_diff_parses_via_parse_patch() {
        let patch = "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n line1\n-old\n+new\n";
        let ops = parse_patch(patch).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            FileOp::Update(u) => {
                assert_eq!(u.path, "f.txt");
                assert!(matches!(u.body, UpdateBody::Numbered(_)));
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn garbage_input_rejected_with_diagnostic() {
        let err = parse_patch("this is not a patch at all\n").unwrap_err();
        assert!(err.contains("no file changes found"));
        assert!(err.contains("*** Begin Patch"));
        assert!(err.contains("unified diff"));
    }

    // ---- Codex parsing ----

    #[test]
    fn codex_parse_update_basic() {
        let patch =
            "*** Begin Patch\n*** Update File: src/main.rs\n@@ fn main()\n-    old();\n+    new();\n*** End Patch\n";
        let ops = parse_patch(patch).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            FileOp::Update(u) => {
                assert_eq!(u.path, "src/main.rs");
                match &u.body {
                    UpdateBody::Contextual(chunks) => {
                        assert_eq!(chunks.len(), 1);
                        assert_eq!(chunks[0].change_context.as_deref(), Some("fn main()"));
                        assert_eq!(chunks[0].old_lines, vec!["    old();"]);
                        assert_eq!(chunks[0].new_lines, vec!["    new();"]);
                    }
                    _ => panic!("expected Contextual"),
                }
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn codex_parse_add_delete_move() {
        let patch = "*** Begin Patch\n\
                     *** Add File: new.txt\n\
                     +hello\n\
                     +world\n\
                     *** Delete File: gone.txt\n\
                     *** Update File: old_name.txt\n\
                     *** Move to: new_name.txt\n\
                     @@\n\
                     -a\n\
                     +b\n\
                     *** End Patch\n";
        let ops = parse_patch(patch).unwrap();
        assert_eq!(ops.len(), 3);
        match &ops[0] {
            FileOp::Add { path, content } => {
                assert_eq!(path, "new.txt");
                assert_eq!(content, "hello\nworld\n");
            }
            _ => panic!("expected Add"),
        }
        assert!(matches!(&ops[1], FileOp::Delete { path } if path == "gone.txt"));
        match &ops[2] {
            FileOp::Update(u) => {
                assert_eq!(u.path, "old_name.txt");
                assert_eq!(u.move_to.as_deref(), Some("new_name.txt"));
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn codex_parse_multiple_files_and_chunks() {
        let patch = "*** Begin Patch\n\
                     *** Update File: a.txt\n\
                     @@ ctx one\n\
                     -x\n\
                     +y\n\
                     @@ ctx two\n\
                     -p\n\
                     +q\n\
                     *** Update File: b.txt\n\
                     @@\n\
                     -1\n\
                     +2\n\
                     *** End Patch\n";
        let ops = parse_patch(patch).unwrap();
        assert_eq!(ops.len(), 2);
        match &ops[0] {
            FileOp::Update(u) => match &u.body {
                UpdateBody::Contextual(chunks) => assert_eq!(chunks.len(), 2),
                _ => panic!("expected Contextual"),
            },
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn codex_parse_ignores_surrounding_junk() {
        let patch = "Here is the patch you asked for:\n\
                     ```patch\n\
                     *** Begin Patch\n\
                     *** Update File: f.txt\n\
                     @@\n\
                     -a\n\
                     +b\n\
                     *** End Patch\n\
                     ```\n\
                     Let me know if it works!\n";
        let ops = parse_patch(patch).unwrap();
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn codex_parse_implicit_envelope() {
        let patch = "*** Update File: f.txt\n@@\n-a\n+b\n";
        let ops = parse_patch(patch).unwrap();
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn codex_parse_truncated_envelope_rejected() {
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@\n-a\n+b\n";
        let err = parse_patch(patch).unwrap_err();
        assert!(err.contains("End Patch"));
        assert!(err.contains("truncated"));
    }

    #[test]
    fn codex_parse_empty_envelope_rejected() {
        let err = parse_patch("*** Begin Patch\n*** End Patch\n").unwrap_err();
        assert!(err.contains("no file changes found"));
    }

    #[test]
    fn codex_parse_duplicate_path_rejected() {
        let patch = "*** Begin Patch\n\
                     *** Update File: f.txt\n\
                     @@\n\
                     -a\n\
                     +b\n\
                     *** Update File: f.txt\n\
                     @@\n\
                     -c\n\
                     +d\n\
                     *** End Patch\n";
        let err = parse_patch(patch).unwrap_err();
        assert!(err.contains("duplicate"));
    }

    #[test]
    fn codex_parse_bad_line_is_diagnostic() {
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@\n-a\nBOGUS LINE\n*** End Patch\n";
        let err = parse_patch(patch).unwrap_err();
        assert!(err.contains("BOGUS LINE"));
        assert!(err.contains("Update File: f.txt"));
    }

    // ---- seek_lines ----

    fn lines(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn seek_exact_and_start_offset() {
        let hay = ["a", "b", "a", "b"];
        assert_eq!(seek_lines(&hay, &lines(&["a", "b"]), 0, false), Some(0));
        assert_eq!(seek_lines(&hay, &lines(&["a", "b"]), 1, false), Some(2));
    }

    #[test]
    fn seek_trim_end_fallback() {
        let hay = ["fn main() {   ", "}"];
        assert_eq!(seek_lines(&hay, &lines(&["fn main() {"]), 0, false), Some(0));
    }

    #[test]
    fn seek_trim_both_fallback() {
        let hay = ["    indented();"];
        assert_eq!(seek_lines(&hay, &lines(&["  indented();"]), 0, false), Some(0));
    }

    #[test]
    fn seek_unicode_punct_fallback() {
        let hay = ["let s = \u{201C}hi\u{201D};", "x \u{2014} y", "a\u{00A0}b"];
        assert_eq!(seek_lines(&hay, &lines(&["let s = \"hi\";"]), 0, false), Some(0));
        assert_eq!(seek_lines(&hay, &lines(&["x - y"]), 0, false), Some(1));
        assert_eq!(seek_lines(&hay, &lines(&["a b"]), 0, false), Some(2));
    }

    #[test]
    fn seek_exact_wins_over_earlier_fuzzy() {
        // A fuzzy candidate appears earlier, an exact one later: exact must win.
        let hay = ["  x  ", "x"];
        assert_eq!(seek_lines(&hay, &lines(&["x"]), 0, false), Some(1));
    }

    #[test]
    fn seek_eof_anchored_tail_only() {
        let hay = ["dup", "mid", "dup"];
        assert_eq!(seek_lines(&hay, &lines(&["dup"]), 0, true), Some(2));
        // Non-EOF finds the first occurrence.
        assert_eq!(seek_lines(&hay, &lines(&["dup"]), 0, false), Some(0));
    }

    #[test]
    fn seek_defensive_cases() {
        let hay = ["a"];
        assert_eq!(seek_lines(&hay, &[], 0, false), Some(0));
        assert_eq!(seek_lines(&hay, &lines(&["a", "b"]), 0, false), None);
    }

    // ---- apply_context_chunks ----

    fn chunk(ctx: Option<&str>, old: &[&str], new: &[&str]) -> ContextChunk {
        ContextChunk {
            change_context: ctx.map(str::to_string),
            old_lines: lines(old),
            new_lines: lines(new),
            is_end_of_file: false,
        }
    }

    #[test]
    fn chunks_basic_replace() {
        let original = "a\nb\nc\n";
        let out = apply_context_chunks(original, &[chunk(None, &["b"], &["B"])], "f").unwrap();
        assert_eq!(out, "a\nB\nc\n");
    }

    #[test]
    fn chunks_change_context_disambiguates() {
        let original = "fn first() {\n    x = 1;\n}\nfn second() {\n    x = 1;\n}\n";
        let out = apply_context_chunks(
            original,
            &[chunk(Some("fn second() {"), &["    x = 1;"], &["    x = 2;"])],
            "f",
        )
        .unwrap();
        assert_eq!(out, "fn first() {\n    x = 1;\n}\nfn second() {\n    x = 2;\n}\n");
    }

    #[test]
    fn chunks_pure_insertion_at_eof() {
        let original = "a\nb\n";
        let out = apply_context_chunks(original, &[chunk(None, &[], &["c", "d"])], "f").unwrap();
        assert_eq!(out, "a\nb\nc\nd\n");
    }

    #[test]
    fn chunks_trailing_newline_sentinel_retry() {
        // The patch represents the file's final newline as a trailing empty
        // old/new line; the file itself has no empty last line.
        let original = "a\nb\n";
        let c = chunk(None, &["b", ""], &["B", ""]);
        let out = apply_context_chunks(original, &[c], "f").unwrap();
        assert_eq!(out, "a\nB\n");
    }

    #[test]
    fn chunks_crlf_file_lf_patch() {
        let original = "one\r\ntwo\r\nthree\r\n";
        let out = apply_context_chunks(original, &[chunk(None, &["two"], &["TWO"])], "f").unwrap();
        assert_eq!(out, "one\r\nTWO\r\nthree\r\n");
    }

    #[test]
    fn chunks_context_not_found_error_is_diagnostic() {
        let original = "a\nb\n";
        let err = apply_context_chunks(original, &[chunk(None, &["missing"], &["x"])], "src/f.rs").unwrap_err();
        assert!(err.contains("src/f.rs"));
        assert!(err.contains("missing"));
        assert!(err.contains("re-read the file"));
        let err = apply_context_chunks(original, &[chunk(Some("nowhere()"), &["a"], &["A"])], "f").unwrap_err();
        assert!(err.contains("nowhere()"));
    }

    #[test]
    fn chunks_eof_marker_targets_tail() {
        let original = "dup\nmid\ndup\n";
        let mut c = chunk(None, &["dup"], &["DUP"]);
        c.is_end_of_file = true;
        let out = apply_context_chunks(original, &[c], "f").unwrap();
        assert_eq!(out, "dup\nmid\nDUP\n");
    }

    // ---- execute (end to end) ----

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
    async fn execute_codex_add_update_delete_move() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("upd.txt"), "a\nb\n").unwrap();
        std::fs::write(dir.path().join("gone.txt"), "x\n").unwrap();
        std::fs::write(dir.path().join("old.txt"), "1\n2\n").unwrap();

        let patch = "*** Begin Patch\n\
                     *** Add File: fresh.txt\n\
                     +hello\n\
                     *** Update File: upd.txt\n\
                     @@\n\
                     -b\n\
                     +B\n\
                     *** Delete File: gone.txt\n\
                     *** Update File: old.txt\n\
                     *** Move to: renamed.txt\n\
                     @@\n\
                     -2\n\
                     +TWO\n\
                     *** End Patch\n";
        let result = ApplyPatchTool
            .execute(serde_json::json!({"patch": patch}), &ctx(dir.path()))
            .await
            .unwrap();

        assert!(result.contains("1 added"));
        assert!(result.contains("2 updated (1 moved)"));
        assert!(result.contains("1 deleted"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("fresh.txt")).unwrap(),
            "hello\n"
        );
        assert_eq!(std::fs::read_to_string(dir.path().join("upd.txt")).unwrap(), "a\nB\n");
        assert!(!dir.path().join("gone.txt").exists());
        assert!(!dir.path().join("old.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("renamed.txt")).unwrap(),
            "1\nTWO\n"
        );
    }

    #[tokio::test]
    async fn execute_codex_patch_shaped_like_real_model_output() {
        // Mirrors the shape gpt-5.6-sol actually emits: envelope + base_path,
        // "@@" with no context text, space-prefixed context lines with deep
        // indentation, non-ASCII content, several -/+ runs in one chunk.
        let dir = tempfile::tempdir().unwrap();
        let original = [
            "async def create_volcengine_asset(",
            "    session: SessionDep,",
            "    current_user: CurrentActiveUserDep,",
            "    request: VolcengineAssetCreateRequest,",
            ") -> AssetResponse:",
            "    \"\"\"",
            "    认证: 需要登录",
            "    权限: volcengine_assets:create:own",
            "    \"\"\"",
            "    user_file = await validate_user_file(session, current_user.id, request.file_id)",
            "    return await build_response(user_file)",
            "",
        ]
        .join("\n");
        std::fs::create_dir_all(dir.path().join("api/v1")).unwrap();
        std::fs::write(dir.path().join("api/v1/assets.py"), &original).unwrap();

        let patch = [
            "*** Begin Patch",
            "*** Update File: api/v1/assets.py",
            "@@",
            " async def create_volcengine_asset(",
            "     session: SessionDep,",
            "     current_user: CurrentActiveUserDep,",
            "     request: VolcengineAssetCreateRequest,",
            "-) -> AssetResponse:",
            "+) -> AssetCreateResponse:",
            "     \"\"\"",
            "     认证: 需要登录",
            "     权限: volcengine_assets:create:own",
            "     \"\"\"",
            "-    user_file = await validate_user_file(session, current_user.id, request.file_id)",
            "+    user_file = await validate_user_file(session, current_user.id, request.file_id)",
            "+    user_file_id = user_file.id",
            "*** End Patch",
        ]
        .join("\n");

        let result = ApplyPatchTool
            .execute(
                serde_json::json!({"patch": patch, "base_path": dir.path().to_string_lossy()}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(result.contains("1 updated"));

        let updated = std::fs::read_to_string(dir.path().join("api/v1/assets.py")).unwrap();
        assert!(updated.contains(") -> AssetCreateResponse:"));
        assert!(!updated.contains(") -> AssetResponse:"));
        assert!(updated.contains("    user_file_id = user_file.id"));
        assert!(updated.contains("    认证: 需要登录"));
        assert!(updated.contains("    return await build_response(user_file)"));
    }

    #[tokio::test]
    async fn execute_add_existing_file_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exists.txt"), "x\n").unwrap();

        let patch = "*** Begin Patch\n*** Add File: exists.txt\n+y\n*** End Patch\n";
        let err = ApplyPatchTool
            .execute(serde_json::json!({"patch": patch}), &ctx(dir.path()))
            .await
            .unwrap_err();
        assert!(err.contains("already exists"));
        assert_eq!(std::fs::read_to_string(dir.path().join("exists.txt")).unwrap(), "x\n");
    }

    #[tokio::test]
    async fn execute_move_dest_exists_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("src.txt"), "a\n").unwrap();
        std::fs::write(dir.path().join("dst.txt"), "keep\n").unwrap();

        let patch = "*** Begin Patch\n\
                     *** Update File: src.txt\n\
                     *** Move to: dst.txt\n\
                     @@\n\
                     -a\n\
                     +b\n\
                     *** End Patch\n";
        let err = ApplyPatchTool
            .execute(serde_json::json!({"patch": patch}), &ctx(dir.path()))
            .await
            .unwrap_err();
        assert!(err.contains("already exists"));
        assert_eq!(std::fs::read_to_string(dir.path().join("dst.txt")).unwrap(), "keep\n");
    }

    #[tokio::test]
    async fn execute_unified_diff_end_to_end_unbroken() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "line1\nold\nline3\n").unwrap();

        let patch = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n line1\n-old\n+new\n line3\n";
        let result = ApplyPatchTool
            .execute(serde_json::json!({"patch": patch}), &ctx(dir.path()))
            .await
            .unwrap();
        assert!(result.contains("1 updated"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "line1\nnew\nline3\n"
        );
    }
}
