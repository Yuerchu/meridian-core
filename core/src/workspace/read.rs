//! Reading a file for the viewer, through the handle that was checked.
//!
//! The panel opens files with nobody watching, so this is `verified::open_read`
//! territory: the I/O happens on the verified handle, never on a re-resolved
//! path. Limits are enforced here rather than in the UI because the UI cannot
//! refuse what has already crossed the IPC boundary.

use std::io::Read;
use std::path::Path;

use serde::Serialize;

/// Beyond this the viewer truncates, so the rest is never read or shipped.
const MAX_BYTES: u64 = 512 * 1024;
const MAX_LINES: usize = 5000;
/// How much of the head is inspected for NUL to call a file binary.
const SNIFF_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct FileContent {
    pub content: String,
    pub truncated: bool,
    pub total_lines: u64,
    pub size_bytes: u64,
    pub binary: bool,
}

pub fn read_file(root: &Path, rel_path: &str) -> Result<FileContent, String> {
    let joined = root.join(rel_path);
    let verified = crate::tools::verified::open_read(&joined, Some(root)).map_err(|e| e.message())?;
    let (mut file, _real) = verified.into_parts();

    let size_bytes = file.metadata().map_err(|e| e.to_string())?.len();

    let mut raw = Vec::new();
    (&mut file)
        .take(MAX_BYTES)
        .read_to_end(&mut raw)
        .map_err(|e| e.to_string())?;
    let mut truncated = size_bytes > MAX_BYTES;

    if raw[..raw.len().min(SNIFF_BYTES)].contains(&0) {
        // Binary: report the fact and ship no bytes. Half a megabyte of mojibake
        // helps nobody and the viewer draws an empty state instead.
        return Ok(FileContent {
            content: String::new(),
            truncated: false,
            total_lines: 0,
            size_bytes,
            binary: true,
        });
    }

    // `total_lines` is presented as the whole file's count, so for a
    // byte-truncated file the remainder is scanned — counted, never kept —
    // rather than the prefix's count being passed off as the total.
    let mut newlines = raw.iter().filter(|&&b| b == b'\n').count() as u64;
    let mut last_byte = raw.last().copied();
    if truncated {
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = std::io::Read::read(&mut file, &mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            newlines += buf[..n].iter().filter(|&&b| b == b'\n').count() as u64;
            last_byte = Some(buf[n - 1]);
        }
    }
    let total_lines = newlines + u64::from(last_byte.is_some_and(|b| b != b'\n'));

    let text = String::from_utf8_lossy(&raw);
    let content = if total_lines as usize > MAX_LINES {
        truncated = true;
        text.lines().take(MAX_LINES).collect::<Vec<_>>().join("\n")
    } else {
        text.into_owned()
    };

    Ok(FileContent {
        content,
        truncated,
        total_lines,
        size_bytes,
        binary: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_file_comes_back_whole() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::tools::verified::resolve_root(dir.path()).unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();

        let got = read_file(&root, "a.txt").unwrap();
        assert_eq!(got.content, "one\ntwo\n");
        assert!(!got.truncated);
        assert!(!got.binary);
        assert_eq!(got.total_lines, 2);
    }

    #[test]
    fn a_nul_in_the_head_means_binary_and_no_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::tools::verified::resolve_root(dir.path()).unwrap();
        std::fs::write(root.join("blob.bin"), b"PNG\x00\x01\x02").unwrap();

        let got = read_file(&root, "blob.bin").unwrap();
        assert!(got.binary);
        assert!(got.content.is_empty());
        assert_eq!(got.size_bytes, 6);
    }

    #[test]
    fn line_cap_truncates_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::tools::verified::resolve_root(dir.path()).unwrap();
        let many: String = (0..MAX_LINES + 10).map(|i| format!("line {i}\n")).collect();
        std::fs::write(root.join("big.txt"), &many).unwrap();

        let got = read_file(&root, "big.txt").unwrap();
        assert!(got.truncated);
        assert_eq!(got.content.lines().count(), MAX_LINES);
        assert_eq!(got.total_lines, (MAX_LINES + 10) as u64);
    }

    #[test]
    fn outside_the_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = crate::tools::verified::resolve_root(dir.path()).unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s").unwrap();

        let rel = format!("../{}", outside.path().file_name().unwrap().to_string_lossy());
        assert!(read_file(&root, &format!("{rel}/secret.txt")).is_err());
    }
}
