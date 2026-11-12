//! Video quality settings the viewer can change at any time.

use serde::{Deserialize, Serialize};

/// How many colours are kept. Fewer colours = smaller, faster pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ColorDepth {
    /// The colours the remote screen really uses, at most 256 of them,
    /// learned by the host (`palette.rs`). Parts that need more (photos,
    /// video) are sent as with [`ColorDepth::Full`].
    Optimized,
    /// 16 million colours (24-bit).
    Full,
    /// 65 thousand colours (16-bit).
    #[default]
    High,
    /// 256 colours (8-bit).
    Low,
    /// Shades of grey only.
    Gray,
}

impl ColorDepth {
    pub const ALL: [ColorDepth; 5] =
        [ColorDepth::Optimized, ColorDepth::Full, ColorDepth::High, ColorDepth::Low, ColorDepth::Gray];
    pub fn label(self) -> &'static str {
        match self {
            ColorDepth::Optimized => "Colour optimised (the screen's own 256)",
            ColorDepth::Full => "Full colour (24-bit)",
            ColorDepth::High => "High colour (16-bit)",
            ColorDepth::Low => "256 colours (8-bit)",
            ColorDepth::Gray => "Greyscale",
        }
    }
    pub fn short_label(self) -> &'static str {
        match self {
            ColorDepth::Optimized => "Colour optimised",
            ColorDepth::Full => "Full colour",
            ColorDepth::High => "High colour",
            ColorDepth::Low => "256 colours",
            ColorDepth::Gray => "Greyscale",
        }
    }
}

/// How the picture is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    /// Per part of the screen: text and windows (at most 256 colours) are sent
    /// lossless, photos and video as JPEG. JPEG parts are sent again in high
    /// quality once they stop changing.
    #[default]
    Auto,
    /// Lossy, small. Good for photos/video and slow links.
    Jpeg,
    /// Lossless with the chosen colour depth. Crisp text.
    Png,
}

impl Codec {
    pub const ALL: [Codec; 3] = [Codec::Auto, Codec::Jpeg, Codec::Png];
    pub fn label(self) -> &'static str {
        match self {
            Codec::Auto => "Auto",
            Codec::Jpeg => "Smooth (JPEG)",
            Codec::Png => "Lossless (PNG)",
        }
    }
}

/// Named starting points shown as buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Fast,
    Balanced,
    Sharp,
    Custom,
}

