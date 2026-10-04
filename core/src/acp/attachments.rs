//! A stored message's attachments, as the blocks of an ACP prompt.
//!
//! The composer stores a message with attachments as the parts envelope
//! (`provider::decode_message_parts`), and the transcript row keeps exactly
//! that. What goes to the agent is different: the text, then for each
//! attachment a short label and one block. Three things decide which block.
//!
//! **What the adapter does with it.** `claude-agent-acp` turns `image` into a
//! Claude image, `resource` with text into a `<context>` block, and
//! `resource_link` into the text `[@name](uri)` — and it drops a blob
//! `resource` without a word. So there is no way to hand it a PDF's bytes: a
//! file that is neither an image Claude takes inline nor text goes as a link,
//! and the agent reads it itself, through its own `Read`, which asks.
//!
//! **What the agent said it takes.** `image` and `resource` are allowed only
//! where [`PromptCapabilities`] said yes, and an attachment that needs one the
//! agent did not advertise is refused rather than sent some other way. The
//! refusal names the capability; the alternative is a prompt that means
//! something different from what the person sent, with nothing said.
//!
//! **Where the agent is.** A link is a path, and a containerised adapter sees
//! only what its command mounts. A path that maps nowhere there is refused —
//! the agent would be told about a file it cannot open. Inline blocks carry
//! their bytes and cross the boundary as they are.
//!
//! The label exists because the adapter ignores a link's `name`, so the only
//! name the model would see is the stored `<uuid>.<ext>`; and because the
//! person's "the second file" has to have something to refer to.

use std::fmt;
use std::path::{Path, PathBuf};

use super::mounts::MountMap;
use super::protocol::{PromptBlock, PromptCapabilities, TextResource};
use crate::provider::{self, MessageContentPart};

/// The largest image sent inline: the Messages API's own limit for one image.
/// A larger one goes as a link, and Claude Code's `Read` downsizes it.
pub(super) const MAX_INLINE_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// The largest text file embedded rather than linked. Embedded text is in the
/// prompt whole, so a log file attached in passing would spend the context on
/// itself before the agent read a word of the question; past this the agent
/// gets a link and reads as much as it needs.
pub(super) const MAX_EMBEDDED_TEXT_BYTES: u64 = 256 * 1024;

/// What Claude accepts as an inline image. Anything else that is an image —
/// svg, bmp, heic — is text (svg) or a link.
const INLINE_IMAGE_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Where attachments live and how the agent names them.
pub(super) struct Reach<'a> {
    /// `<data_dir>/files`: nothing outside it is read, whatever a part says.
    pub files_root: &'a Path,
    pub mounts: &'a MountMap,
    /// Whether the adapter runs behind a container launcher, where a path the
    /// mounts do not cover does not exist.
    pub containerised: bool,
}

/// A message, split into what the person wrote and what they attached.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Prepared {
    /// The text parts joined, or the whole content when it is plain text.
    pub text: String,
    /// A label and a block per attachment, in the order they were attached.
    pub attachments: Vec<PromptBlock>,
}

/// Why a message cannot go to this agent as it is. `ordinal` counts from 1,
/// the way the person numbered their attachments.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    Malformed(String),
    Sticker,
    Unreadable { ordinal: usize },
    MissingCapability { ordinal: usize, capability: &'static str },
    NotInContainer { ordinal: usize },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Malformed(e) => write!(f, "{e}"),
            Refusal::Sticker => f.write_str("Claude Code sessions cannot take stickers."),
            Refusal::Unreadable { ordinal } => {
                write!(f, "Attachment {ordinal} could not be read; it may have been removed.")
            }
            Refusal::MissingCapability { ordinal, capability } => write!(
                f,
                "Attachment {ordinal} needs the agent to accept `{capability}` prompt content, which this adapter \
                 does not advertise."
            ),
            Refusal::NotInContainer { ordinal } => write!(
                f,
                "Attachment {ordinal} is not inside any directory the agent's container mounts, so it cannot open it."
            ),
        }
    }
}

