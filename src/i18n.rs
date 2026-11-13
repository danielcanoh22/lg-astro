//! Bundled English and Spanish translations, plus locale-aware date, clock and greeting text.
//!
//! Messages come from `assets/messages.json`, embedded at compile time and parsed once. Lookups
//! fall back to English and then to the key itself, so a missing string never panics at runtime.
use serde::{Deserialize, Deserializer, Serialize};
use std::{collections::BTreeMap, sync::OnceLock};
use time::OffsetDateTime;

/// Message catalog: key → language code → text.
type Messages = BTreeMap<String, BTreeMap<String, String>>;
/// Returns the parsed message catalog.
///
/// # Panics
///
/// Panics on first use if the bundled `messages.json` is invalid; unit tests parse it.
fn messages() -> &'static Messages {
    static CATALOG: OnceLock<Messages> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../assets/messages.json"))
            .expect("Bundled translations must be valid")
    })
}

/// A supported UI language, persisted by its lowercase code.
///
/// Adding a variant also requires translations, font coverage and the Homebrew description
/// (see `agents.md`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Es,
}
impl Language {
    /// Every language in picker order, sorted by displayed name; [`Language::index`] relies on it.
    pub const ALL: [Self; 2] = [Self::En, Self::Es];
    /// Returns the code used in preferences and as the catalog column.
    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Es => "es",
        }
    }
    /// Returns the language's own name for the picker.
    pub fn name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Es => "Español",
        }
    }
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|language| *language == self)
            .unwrap()
    }
    /// Translates `key`, falling back to English and then to the key itself.
    pub fn t(self, key: &str) -> &str {
        messages()
            .get(key)
            .and_then(|entry| entry.get(self.code()).or_else(|| entry.get("en")))
            .map(String::as_str)
            .unwrap_or(key)
    }
    /// Translates `key` and replaces each `{name}` placeholder with its value.
    pub fn format(self, key: &str, values: &[(&str, &str)]) -> String {
        let mut value = self.t(key).to_owned();
        for (name, replacement) in values {
            value = value.replace(&format!("{{{name}}}"), replacement);
        }
        value
    }
    /// Translates a [`crate::model::CATEGORIES`] name; unknown values read as Utilities.
    pub fn category(self, value: &str) -> &str {
        match value {
            "Streaming" => self.t("categoryStreaming"),
            "Music" => self.t("categoryMusic"),
            "Gaming" => self.t("categoryGaming"),
            "Utilities" => self.t("categoryUtilities"),
            _ => self.t("categoryUtilities"),
        }
    }
    /// Returns the Home greeting for a local `hour` (0–23), naming the user when `name` is set.
    pub fn greeting(self, hour: u8, name: &str) -> String {
        let key = match hour {
            0..=4 => "greetingNight",
            5..=11 => "morning",
            12..=17 => "afternoon",
            _ => "evening",
        };
        let name = name.trim();
        if name.is_empty() {
            self.t(key).to_owned()
        } else {
            self.format(&format!("{key}Named"), &[("name", name)])
        }
    }
    /// Formats the localized, upper-case weekday and date ("WEDNESDAY, OCTOBER 21").
    pub fn date(self, now: OffsetDateTime) -> String {
        let weekday = now.weekday().number_days_from_monday() as usize;
        let month = now.month() as usize - 1;
        match self {
            Self::En => format!(
                "{}, {} {}",
                [
                    "Monday",
                    "Tuesday",
                    "Wednesday",
                    "Thursday",
                    "Friday",
                    "Saturday",
                    "Sunday"
                ][weekday],
                [
                    "January",
                    "February",
                    "March",
                    "April",
                    "May",
                    "June",
                    "July",
                    "August",
                    "September",
                    "October",
                    "November",
                    "December"
                ][month],
                now.day()
            )
            .to_uppercase(),
            Self::Es => format!(
                "{}, {} de {}",
                [
                    "lunes",
                    "martes",
                    "miércoles",
                    "jueves",
                    "viernes",
                    "sábado",
                    "domingo"
                ][weekday],
                now.day(),
                [
                    "enero",
                    "febrero",
                    "marzo",
                    "abril",
                    "mayo",
                    "junio",
                    "julio",
                    "agosto",
                    "septiembre",
                    "octubre",
                    "noviembre",
                    "diciembre"
                ][month]
            )
            .to_uppercase(),
        }
    }
    /// Formats the time; seconds are inserted before any AM/PM suffix.
    pub fn clock(self, now: OffsetDateTime, hour24: bool, seconds: bool) -> String {
        let base = self.clock_base(now, hour24);
        if !seconds {
            return base;
        }
        let tail = format!(":{:02}", now.second());
        match base.find(' ') {
            Some(space) => format!("{}{tail}{}", &base[..space], &base[space..]),
            None => format!("{base}{tail}"),
        }
    }
    /// Formats hours and minutes: English pads 24-hour times ("08:05"), Spanish does not.
    fn clock_base(self, now: OffsetDateTime, hour24: bool) -> String {
        if hour24 {
            return if self == Self::En {
                format!("{:02}:{:02}", now.hour(), now.minute())
            } else {
                format!("{}:{:02}", now.hour(), now.minute())
            };
        }
        let hour = match now.hour() % 12 {
            0 => 12,
            value => value,
        };
        format!(
            "{hour}:{:02} {}",
            now.minute(),
            if now.hour() >= 12 { "PM" } else { "AM" }
        )
    }
}
/// Deserializes [`Language`] leniently for [`crate::model::Preferences`].
pub fn deserialize_language<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Language, D::Error> {
    // Removed languages and unknown codes fall back to English without failing the preferences.
    Ok(
        match serde_json::Value::deserialize(deserializer)?.as_str() {
            Some("es") => Language::Es,
            _ => Language::En,
        },
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn homebrew_description_lists_every_supported_language() {
        let description = include_str!("../assets/homebrew-description.html");
        for language in super::Language::ALL {
            let name = match language {
                super::Language::En => "English",
                super::Language::Es => "Spanish",
            };
            assert!(
                description.contains(name),
                "Homebrew description is missing {name}"
            );
        }
    }
    use super::*;
    #[test]
    fn every_message_has_all_languages_and_no_empty_values() {
        for (key, entry) in messages() {
            for language in Language::ALL {
                assert!(
                    !entry
                        .get(language.code())
                        .unwrap_or_else(|| panic!("Missing {key} / {}", language.code()))
                        .is_empty()
                );
            }
        }
    }
    #[test]
    fn every_translation_preserves_the_english_message_placeholders() {
        fn placeholders(message: &str) -> std::collections::BTreeSet<&str> {
            message
                .split('{')
                .skip(1)
                .map(|part| {
                    part.split_once('}')
                        .expect("Unclosed message placeholder")
                        .0
                })
                .collect()
        }

        for (key, entry) in messages() {
            let expected = placeholders(&entry["en"]);
            for language in Language::ALL {
                assert_eq!(
                    placeholders(&entry[language.code()]),
                    expected,
                    "Mismatched placeholders in {key} / {}",
                    language.code()
                );
            }
        }
    }
    #[test]
    fn localization_keeps_user_provided_names_intact() {
        assert_eq!(
            Language::Es.format("favoriteAdded", &[("app", "Netflix")]),
            "Netflix añadida a favoritos."
        );
        assert_eq!(Language::En.greeting(15, ""), "Good afternoon.");
        assert_eq!(Language::En.greeting(15, "  Ana "), "Good afternoon, Ana.");
        assert_eq!(Language::Es.greeting(2, "Ana"), "¿Aún despiertos, Ana?");
    }
    #[test]
    fn clock_and_date_follow_the_selected_locale() {
        let now = OffsetDateTime::parse(
            "2026-10-21T20:41:00+02:00",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!(Language::Es.date(now), "MIÉRCOLES, 21 DE OCTUBRE");
        assert_eq!(Language::Es.clock(now, true, false), "20:41");
        assert_eq!(Language::En.clock(now, true, false), "20:41");
        assert_eq!(Language::En.clock(now, true, true), "20:41:00");
        assert_eq!(Language::En.clock(now, false, true), "8:41:00 PM");
    }
}
