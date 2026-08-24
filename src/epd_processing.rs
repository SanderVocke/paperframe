use image::RgbaImage;

use crate::{
    dither::Rgb,
    epd_profile::{
        DynamicRangeMode, LevelCompression, LevelCompressionMode, PaperNormalization,
        PaperNormalizationMode, ResolvedDitherOptions, ToneMapping, ToneMappingMode,
    },
};

pub fn apply_adjustments(image: &mut RgbaImage, options: &ResolvedDitherOptions, palette: &[Rgb]) {
    let white = options.dynamic_range.as_ref().and_then(|range| {
        if range.preserve_white {
            white_plan(
                image,
                range.white_preserve_percentile,
                range.white_preserve_min_luma,
                range.white_preserve_max_saturation,
                palette,
            )
        } else {
            None
        }
    });
    if let Some(paper) = &options.paper_normalization {
        apply_paper(image, paper);
    }
    if let Some(tone) = &options.tone_mapping {
        apply_tone(image, tone);
    }
    if let Some(range) = &options.dynamic_range {
        apply_dynamic_range(
            image,
            range.mode,
            range.strength,
            range.low_percentile,
            range.high_percentile,
            palette,
        );
    }
    if let Some(level) = &options.level_compression {
        apply_level(image, level);
    }
    if let Some(plan) = white {
        apply_white_plan(image, &plan);
    }
}

fn clamp_byte(value: f64) -> u8 {
    if !value.is_finite() || value <= 0.0 {
        0
    } else if value >= 255.0 {
        255
    } else {
        (value + 0.5).floor() as u8
    }
}
fn luma(r: f64, g: f64, b: f64) -> f64 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}
fn sat(r: f64, g: f64, b: f64) -> f64 {
    let max = r.max(g).max(b) / 255.;
    let min = r.min(g).min(b) / 255.;
    if max == 0. { 0. } else { (max - min) / max }
}
fn norm(v: f64, min: f64, max: f64) -> f64 {
    ((v - min) / (max - min)).clamp(0., 1.)
}

fn apply_paper(image: &mut RgbaImage, o: &PaperNormalization) {
    if o.mode == PaperNormalizationMode::Off {
        return;
    }
    let strength = o.strength.clamp(0., 1.);
    for p in image.pixels_mut() {
        let (r, g, b) = (p[0] as f64, p[1] as f64, p[2] as f64);
        let y = luma(r, g, b);
        let s = sat(r, g, b);
        if s >= 0.34 && r >= g + 24. && r >= b + 28. {
            let boost = strength * o.preserve_red.clamp(0., 1.);
            p[0] = clamp_byte(r + (255. - r) * 0.08 * boost);
            p[1] = clamp_byte(g * (1. - 0.08 * boost));
            p[2] = clamp_byte(b * (1. - 0.12 * boost));
            continue;
        }
        let dark = norm(112. - y, 0., 72.) * norm(0.42 - s, 0., 0.32);
        if dark > 0. {
            let amount = dark * o.black_anchor.clamp(0., 1.) * strength;
            p[0] = clamp_byte(r * (1. - 0.72 * amount));
            p[1] = clamp_byte(g * (1. - 0.72 * amount));
            p[2] = clamp_byte(b * (1. - 0.72 * amount));
            continue;
        }
        let warm = (r - b).min((r + g) / 2. - b);
        let mask = norm(y, o.min_luma, 210.)
            * norm(245. - y, 0., 80.)
            * norm(o.saturation_threshold - s, 0., o.saturation_threshold)
            * norm(warm, o.warm_bias_threshold, 34.);
        if mask <= 0. {
            continue;
        }
        let amount = mask * strength;
        let target = 252f64.min(y + (o.paper_white[0] - y) * (0.72 + 0.2 * strength));
        p[0] = clamp_byte(r + (target + (o.paper_white[0] - 248.) * 0.4 - r) * amount);
        p[1] = clamp_byte(g + (target + (o.paper_white[1] - 248.) * 0.4 - g) * amount);
        p[2] = clamp_byte(b + (target + (o.paper_white[2] - 248.) * 0.4 - b) * amount);
    }
}