/// Split a stored message into its text and its attachment blocks.
///
/// Plain text comes back as itself with no attachments, so a message without
/// any reaches the agent byte for byte as before. Reads files, so call it off
/// the async runtime.
pub(super) fn prepare(content: &str, caps: PromptCapabilities, reach: &Reach<'_>) -> Result<Prepared, Refusal> {
    let Some(parts) = provider::decode_message_parts(content).map_err(Refusal::Malformed)? else {
        return Ok(Prepared {
            text: content.to_string(),
            attachments: Vec::new(),
        });
    };
    let mut texts = Vec::new();
    let mut attachments = Vec::new();
    let mut ordinal = 0;
    for part in parts {
        match part {
            MessageContentPart::Text { text } => texts.push(text),
            MessageContentPart::Sticker { .. } => return Err(Refusal::Sticker),
            MessageContentPart::ImageUrl { image_url } => {
                ordinal += 1;
                let path = resolve(&image_url.url, reach.files_root, ordinal)?;
                let mime = mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_string();
                let name = file_name(&path);
                attachments.push(label(ordinal, "image"));
                attachments.push(block(&path, &mime, &name, caps, reach, ordinal)?);
            }
            MessageContentPart::File { file } => {
                ordinal += 1;
                let path = resolve(&file.url, reach.files_root, ordinal)?;
                attachments.push(label(ordinal, &file.name));
                attachments.push(block(&path, &file.mime_type, &file.name, caps, reach, ordinal)?);
            }
        }
    }
    Ok(Prepared {
        text: texts.join("\n"),
        attachments,
    })
}

fn label(ordinal: usize, name: &str) -> PromptBlock {
    PromptBlock::text(format!("[Attachment {ordinal}: {name}]"))
}

