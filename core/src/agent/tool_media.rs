//! Media a tool hands back, from the moment the loop receives it to every
//! later reader of the row.
//!
//! A tool row without media is the plain text it always was, byte for byte. A
//! tool row with media stores the same parts envelope a user message with
//! attachments does — `[{"type":"text",…},{"type":"image_url",…}]`, every
//! picture as a `file://` reference into the conversation's own attachment
//! directory — and is marked `schema_version = 3`. Only the turn loop writes
//! that version, so text a tool or an MCP server returns that happens to look
//! like the envelope is still text: the discriminator is not something the
//! content can forge. Base64 never reaches the row; it is produced from the
//! file each time a request is assembled, which is what keeps the replayed
//! bytes identical to the first send (CLAUDE.md, "What was sent is never
//! dropped or rewritten").

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::provider::{MessageContentPart, MessageContentUrl, MessageFileContent};
use crate::tools::MediaOut;

/// `messages.schema_version` of a tool row whose content is the parts
/// envelope. Version 2's message format otherwise.
pub const TOOL_PARTS_SCHEMA_VERSION: i32 = 3;

/// What kind of thing a stored media part is, for the window drawing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMediaKind {
    Image,
    Video,
    Audio,
    File,
}

/// One media item of a tool result, as the window is shown it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolMediaResponse {
    pub kind: ToolMediaKind,
    /// A `file://` URI inside the conversation's attachment directory.
    pub url: String,
    pub mime_type: String,
    pub name: Option<String>,
}

/// A tool row split into what the model read as text and the media beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRowView {
    pub text: String,
    pub media: Vec<ToolMediaResponse>,
}

/// A tool result ready to be written: the row's content and version, and the
/// media for the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredToolResult {
    pub content: String,
    pub schema_version: i32,
    pub media: Vec<ToolMediaResponse>,
}

fn kind_of(mime: &str) -> ToolMediaKind {
    match mime.split('/').next() {
        Some("image") => ToolMediaKind::Image,
        Some("video") => ToolMediaKind::Video,
        Some("audio") => ToolMediaKind::Audio,
        _ => ToolMediaKind::File,
    }
}

fn mime_of_uri(uri: &str) -> String {
    mime_guess::from_path(uri).first_or_octet_stream().to_string()
}

/// Put a tool's media into the conversation's attachment directory and build
/// the row for it. `text` is the already-truncated text; media never takes
/// part in truncation.
///
/// Fails, as a whole, on anything that cannot be stored exactly as given — an
/// unknown type, a file over its cap, a `ManagedFile` outside this
/// conversation's directory — and the caller turns that into an error result.
/// Sending the rest would have the model describe a picture it was never shown.
pub fn store(
    files_root: Option<&Path>,
    conversation_id: &str,
    text: &str,
    media: Vec<MediaOut>,
) -> Result<StoredToolResult, String> {
    if media.is_empty() {
        return Ok(StoredToolResult {
            content: text.to_owned(),
            schema_version: 2,
            media: Vec::new(),
        });
    }
    let files_root = files_root.ok_or("this surface has no attachment directory to store tool media in")?;
    if media.len() > crate::files::MAX_TOOL_MEDIA_ITEMS {
        return Err(format!(
            "the tool returned {} media items; at most {} are accepted",
            media.len(),
            crate::files::MAX_TOOL_MEDIA_ITEMS
        ));
    }
    let mut items = Vec::with_capacity(media.len());
    for item in media {
        let (uri, mime, name) = match item {
            MediaOut::Bytes { mime, bytes, name } => {
                let ext = crate::files::media_extension(&mime)
                    .ok_or_else(|| format!("the tool returned media of unsupported type {mime:?}"))?;
                check_size(&mime, bytes.len() as u64)?;
                let (path, uri) = crate::files::alloc_in_files_root(files_root, conversation_id, ext)?;
                std::fs::write(&path, &bytes).map_err(|e| format!("could not store tool media: {e}"))?;
                (uri, mime, name)
            }
            MediaOut::ManagedFile { uri, mime, name } => {
                let path = crate::files::resolve_conversation_file(&uri, files_root, conversation_id)
                    .ok_or("the tool returned a file outside the conversation's attachment directory")?;
                crate::files::media_extension(&mime)
                    .ok_or_else(|| format!("the tool returned media of unsupported type {mime:?}"))?;
                let len = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
                check_size(&mime, len)?;
                (uri, mime, name)
            }
        };
        items.push(ToolMediaResponse {
            kind: kind_of(&mime),
            url: uri,
            mime_type: mime,
            name,
        });
    }
    let mut parts = Vec::with_capacity(items.len() + 1);
    if !text.is_empty() {
        parts.push(MessageContentPart::Text { text: text.to_owned() });
    }
    for item in &items {
        parts.push(match item.kind {
            ToolMediaKind::Image => MessageContentPart::ImageUrl {
                image_url: MessageContentUrl { url: item.url.clone() },
            },
            _ => MessageContentPart::File {
                file: MessageFileContent {
                    url: item.url.clone(),
                    mime_type: item.mime_type.clone(),
                    name: item.name.clone().unwrap_or_default(),
                },
            },
        });
    }
    Ok(StoredToolResult {
        content: crate::provider::encode_message_parts(&parts)?,
        schema_version: TOOL_PARTS_SCHEMA_VERSION,
        media: items,
    })
}

fn check_size(mime: &str, len: u64) -> Result<(), String> {
    let cap = match kind_of(mime) {
        ToolMediaKind::Image => crate::files::MAX_INLINE_ATTACHMENT_BYTES,
        _ => crate::files::MAX_TOOL_MEDIA_BYTES,
    };
    if len > cap {
        return Err(format!("the tool returned a {mime} of {len} bytes; the limit is {cap}"));
    }
    Ok(())
}

