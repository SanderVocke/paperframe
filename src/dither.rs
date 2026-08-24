use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaletteEntry {
    pub name: &'static str,
    /// Measured/calibrated color used while selecting and diffusing colors.
    pub calibrated: Rgb,
    /// Fully saturated native color written for the display controller.
    pub device: Rgb,
    pub device_index: u8,
}

/// epdoptimize v1.2.0 `spectra6` palette, retaining its calibrated/native split.
pub const SPECTRA6_CALIBRATED: [PaletteEntry; 6] = [
    PaletteEntry {
        name: "black",
        calibrated: Rgb::new(0x1f, 0x22, 0x26),
        device: Rgb::new(0, 0, 0),
        device_index: 0,
    },
    PaletteEntry {
        name: "white",
        calibrated: Rgb::new(0xb9, 0xc7, 0xc9),
        device: Rgb::new(255, 255, 255),
        device_index: 6,
    },
    PaletteEntry {
        name: "blue",
        calibrated: Rgb::new(0x23, 0x3f, 0x8e),
        device: Rgb::new(0, 0, 255),
        device_index: 1,
    },
    PaletteEntry {
        name: "green",
        calibrated: Rgb::new(0x35, 0x56, 0x3a),
        device: Rgb::new(0, 255, 0),
        device_index: 2,
    },
    PaletteEntry {
        name: "red",
        calibrated: Rgb::new(0x62, 0x20, 0x1e),
        device: Rgb::new(255, 0, 0),
        device_index: 3,
    },
    PaletteEntry {
        name: "yellow",
        calibrated: Rgb::new(0xc1, 0xbb, 0x1e),
        device: Rgb::new(255, 255, 0),
        device_index: 5,
    },
];

pub fn calibrated_colors(palette: &[PaletteEntry]) -> Vec<Rgb> {
    palette.iter().map(|entry| entry.calibrated).collect()
}

pub fn device_entry(palette: &[PaletteEntry], palette_index: u8) -> Option<(u8, Rgb)> {
    palette
        .get(palette_index as usize)
        .map(|entry| (entry.device_index, entry.device))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiffusionStep {
    pub dx: i8,
    pub dy: u8,
    pub factor: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffusionKernel {
    FloydSteinberg,
    FalseFloydSteinberg,
    Atkinson,
    Jarvis,
    JarvisJudiceNinke,
    Stucki,
    Burkes,
    Sierra3,
    Sierra2,
    Sierra24A,
    Fan,
    ShiauFan,
    ShiauFan2,
}

impl DiffusionKernel {
    pub const ALL: [Self; 13] = [
        Self::FloydSteinberg,
        Self::FalseFloydSteinberg,
        Self::Atkinson,
        Self::Jarvis,
        Self::JarvisJudiceNinke,
        Self::Stucki,
        Self::Burkes,
        Self::Sierra3,
        Self::Sierra2,
        Self::Sierra24A,
        Self::Fan,
        Self::ShiauFan,
        Self::ShiauFan2,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::FloydSteinberg => "floydSteinberg",
            Self::FalseFloydSteinberg => "falseFloydSteinberg",
            Self::Atkinson => "atkinson",
            Self::Jarvis => "jarvis",
            Self::JarvisJudiceNinke => "jarvisJudiceNinke",
            Self::Stucki => "stucki",
            Self::Burkes => "burkes",
            Self::Sierra3 => "sierra3",
            Self::Sierra2 => "sierra2",
            Self::Sierra24A => "sierra2-4a",
            Self::Fan => "fan",
            Self::ShiauFan => "shiauFan",
            Self::ShiauFan2 => "shiauFan2",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kernel| kernel.name() == name)
    }

    pub fn steps(self) -> &'static [DiffusionStep] {
        match self {
            Self::FloydSteinberg => &FLOYD_STEINBERG,
            Self::FalseFloydSteinberg => &FALSE_FLOYD_STEINBERG,
            Self::Atkinson => &ATKINSON,
            Self::Jarvis => &JARVIS,
            Self::JarvisJudiceNinke => &JARVIS_JUDICE_NINKE,
            Self::Stucki => &STUCKI,
            Self::Burkes => &BURKES,
            Self::Sierra3 => &SIERRA3,
            Self::Sierra2 => &SIERRA2,
            Self::Sierra24A => &SIERRA24A,
            Self::Fan => &FAN,
            Self::ShiauFan => &SHIAU_FAN,
            Self::ShiauFan2 => &SHIAU_FAN2,
        }
    }
}

macro_rules! step {
    ($dx:expr, $dy:expr, $weight:expr, $divisor:expr) => {
        DiffusionStep {
            dx: $dx,
            dy: $dy,
            factor: $weight as f64 / $divisor as f64,
        }
    };
}