fn apply_tone(image: &mut RgbaImage, o: &ToneMapping) {
    let exposure = 2f64.powf(o.exposure);
    if exposure != 1. {
        for p in image.pixels_mut() {
            for c in 0..3 {
                p[c] = clamp_byte(p[c] as f64 * exposure)
            }
        }
    }
    let saturation = (o.saturation + 1.).max(0.);
    if saturation != 1. {
        for p in image.pixels_mut() {
            apply_saturation(p, saturation)
        }
    }
    if o.mode == ToneMappingMode::Off {
        return;
    }
    if o.mode == ToneMappingMode::Contrast {
        let contrast = if o.contrast < 0. {
            (0.5 + o.contrast * 0.5).max(0.5)
        } else {
            o.contrast + 1.
        };
        if contrast != 1. {
            for p in image.pixels_mut() {
                for c in 0..3 {
                    p[c] = clamp_byte((p[c] as f64 - 128.) * contrast + 128.)
                }
            }
        }
    }
    if o.mode == ToneMappingMode::SCurve {
        let mid = o.midpoint.clamp(0.01, 0.99);
        let se = (1. - o.strength * o.shadow_boost * 1.5).clamp(0.15, 3.);
        let he = (1. - o.strength * o.highlight_compress).clamp(0.15, 3.);
        let mut lut = [0u8; 256];
        for (v, out) in lut.iter_mut().enumerate() {
            let n = v as f64 / 255.;
            let r = if n <= mid {
                (n / mid).powf(se) * mid
            } else {
                mid + ((n - mid) / (1. - mid)).powf(he) * (1. - mid)
            };
            *out = clamp_byte(r * 255.)
        }
        for p in image.pixels_mut() {
            p[0] = lut[p[0] as usize];
            p[1] = lut[p[1] as usize];
            p[2] = lut[p[2] as usize];
        }
    }
}
fn apply_saturation(p: &mut image::Rgba<u8>, factor: f64) {
    let (r, g, b) = (p[0] as f64 / 255., p[1] as f64 / 255., p[2] as f64 / 255.);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let light = (max + min) / 2.;
    if max == min {
        return;
    }
    let d = max - min;
    let s = if light > 0.5 {
        d / (2. - max - min)
    } else {
        d / (max + min).max(0.000001)
    };
    let h = if max == r {
        ((g - b) / d + if g < b { 6. } else { 0. }) / 6.
    } else if max == g {
        ((b - r) / d + 2.) / 6.
    } else {
        ((r - g) / d + 4.) / 6.
    };
    let ns = (s * factor).clamp(0., 1.);
    let c = (1. - (2. * light - 1.).abs()) * ns;
    let x = c * (1. - ((h * 6.) % 2. - 1.).abs());
    let m = light - c / 2.;
    let (rp, gp, bp) = match (h * 6.).floor() as i32 {
        0 => (c, x, 0.),
        1 => (x, c, 0.),
        2 => (0., c, x),
        3 => (0., x, c),
        4 => (x, 0., c),
        _ => (c, 0., x),
    };
    p[0] = clamp_byte((rp + m) * 255.);
    p[1] = clamp_byte((gp + m) * 255.);
    p[2] = clamp_byte((bp + m) * 255.);
}

fn apply_level(image: &mut RgbaImage, o: &LevelCompression) {
    if o.mode == LevelCompressionMode::Off {
        return;
    }
    if o.auto {
        let mut outside = 0usize;
        for p in image.pixels() {
            let bad = match o.mode {
                LevelCompressionMode::PerChannel => {
                    (0..3).any(|c| (p[c] as f64) < o.black[c] || (p[c] as f64) > o.white[c])
                }
                LevelCompressionMode::Luma => {
                    let y = luma(p[0] as f64, p[1] as f64, p[2] as f64);
                    y < luma(o.black[0], o.black[1], o.black[2])
                        || y > luma(o.white[0], o.white[1], o.white[2])
                }
                LevelCompressionMode::Off => false,
            };
            outside += usize::from(bad);
        }
        let ratio = outside as f64 / f64::from(image.width()) / f64::from(image.height());
        if ratio < o.auto_threshold {
            return;
        }
    }
    if o.mode == LevelCompressionMode::PerChannel {
        let d = [
            o.white[0] - o.black[0],
            o.white[1] - o.black[1],
            o.white[2] - o.black[2],
        ];
        if d.iter().any(|value| *value <= 0.0) {
            return;
        }
        for p in image.pixels_mut() {
            for c in 0..3 {
                p[c] = clamp_byte(o.black[c] + p[c] as f64 * d[c] / 255.0);
            }
        }
    } else {
        let black = luma(o.black[0], o.black[1], o.black[2]);
        let white = luma(o.white[0], o.white[1], o.white[2]);
        let d = white - black;
        if d <= 0.0 {
            return;
        }
        for p in image.pixels_mut() {
            let y = luma(p[0] as f64, p[1] as f64, p[2] as f64);
            let y_new = black + y * d / 255.0;
            let mut ratio = if y > 0.0 { y_new / y } else { 0.0 };
            let max = *p.0[..3].iter().max().unwrap() as f64;
            if max > 0.0 {
                ratio = ratio.min(255.0 / max);
            }
            for c in 0..3 {
                p[c] = clamp_byte(p[c] as f64 * ratio);
            }
        }
    }
}

