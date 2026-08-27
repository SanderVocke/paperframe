use std::io::Cursor;

use anyhow::{Context, Result, bail};
use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbaImage,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
    imageops,
};
use image::{ImageEncoder, metadata::Orientation as ExifOrientation};
use serde::{Deserialize, Serialize};

use crate::{
    dither::{IndexedImage, Rgb},
    epd_auto::resolve_auto_options,
    epd_pipeline::process_canvas,
    epd_profile::{DitherProfile, PlacementMode, ResizePolicy},
};

pub const RENDER_PROFILE_VERSION: u32 = 2;
pub const MAX_ENCODED_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_SOURCE_DIMENSION: u32 = 20_000;
pub const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;
pub const JPEG_QUALITY: u8 = 90;
pub const THUMBNAIL_MAX_DIMENSION: u32 = 960;
pub const PREVIEW_MAX_DIMENSION: u32 = 1600;
const THUMBNAIL_JPEG_QUALITY: u8 = 80;
const PREVIEW_JPEG_QUALITY: u8 = 85;

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
            let indexed = generate_picture_dither(&source.pixels, target, orientation)?;
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
    let indexed = generate_picture_dither(&source.pixels, target, orientation)?;
    encode_indexed_png(&indexed)
}

pub fn prepare_framebuffer(
    source: &RgbaImage,
    target: FrameTarget,
    orientation: ImageOrientation,
) -> RgbaImage {
    prepare_profile_canvas(
        source,
        target,
        orientation,
        false,
        &DitherProfile::generate_picture_auto_v2(),
    )
}

pub fn prepare_analysis_canvas(
    source: &RgbaImage,
    target: FrameTarget,
    orientation: ImageOrientation,
) -> RgbaImage {
    prepare_profile_canvas(
        source,
        target,
        orientation,
        target == FrameTarget::PaperL,
        &DitherProfile::generate_picture_auto_v2(),
    )
}

fn prepare_profile_canvas(
    source: &RgbaImage,
    target: FrameTarget,
    orientation: ImageOrientation,
    half_size: bool,
    profile: &DitherProfile,
) -> RgbaImage {
    debug_assert_eq!(profile.placement, PlacementMode::AspectFitWhite);
    let (mut logical_width, mut logical_height) = target.logical_dimensions(orientation);
    if half_size {
        logical_width /= 2;
        logical_height /= 2;
    }
    let scale = (logical_width as f64 / source.width() as f64)
        .min(logical_height as f64 / source.height() as f64);
    let render_width = ((source.width() as f64 * scale).round() as u32).clamp(1, logical_width);
    let render_height = ((source.height() as f64 * scale).round() as u32).clamp(1, logical_height);
    // Canvas drawImage uses browser smoothing. Triangle filtering is the closest
    // deterministic image-crate equivalent and is isolated behind the profile.
    let filter = match profile.resize {
        ResizePolicy::BrowserLikeTriangle => imageops::FilterType::Triangle,
    };
    let resized = imageops::resize(source, render_width, render_height, filter);
    let mut logical = RgbaImage::from_pixel(
        logical_width,
        logical_height,
        image::Rgba([255, 255, 255, 255]),
    );
    imageops::overlay(
        &mut logical,
        &resized,
        i64::from((logical_width - render_width) / 2),
        i64::from((logical_height - render_height) / 2),
    );
    if (logical_width, logical_height)
        == if half_size {
            let (width, height) = target.physical_dimensions();
            (width / 2, height / 2)
        } else {
            target.physical_dimensions()
        }
    {
        logical
    } else {
        // GeneratePicture rotates orientation-mismatched sources by 270°.
        imageops::rotate270(&logical)
    }
}

fn generate_picture_dither(
    source: &RgbaImage,
    target: FrameTarget,
    orientation: ImageOrientation,
) -> Result<IndexedImage> {
    let profile = DitherProfile::generate_picture_auto_v2();
    let analysis = prepare_profile_canvas(
        source,
        target,
        orientation,
        target == FrameTarget::PaperL,
        &profile,
    );
    let (_, options) = resolve_auto_options(&analysis, &profile);
    let framebuffer = prepare_profile_canvas(source, target, orientation, false, &profile);
    process_canvas(&framebuffer, &profile, &options)
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
    encode_jpeg_quality(image, JPEG_QUALITY)
}

pub fn thumbnail_jpeg(source: &DecodedImage) -> Result<Vec<u8>> {
    resized_jpeg(
        &source.pixels,
        THUMBNAIL_MAX_DIMENSION,
        THUMBNAIL_JPEG_QUALITY,
    )
}

