use std::collections::{HashMap, HashSet};

use image::RgbaImage;

use crate::{
    dither::DiffusionKernel,
    epd_profile::{
        AutoIntent, ColorMatching, DitherProfile, DitheringMode, DynamicRangeCompression,
        DynamicRangeMode, LevelCompression, LevelCompressionMode, PaperNormalization,
        PaperNormalizationMode, ResolvedDitherOptions, ToneMapping, ToneMappingMode,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageKind {
    Photo,
    LowContrastPhoto,
    HighContrastPhoto,
    FlatIllustration,
    LineArt,
    TextOrUi,
    PixelArt,
    Unknown,
}

const KIND_ORDER: [ImageKind; 8] = [
    ImageKind::Photo,
    ImageKind::LowContrastPhoto,
    ImageKind::HighContrastPhoto,
    ImageKind::FlatIllustration,
    ImageKind::LineArt,
    ImageKind::TextOrUi,
    ImageKind::PixelArt,
    ImageKind::Unknown,
];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImageMetrics {
    pub sample_count: usize,
    pub unique_color_ratio: f64,
    pub top_color_coverage: f64,
    pub palette_entropy: f64,
    pub flat_ratio: f64,
    pub soft_change_ratio: f64,
    pub strong_edge_ratio: f64,
    pub edge_density: f64,
    pub horizontal_edge_ratio: f64,
    pub vertical_edge_ratio: f64,
    pub luma_std_dev: f64,
    pub luma_p05: f64,
    pub luma_p95: f64,
    pub luma_range: f64,
    pub saturation_mean: f64,
    pub saturation_std_dev: f64,
    pub dark_ratio: f64,
    pub light_ratio: f64,
    pub gray_ratio: f64,
    pub high_saturation_ratio: f64,
    pub warm_paper_ratio: f64,
    pub red_ratio: f64,
    pub dark_neutral_ratio: f64,
    pub photo_tile_ratio: f64,
    pub flat_tile_ratio: f64,
    pub text_tile_ratio: f64,
    pub gradient_tile_ratio: f64,
    pub transparent_ratio: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Classification {
    pub kind: ImageKind,
    pub is_photo_style: bool,
    pub confidence: f64,
    pub photo_score: f64,
    pub kind_scores: HashMap<ImageKind, f64>,
    pub metrics: ImageMetrics,
}

#[derive(Clone, Copy)]
struct Sample {
    visible: bool,
    r: f64,
    g: f64,
    b: f64,
    luma: f64,
    saturation: f64,
}

impl Sample {
    fn hidden() -> Self {
        Self {
            visible: false,
            r: 0.0,
            g: 0.0,
            b: 0.0,
            luma: 0.0,
            saturation: 0.0,
        }
    }
}

pub fn classify_image_style(image: &RgbaImage) -> Classification {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let scale = (160.0 / width.max(height) as f64).min(1.0);
    let sw = js_round(width as f64 * scale).max(1.0) as usize;
    let sh = js_round(height as f64 * scale).max(1.0) as usize;
    let mut samples = vec![Sample::hidden(); sw * sh];
    let mut colors = HashMap::<u16, usize>::new();
    let mut lumas = Vec::new();
    let mut visible = 0usize;
    let mut transparent = 0usize;
    let mut luma_sum = 0.0;
    let mut luma_sq = 0.0;
    let mut sat_sum = 0.0;
    let mut sat_sq = 0.0;
    let mut dark = 0;
    let mut light = 0;
    let mut gray = 0;
    let mut high_sat = 0;
    let mut warm = 0;
    let mut red_count = 0;
    let mut dark_neutral = 0;
    for y in 0..sh {
        let sy = ((y as f64 / sh as f64) * height as f64).floor() as usize;
        for x in 0..sw {
            let sx = ((x as f64 / sw as f64) * width as f64).floor() as usize;
            let p = image.get_pixel(sx.min(width - 1) as u32, sy.min(height - 1) as u32);
            if p[3] <= 16 {
                transparent += 1;
                continue;
            }
            let (r, g, b) = (p[0] as f64, p[1] as f64, p[2] as f64);
            let luma = luma(r, g, b);
            let sat = saturation(r, g, b);
            visible += 1;
            luma_sum += luma;
            luma_sq += luma * luma;
            sat_sum += sat;
            sat_sq += sat * sat;
            lumas.push(luma);
            dark += usize::from(luma <= 36.0);
            light += usize::from(luma >= 220.0);
            gray += usize::from(sat <= 0.08);
            high_sat += usize::from(sat >= 0.72);
            warm += usize::from(
                (92.0..=230.0).contains(&luma)
                    && (0.06..=0.56).contains(&sat)
                    && r >= b + 10.0
                    && g >= b - 6.0,
            );
            red_count += usize::from(sat >= 0.34 && r >= g + 24.0 && r >= b + 28.0);
            dark_neutral += usize::from(luma <= 88.0 && sat <= 0.36);
            let key =
                (((p[0] >> 3) as u16) << 10) | (((p[1] >> 3) as u16) << 5) | (p[2] >> 3) as u16;
            *colors.entry(key).or_default() += 1;
            samples[y * sw + x] = Sample {
                visible: true,
                r,
                g,
                b,
                luma,
                saturation: sat,
            };
        }
    }
    if visible == 0 {
        let mut scores = HashMap::new();
        for kind in KIND_ORDER {
            scores.insert(kind, if kind == ImageKind::Unknown { 1.0 } else { 0.0 });
        }
        return Classification {
            kind: ImageKind::Unknown,
            is_photo_style: false,
            confidence: 0.0,
            photo_score: 0.0,
            kind_scores: scores,
            metrics: ImageMetrics {
                transparent_ratio: transparent as f64 / samples.len() as f64,
                ..Default::default()
            },
        };
    }
    let (flat_ratio, soft_change_ratio, strong_edge_ratio) = neighbor_metrics(&samples, sw, sh);
    let (edge_density, horizontal_edge_ratio, vertical_edge_ratio) = edge_metrics(&samples, sw, sh);
    let (photo_tile_ratio, flat_tile_ratio, text_tile_ratio, gradient_tile_ratio) =
        tile_metrics(&samples, sw, sh);
    let mut counts = colors.values().copied().collect::<Vec<_>>();
    counts.sort_unstable_by(|a, b| b.cmp(a));
    let top_color_coverage = counts.iter().take(8).sum::<usize>() as f64 / visible as f64;
    let entropy = counts.iter().fold(0.0, |sum, count| {
        let p = *count as f64 / visible as f64;
        sum - p * p.log2()
    });
    let max_entropy = (colors.len().max(2) as f64).log2();
    let mean = luma_sum / visible as f64;
    let sat_mean = sat_sum / visible as f64;
    let p05 = percentile(&lumas, 0.05);
    let p95 = percentile(&lumas, 0.95);
    let metrics = ImageMetrics {
        sample_count: visible,
        unique_color_ratio: colors.len() as f64 / visible as f64,
        top_color_coverage,
        palette_entropy: if max_entropy == 0.0 {
            0.0
        } else {
            entropy / max_entropy
        },
        flat_ratio,
        soft_change_ratio,
        strong_edge_ratio,
        edge_density,
        horizontal_edge_ratio,
        vertical_edge_ratio,
        luma_std_dev: (luma_sq / visible as f64 - mean * mean).max(0.0).sqrt(),
        luma_p05: p05,
        luma_p95: p95,
        luma_range: p95 - p05,
        saturation_mean: sat_mean,
        saturation_std_dev: (sat_sq / visible as f64 - sat_mean * sat_mean)
            .max(0.0)
            .sqrt(),
        dark_ratio: dark as f64 / visible as f64,
        light_ratio: light as f64 / visible as f64,
        gray_ratio: gray as f64 / visible as f64,
        high_saturation_ratio: high_sat as f64 / visible as f64,
        warm_paper_ratio: warm as f64 / visible as f64,
        red_ratio: red_count as f64 / visible as f64,
        dark_neutral_ratio: dark_neutral as f64 / visible as f64,
        photo_tile_ratio,
        flat_tile_ratio,
        text_tile_ratio,
        gradient_tile_ratio,
        transparent_ratio: transparent as f64 / samples.len() as f64,
    };
    let photo_score = photo_score(&metrics);
    let kind_scores = kind_scores(&metrics, photo_score);
    let kind = KIND_ORDER
        .into_iter()
        .fold(ImageKind::Unknown, |best, candidate| {
            if kind_scores[&candidate] > kind_scores[&best] {
                candidate
            } else {
                best
            }
        });
    Classification {
        kind,
        is_photo_style: photo_score >= 0.5,
        confidence: ((photo_score - 0.5).abs() * 2.0).clamp(0.0, 1.0),
        photo_score,
        kind_scores,
        metrics,
    }
}

fn neighbor_metrics(samples: &[Sample], w: usize, h: usize) -> (f64, f64, f64) {
    let (mut n, mut flat, mut soft, mut strong) = (0usize, 0usize, 0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            let a = samples[y * w + x];
            if !a.visible {
                continue;
            }
            for (nx, ny) in [(x + 1, y), (x, y + 1)] {
                if nx >= w || ny >= h {
                    continue;
                }
                let b = samples[ny * w + nx];
                if !b.visible {
                    continue;
                }
                n += 1;
                let d = diff(a, b);
                if d <= 4.0 {
                    flat += 1
                } else if d <= 28.0 {
                    soft += 1
                } else {
                    strong += 1
                }
            }
        }
    }
    if n == 0 {
        (1.0, 0.0, 0.0)
    } else {
        (
            flat as f64 / n as f64,
            soft as f64 / n as f64,
            strong as f64 / n as f64,
        )
    }
}

fn edge_metrics(samples: &[Sample], w: usize, h: usize) -> (f64, f64, f64) {
    let (mut checked, mut edges, mut horizontal, mut vertical) = (0usize, 0usize, 0usize, 0usize);
    if w < 3 || h < 3 {
        return (0.0, 0.0, 0.0);
    }
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let c = samples[y * w + x];
            let l = samples[y * w + x - 1];
            let r = samples[y * w + x + 1];
            let u = samples[(y - 1) * w + x];
            let d = samples[(y + 1) * w + x];
            if ![c, l, r, u, d].iter().all(|s| s.visible) {
                continue;
            }
            checked += 1;
            let dx = (r.luma - l.luma).abs();
            let dy = (d.luma - u.luma).abs();
            if (dx * dx + dy * dy).sqrt() >= 42.0 {
                edges += 1;
                if dy > dx * 1.2 {
                    horizontal += 1
                } else if dx > dy * 1.2 {
                    vertical += 1
                }
            }
        }
    }
    if checked == 0 || edges == 0 {
        (0.0, 0.0, 0.0)
    } else {
        (
            edges as f64 / checked as f64,
            horizontal as f64 / edges as f64,
            vertical as f64 / edges as f64,
        )
    }
}

