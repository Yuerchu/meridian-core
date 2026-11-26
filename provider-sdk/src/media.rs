//! Canonical media MIME vocabulary shared across provider capabilities,
//! wire adapters, and attachment storage.

/// Largest attachment that may be inlined (base64) into a provider request.
pub const MAX_INLINE_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// Canonical IANA types Meridian can store under `{data_dir}/files`.
///
/// A model's `readable_mime_types` is a subset of this list: a type we cannot
/// store cannot be declared readable. Aliases (`image/jpg`, `audio/mp3`) fold
/// into the canonical member at parse time and never appear as the stored value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub enum ReadableMime {
    #[serde(rename = "image/jpeg")]
    ImageJpeg,
    #[serde(rename = "image/png")]
    ImagePng,
    #[serde(rename = "image/gif")]
    ImageGif,
    #[serde(rename = "image/webp")]
    ImageWebp,
    #[serde(rename = "video/mp4")]
    VideoMp4,
    #[serde(rename = "video/webm")]
    VideoWebm,
    #[serde(rename = "audio/mpeg")]
    AudioMpeg,
    #[serde(rename = "audio/wav")]
    AudioWav,
    #[serde(rename = "audio/ogg")]
    AudioOgg,
    #[serde(rename = "application/pdf")]
    ApplicationPdf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCategory {
    Image,
    Document,
    Video,
    Audio,
}

impl ReadableMime {
    pub const ALL: &[Self] = &[
        Self::ImageJpeg,
        Self::ImagePng,
        Self::ImageGif,
        Self::ImageWebp,
        Self::VideoMp4,
        Self::VideoWebm,
        Self::AudioMpeg,
        Self::AudioWav,
        Self::AudioOgg,
        Self::ApplicationPdf,
    ];

    pub const IMAGES: &[Self] = &[Self::ImageJpeg, Self::ImagePng, Self::ImageGif, Self::ImageWebp];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ImageJpeg => "image/jpeg",
            Self::ImagePng => "image/png",
            Self::ImageGif => "image/gif",
            Self::ImageWebp => "image/webp",
            Self::VideoMp4 => "video/mp4",
            Self::VideoWebm => "video/webm",
            Self::AudioMpeg => "audio/mpeg",
            Self::AudioWav => "audio/wav",
            Self::AudioOgg => "audio/ogg",
            Self::ApplicationPdf => "application/pdf",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::ImageJpeg => "jpg",
            Self::ImagePng => "png",
            Self::ImageGif => "gif",
            Self::ImageWebp => "webp",
            Self::VideoMp4 => "mp4",
            Self::VideoWebm => "webm",
            Self::AudioMpeg => "mp3",
            Self::AudioWav => "wav",
            Self::AudioOgg => "ogg",
            Self::ApplicationPdf => "pdf",
        }
    }

    pub fn category(self) -> MediaCategory {
        match self {
            Self::ImageJpeg | Self::ImagePng | Self::ImageGif | Self::ImageWebp => MediaCategory::Image,
            Self::ApplicationPdf => MediaCategory::Document,
            Self::VideoMp4 | Self::VideoWebm => MediaCategory::Video,
            Self::AudioMpeg | Self::AudioWav | Self::AudioOgg => MediaCategory::Audio,
        }
    }

    /// Strip parameters and fold aliases onto the canonical member.
    pub fn parse(mime: &str) -> Option<Self> {
        let canonical = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        Some(match canonical.as_str() {
            "image/png" => Self::ImagePng,
            "image/jpeg" | "image/jpg" => Self::ImageJpeg,
            "image/webp" => Self::ImageWebp,
            "image/gif" => Self::ImageGif,
            "video/mp4" => Self::VideoMp4,
            "video/webm" => Self::VideoWebm,
            "audio/mpeg" | "audio/mp3" => Self::AudioMpeg,
            "audio/wav" | "audio/x-wav" | "audio/wave" => Self::AudioWav,
            "audio/ogg" => Self::AudioOgg,
            "application/pdf" => Self::ApplicationPdf,
            _ => return None,
        })
    }
}

pub fn image_mimes() -> Vec<ReadableMime> {
    ReadableMime::IMAGES.to_vec()
}

pub fn vision_and_pdf_mimes() -> Vec<ReadableMime> {
    let mut mimes = image_mimes();
    mimes.push(ReadableMime::ApplicationPdf);
    mimes
}
