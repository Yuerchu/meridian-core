use super::{Permission, Tool, ToolContext};
use async_trait::async_trait;
use regex::Regex;

pub struct SearchFilesTool;

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn description(&self) -> &str {
        "Search for a text pattern in files under a directory, or in a single file. Returns matching lines with file paths and line numbers. Supports regex patterns."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Text or regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Absolute path to the directory to search in, or to a single file to search"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of matching lines to return (default: 50)"
                }
            },
            "required": ["pattern", "path"]
        })
    }

    /// Ask rather than Always, for the same reason as `list_directory`: pointed
    /// at an absolute path this greps whatever it is given. Searching inside
    /// the project still does not prompt.
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
        let pattern = args["pattern"]
            .as_str()
            .ok_or("missing 'pattern' argument")?
            .to_string();
        let path_str = args["path"].as_str().ok_or("missing 'path' argument")?;
        let path_buf = match context.resolve_and_validate(path_str)? {
            super::ResolvedTarget::Real(p) => p,
            super::ResolvedTarget::Saf { .. } => {
                return Err("recursive search is not supported in SAF-authorized directories; \
                     enable 'All files access' in Settings to search there"
                    .to_string());
            }
        };
        let max_results = args["max_results"].as_u64().unwrap_or(50) as usize;

        tokio::task::spawn_blocking(move || search(&path_buf, &pattern, max_results))
            .await
            .map_err(|e| format!("task failed: {e}"))?
    }
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "dist",
    "build",
    ".next",
    ".nuxt",
    "vendor",
];

const MAX_DEPTH: usize = 10;
const MAX_LINE_LEN: usize = 500;

fn search(root: &std::path::Path, pattern: &str, max_results: usize) -> Result<String, String> {
    let re = Regex::new(pattern).map_err(|e| format!("invalid regex pattern: {e}"))?;

    let mut matches = Vec::new();
    // Models routinely aim this at one file rather than a tree. Walking a file
    // would surface as a bare "os error 3" on Windows, so search it directly.
    let meta = std::fs::metadata(root).map_err(|e| format!("failed to read '{}': {}", root.display(), e))?;
    if meta.is_file() {
        search_file(root, &re, max_results, &mut matches);
    } else {
        walk_and_search(root, &re, 0, max_results, &mut matches)?;
    }

    if matches.is_empty() {
        return Ok("No matches found.".to_string());
    }

    let total = matches.len();
    let truncated = total >= max_results;
    let mut result = matches.join("\n");
    if truncated {
        result.push_str(&format!("\n\n(showing first {max_results} matches)"));
    }
    Ok(result)
}

fn walk_and_search(
    dir: &std::path::Path,
    re: &Regex,
    depth: usize,
    max_results: usize,
    matches: &mut Vec<String>,
) -> Result<(), String> {
    if depth > MAX_DEPTH || matches.len() >= max_results {
        return Ok(());
    }

    let entries = std::fs::read_dir(dir).map_err(|e| format!("failed to read '{}': {}", dir.display(), e))?;

    for entry in entries {
        if matches.len() >= max_results {
            break;
        }

        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with('.') && depth > 0 {
            continue;
        }
        if SKIP_DIRS.contains(&name_str.as_ref()) {
            continue;
        }

        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            walk_and_search(&entry.path(), re, depth + 1, max_results, matches)?;
        } else if file_type.is_file() {
            search_file(&entry.path(), re, max_results, matches);
        }
    }

    Ok(())
}

const MAX_FILE_SIZE: u64 = 2 * 1024 * 1024; // 2 MB

fn search_file(path: &std::path::Path, re: &Regex, max_results: usize, matches: &mut Vec<String>) {
    // Skip files larger than 2 MB
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    if meta.len() > MAX_FILE_SIZE {
        return;
    }

    // Read first 512 bytes to probe for binary content (NUL byte)
    let probe = match std::fs::File::open(path) {
        Ok(mut f) => {
            use std::io::Read;
            let mut buf = [0u8; 512];
            match f.read(&mut buf) {
                Ok(n) => buf[..n].to_vec(),
                Err(_) => return,
            }
        }
        Err(_) => return,
    };
    if probe.contains(&0) {
        return;
    }

    let content = match std::fs::read(path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let text = match std::str::from_utf8(&content) {
        Ok(t) => t,
        Err(_) => return,
    };

    let display_path = path.display().to_string();

    for (line_num, line) in text.lines().enumerate() {
        if matches.len() >= max_results {
            break;
        }
        if re.is_match(line) {
            let display_line = if line.len() > MAX_LINE_LEN {
                format!("{}...", crate::util::take_bytes_at_char_boundary(line, MAX_LINE_LEN))
            } else {
                line.to_string()
            };
            matches.push(format!("{}:{}:{}", display_path, line_num + 1, display_line));
        }
    }
}
