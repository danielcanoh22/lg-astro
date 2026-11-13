//! Holds the saved text rendering preference and the startup GL configuration derived from it.
//!
//! The mode only takes effect on the next start, because SDL picks the window's sample count
//! when it creates the window.
use serde::{Deserialize, Deserializer, Serialize};

/// The `text_rendering` preference: how text and logo edges are antialiased.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TextRendering {
    /// A multisampled window with intact outlines and no emboldening.
    #[default]
    Msaa,
    /// A single-sample window with analytic edge filters and small-text emboldening.
    Analytic,
}

impl TextRendering {
    /// Every mode in settings picker order.
    pub const ALL: [Self; 2] = [Self::Msaa, Self::Analytic];

    /// Returns the position in [`Self::ALL`].
    pub fn index(self) -> usize {
        match self {
            Self::Msaa => 0,
            Self::Analytic => 1,
        }
    }

    /// Returns the translation key of the settings label.
    pub fn label_key(self) -> &'static str {
        match self {
            Self::Msaa => "textRenderingMsaa",
            Self::Analytic => "textRenderingAnalytic",
        }
    }
}

/// Reads the saved preference, treating a missing, unknown or malformed value as MSAA.
///
/// # Errors
///
/// Fails only when the deserializer itself fails to produce a JSON value.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<TextRendering, D::Error> {
    Ok(
        match serde_json::Value::deserialize(deserializer)?.as_str() {
            Some("analytic") => TextRendering::Analytic,
            _ => TextRendering::Msaa,
        },
    )
}

/// The text configuration chosen at startup and reported in diagnostics.
#[derive(Debug, Serialize)]
pub struct Configuration {
    /// The effective mode after environment overrides.
    pub mode: TextRendering,
    /// MSAA sample counts to try in order when creating the window; 0 is single-sample.
    #[serde(skip)]
    pub sample_counts: &'static [u8],
    /// Small-text emboldening strength in physical pixels per edge.
    pub embolden: f32,
}

impl Configuration {
    /// Resolves the mode and emboldening from the saved preference and the `ASTRO_MSAA` and
    /// `ASTRO_EMBOLDEN` environment values, which take precedence for diagnostics. Unrecognised
    /// values are ignored.
    pub fn new(preference: TextRendering, msaa: Option<&str>, embolden: Option<&str>) -> Self {
        let mode = match msaa {
            Some("4") => TextRendering::Msaa,
            Some("0") => TextRendering::Analytic,
            _ => preference,
        };
        let (sample_counts, default_embolden): (&'static [u8], f32) = match mode {
            TextRendering::Msaa => (&[4, 2, 0], 0.),
            TextRendering::Analytic => (&[0], 0.25),
        };
        Self {
            mode,
            sample_counts,
            embolden: embolden
                .and_then(|value| value.parse::<f32>().ok())
                .filter(|strength| strength.is_finite())
                .unwrap_or(default_embolden),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_style_defaults_keep_no_emboldening_even_on_single_sample_fallbacks() {
        let config = Configuration::new(TextRendering::default(), None, None);
        assert_eq!(config.sample_counts, &[4, 2, 0]);
        assert_eq!(config.embolden, 0.);
        let analytic = Configuration::new(TextRendering::Analytic, None, None);
        assert_eq!(analytic.sample_counts, &[0]);
        assert_eq!(analytic.embolden, 0.25);
    }

    #[test]
    fn environment_overrides_take_precedence_over_both_saved_modes() {
        for preference in TextRendering::ALL {
            let msaa = Configuration::new(preference, Some("4"), None);
            assert_eq!(msaa.sample_counts, &[4, 2, 0]);
            assert_eq!(msaa.embolden, 0.);
            let analytic = Configuration::new(preference, Some("0"), Some("0.4"));
            assert_eq!(analytic.sample_counts, &[0]);
            assert_eq!(analytic.embolden, 0.4);
        }
        let invalid = Configuration::new(TextRendering::Analytic, Some("invalid"), Some("NaN"));
        assert_eq!(invalid.sample_counts, &[0]);
        assert_eq!(invalid.embolden, 0.25);
    }
}
