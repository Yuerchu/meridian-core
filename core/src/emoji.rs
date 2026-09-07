use std::path::{Path, PathBuf};

use image::AnimationDecoder;

const MAX_FILE_SIZE: u64 = 5 * 1024 * 1024; // 5MB

pub fn parse_native_payload(sticker_id: &str, raw: Option<&str>) -> Result<serde_json::Value, String> {
    let payload = match raw {
        Some(raw) => serde_json::from_str::<serde_json::Value>(raw)
            .map_err(|error| format!("Sticker {sticker_id} has invalid native_payload JSON: {error}"))?,
        None => serde_json::json!({}),
    };
    if !payload.is_object() {
        return Err(format!("Sticker {sticker_id} native_payload must be a JSON object"));
    }
    Ok(payload)
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

/// Provider-safe PNG contact sheet containing at most six chronological frames.
pub fn vision_preview_data_uri(path: &Path) -> Result<String, String> {
    use base64::Engine;
    use std::io::BufReader;

    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_lowercase();
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut frames: Vec<image::RgbaImage> = match extension.as_str() {
        "gif" => decode_gif_frames(path)?,
        "webp" => return crate::files::file_to_base64_data_uri(path, "image/webp"),
        "png" | "apng" => {
            let decoder = image::codecs::png::PngDecoder::new(BufReader::new(file)).map_err(|e| e.to_string())?;
            if decoder.is_apng().map_err(|e| e.to_string())? {
                decoder
                    .apng()
                    .map_err(|e| e.to_string())?
                    .into_frames()
                    .take(120)
                    .map(|frame| frame.map(|value| value.into_buffer()).map_err(|e| e.to_string()))
                    .collect::<Result<_, _>>()?
            } else {
                vec![
                    image::DynamicImage::from_decoder(decoder)
                        .map_err(|e| e.to_string())?
                        .to_rgba8(),
                ]
            }
        }
        _ => {
            let mime = mime_guess::from_path(path).first_or_octet_stream().to_string();
            return crate::files::file_to_base64_data_uri(path, &mime);
        }
    };
    if frames.is_empty() {
        return Err("sticker contains no decodable frame".into());
    }
    if frames.len() > 6 {
        let last = frames.len() - 1;
        frames = (0..6).map(|index| frames[index * last / 5].clone()).collect();
    }

    const CELL: u32 = 192;
    let columns = frames.len().min(3) as u32;
    let rows = (frames.len() as u32).div_ceil(columns);
    let mut sheet = image::RgbaImage::new(columns * CELL, rows * CELL);
    for (index, frame) in frames.into_iter().enumerate() {
        let thumb = image::imageops::thumbnail(&frame, CELL, CELL);
        let cell_x = (index as u32 % columns) * CELL;
        let cell_y = (index as u32 / columns) * CELL;
        let x = cell_x + (CELL - thumb.width()) / 2;
        let y = cell_y + (CELL - thumb.height()) / 2;
        image::imageops::overlay(&mut sheet, &thumb, x.into(), y.into());
    }
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(sheet)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())
    ))
}