const FLOYD_STEINBERG: [DiffusionStep; 4] = [
    step!(1, 0, 7, 16),
    step!(-1, 1, 3, 16),
    step!(0, 1, 5, 16),
    step!(1, 1, 1, 16),
];
const FALSE_FLOYD_STEINBERG: [DiffusionStep; 3] =
    [step!(1, 0, 3, 8), step!(0, 1, 3, 8), step!(1, 1, 2, 8)];
const ATKINSON: [DiffusionStep; 6] = [
    step!(1, 0, 1, 8),
    step!(2, 0, 1, 8),
    step!(-1, 1, 1, 8),
    step!(0, 1, 1, 8),
    step!(1, 1, 1, 8),
    step!(0, 2, 1, 8),
];
const JARVIS: [DiffusionStep; 12] = [
    step!(1, 0, 7, 48),
    step!(2, 0, 5, 48),
    step!(-2, 1, 3, 48),
    step!(-1, 1, 5, 48),
    step!(0, 1, 7, 48),
    step!(1, 1, 5, 48),
    step!(2, 1, 3, 48),
    step!(-2, 2, 1, 48),
    step!(-1, 2, 3, 48),
    step!(0, 2, 4, 48),
    step!(1, 2, 3, 48),
    step!(2, 2, 1, 48),
];
const JARVIS_JUDICE_NINKE: [DiffusionStep; 12] = [
    step!(1, 0, 7, 48),
    step!(2, 0, 5, 48),
    step!(-2, 1, 3, 48),
    step!(-1, 1, 5, 48),
    step!(0, 1, 7, 48),
    step!(1, 1, 5, 48),
    step!(2, 1, 3, 48),
    step!(-2, 2, 1, 48),
    step!(-1, 2, 3, 48),
    step!(0, 2, 5, 48),
    step!(1, 2, 3, 48),
    step!(2, 2, 1, 48),
];
const STUCKI: [DiffusionStep; 12] = [
    step!(1, 0, 8, 42),
    step!(2, 0, 4, 42),
    step!(-2, 1, 2, 42),
    step!(-1, 1, 4, 42),
    step!(0, 1, 8, 42),
    step!(1, 1, 4, 42),
    step!(2, 1, 2, 42),
    step!(-2, 2, 1, 42),
    step!(-1, 2, 2, 42),
    step!(0, 2, 4, 42),
    step!(1, 2, 2, 42),
    step!(2, 2, 1, 42),
];
const BURKES: [DiffusionStep; 7] = [
    step!(1, 0, 8, 32),
    step!(2, 0, 4, 32),
    step!(-2, 1, 2, 32),
    step!(-1, 1, 4, 32),
    step!(0, 1, 8, 32),
    step!(1, 1, 4, 32),
    step!(2, 1, 2, 32),
];
const SIERRA3: [DiffusionStep; 10] = [
    step!(1, 0, 5, 32),
    step!(2, 0, 3, 32),
    step!(-2, 1, 2, 32),
    step!(-1, 1, 4, 32),
    step!(0, 1, 5, 32),
    step!(1, 1, 4, 32),
    step!(2, 1, 2, 32),
    step!(-1, 2, 2, 32),
    step!(0, 2, 3, 32),
    step!(1, 2, 2, 32),
];
const SIERRA2: [DiffusionStep; 7] = [
    step!(1, 0, 4, 16),
    step!(2, 0, 3, 16),
    step!(-2, 1, 1, 16),
    step!(-1, 1, 2, 16),
    step!(0, 1, 3, 16),
    step!(1, 1, 2, 16),
    step!(2, 1, 1, 16),
];
const SIERRA24A: [DiffusionStep; 3] = [step!(1, 0, 2, 4), step!(-1, 1, 1, 4), step!(0, 1, 1, 4)];
const FAN: [DiffusionStep; 4] = [
    step!(1, 0, 7, 16),
    step!(-2, 1, 1, 16),
    step!(-1, 1, 3, 16),
    step!(0, 1, 5, 16),
];
const SHIAU_FAN: [DiffusionStep; 4] = [
    step!(1, 0, 4, 8),
    step!(-2, 1, 1, 8),
    step!(-1, 1, 1, 8),
    step!(0, 1, 2, 8),
];
const SHIAU_FAN2: [DiffusionStep; 5] = [
    step!(1, 0, 7, 14),
    step!(-3, 1, 1, 14),
    step!(-2, 1, 1, 14),
    step!(-1, 1, 2, 14),
    step!(0, 1, 3, 14),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedImage {
    pub width: u32,
    pub height: u32,
    pub indices: Vec<u8>,
}

pub fn quantize_rgb(rgba: &[u8], width: u32, height: u32, palette: &[Rgb]) -> Result<IndexedImage> {
    validate(rgba, width, height, palette)?;
    let indices = rgba
        .chunks_exact(4)
        .map(|pixel| closest(pixel[0], pixel[1], pixel[2], palette) as u8)
        .collect();
    Ok(IndexedImage {
        width,
        height,
        indices,
    })
}

pub fn error_diffusion_rgb(
    rgba: &[u8],
    width: u32,
    height: u32,
    palette: &[Rgb],
    kernel: DiffusionKernel,
    serpentine: bool,
) -> Result<IndexedImage> {
    validate(rgba, width, height, palette)?;
    let width_usize = width as usize;
    let height_usize = height as usize;
    let mut data = rgba.to_vec();
    let mut indices = vec![0; width_usize * height_usize];

    for y in 0..height_usize {
        let reverse = serpentine && y % 2 == 1;
        for offset in 0..width_usize {
            let x = if reverse {
                width_usize - 1 - offset
            } else {
                offset
            };
            let pixel = (y * width_usize + x) * 4;
            let old = [data[pixel], data[pixel + 1], data[pixel + 2]];
            let palette_index = closest(old[0], old[1], old[2], palette);
            indices[y * width_usize + x] = palette_index as u8;
            let selected = palette[palette_index];
            data[pixel] = selected.r;
            data[pixel + 1] = selected.g;
            data[pixel + 2] = selected.b;
            data[pixel + 3] = 255;
            let error = [
                old[0] as i16 - selected.r as i16,
                old[1] as i16 - selected.g as i16,
                old[2] as i16 - selected.b as i16,
            ];

            for step in kernel.steps() {
                let signed_dx = if reverse { -step.dx } else { step.dx } as isize;
                let next_x = x as isize + signed_dx;
                let next_y = y + step.dy as usize;
                if next_x < 0 || next_x >= width_usize as isize || next_y >= height_usize {
                    continue;
                }
                let next = (next_y * width_usize + next_x as usize) * 4;
                for channel in 0..3 {
                    data[next + channel] = to_uint8_clamp(
                        data[next + channel] as f64 + error[channel] as f64 * step.factor,
                    );
                }
            }
        }
    }

    Ok(IndexedImage {
        width,
        height,
        indices,
    })
}

fn validate(rgba: &[u8], width: u32, height: u32, palette: &[Rgb]) -> Result<()> {
    if palette.is_empty() || palette.len() > u8::MAX as usize + 1 {
        bail!("palette must contain between 1 and 256 colors");
    }
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow::anyhow!("image dimensions overflow"))?;
    let bytes = pixels
        .checked_mul(4)
        .ok_or_else(|| anyhow::anyhow!("image byte length overflows"))?;
    if rgba.len() != bytes {
        bail!("RGBA byte length does not match image dimensions");
    }
    Ok(())
}

