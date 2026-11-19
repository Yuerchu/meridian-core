use super::*;

fn gif_fixture() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut bytes, 2, 1, &[255, 0, 0, 0, 255, 0, 0, 0, 255]).unwrap();
        for (pixels, disposal, transparent) in [
            (vec![0, 0], gif::DisposalMethod::Keep, None),
            (vec![1, 2], gif::DisposalMethod::Previous, Some(2)),
            (vec![2, 2], gif::DisposalMethod::Background, Some(2)),
            (vec![1, 2], gif::DisposalMethod::Keep, Some(2)),
        ] {
            let frame = gif::Frame {
                width: 2,
                height: 1,
                buffer: pixels.into(),
                dispose: disposal,
                transparent,
                ..gif::Frame::default()
            };
            encoder.write_frame(&frame).unwrap();
        }
    }
    bytes
}

fn apng_fixture() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(3, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[255, 0, 0, 255, 255, 0, 0, 255]).unwrap();
        writer.set_blend_op(png::BlendOp::Over).unwrap();
        writer.set_dispose_op(png::DisposeOp::Previous).unwrap();
        writer.write_image_data(&[0, 255, 0, 255, 0, 0, 0, 0]).unwrap();
        writer.set_dispose_op(png::DisposeOp::None).unwrap();
        writer.write_image_data(&[0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        writer.finish().unwrap();
    }
    bytes
}

#[test]
fn animation_backends_composite_transparency_and_disposal() {
    use image::ImageDecoder;
    let gif = gif_fixture();
    let mut decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&gif)).unwrap();
    decoder.set_limits(decode_limits()).unwrap();
    let frames = thumbnail_frames(decoder.into_frames(), 2).unwrap();
    assert_eq!(frames.len(), 4);
    let red = image::Rgba([255, 0, 0, 255]);
    let green = image::Rgba([0, 255, 0, 255]);
    assert_eq!(*frames[0].get_pixel(0, 0), red);
    assert_eq!(*frames[1].get_pixel(0, 0), green);
    assert_eq!(*frames[1].get_pixel(frames[1].width() - 1, 0), red);
    assert_eq!(*frames[2].get_pixel(0, 0), red);
    assert_eq!(frames[3].get_pixel(frames[3].width() - 1, 0)[3], 0);
    let apng = apng_fixture();
    let frames = thumbnail_frames(png_decoder(&apng).unwrap().apng().unwrap().into_frames(), 2).unwrap();
    assert_eq!(frames.len(), 3);
    assert_eq!(*frames[1].get_pixel(0, 0), green);
    assert_eq!(*frames[1].get_pixel(frames[1].width() - 1, 0), red);
    assert_eq!(*frames[2].get_pixel(0, 0), red);
}

#[test]
fn gif_and_apng_share_vision_and_embedding_previews() {
    use base64::Engine;
    let dir = tempfile::tempdir().unwrap();
    for (ext, bytes) in [("gif", gif_fixture()), ("apng", apng_fixture())] {
        let path = dir.path().join(format!("tiny.{ext}"));
        std::fs::write(&path, bytes).unwrap();
        let uri = vision_preview_data_uri(&path).unwrap();
        let preview = base64::engine::general_purpose::STANDARD
            .decode(uri.strip_prefix("data:image/png;base64,").unwrap())
            .unwrap();
        assert_eq!(preview, embedding_image_bytes(&path).unwrap());
    }
}

#[test]
fn streaming_stops_before_requesting_a_frame_over_either_budget() {
    use std::cell::Cell;
    for (pixels, expected) in [(1, MAX_PREVIEW_FRAMES), (MAX_CANVAS_PIXELS, 3)] {
        let calls = Cell::new(0);
        let frames = std::iter::from_fn(|| {
            calls.set(calls.get() + 1);
            Some(Ok(image::Frame::new(image::RgbaImage::new(1, 1))))
        });
        let thumbs = thumbnail_frames(frames, pixels).unwrap();
        assert_eq!(calls.get(), expected);
        assert_eq!(thumbs.len(), expected);
        assert!(
            thumbs
                .iter()
                .all(|t| t.width() <= PREVIEW_CELL && t.height() <= PREVIEW_CELL)
        );
    }
}

#[test]
fn contact_sheet_samples_six_chronological_frames() {
    let thumbs = (0..13)
        .map(|i| image::RgbaImage::from_pixel(1, 1, image::Rgba([i, 0, 0, 255])))
        .collect();
    let sheet = image::load_from_memory(&contact_sheet(thumbs).unwrap())
        .unwrap()
        .to_rgba8();
    assert_eq!(sheet.dimensions(), (576, 384));
    for (cell, expected) in [0, 2, 4, 7, 9, 12].into_iter().enumerate() {
        assert_eq!(
            sheet.get_pixel((cell as u32 % 3) * 192 + 95, (cell as u32 / 3) * 192 + 95)[0],
            expected
        );
    }
    assert!(contact_sheet(Vec::new()).is_err());
}

