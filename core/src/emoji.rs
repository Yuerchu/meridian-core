use std::path::{Path, PathBuf};

use image::AnimationDecoder;

const MAX_FILE_SIZE: u64 = 5 * 1024 * 1024; // 5MB

/// `provider_id:model_id` for `suggest_sticker_semantics`. Empty means the
/// default assistant, which is what that command used before this existed.
pub const SUGGEST_MODEL_PREFERENCE: &str = "emoji.suggest.model";

/// Absent is no choice. Anything stored has to be a canonical pair; malformed
/// is an error, never "unset", or a broken setting would silently send sticker
/// images to the default assistant.
pub fn parse_suggest_model(raw: Option<String>) -> Result<Option<(String, String)>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let invalid = || format!("preference `{SUGGEST_MODEL_PREFERENCE}` must be a canonical provider:model pair");
    let (provider_id, model_id) = raw.split_once(':').ok_or_else(invalid)?;
    if provider_id.is_empty() || model_id.is_empty() || provider_id.trim() != provider_id || model_id.trim() != model_id
    {
        return Err(invalid());
    }
    Ok(Some((provider_id.to_string(), model_id.to_string())))
}

pub fn packs_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("emoji_packs")
}

pub fn pack_dir(app_data_dir: &Path, pack_id: &str) -> PathBuf {
    packs_dir(app_data_dir).join(pack_id)
}

pub fn emoji_path(app_data_dir: &Path, pack_id: &str, file_name: &str) -> PathBuf {
    pack_dir(app_data_dir, pack_id).join(file_name)
}