fn closest(r: u8, g: u8, b: u8, palette: &[Rgb]) -> usize {
    let mut closest = 0;
    let mut distance = u32::MAX;
    for (index, color) in palette.iter().enumerate() {
        let dr = color.r as i32 - r as i32;
        let dg = color.g as i32 - g as i32;
        let db = color.b as i32 - b as i32;
        let candidate = (dr * dr + dg * dg + db * db) as u32;
        if candidate < distance {
            closest = index;
            distance = candidate;
        }
    }
    closest
}

// epdoptimize v1.2.0 clampByte: clamp, then JavaScript Math.round.
fn to_uint8_clamp(value: f64) -> u8 {
    if value.is_nan() || value <= 0.0 {
        return 0;
    }
    if value >= 255.0 {
        return 255;
    }
    (value + 0.5).floor() as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    const BW: [Rgb; 2] = [Rgb::new(0, 0, 0), Rgb::new(255, 255, 255)];

    #[test]
    fn all_upstream_kernel_names_are_unique_and_resolve() {
        let mut names = DiffusionKernel::ALL.map(DiffusionKernel::name).to_vec();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), DiffusionKernel::ALL.len());
        for kernel in DiffusionKernel::ALL {
            assert_eq!(DiffusionKernel::from_name(kernel.name()), Some(kernel));
            assert!(!kernel.steps().is_empty());
            assert!(kernel.steps().iter().all(|step| step.dy <= 2));
        }
    }

    #[test]
    fn jarvis_variants_preserve_the_upstream_center_weight_difference() {
        assert_eq!(DiffusionKernel::Jarvis.steps()[9].factor, 4.0 / 48.0);
        assert_eq!(
            DiffusionKernel::JarvisJudiceNinke.steps()[9].factor,
            5.0 / 48.0
        );
    }

    #[test]
    fn spectra6_palette_keeps_calibrated_and_native_colors_aligned() {
        let calibrated = calibrated_colors(&SPECTRA6_CALIBRATED);
        let rgba = calibrated
            .iter()
            .flat_map(|color| [color.r, color.g, color.b, 255])
            .collect::<Vec<_>>();
        assert_eq!(
            quantize_rgb(&rgba, calibrated.len() as u32, 1, &calibrated)
                .unwrap()
                .indices,
            [0, 1, 2, 3, 4, 5]
        );
        assert_eq!(
            device_entry(&SPECTRA6_CALIBRATED, 0),
            Some((0, Rgb::new(0, 0, 0)))
        );
        assert_eq!(
            device_entry(&SPECTRA6_CALIBRATED, 1),
            Some((6, Rgb::new(255, 255, 255)))
        );
        assert_eq!(
            device_entry(&SPECTRA6_CALIBRATED, 5),
            Some((5, Rgb::new(255, 255, 0)))
        );
        assert_eq!(
            SPECTRA6_CALIBRATED[2].calibrated,
            Rgb::new(0x23, 0x3f, 0x8e)
        );
    }

    #[test]
    fn palette_ties_choose_the_first_entry() {
        let input = [127, 127, 127, 255];
        assert_eq!(quantize_rgb(&input, 1, 1, &BW).unwrap().indices, [0]);
    }

    #[test]
    fn pure_palette_colors_are_stable_for_every_kernel_and_scan() {
        let input = [0, 0, 0, 255, 255, 255, 255, 12];
        for kernel in DiffusionKernel::ALL {
            for serpentine in [false, true] {
                assert_eq!(
                    error_diffusion_rgb(&input, 2, 1, &BW, kernel, serpentine)
                        .unwrap()
                        .indices,
                    [0, 1]
                );
            }
        }
    }

    #[test]
    fn serpentine_is_deterministic() {
        let input = [
            15, 91, 127, 184, 220, 201, 53, 142, 75, 116, 38, 166, 244, 103, 62, 149, 211, 85, 174,
            29,
        ]
        .into_iter()
        .flat_map(|value| [value, value, value, 255])
        .collect::<Vec<_>>();
        let serpentine =
            error_diffusion_rgb(&input, 5, 4, &BW, DiffusionKernel::FloydSteinberg, true).unwrap();
        assert_eq!(
            serpentine,
            error_diffusion_rgb(&input, 5, 4, &BW, DiffusionKernel::FloydSteinberg, true).unwrap()
        );
    }

    #[test]
    fn rejects_bad_dimensions_and_palettes() {
        assert!(quantize_rgb(&[], 1, 1, &BW).is_err());
        assert!(quantize_rgb(&[], 0, 0, &[]).is_err());
    }

    #[test]
    fn byte_clamping_matches_upstream_math_round() {
        assert_eq!(to_uint8_clamp(-1.0), 0);
        assert_eq!(to_uint8_clamp(10.49), 10);
        assert_eq!(to_uint8_clamp(10.5), 11);
        assert_eq!(to_uint8_clamp(11.5), 12);
        assert_eq!(to_uint8_clamp(300.0), 255);
    }

    #[derive(Deserialize)]
    struct Fixture {
        upstream: Upstream,
        comparison: String,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        palette: Vec<[u8; 3]>,
        cases: Vec<FixtureCase>,
    }
    #[derive(Deserialize)]
    struct Upstream {
        version: String,
        commit: String,
    }
    #[derive(Deserialize)]
    struct FixtureCase {
        kernel: String,
        serpentine: bool,
        indices: Vec<u8>,
    }

    fn verify_upstream_fixture(contents: &str) {
        let fixture: Fixture = serde_json::from_str(contents).unwrap();
        assert_eq!(fixture.upstream.version, "1.2.0");
        assert_eq!(
            fixture.upstream.commit,
            "555c7a3c42d9233a17b26ec399f3d57340dde8f5"
        );
        assert!(fixture.comparison.starts_with("exact palette indices"));
        let palette = fixture
            .palette
            .into_iter()
            .map(|[r, g, b]| Rgb::new(r, g, b))
            .collect::<Vec<_>>();
        for case in fixture.cases {
            let kernel = DiffusionKernel::from_name(&case.kernel).unwrap();
            let actual = error_diffusion_rgb(
                &fixture.rgba,
                fixture.width,
                fixture.height,
                &palette,
                kernel,
                case.serpentine,
            )
            .unwrap();
            assert_eq!(
                actual.indices, case.indices,
                "upstream mismatch for {} serpentine={}",
                case.kernel, case.serpentine
            );
        }
    }

    #[test]
    fn matches_pinned_epdoptimize_error_diffusion_fixtures_exactly() {
        verify_upstream_fixture(include_str!(
            "../tests/fixtures/epdoptimize-v1.2.0-error-diffusion.json"
        ));
        verify_upstream_fixture(include_str!(
            "../tests/fixtures/epdoptimize-v1.2.0-photo-like.json"
        ));
    }
}