#[derive(Default)]
struct Tile {
    unique: f64,
    gray: f64,
    flat: f64,
    soft: f64,
    strong: f64,
    luma_std: f64,
}
fn tile_stats(
    samples: &[Sample],
    w: usize,
    h: usize,
    tx: usize,
    ty: usize,
    size: usize,
) -> Option<Tile> {
    let maxx = (tx + size).min(w);
    let maxy = (ty + size).min(h);
    let mut colors = HashSet::new();
    let (mut visible, mut gray, mut lsum, mut lsq, mut n, mut flat, mut soft, mut strong) =
        (0usize, 0usize, 0.0, 0.0, 0usize, 0usize, 0usize, 0usize);
    for y in ty..maxy {
        for x in tx..maxx {
            let a = samples[y * w + x];
            if !a.visible {
                continue;
            }
            visible += 1;
            lsum += a.luma;
            lsq += a.luma * a.luma;
            gray += usize::from(a.saturation <= 0.08);
            colors.insert(
                ((a.r as u8 >> 3) as u16) << 10
                    | ((a.g as u8 >> 3) as u16) << 5
                    | (a.b as u8 >> 3) as u16,
            );
            for (nx, ny) in [(x + 1, y), (x, y + 1)] {
                if nx >= maxx || ny >= maxy {
                    continue;
                }
                let b = samples[ny * w + nx];
                if !b.visible {
                    continue;
                }
                n += 1;
                let d = diff(a, b);
                if d <= 4.0 {
                    flat += 1
                } else if d <= 28.0 {
                    soft += 1
                } else {
                    strong += 1
                }
            }
        }
    }
    if visible < 12.max(size * size / 4) {
        return None;
    }
    let mean = lsum / visible as f64;
    Some(Tile {
        unique: colors.len() as f64 / visible as f64,
        gray: gray as f64 / visible as f64,
        flat: if n == 0 { 1.0 } else { flat as f64 / n as f64 },
        soft: if n == 0 { 0.0 } else { soft as f64 / n as f64 },
        strong: if n == 0 {
            0.0
        } else {
            strong as f64 / n as f64
        },
        luma_std: (lsq / visible as f64 - mean * mean).max(0.0).sqrt(),
    })
}
fn tile_metrics(samples: &[Sample], w: usize, h: usize) -> (f64, f64, f64, f64) {
    let size = 8.max(w.min(h) / 10);
    let (mut total, mut photo, mut flat, mut text, mut gradient) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    for y in (0..h).step_by(size) {
        for x in (0..w).step_by(size) {
            let Some(t) = tile_stats(samples, w, h, x, y, size) else {
                continue;
            };
            total += 1;
            text += usize::from(t.strong >= 0.16 && t.gray >= 0.55 && t.luma_std >= 38.0);
            flat += usize::from(t.unique <= 0.12 && t.flat >= 0.62);
            photo += usize::from(t.unique >= 0.18 && t.luma_std >= 18.0 && t.flat <= 0.68);
            gradient += usize::from(t.soft >= 0.38 && t.strong <= 0.16 && t.luma_std >= 12.0);
        }
    }
    if total == 0 {
        (0.0, 0.0, 0.0, 0.0)
    } else {
        (
            photo as f64 / total as f64,
            flat as f64 / total as f64,
            text as f64 / total as f64,
            gradient as f64 / total as f64,
        )
    }
}
fn diff(a: Sample, b: Sample) -> f64 {
    ((a.r - b.r).powi(2) + (a.g - b.g).powi(2) + (a.b - b.b).powi(2)).sqrt()
}
fn luma(r: f64, g: f64, b: f64) -> f64 {
    r * 0.2126 + g * 0.7152 + b * 0.0722
}
fn saturation(r: f64, g: f64, b: f64) -> f64 {
    let max = r.max(g).max(b) / 255.0;
    let min = r.min(g).min(b) / 255.0;
    if max == 0.0 { 0.0 } else { (max - min) / max }
}
fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut s = values.to_vec();
    s.sort_by(f64::total_cmp);
    let i = js_round((s.len() - 1) as f64 * p).clamp(0.0, (s.len() - 1) as f64) as usize;
    s[i]
}
fn normalize(v: f64, min: f64, max: f64) -> f64 {
    if max <= min {
        return f64::from(v >= max);
    }
    ((v - min) / (max - min)).clamp(0.0, 1.0)
}
fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}
fn photo_score(m: &ImageMetrics) -> f64 {
    let desat = normalize(m.gray_ratio, 0.45, 0.75)
        .min(normalize(m.photo_tile_ratio, 0.34, 0.58))
        .min(normalize(m.luma_std_dev, 48., 76.))
        .min(normalize(1. - m.flat_ratio, 0.34, 0.58))
        .min(normalize(m.palette_entropy, 0.62, 0.88));
    let hard = if m.flat_ratio > 0.35 {
        normalize(m.strong_edge_ratio, 0.28, 0.58)
    } else {
        0.
    };
    (normalize(m.unique_color_ratio, 0.08, 0.35) * 0.34
        + normalize(m.soft_change_ratio, 0.18, 0.48) * 0.22
        + normalize(1. - m.flat_ratio, 0.2, 0.65) * 0.16
        + normalize(m.luma_std_dev, 24., 72.) * 0.1
        + normalize(m.saturation_std_dev, 0.08, 0.26) * 0.06
        + normalize(m.palette_entropy, 0.55, 0.9) * 0.07
        + normalize(m.photo_tile_ratio, 0.18, 0.62) * 0.13
        + desat * 0.24
        - normalize(m.flat_ratio, 0.55, 0.88) * 0.12
        - normalize(m.top_color_coverage, 0.45, 0.86) * 0.1
        - hard * 0.08)
        .clamp(0., 1.)
}
fn kind_scores(m: &ImageMetrics, p: f64) -> HashMap<ImageKind, f64> {
    let endpoint = m.dark_ratio + m.light_ratio;
    let line_pen = normalize(m.photo_tile_ratio, 0.32, 0.58);
    let low = normalize(92. - m.luma_range, 0., 72.);
    let mut s = HashMap::new();
    s.insert(
        ImageKind::Photo,
        (p * 0.55
            + normalize(m.photo_tile_ratio, 0.12, 0.62) * 0.25
            + normalize(m.palette_entropy, 0.55, 0.9) * 0.12
            + normalize(m.soft_change_ratio, 0.22, 0.48) * 0.08)
            .clamp(0., 1.),
    );
    s.insert(
        ImageKind::LowContrastPhoto,
        (p * 0.38
            + normalize(34. - m.luma_std_dev, 0., 22.) * 0.24
            + low * 0.22
            + normalize(m.gradient_tile_ratio, 0.16, 0.55) * 0.18
            + normalize(m.soft_change_ratio, 0.24, 0.5) * 0.1)
            .clamp(0., 1.),
    );
    s.insert(
        ImageKind::HighContrastPhoto,
        (p * 0.42
            + normalize(m.luma_std_dev, 58., 92.) * 0.3
            + normalize(endpoint, 0.18, 0.42) * 0.18
            + normalize(m.photo_tile_ratio, 0.18, 0.58) * 0.1)
            .clamp(0., 1.),
    );
    s.insert(
        ImageKind::FlatIllustration,
        ((1. - p) * 0.32
            + normalize(m.flat_ratio, 0.52, 0.9) * 0.2
            + normalize(m.top_color_coverage, 0.38, 0.85) * 0.22
            + normalize(m.flat_tile_ratio, 0.18, 0.72) * 0.18
            + normalize(m.high_saturation_ratio, 0.08, 0.38) * 0.08)
            .clamp(0., 1.),
    );
    s.insert(
        ImageKind::LineArt,
        (normalize(m.gray_ratio, 0.48, 0.9) * 0.28
            + normalize(m.edge_density, 0.05, 0.22) * 0.24
            + normalize(m.flat_ratio, 0.5, 0.86) * 0.18
            + normalize(m.top_color_coverage, 0.45, 0.9) * 0.18
            + normalize(0.16 - m.high_saturation_ratio, 0., 0.16) * 0.12
            - line_pen * 0.14)
            .clamp(0., 1.),
    );
    s.insert(
        ImageKind::TextOrUi,
        (normalize(m.text_tile_ratio, 0.05, 0.35) * 0.32
            + normalize(m.edge_density, 0.06, 0.24) * 0.22
            + normalize(m.gray_ratio, 0.42, 0.86) * 0.16
            + normalize(m.top_color_coverage, 0.42, 0.86) * 0.16
            + normalize(m.flat_tile_ratio, 0.12, 0.58) * 0.14)
            .clamp(0., 1.),
    );
    s.insert(
        ImageKind::PixelArt,
        (normalize(m.flat_ratio, 0.62, 0.94) * 0.25
            + normalize(m.top_color_coverage, 0.5, 0.92) * 0.24
            + normalize(m.flat_tile_ratio, 0.25, 0.82) * 0.2
            + normalize(m.high_saturation_ratio, 0.08, 0.45) * 0.16
            + normalize(0.22 - m.soft_change_ratio, 0., 0.22) * 0.15)
            .clamp(0., 1.),
    );
    s.insert(ImageKind::Unknown, 0.);
    s
}