fn decode_gif_frames(path: &Path) -> Result<Vec<image::RgbaImage>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() < 13 || !matches!(&bytes[..6], b"GIF87a" | b"GIF89a") {
        return Err("invalid GIF header".into());
    }
    let mut cursor = 6usize;
    let read_u8 = |cursor: &mut usize| -> Result<u8, String> {
        let value = *bytes.get(*cursor).ok_or("truncated GIF")?;
        *cursor += 1;
        Ok(value)
    };
    let read_u16 = |cursor: &mut usize| -> Result<u16, String> {
        let low = read_u8(cursor)? as u16;
        let high = read_u8(cursor)? as u16;
        Ok(low | high << 8)
    };
    let width = read_u16(&mut cursor)? as u32;
    let height = read_u16(&mut cursor)? as u32;
    if width == 0 || height == 0 || width.saturating_mul(height) > 16_777_216 {
        return Err("GIF dimensions are invalid or too large".into());
    }
    let packed = read_u8(&mut cursor)?;
    let _background = read_u8(&mut cursor)?;
    let _aspect = read_u8(&mut cursor)?;
    let read_palette = |cursor: &mut usize, count: usize| -> Result<Vec<[u8; 3]>, String> {
        let end = cursor.checked_add(count * 3).ok_or("GIF palette overflow")?;
        let source = bytes.get(*cursor..end).ok_or("truncated GIF palette")?;
        *cursor = end;
        Ok(source.as_chunks::<3>().0.to_vec())
    };
    let global_palette = if packed & 0x80 != 0 {
        read_palette(&mut cursor, 2usize << (packed & 0x07))?
    } else {
        Vec::new()
    };
    let read_sub_blocks = |cursor: &mut usize| -> Result<Vec<u8>, String> {
        let mut output = Vec::new();
        loop {
            let size = read_u8(cursor)? as usize;
            if size == 0 {
                break;
            }
            let end = cursor.checked_add(size).ok_or("GIF block overflow")?;
            output.extend_from_slice(bytes.get(*cursor..end).ok_or("truncated GIF block")?);
            *cursor = end;
        }
        Ok(output)
    };

    #[derive(Clone, Copy, Default)]
    struct Control {
        disposal: u8,
        transparent: Option<u8>,
    }
    struct Previous {
        disposal: u8,
        rect: (u32, u32, u32, u32),
        restore: Option<image::RgbaImage>,
    }

    let mut control = Control::default();
    let mut canvas = image::RgbaImage::new(width, height);
    let mut previous: Option<Previous> = None;
    let mut frames = Vec::new();
    while cursor < bytes.len() && frames.len() < 120 {
        match read_u8(&mut cursor)? {
            0x3B => break,
            0x21 => {
                let label = read_u8(&mut cursor)?;
                if label == 0xF9 {
                    let size = read_u8(&mut cursor)? as usize;
                    if size != 4 {
                        return Err("invalid GIF graphic control extension".into());
                    }
                    let flags = read_u8(&mut cursor)?;
                    let _delay = read_u16(&mut cursor)?;
                    let transparent = read_u8(&mut cursor)?;
                    let _terminator = read_u8(&mut cursor)?;
                    control = Control {
                        disposal: (flags >> 2) & 0x07,
                        transparent: (flags & 0x01 != 0).then_some(transparent),
                    };
                } else {
                    let _ = read_sub_blocks(&mut cursor)?;
                }
            }
            0x2C => {
                if let Some(previous) = previous.take() {
                    match previous.disposal {
                        2 => {
                            let (left, top, frame_width, frame_height) = previous.rect;
                            for y in top..top.saturating_add(frame_height).min(height) {
                                for x in left..left.saturating_add(frame_width).min(width) {
                                    canvas.put_pixel(x, y, image::Rgba([0, 0, 0, 0]));
                                }
                            }
                        }
                        3 => {
                            if let Some(restored) = previous.restore {
                                canvas = restored;
                            }
                        }
                        _ => {}
                    }
                }
                let left = read_u16(&mut cursor)? as u32;
                let top = read_u16(&mut cursor)? as u32;
                let frame_width = read_u16(&mut cursor)? as u32;
                let frame_height = read_u16(&mut cursor)? as u32;
                let image_flags = read_u8(&mut cursor)?;
                let local_palette;
                let palette = if image_flags & 0x80 != 0 {
                    local_palette = read_palette(&mut cursor, 2usize << (image_flags & 0x07))?;
                    &local_palette
                } else {
                    &global_palette
                };
                if palette.is_empty() {
                    return Err("GIF frame has no colour table".into());
                }
                let min_code_size = read_u8(&mut cursor)?;
                let compressed = read_sub_blocks(&mut cursor)?;
                let decoded = gif_lzw_decode(&compressed, min_code_size, (frame_width * frame_height) as usize)?;
                let restore = (control.disposal == 3).then(|| canvas.clone());
                let rows: Vec<u32> = if image_flags & 0x40 != 0 {
                    [(0, 8), (4, 8), (2, 4), (1, 2)]
                        .into_iter()
                        .flat_map(|(start, step)| (start..frame_height).step_by(step))
                        .collect()
                } else {
                    (0..frame_height).collect()
                };
                let mut source = 0usize;
                for row in rows {
                    for column in 0..frame_width {
                        let Some(&index) = decoded.get(source) else { break };
                        source += 1;
                        if control.transparent == Some(index) {
                            continue;
                        }
                        let Some(rgb) = palette.get(index as usize) else {
                            continue;
                        };
                        let x = left + column;
                        let y = top + row;
                        if x < width && y < height {
                            canvas.put_pixel(x, y, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
                        }
                    }
                }
                frames.push(canvas.clone());
                previous = Some(Previous {
                    disposal: control.disposal,
                    rect: (left, top, frame_width, frame_height),
                    restore,
                });
                control = Control::default();
            }
            _ => return Err("unsupported GIF block".into()),
        }
    }
    if frames.is_empty() {
        return Err("GIF contains no image frames".into());
    }
    Ok(frames)
}