/// Containment without the 20 MiB inlining cap: a link reads nothing, so a
/// large PDF is exactly what it is for, and the two inline kinds below carry
/// limits of their own well under that cap.
fn resolve(uri: &str, files_root: &Path, ordinal: usize) -> Result<PathBuf, Refusal> {
    crate::files::resolve_managed_file(uri, files_root).ok_or(Refusal::Unreadable { ordinal })
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The block one attachment travels as: an inline image, embedded text, or a
/// link — tried in that order, each only where it fits.
fn block(
    path: &Path,
    mime: &str,
    name: &str,
    caps: PromptCapabilities,
    reach: &Reach<'_>,
    ordinal: usize,
) -> Result<PromptBlock, Refusal> {
    let size = std::fs::metadata(path)
        .map_err(|_| Refusal::Unreadable { ordinal })?
        .len();
    if INLINE_IMAGE_TYPES.contains(&mime) && size <= MAX_INLINE_IMAGE_BYTES {
        if !caps.image {
            return Err(Refusal::MissingCapability {
                ordinal,
                capability: "image",
            });
        }
        use base64::Engine;
        let bytes = std::fs::read(path).map_err(|_| Refusal::Unreadable { ordinal })?;
        return Ok(PromptBlock::Image {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            mime_type: mime.to_string(),
        });
    }
    if size <= MAX_EMBEDDED_TEXT_BYTES {
        let bytes = std::fs::read(path).map_err(|_| Refusal::Unreadable { ordinal })?;
        if let Some(text) = as_text(bytes) {
            if !caps.embedded_context {
                return Err(Refusal::MissingCapability {
                    ordinal,
                    capability: "embeddedContext",
                });
            }
            return Ok(PromptBlock::Resource {
                resource: TextResource {
                    uri: file_uri(&agent_path(path, reach).unwrap_or_else(|| host_path(path))),
                    text,
                    mime_type: None,
                },
            });
        }
    }
    let Some(seen) = agent_path(path, reach) else {
        return Err(Refusal::NotInContainer { ordinal });
    };
    Ok(PromptBlock::ResourceLink {
        uri: file_uri(&seen),
        name: name.to_string(),
        mime_type: Some(mime.to_string()),
    })
}

/// The file as text, when it is text.
///
/// Decided by the bytes rather than the MIME type, because the type comes from
/// the extension and the extension lies in the cases that matter: `.ts` is
/// `video/mp2t` to `mime_guess`, so a type list would link every TypeScript
/// file and embed nothing a programmer attaches. UTF-8 with no NUL is the
/// usual test for "a person could read this".
fn as_text(bytes: Vec<u8>) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// How the agent names this host path: translated through the mounts when it
/// runs in a container, `None` when no mount covers it; the host path as it is
/// otherwise.
fn agent_path(path: &Path, reach: &Reach<'_>) -> Option<String> {
    let host = host_path(path);
    if reach.containerised {
        reach.mounts.to_container(Path::new(&host))
    } else {
        Some(host)
    }
}

/// The path without Windows' verbatim `\\?\` prefix, which
/// `resolve_attachment_uri`'s canonicalisation adds and which turns into
/// `file://///?/C:/…` — a URI nothing on the other side resolves.
fn host_path(path: &Path) -> String {
    #[cfg(windows)]
    let path = dunce::simplified(path);
    path.to_string_lossy().into_owned()
}

fn file_uri(path: &str) -> String {
    let path = path.replace('\\', "/");
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    const BOTH: PromptCapabilities = PromptCapabilities {
        image: true,
        audio: false,
        embedded_context: true,
    };

    /// A files root with one conversation directory, canonicalised the way
    /// `resolve_attachment_uri` will see it (a temp dir can be an 8.3 short
    /// name on Windows, which canonicalisation expands).
    struct Root {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    impl Root {
        fn new() -> Root {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("files");
            std::fs::create_dir_all(root.join("c1")).unwrap();
            let root = PathBuf::from(host_path(&std::fs::canonicalize(&root).unwrap()));
            Root { _dir: dir, root }
        }

        /// Write a file and hand back its stored URI.
        fn put(&self, name: &str, bytes: &[u8]) -> String {
            let path = self.root.join("c1").join(name);
            std::fs::write(&path, bytes).unwrap();
            format!("file:///{}", path.to_string_lossy().replace('\\', "/"))
        }

        fn reach<'a>(&'a self, mounts: &'a MountMap, containerised: bool) -> Reach<'a> {
            Reach {
                files_root: &self.root,
                mounts,
                containerised,
            }
        }
    }

    fn file(uri: &str, mime: &str, name: &str) -> serde_json::Value {
        serde_json::json!({"type": "file", "file": {"url": uri, "mime_type": mime, "name": name}})
    }

    fn image(uri: &str) -> serde_json::Value {
        serde_json::json!({"type": "image_url", "image_url": {"url": uri}})
    }

    fn envelope(parts: Vec<serde_json::Value>) -> String {
        serde_json::to_string(&parts).unwrap()
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    const PDF: &[u8] = b"%PDF-1.7\n\0\x01\x02binary";

    #[test]
    fn plain_text_is_the_message_and_nothing_else() {
        let root = Root::new();
        let none = MountMap::default();
        for text in ["fix the queue", "[QQ] looks like a label", ""] {
            let prepared = prepare(text, BOTH, &root.reach(&none, false)).unwrap();
            assert_eq!(prepared.text, text);
            assert!(prepared.attachments.is_empty());
        }
    }

    /// The order the person attached them in, each behind its label: an inline
    /// image, embedded text, and a link carrying the original name. The image
    /// is bare base64 — a `data:` URI there is not an image to the adapter.
    #[test]
    fn each_attachment_goes_as_what_it_is_behind_a_numbered_label() {
        let root = Root::new();
        let none = MountMap::default();
        let png = root.put("a.png", PNG);
        let ts = root.put("b.ts", b"export const a = 1\n");
        let pdf = root.put("c.pdf", PDF);
        let content = envelope(vec![
            serde_json::json!({"type": "text", "text": "compare these"}),
            image(&png),
            file(&ts, "video/mp2t", "queue.ts"),
            file(&pdf, "application/pdf", "report.pdf"),
        ]);
        let prepared = prepare(&content, BOTH, &root.reach(&none, false)).unwrap();
        assert_eq!(prepared.text, "compare these");

        let pdf_path = host_path(&root.root.join("c1").join("c.pdf"));
        assert_eq!(
            prepared.attachments,
            vec![
                PromptBlock::text("[Attachment 1: image]"),
                PromptBlock::Image {
                    data: base64::engine::general_purpose::STANDARD.encode(PNG),
                    mime_type: "image/png".into(),
                },
                PromptBlock::text("[Attachment 2: queue.ts]"),
                PromptBlock::Resource {
                    resource: TextResource {
                        uri: file_uri(&host_path(&root.root.join("c1").join("b.ts"))),
                        text: "export const a = 1\n".into(),
                        mime_type: None,
                    },
                },
                PromptBlock::text("[Attachment 3: report.pdf]"),
                PromptBlock::ResourceLink {
                    uri: file_uri(&pdf_path),
                    name: "report.pdf".into(),
                    mime_type: Some("application/pdf".into()),
                },
            ]
        );
        if let PromptBlock::ResourceLink { uri, .. } = &prepared.attachments[5] {
            assert!(!uri.contains("?"), "the verbatim prefix leaked into the link: {uri}");
        }
    }

    /// Only jpeg/png/gif/webp under 5 MiB are images to Claude. An svg is
    /// text; a big png is a link the agent's `Read` will downsize.
    #[test]
    fn an_image_claude_cannot_take_inline_is_text_or_a_link() {
        let root = Root::new();
        let none = MountMap::default();
        let svg = root.put("d.svg", b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>");
        let big = root.put("e.png", &vec![0u8; (MAX_INLINE_IMAGE_BYTES + 1) as usize]);
        let content = envelope(vec![image(&svg), image(&big)]);
        let prepared = prepare(&content, BOTH, &root.reach(&none, false)).unwrap();
        assert!(matches!(prepared.attachments[1], PromptBlock::Resource { .. }));
        assert!(matches!(prepared.attachments[3], PromptBlock::ResourceLink { .. }));
    }

    /// A file past the 20 MiB inlining cap is still linked: a link reads
    /// nothing, and big documents are what it is for.
    #[test]
    fn a_file_past_the_inlining_cap_is_still_linked() {
        let root = Root::new();
        let none = MountMap::default();
        let big = root.put(
            "h.pdf",
            &vec![0u8; (crate::files::MAX_INLINE_ATTACHMENT_BYTES + 1) as usize],
        );
        let prepared = prepare(
            &envelope(vec![file(&big, "application/pdf", "manual.pdf")]),
            BOTH,
            &root.reach(&none, false),
        )
        .unwrap();
        assert!(matches!(prepared.attachments[1], PromptBlock::ResourceLink { .. }));
    }

    /// Text past the embedding limit, and anything that is not UTF-8 without
    /// a NUL, is linked rather than embedded.
    #[test]
    fn long_or_binary_text_is_linked() {
        let root = Root::new();
        let none = MountMap::default();
        let long = root.put("f.log", &vec![b'a'; (MAX_EMBEDDED_TEXT_BYTES + 1) as usize]);
        let latin1 = root.put("g.txt", b"caf\xe9");
        let content = envelope(vec![
            file(&long, "text/plain", "f.log"),
            file(&latin1, "text/plain", "g.txt"),
        ]);
        let prepared = prepare(&content, BOTH, &root.reach(&none, false)).unwrap();
        assert!(matches!(prepared.attachments[1], PromptBlock::ResourceLink { .. }));
        assert!(matches!(prepared.attachments[3], PromptBlock::ResourceLink { .. }));
    }
    #[test]
    fn a_sticker_is_refused() {
        let root = Root::new();
        let none = MountMap::default();
        let content = envelope(vec![serde_json::json!({"type": "sticker", "sticker_id": "s1"})]);
        assert_eq!(
            prepare(&content, BOTH, &root.reach(&none, false)),
            Err(Refusal::Sticker)
        );
    }

    /// An attachment needing a capability the agent did not advertise is
    /// refused, naming it — not sent as a link instead.
    #[test]
    fn a_missing_capability_refuses_rather_than_degrades() {
        let root = Root::new();
        let none = MountMap::default();
        let png = root.put("a.png", PNG);
        let txt = root.put("b.txt", b"notes");
        let no_image = PromptCapabilities { image: false, ..BOTH };
        let no_context = PromptCapabilities {
            embedded_context: false,
            ..BOTH
        };
        assert_eq!(
            prepare(&envelope(vec![image(&png)]), no_image, &root.reach(&none, false)),
            Err(Refusal::MissingCapability {
                ordinal: 1,
                capability: "image"
            })
        );
        assert_eq!(
            prepare(
                &envelope(vec![image(&png), file(&txt, "text/plain", "b.txt")]),
                no_context,
                &root.reach(&none, false)
            ),
            Err(Refusal::MissingCapability {
                ordinal: 2,
                capability: "embeddedContext"
            })
        );
    }

    /// Behind a container a link is a container path, and one the mounts do
    /// not cover is refused. An inline image carries its bytes and needs no
    /// mount at all.
    #[test]
    fn a_container_sees_links_through_its_mounts() {
        let root = Root::new();
        let pdf = root.put("c.pdf", PDF);
        let png = root.put("a.png", PNG);
        let content = envelope(vec![file(&pdf, "application/pdf", "report.pdf")]);

        let unmounted = MountMap::from_command("docker", &["run".into(), "-i".into(), "agent".into()]);
        assert_eq!(
            prepare(&content, BOTH, &root.reach(&unmounted, true)),
            Err(Refusal::NotInContainer { ordinal: 1 })
        );
        assert!(prepare(&envelope(vec![image(&png)]), BOTH, &root.reach(&unmounted, true)).is_ok());

        let spec = format!("{}:/data", root.root.to_string_lossy());
        let mounted = MountMap::from_command("docker", &["run".into(), "-v".into(), spec, "agent".into()]);
        let prepared = prepare(&content, BOTH, &root.reach(&mounted, true)).unwrap();
        let PromptBlock::ResourceLink { uri, .. } = &prepared.attachments[1] else {
            panic!("expected a link, got {:?}", prepared.attachments[1]);
        };
        assert_eq!(uri, "file:///data/c1/c.pdf");
    }

    /// Nothing outside the files root is read, whatever the stored part says —
    /// message content is model-influenced.
    #[test]
    fn a_file_outside_the_files_root_is_unreadable() {
        let root = Root::new();
        let none = MountMap::default();
        let outside = root.root.parent().unwrap().join("secret.txt");
        std::fs::write(&outside, b"secret").unwrap();
        let uri = format!("file:///{}", outside.to_string_lossy().replace('\\', "/"));
        assert_eq!(
            prepare(
                &envelope(vec![file(&uri, "text/plain", "secret.txt")]),
                BOTH,
                &root.reach(&none, false)
            ),
            Err(Refusal::Unreadable { ordinal: 1 })
        );
    }
}
