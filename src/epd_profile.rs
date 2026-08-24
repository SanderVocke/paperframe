use anyhow::{Result, bail};

use crate::dither::{DiffusionKernel, Rgb};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteColor {
    pub name: String,
    pub color: Rgb,
    pub device: Rgb,
    pub device_index: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoIntent {
    Natural,
    Vivid,
    Readable,
    Faithful,
    LowNoise,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DitheringMode {
    ErrorDiffusion,
    QuantizationOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMatching {
    Rgb,
    Lab,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementMode {
    AspectFitWhite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizePolicy {
    BrowserLikeTriangle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ManualAdjustments {
    /// Exposure in stops, matching epdoptimize's brightness slider mapping.
    pub exposure: f64,
    pub contrast: f64,
    pub saturation: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToneMappingMode {
    Off,
    Contrast,
    SCurve,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToneMapping {
    pub mode: ToneMappingMode,
    pub exposure: f64,
    pub contrast: f64,
    pub saturation: f64,
    pub strength: f64,
    pub shadow_boost: f64,
    pub highlight_compress: f64,
    pub midpoint: f64,
}

impl Default for ToneMapping {
    fn default() -> Self {
        Self {
            mode: ToneMappingMode::Off,
            exposure: 0.0,
            contrast: 0.0,
            saturation: 0.0,
            strength: 0.8,
            shadow_boost: 0.0,
            highlight_compress: -1.5,
            midpoint: 0.5,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DynamicRangeMode {
    Off,
    Display,
    Auto,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DynamicRangeCompression {
    pub mode: DynamicRangeMode,
    pub strength: f64,
    pub low_percentile: f64,
    pub high_percentile: f64,
    pub preserve_white: bool,
    pub white_preserve_percentile: f64,
    pub white_preserve_min_luma: f64,
    pub white_preserve_max_saturation: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelCompressionMode {
    Off,
    Luma,
    PerChannel,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LevelCompression {
    pub mode: LevelCompressionMode,
    pub black: [f64; 3],
    pub white: [f64; 3],
    pub auto: bool,
    pub auto_threshold: f64,
    pub percentile_clip: Option<(f64, f64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaperNormalizationMode {
    Off,
    WarmPaper,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaperNormalization {
    pub mode: PaperNormalizationMode,
    pub strength: f64,
    pub min_luma: f64,
    pub saturation_threshold: f64,
    pub warm_bias_threshold: f64,
    pub black_anchor: f64,
    pub preserve_red: f64,
    pub paper_white: [f64; 3],
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedDitherOptions {
    pub mode: DitheringMode,
    pub kernel: DiffusionKernel,
    pub serpentine: bool,
    pub color_matching: ColorMatching,
    pub tone_mapping: Option<ToneMapping>,
    pub dynamic_range: Option<DynamicRangeCompression>,
    pub level_compression: Option<LevelCompression>,
    pub paper_normalization: Option<PaperNormalization>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DitherProfile {
    pub name: &'static str,
    pub palette: Vec<PaletteColor>,
    pub auto_intent: AutoIntent,
    pub selected_kernel: DiffusionKernel,
    pub force_serpentine: bool,
    pub manual: ManualAdjustments,
    pub placement: PlacementMode,
    pub resize: ResizePolicy,
}

impl DitherProfile {
    pub fn generate_picture_auto_v2() -> Self {
        let values = [
            ("black", Rgb::new(0x1f, 0x22, 0x26), Rgb::new(0, 0, 0), 0),
            (
                "white",
                Rgb::new(0xd6, 0xd6, 0xd6),
                Rgb::new(255, 255, 255),
                6,
            ),
            ("blue", Rgb::new(0x41, 0x6c, 0xe1), Rgb::new(0, 0, 255), 1),
            ("green", Rgb::new(0x06, 0x74, 0x06), Rgb::new(0, 255, 0), 2),
            ("red", Rgb::new(0xea, 0x48, 0x43), Rgb::new(255, 0, 0), 3),
            (
                "yellow",
                Rgb::new(0xdb, 0xd5, 0x29),
                Rgb::new(255, 255, 0),
                5,
            ),
        ];
        Self {
            name: "generate_picture_auto_v2",
            palette: values
                .into_iter()
                .map(|(name, color, device, device_index)| PaletteColor {
                    name: name.into(),
                    color,
                    device,
                    device_index,
                })
                .collect(),
            auto_intent: AutoIntent::Natural,
            selected_kernel: DiffusionKernel::FloydSteinberg,
            force_serpentine: true,
            manual: ManualAdjustments {
                exposure: 0.0,
                contrast: 0.0,
                saturation: 0.0,
            },
            placement: PlacementMode::AspectFitWhite,
            resize: ResizePolicy::BrowserLikeTriangle,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.palette.is_empty() || self.palette.len() > 16 {
            bail!("profile palette must contain between 1 and 16 colors");
        }
        let mut names = std::collections::HashSet::new();
        let mut indices = std::collections::HashSet::new();
        for entry in &self.palette {
            if entry.name.trim().is_empty() || !names.insert(entry.name.as_str()) {
                bail!("profile palette names must be non-empty and unique");
            }
            if !matches!(entry.device_index, 0 | 1 | 2 | 3 | 5 | 6)
                || !indices.insert(entry.device_index)
            {
                bail!("profile device indices must be unique supported hardware values");
            }
        }
        for value in [
            self.manual.exposure,
            self.manual.contrast,
            self.manual.saturation,
        ] {
            if !value.is_finite() || !(-1.0..=1.0).contains(&value) {
                bail!("profile manual adjustments must be finite and between -1 and 1");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_picture_profile_has_exact_web_defaults() {
        let profile = DitherProfile::generate_picture_auto_v2();
        profile.validate().unwrap();
        assert_eq!(profile.name, "generate_picture_auto_v2");
        assert_eq!(profile.auto_intent, AutoIntent::Natural);
        assert_eq!(profile.selected_kernel, DiffusionKernel::FloydSteinberg);
        assert!(profile.force_serpentine);
        assert_eq!(profile.manual.exposure, 0.0);
        assert_eq!(
            profile
                .palette
                .iter()
                .map(|entry| (entry.name.as_str(), entry.color, entry.device_index))
                .collect::<Vec<_>>(),
            vec![
                ("black", Rgb::new(0x1f, 0x22, 0x26), 0),
                ("white", Rgb::new(0xd6, 0xd6, 0xd6), 6),
                ("blue", Rgb::new(0x41, 0x6c, 0xe1), 1),
                ("green", Rgb::new(0x06, 0x74, 0x06), 2),
                ("red", Rgb::new(0xea, 0x48, 0x43), 3),
                ("yellow", Rgb::new(0xdb, 0xd5, 0x29), 5),
            ]
        );
    }

    #[test]
    fn profile_model_accepts_future_palette_and_slider_values() {
        let mut profile = DitherProfile::generate_picture_auto_v2();
        profile.manual.exposure = 0.25;
        profile.manual.contrast = 0.2;
        profile.manual.saturation = 0.4;
        profile.palette[2].color = Rgb::new(20, 40, 200);
        profile.validate().unwrap();

        profile.manual.saturation = f64::NAN;
        assert!(profile.validate().is_err());
        profile.manual.saturation = 0.0;
        profile.palette[2].device_index = 4;
        assert!(profile.validate().is_err());
    }
}