pub fn resolve_auto_options(
    image: &RgbaImage,
    profile: &DitherProfile,
) -> (Classification, ResolvedDitherOptions) {
    let c = classify_image_style(image);
    let preset = recommended_preset(&c, profile.auto_intent);
    let mut o = base_options(c.kind, preset);
    // GeneratePicture uses the legacy natural recommender. These reachable
    // tunings mirror epdoptimize 1.2.0; the selected web matrix is forced last.
    apply_auto_tuning(&mut o, &c, profile.auto_intent);
    // GeneratePicture.buildDitherOptions overwrites the recommendation mode,
    // while app.js overwrites the matrix with the selected web default.
    o.mode = DitheringMode::ErrorDiffusion;
    o.kernel = profile.selected_kernel;
    o.serpentine = profile.force_serpentine;
    (c, o)
}

fn tone(mode: ToneMappingMode, exposure: f64, saturation: f64, contrast: f64) -> ToneMapping {
    ToneMapping {
        mode,
        exposure,
        saturation,
        contrast,
        ..Default::default()
    }
}
fn dr(mode: DynamicRangeMode, strength: f64) -> DynamicRangeCompression {
    DynamicRangeCompression {
        mode,
        strength,
        low_percentile: 0.01,
        high_percentile: 0.99,
        preserve_white: false,
        white_preserve_percentile: 0.99,
        white_preserve_min_luma: 150.,
        white_preserve_max_saturation: 0.18,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Preset {
    Balanced,
    Dynamic,
    Vivid,
    Soft,
    Grayscale,
    Restore,
    PosterScan,
}

fn base_options(kind: ImageKind, fallback: Preset) -> ResolvedDitherOptions {
    let mut o = ResolvedDitherOptions {
        mode: DitheringMode::ErrorDiffusion,
        kernel: DiffusionKernel::FloydSteinberg,
        serpentine: true,
        color_matching: ColorMatching::Rgb,
        tone_mapping: None,
        dynamic_range: None,
        level_compression: None,
        paper_normalization: None,
    };
    match kind {
        ImageKind::TextOrUi => {
            o.mode = DitheringMode::QuantizationOnly;
            o.color_matching = ColorMatching::Lab;
            o.tone_mapping = Some(tone(ToneMappingMode::Contrast, 0.07, 0.0, 0.18));
            o.dynamic_range = Some(dr(DynamicRangeMode::Display, 0.75));
        }
        ImageKind::LineArt => {
            o.mode = DitheringMode::QuantizationOnly;
            o.color_matching = ColorMatching::Lab;
            o.tone_mapping = Some(tone(ToneMappingMode::Contrast, 0., -0.2, 0.25));
            o.dynamic_range = Some(dr(DynamicRangeMode::Display, 0.65));
        }
        ImageKind::PixelArt => {
            o.mode = DitheringMode::QuantizationOnly;
            o.tone_mapping = Some(tone(ToneMappingMode::Off, 0., 0., 0.));
            o.dynamic_range = Some(dr(DynamicRangeMode::Off, 1.));
        }
        ImageKind::FlatIllustration => {
            let mut t = tone(ToneMappingMode::SCurve, 0., 0.45, 0.);
            t.strength = 0.72;
            t.shadow_boost = 0.08;
            t.highlight_compress = -1.3;
            o.tone_mapping = Some(t);
        }
        ImageKind::LowContrastPhoto => {
            o.color_matching = ColorMatching::Lab;
            let mut t = tone(ToneMappingMode::SCurve, 0.138, -1.0, 0.0);
            t.strength = 1.;
            t.shadow_boost = 0.25;
            t.highlight_compress = -0.75;
            t.midpoint = 0.46;
            o.tone_mapping = Some(t);
            let mut d = dr(DynamicRangeMode::Auto, 0.9);
            d.low_percentile = 0.02;
            d.high_percentile = 0.98;
            o.dynamic_range = Some(d);
            o.level_compression = Some(LevelCompression {
                mode: LevelCompressionMode::Luma,
                black: [8.; 3],
                white: [245.; 3],
                auto: false,
                auto_threshold: 0.1,
                percentile_clip: None,
            });
        }
        ImageKind::HighContrastPhoto => {
            o.kernel = DiffusionKernel::Stucki;
            o.dynamic_range = Some(dr(DynamicRangeMode::Display, 0.9));
        }
        ImageKind::Photo if fallback == Preset::Soft => {
            o.kernel = DiffusionKernel::Stucki;
            // enforceMinimumAutoContrast materializes the soft preset with
            // neutral contrast before GeneratePicture removes processingPreset.
            o.tone_mapping = Some(tone(ToneMappingMode::Contrast, 0.0, 0.1, 0.0));
            o.dynamic_range = Some(dr(DynamicRangeMode::Display, 1.0));
        }
        _ => {}
    }
    o
}

fn recommended_preset(c: &Classification, intent: AutoIntent) -> Preset {
    let m = &c.metrics;
    let score = |kind| *c.kind_scores.get(&kind).unwrap_or(&0.0);
    let mut scores = [
        (Preset::Balanced, 0.52),
        (Preset::Dynamic, 0.48),
        (Preset::Vivid, 0.45),
        (Preset::Soft, 0.44),
        (Preset::Grayscale, 0.28),
        (Preset::Restore, 0.34),
        (Preset::PosterScan, 0.32),
    ];
    let add = |preset: Preset, value: f64, scores: &mut [(Preset, f64)]| {
        scores.iter_mut().find(|item| item.0 == preset).unwrap().1 += value;
    };
    if c.is_photo_style {
        add(Preset::Dynamic, 0.18, &mut scores);
        add(Preset::Balanced, 0.12, &mut scores);
        add(
            Preset::Soft,
            if m.luma_std_dev >= 68.0 { 0.2 } else { 0.06 },
            &mut scores,
        );
    } else {
        add(Preset::Vivid, 0.28, &mut scores);
        add(Preset::Balanced, 0.08, &mut scores);
    }
    add(
        Preset::Dynamic,
        score(ImageKind::LowContrastPhoto) * 0.24,
        &mut scores,
    );
    add(
        Preset::Restore,
        score(ImageKind::LowContrastPhoto) * 0.34,
        &mut scores,
    );
    add(
        Preset::Soft,
        score(ImageKind::HighContrastPhoto) * 0.26,
        &mut scores,
    );
    add(
        Preset::Vivid,
        score(ImageKind::FlatIllustration) * 0.24 + score(ImageKind::PixelArt) * 0.18,
        &mut scores,
    );
    add(
        Preset::Balanced,
        (score(ImageKind::TextOrUi) + score(ImageKind::LineArt)) * 0.18,
        &mut scores,
    );
    add(
        Preset::Grayscale,
        (score(ImageKind::TextOrUi) + score(ImageKind::LineArt))
            * if m.gray_ratio >= 0.7 { 0.24 } else { 0.08 },
        &mut scores,
    );
    if m.saturation_mean <= 0.1 && m.gray_ratio >= 0.82 {
        add(Preset::Grayscale, 0.22, &mut scores);
    }
    if m.luma_range <= 96.0 && m.luma_std_dev <= 38.0 {
        add(Preset::Restore, 0.2, &mut scores);
    }
    if is_warm_poster(c) {
        add(Preset::PosterScan, 0.42, &mut scores);
    }
    match intent {
        AutoIntent::Vivid => add(Preset::Vivid, 0.18, &mut scores),
        AutoIntent::Faithful => add(Preset::Balanced, 0.16, &mut scores),
        AutoIntent::LowNoise => add(Preset::Soft, 0.16, &mut scores),
        AutoIntent::Readable => {
            add(Preset::Balanced, 0.14, &mut scores);
            add(Preset::Grayscale, 0.1, &mut scores);
        }
        AutoIntent::Natural => {}
    }
    scores
        .into_iter()
        .reduce(|best, current| if current.1 > best.1 { current } else { best })
        .unwrap()
        .0
}

fn is_warm_poster(c: &Classification) -> bool {
    let m = &c.metrics;
    m.warm_paper_ratio >= 0.18
        && (m.dark_neutral_ratio >= 0.025 || m.red_ratio >= 0.008 || m.strong_edge_ratio >= 0.05)
        && (matches!(
            c.kind,
            ImageKind::FlatIllustration | ImageKind::TextOrUi | ImageKind::LineArt
        ) || m.flat_ratio >= 0.5
            || m.top_color_coverage >= 0.36)
}

fn is_restorable(c: &Classification) -> bool {
    let m = &c.metrics;
    c.kind == ImageKind::LowContrastPhoto
        || (m.luma_range <= 96.0
            && m.luma_std_dev <= 32.0
            && (m.gray_ratio >= 0.5 || m.saturation_mean <= 0.18)
            && c.kind != ImageKind::PixelArt
            && (m.edge_density >= 0.015
                || m.soft_change_ratio >= 0.12
                || m.gradient_tile_ratio >= 0.04
                || m.text_tile_ratio >= 0.04
                || (m.luma_range <= 70.0
                    && m.gray_ratio >= 0.7
                    && m.palette_entropy >= 0.35
                    && m.top_color_coverage <= 0.92)))
}

fn apply_auto_tuning(o: &mut ResolvedDitherOptions, c: &Classification, intent: AutoIntent) {
    let m = &c.metrics;
    if !is_restorable(c)
        && c.kind == ImageKind::FlatIllustration
        && m.gray_ratio >= 0.82
        && m.top_color_coverage >= 0.9
        && (m.text_tile_ratio >= 0.1 || m.edge_density >= 0.16)
    {
        o.color_matching = ColorMatching::Lab;
        o.mode = DitheringMode::QuantizationOnly;
        o.tone_mapping = Some(tone(ToneMappingMode::Contrast, 0.043, -0.1, 0.2));
        o.dynamic_range = Some(dr(DynamicRangeMode::Display, 0.75));
    }
    if is_restorable(c) {
        let mut t = tone(
            ToneMappingMode::SCurve,
            if m.luma_p95 <= 190.0 { 0.138 } else { 0.084 },
            if m.gray_ratio >= 0.72 { -1.0 } else { -0.1 },
            0.0,
        );
        t.strength = if m.luma_range <= 70.0 { 1.0 } else { 0.92 };
        t.shadow_boost = if m.luma_p05 >= 55. { 0.2 } else { 0.28 };
        t.highlight_compress = -0.75;
        t.midpoint = if m.luma_p95 <= 190. { 0.44 } else { 0.46 };
        o.tone_mapping = Some(t);
        let mut d = dr(
            DynamicRangeMode::Auto,
            if m.luma_range <= 70.0 { 0.96 } else { 0.9 },
        );
        d.low_percentile = 0.02;
        d.high_percentile = 0.98;
        o.dynamic_range = Some(d);
        o.level_compression = Some(LevelCompression {
            mode: LevelCompressionMode::Luma,
            black: [8.; 3],
            white: [245.; 3],
            auto: false,
            auto_threshold: 0.1,
            percentile_clip: None,
        });
        o.color_matching = if m.gray_ratio >= 0.55 {
            ColorMatching::Lab
        } else {
            ColorMatching::Rgb
        };
    }
    if is_warm_poster(c) {
        let mut t = tone(ToneMappingMode::SCurve, 0.057, 0.05, 0.0);
        t.strength = 0.92;
        t.shadow_boost = 0.08;
        t.highlight_compress = -0.55;
        t.midpoint = 0.44;
        o.tone_mapping = Some(t);
        let mut range = dr(DynamicRangeMode::Auto, 1.0);
        range.low_percentile = 0.015;
        range.high_percentile = 0.985;
        o.dynamic_range = Some(range);
        o.level_compression = Some(LevelCompression {
            mode: LevelCompressionMode::Luma,
            black: [3.0; 3],
            white: [252.0; 3],
            auto: false,
            auto_threshold: 0.1,
            percentile_clip: None,
        });
        o.paper_normalization = Some(PaperNormalization {
            mode: PaperNormalizationMode::WarmPaper,
            strength: 0.95,
            min_luma: 82.0,
            saturation_threshold: 0.56,
            warm_bias_threshold: 8.0,
            black_anchor: 0.95,
            preserve_red: 0.85,
            paper_white: [248.0, 248.0, 246.0],
        });
    }
    if let Some(d) = &mut o.dynamic_range
        && d.mode != DynamicRangeMode::Off
    {
        d.preserve_white = true;
        d.white_preserve_percentile = 0.99;
        d.white_preserve_min_luma = 150.;
    }
    let _ = intent;
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use image::Rgba;
    use serde_json::Value;

    use crate::{epd_pipeline::process_canvas, epd_processing::apply_adjustments};
    #[test]
    fn solid_flat_image_is_classified_deterministically() {
        let image = RgbaImage::from_pixel(120, 80, Rgba([230, 40, 35, 255]));
        let a = classify_image_style(&image);
        let b = classify_image_style(&image);
        assert_eq!(a, b);
        assert!(matches!(
            a.kind,
            ImageKind::FlatIllustration | ImageKind::PixelArt
        ));
    }
    #[test]
    fn matches_generate_picture_auto_oracle_cases() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/generate-picture-auto-v2.json"
        ))
        .unwrap();
        let profile = DitherProfile::generate_picture_auto_v2();
        let palette = profile
            .palette
            .iter()
            .map(|entry| entry.color)
            .collect::<Vec<_>>();
        for case in fixture["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let width = case["width"].as_u64().unwrap() as u32;
            let height = case["height"].as_u64().unwrap() as u32;
            let input = base64::engine::general_purpose::STANDARD
                .decode(case["input_rgba_b64"].as_str().unwrap())
                .unwrap();
            let image = RgbaImage::from_raw(width, height, input).unwrap();
            let (classification, options) = resolve_auto_options(&image, &profile);
            assert_options_match(&options, &case["resolved_options"], name);
            assert_eq!(
                kind_name(classification.kind),
                case["classification"]["kind"].as_str().unwrap(),
                "classification mismatch for {name}"
            );
            let expected_metrics = &case["classification"]["metrics"];
            for (key, actual) in [
                ("photoTileRatio", classification.metrics.photo_tile_ratio),
                ("flatTileRatio", classification.metrics.flat_tile_ratio),
                ("textTileRatio", classification.metrics.text_tile_ratio),
                ("lumaStdDev", classification.metrics.luma_std_dev),
                ("warmPaperRatio", classification.metrics.warm_paper_ratio),
            ] {
                let expected = expected_metrics[key].as_f64().unwrap();
                assert!(
                    (actual - expected).abs() < 1e-12,
                    "metric {key} mismatch for {name}: {actual} != {expected}"
                );
            }

            let mut adjusted = image.clone();
            apply_adjustments(&mut adjusted, &options, &palette);
            let expected_adjusted = base64::engine::general_purpose::STANDARD
                .decode(case["adjusted_rgba_b64"].as_str().unwrap())
                .unwrap();
            assert_eq!(
                adjusted.as_raw(),
                &expected_adjusted,
                "adjustment mismatch for {name}"
            );

            let indexed = process_canvas(&image, &profile, &options).unwrap();
            let mut packed = vec![0_u8; indexed.indices.len().div_ceil(2)];
            for (index, value) in indexed.indices.iter().enumerate() {
                if index % 2 == 0 {
                    packed[index / 2] = value << 4;
                } else {
                    packed[index / 2] |= value;
                }
            }
            let expected_packed = base64::engine::general_purpose::STANDARD
                .decode(case["packed_b64"].as_str().unwrap())
                .unwrap();
            assert_eq!(packed, expected_packed, "dither mismatch for {name}");
        }
    }

    fn assert_options_match(options: &ResolvedDitherOptions, expected: &Value, name: &str) {
        assert_eq!(expected["ditheringType"], "errorDiffusion", "{name}");
        assert_eq!(expected["errorDiffusionMatrix"], "floydSteinberg", "{name}");
        assert_eq!(expected["serpentine"], true, "{name}");
        assert_eq!(
            options.color_matching,
            if expected["colorMatching"] == "lab" {
                ColorMatching::Lab
            } else {
                ColorMatching::Rgb
            },
            "{name}"
        );
        match expected.get("toneMapping") {
            Some(value) => {
                let actual = options
                    .tone_mapping
                    .as_ref()
                    .unwrap_or_else(|| panic!("missing tone for {name}"));
                let mode = match value["mode"].as_str().unwrap() {
                    "off" => ToneMappingMode::Off,
                    "contrast" => ToneMappingMode::Contrast,
                    "scurve" => ToneMappingMode::SCurve,
                    other => panic!("unexpected tone mode {other}"),
                };
                assert_eq!(actual.mode, mode, "{name}");
                for (key, actual_value) in [
                    ("exposure", actual.exposure),
                    ("contrast", actual.contrast),
                    ("saturation", actual.saturation),
                    ("strength", actual.strength),
                    ("shadowBoost", actual.shadow_boost),
                    ("highlightCompress", actual.highlight_compress),
                    ("midpoint", actual.midpoint),
                ] {
                    if let Some(expected_value) = value.get(key).and_then(Value::as_f64) {
                        assert!(
                            (actual_value - expected_value).abs() < 1e-12,
                            "tone {key} for {name}"
                        );
                    }
                }
            }
            None => assert!(options.tone_mapping.is_none(), "unexpected tone for {name}"),
        }
        match expected.get("dynamicRangeCompression") {
            Some(value) => {
                let actual = options
                    .dynamic_range
                    .as_ref()
                    .unwrap_or_else(|| panic!("missing range for {name}"));
                assert_eq!(
                    actual.mode == DynamicRangeMode::Off,
                    value["mode"] == "off",
                    "{name}"
                );
                if let Some(strength) = value.get("strength").and_then(Value::as_f64) {
                    assert!(
                        (actual.strength - strength).abs() < 1e-12,
                        "range strength for {name}"
                    );
                }
            }
            None => assert!(
                options.dynamic_range.is_none(),
                "unexpected range for {name}"
            ),
        }
        assert_eq!(
            options.level_compression.is_some(),
            expected.get("levelCompression").is_some(),
            "level options for {name}"
        );
        assert_eq!(
            options.paper_normalization.is_some(),
            expected.get("paperNormalization").is_some(),
            "paper options for {name}"
        );
    }

    fn kind_name(kind: ImageKind) -> &'static str {
        match kind {
            ImageKind::Photo => "photo",
            ImageKind::LowContrastPhoto => "lowContrastPhoto",
            ImageKind::HighContrastPhoto => "highContrastPhoto",
            ImageKind::FlatIllustration => "flatIllustration",
            ImageKind::LineArt => "lineArt",
            ImageKind::TextOrUi => "textOrUi",
            ImageKind::PixelArt => "pixelArt",
            ImageKind::Unknown => "unknown",
        }
    }

    #[test]
    fn auto_profile_forces_web_floyd_and_serpentine() {
        let image = RgbaImage::from_fn(160, 120, |x, y| {
            Rgba([
                (x % 256) as u8,
                (y * 2 % 256) as u8,
                ((x + y) % 256) as u8,
                255,
            ])
        });
        let (_, o) = resolve_auto_options(&image, &DitherProfile::generate_picture_auto_v2());
        if o.mode == DitheringMode::ErrorDiffusion {
            assert_eq!(o.kernel, DiffusionKernel::FloydSteinberg);
            assert!(o.serpentine);
        }
    }
}