pub fn rgb_to_lab(rgb: Rgb) -> [f64; 3] {
    let linear = |v: u8| {
        let n = v as f64 / 255.;
        if n > 0.04045 {
            ((n + 0.055) / 1.055).powf(2.4)
        } else {
            n / 12.92
        }
    };
    let (r, g, b) = (linear(rgb.r), linear(rgb.g), linear(rgb.b));
    let (x, y, z) = (
        (r * 0.4124564 + g * 0.3575761 + b * 0.1804375) * 100.,
        (r * 0.2126729 + g * 0.7151522 + b * 0.072175) * 100.,
        (r * 0.0193339 + g * 0.119192 + b * 0.9503041) * 100.,
    );
    let pivot = |v: f64| {
        if v > 0.008856 {
            v.cbrt()
        } else {
            7.787 * v + 16. / 116.
        }
    };
    let (xn, yn, zn) = (pivot(x / 95.047), pivot(y / 100.), pivot(z / 108.883));
    [116. * yn - 16., 500. * (xn - yn), 200. * (yn - zn)]
}
fn lab_to_rgb(l: f64, a: f64, b: f64) -> Rgb {
    let mut y = (l + 16.) / 116.;
    let mut x = a / 500. + y;
    let mut z = y - b / 200.;
    let inv = |v: f64| {
        if v > 0.206897 {
            v.powi(3)
        } else {
            (v - 16. / 116.) / 7.787
        }
    };
    x = inv(x) * 95.047;
    y = inv(y) * 100.;
    z = inv(z) * 108.883;
    let (x, y, z) = (x / 100., y / 100., z / 100.);
    let conv = |v: f64| {
        if v > 0.0031308 {
            1.055 * v.powf(1. / 2.4) - 0.055
        } else {
            12.92 * v
        }
    };
    Rgb::new(
        clamp_byte(conv(x * 3.2404542 + y * -1.5371385 + z * -0.4985314) * 255.),
        clamp_byte(conv(x * -0.969266 + y * 1.8760108 + z * 0.041556) * 255.),
        clamp_byte(conv(x * 0.0556434 + y * -0.2040259 + z * 1.0572252) * 255.),
    )
}
fn lab_lightness(rgb: Rgb) -> f64 {
    rgb_to_lab(rgb)[0]
}
fn endpoints(palette: &[Rgb]) -> (Rgb, Rgb) {
    let mut black = palette[0];
    let mut white = palette[0];
    for &p in palette {
        if luma(p.r as f64, p.g as f64, p.b as f64)
            < luma(black.r as f64, black.g as f64, black.b as f64)
        {
            black = p
        }
        if luma(p.r as f64, p.g as f64, p.b as f64)
            > luma(white.r as f64, white.g as f64, white.b as f64)
        {
            white = p
        }
    }
    (black, white)
}
fn apply_dynamic_range(
    image: &mut RgbaImage,
    mode: DynamicRangeMode,
    strength: f64,
    low: f64,
    high: f64,
    palette: &[Rgb],
) {
    if mode == DynamicRangeMode::Off || strength <= 0. {
        return;
    }
    let (black, white) = endpoints(palette);
    let bl = lab_lightness(black);
    let wl = lab_lightness(white);
    let tr = wl - bl;
    if tr <= 0. {
        return;
    }
    let (mut sb, mut sw) = (0., 100.);
    if mode == DynamicRangeMode::Auto {
        let mut vals = image
            .pixels()
            .map(|p| lab_lightness(Rgb::new(p[0], p[1], p[2])))
            .collect::<Vec<_>>();
        vals.sort_by(f64::total_cmp);
        let pick = |p: f64| {
            vals[js_round((vals.len() - 1) as f64 * p).clamp(0., (vals.len() - 1) as f64) as usize]
        };
        sb = pick(low);
        sw = pick(high)
    }
    let sr = sw - sb;
    if sr <= 0.0001 {
        return;
    }
    for p in image.pixels_mut() {
        let src = Rgb::new(p[0], p[1], p[2]);
        let lab = rgb_to_lab(src);
        let n = ((lab[0] - sb) / sr).clamp(0., 1.);
        let target = bl + n * tr;
        let protection = smoothstep(0.18, 0.68, sat(p[0] as f64, p[1] as f64, p[2] as f64)) * 0.85;
        let amount = strength.clamp(0., 1.) * (1. - protection);
        let out = lab_to_rgb_with_chroma_guard(src, lab, target, amount);
        p[0] = out.r;
        p[1] = out.g;
        p[2] = out.b;
    }
}
fn lab_to_rgb_with_chroma_guard(source: Rgb, lab: [f64; 3], target_l: f64, amount: f64) -> Rgb {
    let source_saturation = sat(source.r as f64, source.g as f64, source.b as f64);
    let source_luma = luma(source.r as f64, source.g as f64, source.b as f64);
    let convert = |fit: f64| lab_to_rgb(lab[0] + (target_l - lab[0]) * fit, lab[1], lab[2]);
    let protected = |candidate: Rgb| {
        if source_saturation < 0.16 {
            return true;
        }
        let result_saturation = sat(candidate.r as f64, candidate.g as f64, candidate.b as f64);
        let minimum = 0.12_f64.max(source_saturation * 0.72);
        result_saturation >= minimum
            || luma(candidate.r as f64, candidate.g as f64, candidate.b as f64) <= source_luma + 4.0
    };
    let result = convert(amount);
    if target_l <= lab[0] || protected(result) {
        return result;
    }
    let mut low = 0.0;
    let mut high = amount;
    let mut result = source;
    for _ in 0..5 {
        let mid = (low + high) / 2.0;
        let candidate = convert(mid);
        if protected(candidate) {
            low = mid;
            result = candidate;
        } else {
            high = mid;
        }
    }
    result
}