/// A stored tool row as text and media. Anything other than a version-3 row
/// is text whatever it looks like.
pub fn view(content: &str, schema_version: i32) -> Result<ToolRowView, String> {
    if schema_version != TOOL_PARTS_SCHEMA_VERSION {
        return Ok(ToolRowView {
            text: content.to_owned(),
            media: Vec::new(),
        });
    }
    let parts = crate::provider::decode_message_parts(content)?
        .ok_or("a tool row of schema_version 3 does not hold a parts envelope")?;
    let mut texts = Vec::new();
    let mut media = Vec::new();
    for part in parts {
        match part {
            MessageContentPart::Text { text } => texts.push(text),
            MessageContentPart::ImageUrl { image_url } => media.push(ToolMediaResponse {
                kind: ToolMediaKind::Image,
                mime_type: mime_of_uri(&image_url.url),
                url: image_url.url,
                name: None,
            }),
            MessageContentPart::File { file } => media.push(ToolMediaResponse {
                kind: kind_of(&file.mime_type),
                url: file.url,
                mime_type: file.mime_type,
                name: (!file.name.is_empty()).then_some(file.name),
            }),
            MessageContentPart::Sticker { .. } => return Err("a tool row holds a sticker part".into()),
        }
    }
    Ok(ToolRowView {
        text: texts.join("\n"),
        media,
    })
}

/// A stored tool row as the text a summary, an excerpt or a reviewer reads:
/// the text, and a count where the media was. Never a URI, never base64.
pub fn result_text(content: &str, schema_version: i32) -> String {
    if schema_version != TOOL_PARTS_SCHEMA_VERSION {
        return content.to_owned();
    }
    match view(content, schema_version) {
        Ok(view) if view.media.is_empty() => view.text,
        Ok(view) if view.text.is_empty() => format!("[{} media]", view.media.len()),
        Ok(view) => format!("{} [+{} media]", view.text, view.media.len()),
        Err(_) => "[unreadable tool result]".to_owned(),
    }
}

/// [`result_text`] for a message already in a request.
pub(crate) fn message_text(message: &crate::provider::ChatMessage) -> String {
    if message.origin == crate::provider::MessageOrigin::ToolParts {
        result_text(&message.content, TOOL_PARTS_SCHEMA_VERSION)
    } else {
        message.content.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(name: Option<&str>) -> MediaOut {
        MediaOut::Bytes {
            mime: "image/png".into(),
            bytes: b"PNGDATA".to_vec(),
            name: name.map(str::to_owned),
        }
    }

    #[test]
    fn text_alone_is_stored_as_the_text_it_always_was() {
        let stored = store(None, "c", r#"[{"a":1}]"#, Vec::new()).unwrap();
        assert_eq!(stored.content, r#"[{"a":1}]"#);
        assert_eq!(stored.schema_version, 2);
    }

    #[test]
    fn bytes_land_in_the_conversation_directory_as_a_file_reference() {
        let dir = tempfile::tempdir().unwrap();
        let stored = store(Some(dir.path()), "c", "done", vec![png(None)]).unwrap();
        assert_eq!(stored.schema_version, TOOL_PARTS_SCHEMA_VERSION);
        assert_eq!(stored.media.len(), 1);
        assert_eq!(stored.media[0].kind, ToolMediaKind::Image);
        let path = crate::files::resolve_conversation_file(&stored.media[0].url, dir.path(), "c").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"PNGDATA");
        assert!(stored.content.contains("file://") && !stored.content.contains("base64"));
        let view = view(&stored.content, stored.schema_version).unwrap();
        assert_eq!(view.text, "done");
        assert_eq!(view.media, stored.media);
        assert_eq!(result_text(&stored.content, stored.schema_version), "done [+1 media]");
    }

    #[test]
    fn a_managed_file_from_another_conversation_fails_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = store(Some(dir.path()), "other", "", vec![png(None)]).unwrap();
        let error = store(
            Some(dir.path()),
            "c",
            "here it is",
            vec![MediaOut::ManagedFile {
                uri: elsewhere.media[0].url.clone(),
                mime: "image/png".into(),
                name: None,
            }],
        )
        .unwrap_err();
        assert!(error.contains("outside the conversation"), "{error}");
    }

    #[test]
    fn an_unknown_type_or_too_many_items_fail_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let tiff = MediaOut::Bytes {
            mime: "image/tiff".into(),
            bytes: vec![1],
            name: None,
        };
        assert!(store(Some(dir.path()), "c", "", vec![tiff]).is_err());
        let many = (0..=crate::files::MAX_TOOL_MEDIA_ITEMS).map(|_| png(None)).collect();
        assert!(store(Some(dir.path()), "c", "", many).is_err());
        assert!(store(None, "c", "", vec![png(None)]).is_err());
    }

    #[test]
    fn a_video_is_a_file_part_with_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let video = MediaOut::Bytes {
            mime: "video/mp4".into(),
            bytes: vec![0; 16],
            name: Some("clip.mp4".into()),
        };
        let stored = store(Some(dir.path()), "c", "", vec![video]).unwrap();
        let view = view(&stored.content, stored.schema_version).unwrap();
        assert_eq!(view.media[0].kind, ToolMediaKind::Video);
        assert_eq!(view.media[0].name.as_deref(), Some("clip.mp4"));
        assert_eq!(result_text(&stored.content, stored.schema_version), "[1 media]");
    }
}