#[test]
fn dimensions_and_decoder_allocations_are_bounded() {
    use image::ImageDecoder;
    assert!(canvas_pixels((0, 1)).is_err());
    assert!(canvas_pixels((u32::MAX, u32::MAX)).is_err());
    assert!(canvas_pixels((4096, 4096)).is_ok());
    // Header-only huge canvas: rejection requires no huge allocation.
    let mut gif = gif_fixture();
    gif[6..10].copy_from_slice(&[255, 255, 255, 255]);
    assert!(preview_png(&gif, "gif").unwrap_err().contains("dimensions"));
    let mut png = Vec::new();
    {
        let encoder = png::Encoder::new(&mut png, 100_000, 100_000);
        let _header = encoder.write_header().unwrap();
    }
    assert!(preview_png(&png, "png").is_err());
    // Prove the installed backends honor preallocation limits using tiny
    // ordinary images and a deliberately insufficient allocation budget.
    let mut limits = decode_limits();
    limits.max_alloc = Some(1);
    let mut decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(gif_fixture())).unwrap();
    decoder.set_limits(limits.clone()).unwrap();
    assert!(matches!(
        decoder.into_frames().next().unwrap(),
        Err(image::ImageError::Limits(_))
    ));
    let apng = apng_fixture();
    let mut decoder = png_decoder(&apng).unwrap();
    decoder.set_limits(limits).unwrap();
    assert!(matches!(
        decoder.apng().unwrap().into_frames().next().unwrap(),
        Err(image::ImageError::Limits(_))
    ));
}

#[test]
fn gif_frame_larger_than_canvas_is_rejected_before_decoding() {
    let mut bytes = gif_fixture();
    // Existing first frame is 2x1; shrink only the logical canvas to 1x1.
    bytes[6..8].copy_from_slice(&1_u16.to_le_bytes());
    let error = preview_png(&bytes, "gif").unwrap_err();
    assert!(error.contains("exceeds limit"), "{error}");
}

#[test]
fn gif_local_palette_interlace_and_offset_are_composited() {
    let mut bytes = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut bytes, 2, 4, &[0, 0, 0, 255, 255, 255]).unwrap();
        let frame = gif::Frame {
            left: 1,
            width: 1,
            height: 4,
            interlaced: true,
            palette: Some(vec![255, 0, 0, 0, 255, 0]),
            // Interlaced stored row order for four rows is 0, 2, 1, 3.
            buffer: vec![0, 0, 1, 1].into(),
            ..gif::Frame::default()
        };
        encoder.write_frame(&frame).unwrap();
    }
    let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).unwrap();
    let frame = decoder.into_frames().next().unwrap().unwrap().into_buffer();
    for y in 0..4 {
        assert_eq!(frame.get_pixel(0, y)[3], 0);
        assert_eq!(
            *frame.get_pixel(1, y),
            if y % 2 == 0 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 255, 0, 255])
            }
        );
    }
}

#[test]
fn apng_hidden_default_image_is_not_an_animation_frame() {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(1, 0).unwrap();
        encoder.set_sep_def_img(true).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[255, 0, 0, 255]).unwrap();
        writer.write_image_data(&[0, 255, 0, 255]).unwrap();
        writer.finish().unwrap();
    }
    let frames = thumbnail_frames(png_decoder(&bytes).unwrap().apng().unwrap().into_frames(), 1).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(*frames[0].get_pixel(0, 0), image::Rgba([0, 255, 0, 255]));
}

#[test]
fn malformed_and_oversize_inputs_fail_safely() {
    for ext in ["gif", "png", "apng"] {
        assert!(preview_png(b"not an image", ext).is_err());
    }
    let mut gif = gif_fixture();
    gif.truncate(20);
    assert!(preview_png(&gif, "gif").is_err());
    let dir = tempfile::tempdir().unwrap();
    for ext in ["gif", "png", "webp", "jpg"] {
        let path = dir.path().join(format!("large.{ext}"));
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_FILE_SIZE + 1)
            .unwrap();
        assert!(vision_preview_data_uri(&path).is_err());
        assert!(embedding_image_bytes(&path).is_err());
    }
}

#[test]
fn still_png_embedding_preserves_original_bytes() {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("still.png");
    std::fs::write(&path, bytes.get_ref()).unwrap();
    assert_eq!(embedding_image_bytes(&path).unwrap(), bytes.into_inner());
}