impl Preset {
    pub const BUTTONS: [Preset; 3] = [Preset::Fast, Preset::Balanced, Preset::Sharp];
    pub fn label(self) -> &'static str {
        match self {
            Preset::Fast => "Fast",
            Preset::Balanced => "Balanced",
            Preset::Sharp => "Sharp",
            Preset::Custom => "Custom",
        }
    }
    pub fn hint(self) -> &'static str {
        match self {
            Preset::Fast => "Slow links: smaller picture, JPEG only",
            Preset::Balanced => "Full size: crisp text, smooth video",
            Preset::Sharp => "Fast networks: everything lossless",
            Preset::Custom => "Your own mix",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quality {
    /// Size of the picture compared with the real screen, 25..=100 %.
    pub scale_pct: u8,
    /// Highest pictures per second, 1..=30.
    pub max_fps: u8,
    pub codec: Codec,
    /// JPEG quality 10..=95 (ignored for PNG; used by Auto for photos).
    pub jpeg_quality: u8,
    pub color: ColorDepth,
}

impl Default for Quality {
    fn default() -> Self {
        Quality::preset(Preset::Balanced)
    }
}

impl Quality {
    pub fn preset(p: Preset) -> Quality {
        match p {
            Preset::Fast => Quality { scale_pct: 75, max_fps: 20, codec: Codec::Jpeg, jpeg_quality: 50, color: ColorDepth::Full },
            Preset::Balanced | Preset::Custom => {
                Quality { scale_pct: 100, max_fps: 30, codec: Codec::Auto, jpeg_quality: 75, color: ColorDepth::Optimized }
            }
            Preset::Sharp => Quality { scale_pct: 100, max_fps: 30, codec: Codec::Png, jpeg_quality: 90, color: ColorDepth::Full },
        }
    }

    /// Presets of version 0.1 (blurry: smaller picture, 16-bit colour). Saved
    /// settings equal to one of them are moved to the new preset of that name.
    pub fn upgraded(self) -> Quality {
        let old = [
            (Quality { scale_pct: 60, max_fps: 12, codec: Codec::Jpeg, jpeg_quality: 35, color: ColorDepth::Low }, Preset::Fast),
            (Quality { scale_pct: 85, max_fps: 20, codec: Codec::Jpeg, jpeg_quality: 65, color: ColorDepth::High }, Preset::Balanced),
            (Quality { scale_pct: 100, max_fps: 25, codec: Codec::Png, jpeg_quality: 85, color: ColorDepth::High }, Preset::Sharp),
        ];
        old.into_iter().find(|(q, _)| *q == self).map(|(_, p)| Quality::preset(p)).unwrap_or(self)
    }

    /// Which preset these settings match (colours aside: they are chosen
    /// separately), or `Custom`.
    pub fn matching_preset(&self) -> Preset {
        let me = self.clamped();
        Preset::BUTTONS.into_iter().find(|p| self.with_preset(*p) == me).unwrap_or(Preset::Custom)
    }

    /// Preset `p`, keeping the colours chosen here.
    pub fn with_preset(&self, p: Preset) -> Quality {
        Quality { color: self.color, ..Quality::preset(p) }
    }

    /// Keep every value in its allowed range (values come from the network).
    pub fn clamped(mut self) -> Quality {
        self.scale_pct = self.scale_pct.clamp(25, 100);
        self.max_fps = self.max_fps.clamp(1, 30);
        self.jpeg_quality = self.jpeg_quality.clamp(10, 95);
        self
    }

    /// Size of a scaled picture for a monitor of `w` x `h` pixels (never 0).
    pub fn scaled_size(&self, w: u32, h: u32) -> (u32, u32) {
        let pct = self.clamped().scale_pct as u32;
        (((w * pct) / 100).max(1), ((h * pct) / 100).max(1))
    }

    /// Time between pictures in milliseconds.
    pub fn frame_interval_ms(&self) -> u64 {
        1000 / self.clamped().max_fps as u64
    }

    /// Short one-line description for status bars.
    pub fn summary(&self) -> String {
        let q = self.clamped();
        let codec = match q.codec {
            Codec::Auto => "Auto".to_string(),
            Codec::Jpeg => format!("JPEG {}", q.jpeg_quality),
            Codec::Png => "PNG".to_string(),
        };
        let color = match q.color {
            ColorDepth::Optimized => "optimised",
            ColorDepth::Full => "24-bit",
            ColorDepth::High => "16-bit",
            ColorDepth::Low => "8-bit",
            ColorDepth::Gray => "grey",
        };
        format!("{}% · {} fps · {} · {}", q.scale_pct, q.max_fps, codec, color)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_round_trip() {
        for p in Preset::BUTTONS {
            assert_eq!(Quality::preset(p).matching_preset(), p);
        }
        let mut q = Quality::preset(Preset::Fast);
        q.max_fps = 7;
        assert_eq!(q.matching_preset(), Preset::Custom);
        let grey = Quality { color: ColorDepth::Gray, ..Quality::preset(Preset::Sharp) };
        assert_eq!(grey.matching_preset(), Preset::Sharp, "colours are chosen apart from presets");
        assert_eq!(grey.with_preset(Preset::Fast).color, ColorDepth::Gray);
    }

    #[test]
    fn clamps_hostile_values() {
        let q = Quality { scale_pct: 0, max_fps: 200, codec: Codec::Png, jpeg_quality: 1, color: ColorDepth::Gray }.clamped();
        assert_eq!(q.scale_pct, 25);
        assert_eq!(q.max_fps, 30);
        assert_eq!(q.jpeg_quality, 10);
    }

    #[test]
    fn scaled_size_and_interval() {
        let q = Quality { scale_pct: 50, max_fps: 20, ..Quality::default() };
        assert_eq!(q.scaled_size(1920, 1080), (960, 540));
        assert_eq!(q.frame_interval_ms(), 50);
    }

    #[test]
    fn json_is_stable() {
        let q = Quality::preset(Preset::Sharp);
        let s = serde_json::to_string(&q).unwrap();
        assert!(s.contains("\"codec\":\"png\""));
        let back: Quality = serde_json::from_str(&s).unwrap();
        assert_eq!(back, q);
    }

    #[test]
    fn summary_text() {
        assert_eq!(Quality::preset(Preset::Fast).summary(), "75% · 20 fps · JPEG 50 · 24-bit");
        assert_eq!(Quality::preset(Preset::Balanced).summary(), "100% · 30 fps · Auto · optimised");
    }

    #[test]
    fn old_presets_are_upgraded() {
        let old_balanced = Quality { scale_pct: 85, max_fps: 20, codec: Codec::Jpeg, jpeg_quality: 65, color: ColorDepth::High };
        assert_eq!(old_balanced.upgraded(), Quality::preset(Preset::Balanced));
        let mine = Quality { scale_pct: 50, ..Quality::default() };
        assert_eq!(mine.upgraded(), mine, "own settings are kept");
    }
}