fn gif_lzw_decode(data: &[u8], minimum_size: u8, expected: usize) -> Result<Vec<u8>, String> {
    if !(2..=8).contains(&minimum_size) {
        return Err("invalid GIF LZW code size".into());
    }
    let clear = 1usize << minimum_size;
    let end = clear + 1;
    let reset = || {
        let mut dictionary: Vec<Vec<u8>> = (0..clear).map(|value| vec![value as u8]).collect();
        dictionary.push(Vec::new());
        dictionary.push(Vec::new());
        dictionary
    };
    let mut dictionary = reset();
    let mut code_size = minimum_size as usize + 1;
    let mut bit = 0usize;
    let mut previous: Option<Vec<u8>> = None;
    let mut output = Vec::with_capacity(expected);
    while bit + code_size <= data.len() * 8 {
        let mut code = 0usize;
        for offset in 0..code_size {
            let at = bit + offset;
            code |= (((data[at / 8] >> (at % 8)) & 1) as usize) << offset;
        }
        bit += code_size;
        if code == clear {
            dictionary = reset();
            code_size = minimum_size as usize + 1;
            previous = None;
            continue;
        }
        if code == end {
            break;
        }
        let entry = if code < dictionary.len() && !dictionary[code].is_empty() {
            dictionary[code].clone()
        } else if code == dictionary.len() {
            let mut value = previous.clone().ok_or("invalid GIF LZW stream")?;
            value.push(value[0]);
            value
        } else {
            return Err("invalid GIF LZW dictionary reference".into());
        };
        output.extend_from_slice(&entry);
        if let Some(mut prior) = previous {
            prior.push(entry[0]);
            if dictionary.len() < 4096 {
                dictionary.push(prior);
                if dictionary.len() == (1usize << code_size) && code_size < 12 {
                    code_size += 1;
                }
            }
        }
        previous = Some(entry);
        if output.len() >= expected {
            break;
        }
    }
    output.truncate(expected);
    if output.len() < expected {
        return Err("truncated GIF pixel stream".into());
    }
    Ok(output)
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
    fn native_payload_requires_a_json_object() {
        assert_eq!(parse_native_payload("s1", None).unwrap(), serde_json::json!({}));
        assert_eq!(
            parse_native_payload("s1", Some(r#"{"url":"https://example.test/a.gif"}"#)).unwrap()["url"],
            "https://example.test/a.gif"
        );
        assert!(parse_native_payload("s1", Some("not-json")).is_err());
        assert!(parse_native_payload("s1", Some("[]")).is_err());
        assert!(parse_native_payload("s1", Some("null")).is_err());
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
    }
}
