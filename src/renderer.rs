use std::io::Cursor;

use anyhow::{Context, Result, bail};
use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbaImage,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
    imageops,
};
use image::{ImageEncoder, metadata::Orientation as ExifOrientation};
use serde::{Deserialize, Serialize};

use crate::dither::{IndexedImage, Rgb};

pub const RENDER_PROFILE_VERSION: u32 = 1;
pub const MAX_ENCODED_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_SOURCE_DIMENSION: u32 = 20_000;
pub const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;
pub const JPEG_QUALITY: u8 = 90;

pub const DEVICE_PALETTE: [(u8, Rgb); 6] = [
    (0, Rgb::new(0, 0, 0)),
    (1, Rgb::new(0, 0, 255)),
    (2, Rgb::new(0, 255, 0)),
    (3, Rgb::new(255, 0, 0)),
    (5, Rgb::new(255, 255, 0)),
    (6, Rgb::new(255, 255, 255)),
];

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FrameTarget {
    #[default]
    #[serde(rename = "paper_7")]
    Paper7,
    PaperL,
}

impl FrameTarget {
    pub fn physical_dimensions(self) -> (u32, u32) {
        match self {
            Self::Paper7 => (800, 480),
            Self::PaperL => (1200, 1600),
        }
    }

    pub fn logical_dimensions(self, orientation: ImageOrientation) -> (u32, u32) {
        let (width, height) = self.physical_dimensions();
        if orientation.is_portrait() == (height > width) {
            (width, height)
        } else {
            (height, width)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    ServerDitherBmp,
    #[default]
    DirectJpeg,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImageOrientation {
    Portrait,
    Landscape,
}

impl ImageOrientation {
    pub fn infer(width: u32, height: u32) -> Self {
        if height > width {
            Self::Portrait
        } else {
            Self::Landscape
        }
    }

    pub fn is_portrait(self) -> bool {
        self == Self::Portrait
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrientationSource {
    Inferred,
    Override,
}

#[derive(Clone, Debug)]
pub struct DecodedImage {
    pub pixels: RgbaImage,
    pub format: ImageFormat,
    pub orientation: ImageOrientation,
}

#[derive(Clone, Debug)]
pub struct RenderedImage {
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
    pub extension: &'static str,
}

pub fn decode_source(bytes: &[u8]) -> Result<DecodedImage> {
    decode_source_named(bytes, None)
}

pub fn decode_source_named(bytes: &[u8], name: Option<&str>) -> Result<DecodedImage> {
    validate_encoded_length(bytes.len())?;
    let mut reader = ImageReader::new(Cursor::new(bytes));
    if let Some(format) = name.and_then(|name| ImageFormat::from_path(name).ok()) {
        reader.set_format(format);
    }
    let mut reader = reader
        .with_guessed_format()
        .context("failed to inspect image format")?;
    let format = reader
        .format()
        .context("unsupported or unrecognized raster image format")?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .context("failed to create image decoder")?;
    let exif_orientation = decoder
        .orientation()
        .unwrap_or(ExifOrientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).context("failed to decode raster image")?;
    image.apply_orientation(exif_orientation);
    let mut pixels = image.into_rgba8();
    composite_alpha_on_white(&mut pixels);
    let orientation = ImageOrientation::infer(pixels.width(), pixels.height());
    Ok(DecodedImage {
        pixels,
        format,
        orientation,
    })
}

fn validate_encoded_length(length: usize) -> Result<()> {
    if length == 0 {
        bail!("image is empty");
    }
    if length as u64 > MAX_ENCODED_BYTES {
        bail!("encoded image exceeds the 100 MiB source limit");
    }
    Ok(())
}

pub fn render_with_original(
    source: &DecodedImage,
    original: &[u8],
    target: FrameTarget,
    mode: OutputMode,
    orientation: ImageOrientation,
) -> Result<RenderedImage> {
    if mode == OutputMode::DirectJpeg && source.format == ImageFormat::Jpeg {
        return Ok(RenderedImage {
            bytes: original.to_vec(),
            media_type: "image/jpeg",
            extension: "jpg",
        });
    }
    render(source, target, mode, orientation)
}

pub fn render(
    source: &DecodedImage,
    target: FrameTarget,
    mode: OutputMode,
    orientation: ImageOrientation,
) -> Result<RenderedImage> {
    match mode {
        OutputMode::ServerDitherBmp => {
            let framebuffer = prepare_framebuffer(&source.pixels, target, orientation);
            let indexed = firmware_dither(&framebuffer)?;
            Ok(RenderedImage {
                bytes: encode_bmp4(&indexed)?,
                media_type: "image/bmp",
                extension: "bmp",
            })
        }
        OutputMode::DirectJpeg => Ok(RenderedImage {
            bytes: encode_jpeg(&source.pixels)?,
            media_type: "image/jpeg",
            extension: "jpg",
        }),
    }
}

pub fn dithered_preview_png(
    source: &DecodedImage,
    target: FrameTarget,
    orientation: ImageOrientation,
) -> Result<Vec<u8>> {
    let framebuffer = prepare_framebuffer(&source.pixels, target, orientation);
    let indexed = firmware_dither(&framebuffer)?;
    encode_indexed_png(&indexed)
}

pub fn prepare_framebuffer(
    source: &RgbaImage,
    target: FrameTarget,
    orientation: ImageOrientation,
) -> RgbaImage {
    let (logical_width, logical_height) = target.logical_dimensions(orientation);
    let divisor = if source.width() > 3200 {
        8
    } else if source.width() > 1600 {
        4
    } else if source.width() > 800 {
        2
    } else {
        1
    };
    let prescaled = if divisor > 1 {
        resize_nearest_firmware(
            source,
            (source.width() / divisor).max(1),
            (source.height() / divisor).max(1),
        )
    } else {
        source.clone()
    };
    let mut logical = resize_nearest_firmware(&prescaled, logical_width, logical_height);
    for pixel in logical.pixels_mut() {
        // JPEGDEC supplies RGB565 to the firmware callback.
        pixel[0] &= 0xF8;
        pixel[1] &= 0xFC;
        pixel[2] &= 0xF8;
        pixel[3] = 255;
    }
    if (logical_width, logical_height) == target.physical_dimensions() {
        logical
    } else {
        // Fixed clockwise convention: rotate the physical frame clockwise for this image.
        imageops::rotate90(&logical)
    }
}

fn resize_nearest_firmware(source: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    RgbaImage::from_fn(width, height, |x, y| {
        let source_x = (u64::from(x) * u64::from(source.width()) / u64::from(width)) as u32;
        let source_y = (u64::from(y) * u64::from(source.height()) / u64::from(height)) as u32;
        *source.get_pixel(
            source_x.min(source.width() - 1),
            source_y.min(source.height() - 1),
        )
    })
}

pub fn firmware_dither(image: &RgbaImage) -> Result<IndexedImage> {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let channels = width
        .checked_mul(3)
        .context("dither error buffer size overflow")?;
    let mut current = vec![0_i32; channels];
    let mut next = vec![0_i32; channels];
    let mut indices = vec![6_u8; width.checked_mul(height).context("image size overflow")?];

    for y in 0..height {
        for x in 0..width {
            let pixel = image.get_pixel(x as u32, y as u32);
            let mut original = [pixel[0] as i32, pixel[1] as i32, pixel[2] as i32];
            snap_color(&mut original);
            let adjusted = [
                (original[0] + current[x * 3]).clamp(0, 255),
                (original[1] + current[x * 3 + 1]).clamp(0, 255),
                (original[2] + current[x * 3 + 2]).clamp(0, 255),
            ];
            let (device_index, selected) = closest_device(adjusted);
            indices[y * width + x] = device_index;
            let error = [
                adjusted[0] - selected.r as i32,
                adjusted[1] - selected.g as i32,
                adjusted[2] - selected.b as i32,
            ];
            if x + 1 < width {
                add_error(&mut current, x + 1, error, 7);
            }
            if x > 0 {
                add_error(&mut next, x - 1, error, 3);
            }
            add_error(&mut next, x, error, 5);
            if x + 1 < width {
                add_error(&mut next, x + 1, error, 1);
            }
        }
        std::mem::swap(&mut current, &mut next);
        next.fill(0);
    }

    Ok(IndexedImage {
        width: image.width(),
        height: image.height(),
        indices,
    })
}

fn add_error(buffer: &mut [i32], x: usize, error: [i32; 3], weight: i32) {
    for channel in 0..3 {
        buffer[x * 3 + channel] += (error[channel] * weight) >> 4;
    }
}

fn snap_color(rgb: &mut [i32; 3]) {
    let [r, g, b] = *rgb;
    let replacement = if r < 25 && g < 25 && b < 25 {
        Some([0, 0, 0])
    } else if r > 230 && g > 230 && b > 230 {
        Some([255, 255, 255])
    } else if r > 230 && g < 25 && b < 25 {
        Some([255, 0, 0])
    } else if r < 35 && g > 220 && b < 35 {
        Some([0, 255, 0])
    } else if r < 25 && g < 25 && b > 230 {
        Some([0, 0, 255])
    } else if r > 205 && g > 205 && b < 50 {
        Some([255, 255, 0])
    } else {
        None
    };
    if let Some(replacement) = replacement {
        *rgb = replacement;
    }
}

fn closest_device(rgb: [i32; 3]) -> (u8, Rgb) {
    let mut best = DEVICE_PALETTE[0];
    let mut distance = i64::MAX;
    for candidate in DEVICE_PALETTE {
        let dr = rgb[0] - candidate.1.r as i32;
        let dg = rgb[1] - candidate.1.g as i32;
        let db = rgb[2] - candidate.1.b as i32;
        let candidate_distance = (dr * dr + dg * dg + db * db) as i64;
        if candidate_distance < distance {
            best = candidate;
            distance = candidate_distance;
        }
    }
    best
}

pub fn encode_bmp4(image: &IndexedImage) -> Result<Vec<u8>> {
    if image.width == 0 || image.height == 0 || image.width & 1 != 0 {
        bail!("4-bit BMP requires non-zero dimensions and an even width");
    }
    let pixels = (image.width as usize)
        .checked_mul(image.height as usize)
        .context("BMP dimensions overflow")?;
    if image.indices.len() != pixels {
        bail!("indexed pixel count does not match BMP dimensions");
    }
    if image
        .indices
        .iter()
        .any(|index| !matches!(index, 0 | 1 | 2 | 3 | 5 | 6))
    {
        bail!("indexed image contains an unsupported firmware color");
    }
    let row_bytes = image.width as usize / 2;
    let stride = (row_bytes + 3) & !3;
    let pixel_bytes = stride
        .checked_mul(image.height as usize)
        .context("BMP pixel length overflow")?;
    let header_size = 118_usize;
    let file_size = header_size
        .checked_add(pixel_bytes)
        .context("BMP length overflow")?;
    let mut output = vec![0_u8; file_size];
    output[0..2].copy_from_slice(b"BM");
    put_u32(&mut output, 2, u32::try_from(file_size)?);
    put_u32(&mut output, 10, header_size as u32);
    put_u32(&mut output, 14, 40);
    put_i32(&mut output, 18, i32::try_from(image.width)?);
    put_i32(&mut output, 22, -i32::try_from(image.height)?);
    put_u16(&mut output, 26, 1);
    put_u16(&mut output, 28, 4);
    put_u32(&mut output, 34, u32::try_from(pixel_bytes)?);
    put_i32(&mut output, 38, 2835);
    put_i32(&mut output, 42, 2835);
    put_u32(&mut output, 46, 16);
    put_u32(&mut output, 50, 16);
    for (index, color) in DEVICE_PALETTE {
        let offset = 54 + index as usize * 4;
        output[offset] = color.b;
        output[offset + 1] = color.g;
        output[offset + 2] = color.r;
    }
    for y in 0..image.height as usize {
        let source = &image.indices[y * image.width as usize..(y + 1) * image.width as usize];
        let destination = header_size + y * stride;
        for x in (0..source.len()).step_by(2) {
            output[destination + x / 2] = source[x] << 4 | source[x + 1];
        }
    }
    Ok(output)
}

pub fn validate_bmp4(bytes: &[u8], target: FrameTarget) -> Result<()> {
    if bytes.len() < 118 || &bytes[0..2] != b"BM" {
        bail!("invalid BMP header");
    }
    let width = get_i32(bytes, 18)?;
    let height = get_i32(bytes, 22)?;
    let bpp = get_u16(bytes, 28)?;
    let offset = get_u32(bytes, 10)? as usize;
    let expected = target.physical_dimensions();
    if width != expected.0 as i32 || height != -(expected.1 as i32) || bpp != 4 || offset != 118 {
        bail!("BMP does not match target dimensions or firmware format");
    }
    let row_bytes = expected.0 as usize / 2;
    let stride = (row_bytes + 3) & !3;
    if bytes.len() != 118 + stride * expected.1 as usize {
        bail!("BMP payload length is invalid");
    }
    Ok(())
}

pub fn encode_jpeg(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    JpegEncoder::new_with_quality(&mut output, JPEG_QUALITY)
        .encode_image(&DynamicImage::ImageRgba8(image.clone()))
        .context("failed to encode JPEG")?;
    Ok(output)
}

pub fn encode_indexed_png(image: &IndexedImage) -> Result<Vec<u8>> {
    let mut rgba = Vec::with_capacity(image.indices.len() * 4);
    for index in &image.indices {
        let color = DEVICE_PALETTE
            .iter()
            .find(|candidate| candidate.0 == *index)
            .context("indexed preview contains an unsupported color")?
            .1;
        rgba.extend_from_slice(&[color.r, color.g, color.b, 255]);
    }
    let mut output = Vec::new();
    PngEncoder::new(&mut output)
        .write_image(
            &rgba,
            image.width,
            image.height,
            image::ExtendedColorType::Rgba8,
        )
        .context("failed to encode preview PNG")?;
    Ok(output)
}

fn composite_alpha_on_white(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        let alpha = pixel[3] as u16;
        for channel in 0..3 {
            pixel[channel] =
                ((pixel[channel] as u16 * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
        }
        pixel[3] = 255;
    }
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_i32(bytes: &mut [u8], offset: usize, value: i32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn get_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .context("truncated BMP")?
            .try_into()?,
    ))
}
fn get_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .context("truncated BMP")?
            .try_into()?,
    ))
}
fn get_i32(bytes: &[u8], offset: usize) -> Result<i32> {
    Ok(i32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .context("truncated BMP")?
            .try_into()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};
    use serde::Deserialize;

    fn source(width: u32, height: u32) -> RgbaImage {
        ImageBuffer::from_fn(width, height, |x, y| {
            Rgba([(x * 37) as u8, (y * 59) as u8, ((x + y) * 23) as u8, 255])
        })
    }

    #[test]
    fn orientation_inference_treats_square_as_landscape() {
        assert_eq!(ImageOrientation::infer(10, 11), ImageOrientation::Portrait);
        assert_eq!(ImageOrientation::infer(10, 10), ImageOrientation::Landscape);
    }

    #[test]
    fn firmware_nearest_resize_uses_floor_mapped_source_coordinates() {
        let input = RgbaImage::from_fn(3, 2, |x, y| Rgba([(x + y * 3) as u8, 0, 0, 255]));
        let output = resize_nearest_firmware(&input, 5, 3);
        let values = output.pixels().map(|pixel| pixel[0]).collect::<Vec<_>>();
        assert_eq!(values, [0, 0, 1, 1, 2, 0, 0, 1, 1, 2, 3, 3, 4, 4, 5]);
    }

    #[test]
    fn framebuffer_uses_exact_target_dimensions_for_both_orientations() {
        for target in [FrameTarget::Paper7, FrameTarget::PaperL] {
            for orientation in [ImageOrientation::Portrait, ImageOrientation::Landscape] {
                let output = prepare_framebuffer(&source(7, 11), target, orientation);
                assert_eq!(output.dimensions(), target.physical_dimensions());
            }
        }
    }

    #[derive(Deserialize)]
    struct FirmwareFixture {
        firmware_commit: String,
        profile_version: u32,
        comparison: String,
        dither_case: FirmwareDitherCase,
        negative_shift_cases: Vec<NegativeShiftCase>,
        nearest_resize_case: ResizeCase,
        bmp_targets: Vec<BmpTargetCase>,
    }
    #[derive(Deserialize)]
    struct FirmwareDitherCase {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        indices: Vec<u8>,
    }
    #[derive(Deserialize)]
    struct NegativeShiftCase {
        error: i32,
        weight: i32,
        expected: i32,
    }
    #[derive(Deserialize)]
    struct ResizeCase {
        source_width: u32,
        source_height: u32,
        target_width: u32,
        target_height: u32,
        expected_source_indices: Vec<u8>,
    }
    #[derive(Deserialize)]
    struct BmpTargetCase {
        name: String,
        width: u32,
        height: u32,
        file_size: usize,
    }

    #[test]
    fn firmware_profile_fixture_matches_exactly() {
        let fixture: FirmwareFixture =
            serde_json::from_str(include_str!("../tests/fixtures/firmware-profile-v1.json"))
                .unwrap();
        assert_eq!(fixture.firmware_commit, "0961aa6");
        assert_eq!(fixture.profile_version, RENDER_PROFILE_VERSION);
        assert!(fixture.comparison.starts_with("exact independently"));
        let case = fixture.dither_case;
        let pixels = RgbaImage::from_raw(case.width, case.height, case.rgba).unwrap();
        assert_eq!(firmware_dither(&pixels).unwrap().indices, case.indices);
        for case in fixture.negative_shift_cases {
            assert_eq!((case.error * case.weight) >> 4, case.expected);
        }
        let case = fixture.nearest_resize_case;
        let source = RgbaImage::from_fn(case.source_width, case.source_height, |x, y| {
            Rgba([(x + y * case.source_width) as u8, 0, 0, 255])
        });
        let resized = resize_nearest_firmware(&source, case.target_width, case.target_height);
        assert_eq!(
            resized.pixels().map(|pixel| pixel[0]).collect::<Vec<_>>(),
            case.expected_source_indices
        );
        for case in fixture.bmp_targets {
            let target = match case.name.as_str() {
                "paper_7" => FrameTarget::Paper7,
                "paper_l" => FrameTarget::PaperL,
                _ => panic!("unknown target fixture"),
            };
            assert_eq!(target.physical_dimensions(), (case.width, case.height));
            let bmp = encode_bmp4(&IndexedImage {
                width: case.width,
                height: case.height,
                indices: vec![6; case.width as usize * case.height as usize],
            })
            .unwrap();
            assert_eq!(bmp.len(), case.file_size);
            validate_bmp4(&bmp, target).unwrap();
        }
    }

    #[test]
    fn bmp_encoder_packs_top_down_nibbles_and_validates_targets() {
        let mut indices = vec![6; 800 * 480];
        indices[0..6].copy_from_slice(&[0, 1, 2, 3, 5, 6]);
        let bmp = encode_bmp4(&IndexedImage {
            width: 800,
            height: 480,
            indices,
        })
        .unwrap();
        validate_bmp4(&bmp, FrameTarget::Paper7).unwrap();
        assert_eq!(&bmp[118..121], &[0x01, 0x23, 0x56]);
        assert_eq!(get_i32(&bmp, 22).unwrap(), -480);
        let independently_decoded = image::load_from_memory_with_format(&bmp, ImageFormat::Bmp)
            .unwrap()
            .into_rgb8();
        assert_eq!(independently_decoded.dimensions(), (800, 480));
        assert_eq!(independently_decoded.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(independently_decoded.get_pixel(1, 0).0, [0, 0, 255]);
    }

    #[test]
    fn dithered_preview_pixels_match_the_served_bmp_exactly() {
        let decoded = DecodedImage {
            pixels: source(17, 29),
            format: ImageFormat::Png,
            orientation: ImageOrientation::Portrait,
        };
        let rendered = render(
            &decoded,
            FrameTarget::Paper7,
            OutputMode::ServerDitherBmp,
            ImageOrientation::Portrait,
        )
        .unwrap();
        let preview =
            dithered_preview_png(&decoded, FrameTarget::Paper7, ImageOrientation::Portrait)
                .unwrap();
        let bmp_pixels = image::load_from_memory_with_format(&rendered.bytes, ImageFormat::Bmp)
            .unwrap()
            .into_rgb8();
        let preview_pixels = image::load_from_memory_with_format(&preview, ImageFormat::Png)
            .unwrap()
            .into_rgb8();
        assert_eq!(bmp_pixels, preview_pixels);
    }

    #[test]
    fn jpeg_and_png_outputs_decode() {
        let input = source(4, 3);
        let jpeg = encode_jpeg(&input).unwrap();
        assert_eq!(image::guess_format(&jpeg).unwrap(), ImageFormat::Jpeg);
        let indexed = firmware_dither(&input).unwrap();
        let png = encode_indexed_png(&indexed).unwrap();
        assert_eq!(image::guess_format(&png).unwrap(), ImageFormat::Png);
    }

    #[test]
    fn enabled_static_codec_families_round_trip_through_decoder() {
        let input = DynamicImage::ImageRgba8(source(4, 3));
        for (format, name) in [
            (ImageFormat::Png, "test.png"),
            (ImageFormat::Jpeg, "test.jpg"),
            (ImageFormat::Gif, "test.gif"),
            (ImageFormat::WebP, "test.webp"),
            (ImageFormat::Bmp, "test.bmp"),
            (ImageFormat::Ico, "test.ico"),
            (ImageFormat::Tiff, "test.tiff"),
            (ImageFormat::Tga, "test.tga"),
            (ImageFormat::Pnm, "test.ppm"),
            (ImageFormat::Hdr, "test.hdr"),
            (ImageFormat::OpenExr, "test.exr"),
            (ImageFormat::Farbfeld, "test.ff"),
            (ImageFormat::Qoi, "test.qoi"),
        ] {
            let mut cursor = Cursor::new(Vec::new());
            if format == ImageFormat::Farbfeld {
                DynamicImage::ImageRgba16(input.to_rgba16())
                    .write_to(&mut cursor, format)
                    .unwrap();
            } else if format == ImageFormat::OpenExr {
                DynamicImage::ImageRgba32F(input.to_rgba32f())
                    .write_to(&mut cursor, format)
                    .unwrap();
            } else if format == ImageFormat::Hdr {
                DynamicImage::ImageRgb32F(input.to_rgb32f())
                    .write_to(&mut cursor, format)
                    .unwrap();
            } else if matches!(format, ImageFormat::Jpeg | ImageFormat::Pnm) {
                DynamicImage::ImageRgb8(input.to_rgb8())
                    .write_to(&mut cursor, format)
                    .unwrap();
            } else {
                input.write_to(&mut cursor, format).unwrap();
            }
            let decoded = decode_source_named(cursor.get_ref(), Some(name))
                .unwrap_or_else(|error| panic!("{format:?}: {error:#}"));
            assert_eq!(decoded.pixels.dimensions(), (4, 3), "{format:?}");
        }
    }

    #[test]
    fn committed_visual_regressions_are_full_frame_and_six_color() {
        for fixture in [
            include_bytes!("../tests/fixtures/visual/paper7-landscape.png").as_slice(),
            include_bytes!("../tests/fixtures/visual/paper7-portrait.png").as_slice(),
        ] {
            let image = image::load_from_memory_with_format(fixture, ImageFormat::Png)
                .unwrap()
                .into_rgb8();
            assert_eq!(
                image.dimensions(),
                FrameTarget::Paper7.physical_dimensions()
            );
            assert!(image.pixels().all(|pixel| {
                DEVICE_PALETTE
                    .iter()
                    .any(|(_, color)| pixel.0 == [color.r, color.g, color.b])
            }));
        }
    }

    #[test]
    #[ignore = "release-mode rendering benchmark smoke test"]
    fn benchmark_full_frame_profiles() {
        let input = source(1600, 1200);
        for target in [FrameTarget::Paper7, FrameTarget::PaperL] {
            let started = std::time::Instant::now();
            let framebuffer = prepare_framebuffer(&input, target, ImageOrientation::Landscape);
            let indexed = firmware_dither(&framebuffer).unwrap();
            let bmp = encode_bmp4(&indexed).unwrap();
            eprintln!(
                "target={target:?} elapsed_ms={} bytes={}",
                started.elapsed().as_millis(),
                bmp.len()
            );
            validate_bmp4(&bmp, target).unwrap();
        }
    }

    #[test]
    fn corrupt_and_overdimensioned_sources_are_rejected() {
        assert!(validate_encoded_length(0).is_err());
        assert!(validate_encoded_length(MAX_ENCODED_BYTES as usize + 1).is_err());
        assert!(decode_source_named(b"not an image", Some("bad.png")).is_err());
        let input = DynamicImage::ImageRgba8(source(2, 3));
        let mut cursor = Cursor::new(Vec::new());
        input.write_to(&mut cursor, ImageFormat::Bmp).unwrap();
        let mut bmp = cursor.into_inner();
        bmp[18..22].copy_from_slice(&(MAX_SOURCE_DIMENSION + 1).to_le_bytes());
        assert!(decode_source_named(&bmp, Some("large.bmp")).is_err());
    }

    #[test]
    fn animated_gif_uses_its_first_frame() {
        let first = RgbaImage::from_pixel(2, 1, Rgba([255, 0, 0, 255]));
        let second = RgbaImage::from_pixel(2, 1, Rgba([0, 0, 255, 255]));
        let mut bytes = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut bytes)
            .encode_frames([image::Frame::new(first), image::Frame::new(second)])
            .unwrap();
        let decoded = decode_source_named(&bytes, Some("animated.gif")).unwrap();
        assert_eq!(decoded.pixels.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn transparent_pixels_are_composited_on_white() {
        let mut pixels = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 0]));
        composite_alpha_on_white(&mut pixels);
        assert_eq!(pixels.get_pixel(0, 0).0, [255, 255, 255, 255]);
    }
}