pub fn ensure_pack_dir(app_data_dir: &Path, pack_id: &str) -> Result<PathBuf, String> {
    let dir = pack_dir(app_data_dir, pack_id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create pack directory: {e}"))?;
    Ok(dir)
}

pub fn import_file(app_data_dir: &Path, pack_id: &str, source_path: &Path) -> Result<(String, String), String> {
    let meta = std::fs::metadata(source_path).map_err(|e| format!("Cannot read file: {e}"))?;

    if meta.len() > MAX_FILE_SIZE {
        return Err(format!(
            "File too large ({:.1} MB). Maximum is 5 MB.",
            meta.len() as f64 / 1024.0 / 1024.0,
        ));
    }

    let file_name = source_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("Invalid file name")?
        .to_string();

    let format = detect_format(&file_name)?;
    let dir = ensure_pack_dir(app_data_dir, pack_id)?;
    let dest = dir.join(&file_name);
    std::fs::copy(source_path, &dest).map_err(|e| format!("Failed to copy file: {e}"))?;

    Ok((file_name, format))
}

pub fn delete_file(app_data_dir: &Path, pack_id: &str, file_name: &str) {
    let path = emoji_path(app_data_dir, pack_id, file_name);
    let _ = std::fs::remove_file(path);
}

pub fn delete_pack_dir(app_data_dir: &Path, pack_id: &str) {
    let dir = pack_dir(app_data_dir, pack_id);
    let _ = std::fs::remove_dir_all(dir);
}

// Limits are checked before requesting a decoded frame. Only small thumbnails
// survive each iteration; a long animation deliberately previews a bounded prefix.
const PREVIEW_CELL: u32 = 192;
const MAX_DIMENSION: u32 = 4096;
const MAX_CANVAS_PIXELS: u64 = 16_777_216;
const MAX_TOTAL_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_PREVIEW_FRAMES: usize = 120;
const MAX_DECODE_ALLOC: u64 = 128 * 1024 * 1024;

fn read_image_input(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_SIZE {
        return Err("sticker must be a regular file of at most 5 MiB".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE_SIZE {
        return Err("sticker exceeds 5 MiB".into());
    }
    Ok(bytes)
}

fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

fn canvas_pixels(dimensions: (u32, u32)) -> Result<u64, String> {
    let (width, height) = dimensions;
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION || pixels > MAX_CANVAS_PIXELS {
        return Err("sticker dimensions are invalid or too large".into());
    }
    Ok(pixels)
}

fn thumbnail_frames(
    frames: impl Iterator<Item = image::ImageResult<image::Frame>>,
    pixels: u64,
) -> Result<Vec<image::RgbaImage>, String> {
    // The canvas size is validated by the caller before constructing/advancing
    // the animation iterator. No extra frame is decoded to detect truncation.
    // Reserve one canvas for an APNG default image excluded from its animation.
    let count = MAX_PREVIEW_FRAMES.min((MAX_TOTAL_PIXELS / pixels).saturating_sub(1) as usize);
    frames
        .take(count)
        .map(|frame| {
            let frame = frame.map_err(|e| e.to_string())?;
            Ok(image::imageops::thumbnail(frame.buffer(), PREVIEW_CELL, PREVIEW_CELL))
        })
        .collect()
}

fn png_decoder(bytes: &[u8]) -> Result<image::codecs::png::PngDecoder<std::io::Cursor<&[u8]>>, String> {
    // Unlike set_limits after new(), with_limits bounds PNG header/chunk
    // processing as well as the later APNG buffers.
    image::codecs::png::PngDecoder::with_limits(std::io::Cursor::new(bytes), decode_limits()).map_err(|e| e.to_string())
}

fn preview_png(bytes: &[u8], extension: &str) -> Result<Vec<u8>, String> {
    use image::ImageDecoder;
    let thumbs = match extension {
        "gif" => {
            let mut decoder =
                image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
            let pixels = canvas_pixels(decoder.dimensions())?;
            let mut limits = decode_limits();
            // The image backend validates each frame rectangle against these
            // dimensions before allocating, even if a malformed GIF describes
            // a frame larger than its logical screen.
            limits.max_image_width = Some(decoder.dimensions().0);
            limits.max_image_height = Some(decoder.dimensions().1);
            decoder.set_limits(limits).map_err(|e| e.to_string())?;
            thumbnail_frames(decoder.into_frames(), pixels)?
        }
        "png" | "apng" => {
            let decoder = png_decoder(bytes)?;
            let pixels = canvas_pixels(decoder.dimensions())?;
            if decoder.is_apng().map_err(|e| e.to_string())? {
                thumbnail_frames(decoder.apng().map_err(|e| e.to_string())?.into_frames(), pixels)?
            } else {
                let frame = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
                vec![frame.thumbnail(PREVIEW_CELL, PREVIEW_CELL).to_rgba8()]
            }
        }
        _ => return Err("sticker preview requires GIF or PNG".into()),
    };
    contact_sheet(thumbs)
}

fn contact_sheet(thumbs: Vec<image::RgbaImage>) -> Result<Vec<u8>, String> {
    if thumbs.is_empty() {
        return Err("sticker contains no decodable frame".into());
    }
    let count = thumbs.len().min(6);
    let columns = count.min(3) as u32;
    let rows = (count as u32).div_ceil(columns);
    let mut sheet = image::RgbaImage::new(columns * PREVIEW_CELL, rows * PREVIEW_CELL);
    for index in 0..count {
        let source = if thumbs.len() > 6 {
            index * (thumbs.len() - 1) / 5
        } else {
            index
        };
        let thumb = &thumbs[source];
        let x = (index as u32 % columns) * PREVIEW_CELL + (PREVIEW_CELL - thumb.width()) / 2;
        let y = (index as u32 / columns) * PREVIEW_CELL + (PREVIEW_CELL - thumb.height()) / 2;
        image::imageops::overlay(&mut sheet, thumb, x.into(), y.into());
    }
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(sheet)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(bytes.into_inner())
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Provider-safe PNG contact sheet containing at most six chronological frames.
pub fn vision_preview_data_uri(path: &Path) -> Result<String, String> {
    use base64::Engine;
    let bytes = read_image_input(path)?;
    let extension = extension(path);
    let (mime, bytes) = match extension.as_str() {
        "gif" | "png" | "apng" => ("image/png".to_owned(), preview_png(&bytes, &extension)?),
        _ => (mime_guess::from_path(path).first_or_octet_stream().to_string(), bytes),
    };
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// Animated images use the same contact sheet as vision; stills retain their
/// original bytes for the embedding engine. All paths have the input byte cap.
pub fn embedding_image_bytes(path: &Path) -> Result<Vec<u8>, String> {
    use image::ImageDecoder;
    let bytes = read_image_input(path)?;
    let extension = extension(path);
    match extension.as_str() {
        "gif" => preview_png(&bytes, &extension),
        "png" | "apng" => {
            let decoder = png_decoder(&bytes)?;
            canvas_pixels(decoder.dimensions())?;
            if decoder.is_apng().map_err(|e| e.to_string())? {
                preview_png(&bytes, &extension)
            } else {
                Ok(bytes)
            }
        }
        _ => Ok(bytes),
    }
}

fn detect_format(file_name: &str) -> Result<String, String> {
    let ext = Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "gif" => Ok("gif".into()),
        "apng" => Ok("apng".into()),
        "png" => Ok("png".into()),
        "webp" => Ok("webp".into()),
        "jpg" | "jpeg" => Ok("jpg".into()),
        "bmp" => Ok("bmp".into()),
        "json" => Ok("lottie".into()),
        _ => Err(format!(
            "Unsupported format: .{ext}. Supported: gif, apng, png, webp, jpg, bmp, json (Lottie)."
        )),
    }
}

#[cfg(test)]
#[path = "emoji/resource_tests.rs"]
mod resource_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_format() {
        assert_eq!(detect_format("wave.gif").unwrap(), "gif");
        assert_eq!(detect_format("smile.apng").unwrap(), "apng");
        assert_eq!(detect_format("star.png").unwrap(), "png");
        assert_eq!(detect_format("anim.webp").unwrap(), "webp");
        assert_eq!(detect_format("photo.jpg").unwrap(), "jpg");
        assert_eq!(detect_format("photo.jpeg").unwrap(), "jpg");
        assert_eq!(detect_format("icon.bmp").unwrap(), "bmp");
        assert_eq!(detect_format("fancy.json").unwrap(), "lottie");
        assert!(detect_format("video.mp4").is_err());
    }

    #[test]
    fn test_paths() {
        let data = Path::new("/app/data");
        assert_eq!(pack_dir(data, "p1"), PathBuf::from("/app/data/emoji_packs/p1"));
        assert_eq!(
            emoji_path(data, "p1", "wave.gif"),
            PathBuf::from("/app/data/emoji_packs/p1/wave.gif"),
        );
    }

    #[test]
    fn gif_preview_is_normalised_to_png() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.gif");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode("R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==")
            .unwrap();
        std::fs::write(&path, bytes).unwrap();
        let preview = vision_preview_data_uri(&path).unwrap();
        assert!(preview.starts_with("data:image/png;base64,"));
        let still = embedding_image_bytes(&path).unwrap();
        assert_eq!(&still[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn an_absent_suggest_model_is_no_choice() {
        assert_eq!(parse_suggest_model(None), Ok(None));
    }

    #[test]
    fn a_stored_suggest_model_is_read_back() {
        assert_eq!(
            parse_suggest_model(Some("p1:claude-haiku-5".into())),
            Ok(Some(("p1".into(), "claude-haiku-5".into())))
        );
    }

    #[test]
    fn a_malformed_suggest_model_is_an_error() {
        for bad in ["", "p1", "p1:", ":m", " p1:m", "p1:m "] {
            assert!(parse_suggest_model(Some(bad.into())).is_err(), "{bad:?}");
        }
    }
}