fn smoothstep(a: f64, b: f64, v: f64) -> f64 {
    let x = norm(v, a, b);
    x * x * (3. - 2. * x)
}
fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

struct WhitePlan {
    lumas: Vec<f64>,
    candidates: Vec<bool>,
    source: f64,
    target: Rgb,
    target_luma: f64,
}
fn white_plan(
    image: &RgbaImage,
    p: f64,
    min: f64,
    max_sat: f64,
    palette: &[Rgb],
) -> Option<WhitePlan> {
    let mut lumas = Vec::with_capacity((image.width() * image.height()) as usize);
    let mut candidates = Vec::with_capacity(lumas.capacity());
    let mut whites = Vec::new();
    for px in image.pixels() {
        if px[3] <= 16 {
            lumas.push(-1.);
            candidates.push(false);
            continue;
        }
        let y = luma(px[0] as f64, px[1] as f64, px[2] as f64);
        let c = sat(px[0] as f64, px[1] as f64, px[2] as f64) <= max_sat.clamp(0., 1.);
        lumas.push(y);
        candidates.push(c);
        if c {
            whites.push(y)
        }
    }
    if whites.is_empty() {
        return None;
    }
    whites.sort_by(f64::total_cmp);
    let source = whites
        [js_round((whites.len() - 1) as f64 * p).clamp(0., (whites.len() - 1) as f64) as usize];
    if source < min {
        return None;
    }
    let (_, target) = endpoints(palette);
    Some(WhitePlan {
        lumas,
        candidates,
        source,
        target,
        target_luma: luma(target.r as f64, target.g as f64, target.b as f64),
    })
}
fn apply_white_plan(image: &mut RgbaImage, p: &WhitePlan) {
    for (i, px) in image.pixels_mut().enumerate() {
        if !p.candidates[i] || p.lumas[i] + 0.0001 < p.source {
            continue;
        }
        if luma(px[0] as f64, px[1] as f64, px[2] as f64) >= p.target_luma {
            continue;
        }
        px[0] = p.target.r;
        px[1] = p.target.g;
        px[2] = p.target.b;
    }
}

pub fn lab_distance(a: Rgb, b: Rgb) -> f64 {
    let a = rgb_to_lab(a);
    let b = rgb_to_lab(b);
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    #[test]
    fn lab_reference_colors_are_stable() {
        let white = rgb_to_lab(Rgb::new(255, 255, 255));
        assert!((white[0] - 100.).abs() < 0.0001);
        assert!(lab_distance(Rgb::new(10, 20, 30), Rgb::new(10, 20, 30)) < 1e-12);
    }
    #[test]
    fn future_manual_adjustments_execute_through_tone_model() {
        let mut image = RgbaImage::from_pixel(1, 1, Rgba([100, 120, 140, 255]));
        apply_tone(
            &mut image,
            &ToneMapping {
                mode: ToneMappingMode::Contrast,
                exposure: 0.25,
                contrast: 0.2,
                saturation: 0.3,
                ..Default::default()
            },
        );
        assert_ne!(image.get_pixel(0, 0).0, [100, 120, 140, 255]);
    }
}
