use anyhow::{Context, Result};
use image::RgbaImage;

use crate::{
    dither::{IndexedImage, Rgb},
    epd_processing::{apply_adjustments, lab_distance},
    epd_profile::{ColorMatching, DitherProfile, DitheringMode, ResolvedDitherOptions},
};

pub fn process_canvas(
    canvas: &RgbaImage,
    profile: &DitherProfile,
    options: &ResolvedDitherOptions,
) -> Result<IndexedImage> {
    let mut adjusted = canvas.clone();
    adjust_canvas(&mut adjusted, profile, options)?;
    process_adjusted_canvas(&adjusted, profile, options)
}

/// Applies profile adjustments without quantizing. Composition layers that must
/// retain exact palette colors, such as text, are added after this stage.
pub fn adjust_canvas(
    canvas: &mut RgbaImage,
    profile: &DitherProfile,
    options: &ResolvedDitherOptions,
) -> Result<()> {
    profile.validate()?;
    let palette = profile
        .palette
        .iter()
        .map(|entry| entry.color)
        .collect::<Vec<_>>();
    apply_adjustments(canvas, options, &palette);
    Ok(())
}

/// Quantizes an already-adjusted canvas. This keeps high-contrast composition
/// layers out of the photo analysis and tone-adjustment stages.
pub fn process_adjusted_canvas(
    canvas: &RgbaImage,
    profile: &DitherProfile,
    options: &ResolvedDitherOptions,
) -> Result<IndexedImage> {
    profile.validate()?;
    let palette = profile
        .palette
        .iter()
        .map(|entry| entry.color)
        .collect::<Vec<_>>();
    let positions = match options.mode {
        DitheringMode::QuantizationOnly => quantize(canvas, &palette, options.color_matching),
        DitheringMode::ErrorDiffusion => diffuse(canvas, &palette, options),
    };
    let indices = positions
        .into_iter()
        .map(|position| {
            profile
                .palette
                .get(position as usize)
                .map(|entry| entry.device_index)
                .context("dither produced an invalid palette position")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(IndexedImage {
        width: canvas.width(),
        height: canvas.height(),
        indices,
    })
}

fn quantize(image: &RgbaImage, palette: &[Rgb], matching: ColorMatching) -> Vec<u8> {
    image
        .pixels()
        .map(|p| closest(Rgb::new(p[0], p[1], p[2]), palette, matching) as u8)
        .collect()
}
fn diffuse(image: &RgbaImage, palette: &[Rgb], o: &ResolvedDitherOptions) -> Vec<u8> {
    let w = image.width() as usize;
    let h = image.height() as usize;
    let mut data = image.as_raw().clone();
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        let reverse = o.serpentine && y % 2 == 1;
        for offset in 0..w {
            let x = if reverse { w - 1 - offset } else { offset };
            let i = (y * w + x) * 4;
            let old = Rgb::new(data[i], data[i + 1], data[i + 2]);
            let pi = closest(old, palette, o.color_matching);
            out[y * w + x] = pi as u8;
            let selected = palette[pi];
            data[i] = selected.r;
            data[i + 1] = selected.g;
            data[i + 2] = selected.b;
            data[i + 3] = 255;
            let err = [
                old.r as i16 - selected.r as i16,
                old.g as i16 - selected.g as i16,
                old.b as i16 - selected.b as i16,
            ];
            for step in o.kernel.steps() {
                let dx = if reverse { -step.dx } else { step.dx } as isize;
                let nx = x as isize + dx;
                let ny = y + step.dy as usize;
                if nx < 0 || nx >= w as isize || ny >= h {
                    continue;
                }
                let ni = (ny * w + nx as usize) * 4;
                for c in 0..3 {
                    data[ni + c] = clamp_byte(data[ni + c] as f64 + err[c] as f64 * step.factor)
                }
            }
        }
    }
    out
}
fn closest(pixel: Rgb, palette: &[Rgb], matching: ColorMatching) -> usize {
    let mut best = 0;
    let mut distance = f64::INFINITY;
    for (i, &candidate) in palette.iter().enumerate() {
        let d = match matching {
            ColorMatching::Rgb => {
                let dr = candidate.r as f64 - pixel.r as f64;
                let dg = candidate.g as f64 - pixel.g as f64;
                let db = candidate.b as f64 - pixel.b as f64;
                (dr * dr + dg * dg + db * db).sqrt()
            }
            ColorMatching::Lab => lab_distance(candidate, pixel),
        };
        if d < distance {
            distance = d;
            best = i
        }
    }
    best
}
fn clamp_byte(v: f64) -> u8 {
    if !v.is_finite() || v <= 0. {
        0
    } else if v >= 255. {
        255
    } else {
        (v + 0.5).floor() as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        dither::DiffusionKernel,
        epd_profile::{ColorMatching, DitherProfile, DitheringMode},
    };
    use image::Rgba;
    #[test]
    fn custom_palette_positions_map_to_hardware_indices() {
        let profile = DitherProfile::generate_picture_auto_v2();
        let image = RgbaImage::from_fn(6, 1, |x, _| {
            let c = profile.palette[x as usize].color;
            Rgba([c.r, c.g, c.b, 255])
        });
        let options = ResolvedDitherOptions {
            mode: DitheringMode::QuantizationOnly,
            kernel: DiffusionKernel::FloydSteinberg,
            serpentine: true,
            color_matching: ColorMatching::Rgb,
            tone_mapping: None,
            dynamic_range: None,
            level_compression: None,
            paper_normalization: None,
        };
        assert_eq!(
            process_canvas(&image, &profile, &options).unwrap().indices,
            [0, 6, 1, 2, 3, 5]
        );
    }
}
