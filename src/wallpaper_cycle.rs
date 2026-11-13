//! Automatic background cycling with one fixed interval and no per-image exclusions.
//!
//! The pure scheduling rules live here; the event loop in `main.rs` decides when the launcher
//! is idle enough to switch and [`crate::state::State`] applies the change.
use serde::{Deserialize, Deserializer};
use std::time::Duration;

/// Seconds between automatic background changes.
pub const INTERVAL: u64 = 15 * 60;

/// Returns whether cycling is enabled and [`INTERVAL`] seconds have passed since the last change.
///
/// Both timestamps are Unix seconds. A `changed` time in the future (the TV clock moved back)
/// saturates to zero elapsed, so the background waits instead of switching immediately.
pub fn due(enabled: bool, changed: u64, now: u64) -> bool {
    enabled && now.saturating_sub(changed) >= INTERVAL
}

/// Returns whether the one launch-time advance should run now.
///
/// It waits for the wallpaper catalog scan so uploaded backgrounds can participate, but gives up
/// waiting after three seconds so a slow scan cannot postpone the change indefinitely.
pub fn startup_due(enabled: bool, pending: bool, catalog_ready: bool, elapsed: Duration) -> bool {
    enabled && pending && (catalog_ready || elapsed >= Duration::from_secs(3))
}

/// Deserializes the `wallpaper_cycle` preference as an on/off flag.
///
/// Accepts the current boolean and the string modes saved by the former duration-based UI;
/// any other value reads as disabled instead of failing the whole preferences file.
///
/// # Errors
///
/// Returns the deserializer's error only when the value itself cannot be read.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Bool(enabled) => enabled,
        // Preserve enabled preferences from the former duration-based UI.
        serde_json::Value::String(mode) => matches!(
            mode.as_str(),
            "on" | "launch" | "quarter" | "hourly" | "daily"
        ),
        _ => false,
    })
}

/// Returns the background after `current` in gallery order, wrapping at the end.
///
/// Unavailable images are expected to be absent from `ids`. An unknown `current` starts from
/// the first entry, and `None` means there is no other background to switch to.
pub fn next(ids: &[String], current: &str) -> Option<String> {
    let start = ids
        .iter()
        .position(|id| id == current)
        .map_or(0, |index| index + 1);
    (0..ids.len())
        .map(|offset| &ids[(start + offset) % ids.len()])
        .find(|id| *id != current)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_available_background_participates_in_order_and_wraps() {
        let ids = vec!["slate".into(), "dunes".into(), "custom-id".into()];
        assert_eq!(next(&ids, "slate").as_deref(), Some("dunes"));
        assert_eq!(next(&ids, "dunes").as_deref(), Some("custom-id"));
        assert_eq!(next(&ids, "custom-id").as_deref(), Some("slate"));
        assert_eq!(next(&ids, "missing").as_deref(), Some("slate"));
        assert_eq!(next(&["slate".into()], "slate"), None);
        assert_eq!(next(&[], "slate"), None);
    }

    #[test]
    fn cycling_is_due_only_when_enabled_after_fifteen_minutes() {
        assert!(!due(false, 0, u64::MAX));
        assert!(!due(true, 100, 100 + INTERVAL - 1));
        assert!(due(true, 100, 100 + INTERVAL));
        assert!(!due(true, 1_800_000_000, 1_577_836_800));
        assert!(!due(true, u64::MAX, u64::MAX));
    }

    #[test]
    fn enabled_rotation_advances_once_after_startup_catalog_or_timeout() {
        assert!(!startup_due(false, true, true, Duration::ZERO));
        assert!(!startup_due(true, false, true, Duration::ZERO));
        assert!(!startup_due(true, true, false, Duration::from_secs(2)));
        assert!(startup_due(true, true, true, Duration::ZERO));
        assert!(startup_due(true, true, false, Duration::from_secs(3)));
    }

    #[test]
    fn on_off_preferences_default_off_round_trip_and_load_legacy_modes() {
        assert!(!crate::model::Preferences::default().wallpaper_cycle);
        for enabled in [false, true] {
            let preferences: crate::model::Preferences =
                serde_json::from_value(serde_json::json!({"wallpaper_cycle": enabled})).unwrap();
            assert_eq!(preferences.wallpaper_cycle, enabled);
            assert_eq!(
                serde_json::to_value(preferences).unwrap()["wallpaper_cycle"],
                enabled
            );
        }
        for mode in ["on", "launch", "quarter", "hourly", "daily"] {
            let preferences: crate::model::Preferences =
                serde_json::from_value(serde_json::json!({"wallpaper_cycle": mode})).unwrap();
            assert!(preferences.wallpaper_cycle);
        }
        for value in [
            serde_json::json!("off"),
            serde_json::json!("weekly"),
            serde_json::json!(3),
            serde_json::Value::Null,
        ] {
            let preferences: crate::model::Preferences = serde_json::from_value(
                serde_json::json!({"wallpaper_cycle": value, "greeting_name": "Ana"}),
            )
            .unwrap();
            assert!(!preferences.wallpaper_cycle);
            assert_eq!(preferences.greeting_name, "Ana");
        }
    }
}