pub fn preview_jpeg(source: &DecodedImage) -> Result<Vec<u8>> {
    resized_jpeg(&source.pixels, PREVIEW_MAX_DIMENSION, PREVIEW_JPEG_QUALITY)
}

fn resized_jpeg(image: &RgbaImage, maximum_dimension: u32, quality: u8) -> Result<Vec<u8>> {
    let longest = image.width().max(image.height());
    if longest <= maximum_dimension {
        return encode_jpeg_quality(image, quality);
    }
    let scale = maximum_dimension as f64 / longest as f64;
    let width = ((image.width() as f64 * scale).round() as u32).max(1);
    let height = ((image.height() as f64 * scale).round() as u32).max(1);
    let resized = imageops::resize(image, width, height, imageops::FilterType::Lanczos3);
    encode_jpeg_quality(&resized, quality)
}

fn encode_jpeg_quality(image: &RgbaImage, quality: u8) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    JpegEncoder::new_with_quality(&mut output, quality)
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
    fn framebuffer_uses_exact_target_dimensions_for_both_orientations() {
        for target in [FrameTarget::Paper7, FrameTarget::PaperL] {
            for orientation in [ImageOrientation::Portrait, ImageOrientation::Landscape] {
                let output = prepare_framebuffer(&source(7, 11), target, orientation);
                assert_eq!(output.dimensions(), target.physical_dimensions());
            }
        }
    }

    #[test]
    fn generate_picture_canvas_aspect_fits_on_white() {
        let input = RgbaImage::from_pixel(100, 100, Rgba([255, 0, 0, 255]));
        let output = prepare_framebuffer(&input, FrameTarget::Paper7, ImageOrientation::Landscape);
        assert_eq!(output.dimensions(), (800, 480));
        assert_eq!(output.get_pixel(0, 0).0, [255, 255, 255, 255]);
        assert_eq!(output.get_pixel(400, 240).0, [255, 0, 0, 255]);
        let analysis =
            prepare_analysis_canvas(&input, FrameTarget::PaperL, ImageOrientation::Portrait);
        assert_eq!(analysis.dimensions(), (600, 800));
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
    fn canonical_sources_match_generate_picture_browser_bmps_exactly() {
        for (source_bytes, expected, target, orientation) in [
            (
                include_bytes!("../tests/fixtures/browser/paper7-source.png").as_slice(),
                include_bytes!("../tests/fixtures/browser/paper7-auto.bmp").as_slice(),
                FrameTarget::Paper7,
                ImageOrientation::Landscape,
            ),
            (
                include_bytes!("../tests/fixtures/browser/paperl-source.png").as_slice(),
                include_bytes!("../tests/fixtures/browser/paperl-auto.bmp").as_slice(),
                FrameTarget::PaperL,
                ImageOrientation::Portrait,
            ),
        ] {
            let decoded = decode_source(source_bytes).unwrap();
            let rendered =
                render(&decoded, target, OutputMode::ServerDitherBmp, orientation).unwrap();
            assert_eq!(
                rendered.bytes, expected,
                "browser parity mismatch for {target:?}"
            );
        }
    }

    #[test]
    fn resized_browser_fixtures_meet_generate_picture_similarity_bounds() {
        for (source_bytes, expected, target, orientation) in [
            (
                include_bytes!("../tests/fixtures/browser/paper7-resized-source.png").as_slice(),
                include_bytes!("../tests/fixtures/browser/paper7-resized-auto.bmp").as_slice(),
                FrameTarget::Paper7,
                ImageOrientation::Landscape,
            ),
            (
                include_bytes!("../tests/fixtures/browser/paperl-rotated-resized-source.png")
                    .as_slice(),
                include_bytes!("../tests/fixtures/browser/paperl-rotated-resized-auto.bmp")
                    .as_slice(),
                FrameTarget::PaperL,
                ImageOrientation::Landscape,
            ),
        ] {
            let decoded = decode_source(source_bytes).unwrap();
            let profile = DitherProfile::generate_picture_auto_v2();
            let analysis = prepare_analysis_canvas(&decoded.pixels, target, orientation);
            let (classification, _) = resolve_auto_options(&analysis, &profile);
            assert_eq!(classification.kind, crate::epd_auto::ImageKind::Photo);
            let rendered =
                render(&decoded, target, OutputMode::ServerDitherBmp, orientation).unwrap();
            let actual = image::load_from_memory_with_format(&rendered.bytes, ImageFormat::Bmp)
                .unwrap()
                .into_rgb8();
            let expected = image::load_from_memory_with_format(expected, ImageFormat::Bmp)
                .unwrap()
                .into_rgb8();
            let similarity = box_filtered_ssim(&actual, &expected, 8);
            let exact_mismatches = actual
                .pixels()
                .zip(expected.pixels())
                .filter(|(actual, expected)| actual != expected)
                .count();
            let mut maximum_coverage_delta = 0.0_f64;
            assert!(similarity >= 0.97, "SSIM {similarity} for {target:?}");
            for (_, color) in DEVICE_PALETTE {
                let actual_ratio = actual
                    .pixels()
                    .filter(|pixel| pixel.0 == [color.r, color.g, color.b])
                    .count() as f64
                    / f64::from(actual.width() * actual.height());
                let expected_ratio = expected
                    .pixels()
                    .filter(|pixel| pixel.0 == [color.r, color.g, color.b])
                    .count() as f64
                    / f64::from(expected.width() * expected.height());
                let delta = (actual_ratio - expected_ratio).abs();
                maximum_coverage_delta = maximum_coverage_delta.max(delta);
                assert!(
                    delta <= 0.02,
                    "coverage mismatch for {color:?} on {target:?}"
                );
            }
            eprintln!(
                "target={target:?} box8_ssim={similarity:.9} exact_mismatches={exact_mismatches} max_coverage_delta={maximum_coverage_delta:.9}"
            );
        }
    }

    fn box_filtered_ssim(a: &image::RgbImage, b: &image::RgbImage, block: u32) -> f64 {
        assert_eq!(a.dimensions(), b.dimensions());
        let mut av = Vec::new();
        let mut bv = Vec::new();
        for y in (0..a.height()).step_by(block as usize) {
            for x in (0..a.width()).step_by(block as usize) {
                for channel in 0..3 {
                    let mut a_sum = 0.0;
                    let mut b_sum = 0.0;
                    let mut count = 0.0;
                    for yy in y..(y + block).min(a.height()) {
                        for xx in x..(x + block).min(a.width()) {
                            a_sum += f64::from(a.get_pixel(xx, yy)[channel]);
                            b_sum += f64::from(b.get_pixel(xx, yy)[channel]);
                            count += 1.0;
                        }
                    }
                    av.push(a_sum / count);
                    bv.push(b_sum / count);
                }
            }
        }
        let a_mean = av.iter().sum::<f64>() / av.len() as f64;
        let b_mean = bv.iter().sum::<f64>() / bv.len() as f64;
        let mut a_var = 0.0;
        let mut b_var = 0.0;
        let mut covariance = 0.0;
        for (a, b) in av.iter().zip(&bv) {
            a_var += (a - a_mean).powi(2);
            b_var += (b - b_mean).powi(2);
            covariance += (a - a_mean) * (b - b_mean);
        }
        let divisor = (av.len() - 1).max(1) as f64;
        a_var /= divisor;
        b_var /= divisor;
        covariance /= divisor;
        let c1 = (0.01_f64 * 255.0).powi(2);
        let c2 = (0.03_f64 * 255.0).powi(2);
        ((2.0 * a_mean * b_mean + c1) * (2.0 * covariance + c2))
            / ((a_mean.powi(2) + b_mean.powi(2) + c1) * (a_var + b_var + c2))
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
    fn thumbnail_and_preview_jpegs_are_bounded_without_upscaling() {
        let large = DecodedImage {
            pixels: source(2000, 1000),
            format: ImageFormat::Png,
            orientation: ImageOrientation::Landscape,
        };
        let thumbnail = image::load_from_memory(&thumbnail_jpeg(&large).unwrap()).unwrap();
        let preview = image::load_from_memory(&preview_jpeg(&large).unwrap()).unwrap();
        assert_eq!((thumbnail.width(), thumbnail.height()), (960, 480));
        assert_eq!((preview.width(), preview.height()), (1600, 800));

        let small = DecodedImage {
            pixels: source(40, 20),
            format: ImageFormat::Png,
            orientation: ImageOrientation::Landscape,
        };
        let thumbnail = image::load_from_memory(&thumbnail_jpeg(&small).unwrap()).unwrap();
        assert_eq!((thumbnail.width(), thumbnail.height()), (40, 20));
    }

    #[test]
    fn jpeg_and_png_outputs_decode() {
        let input = source(4, 3);
        let jpeg = encode_jpeg(&input).unwrap();
        assert_eq!(image::guess_format(&jpeg).unwrap(), ImageFormat::Jpeg);
        let indexed = IndexedImage {
            width: 4,
            height: 3,
            indices: vec![6; 12],
        };
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
    #[ignore = "release-mode rendering benchmark smoke test"]
    fn benchmark_full_frame_profiles() {
        let input = source(1600, 1200);
        for target in [FrameTarget::Paper7, FrameTarget::PaperL] {
            let started = std::time::Instant::now();
            let indexed =
                generate_picture_dither(&input, target, ImageOrientation::Landscape).unwrap();
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
