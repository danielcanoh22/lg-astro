//! Placeholder apps for the explicit `--test-extra-apps` scroll preview.
//!
//! The preview clones real inventory entries as artwork templates and is applied only after a
//! successful inventory read; it is never a substitute for a failed TV inventory. Placeholder
//! IDs are recognisable so they are never launched or removed on the TV.
use crate::{i18n::Language, model::Inventory};

/// App-ID prefix that marks a placeholder card; no installed app uses it.
const PREFIX: &str = "com.astro.scroll-preview.";

/// Returns whether `id` belongs to a scroll-preview placeholder rather than an installed app.
pub fn is_placeholder(id: &str) -> bool {
    id.starts_with(PREFIX)
}

/// Appends up to `count` labelled placeholder apps that reuse the artwork of the real apps.
///
/// IDs are deterministic (`000`, `001`, …), so calling it again after a refresh skips
/// placeholders that already exist. Does nothing when there are no real apps to copy.
pub fn append(inventory: &mut Inventory, count: usize, language: Language) {
    if count == 0 {
        return;
    }
    let templates = inventory.apps.clone();
    if templates.is_empty() {
        eprintln!("Astro scroll preview: no real apps to use as artwork templates");
        return;
    }
    let mut added = 0;
    for index in 0..count {
        let id = format!("{PREFIX}{index:03}");
        if inventory.apps.iter().any(|app| app.id == id) {
            continue;
        }
        let mut app = templates[index % templates.len()].clone();
        app.id = id;
        app.title = format!(
            "{} {:03} · {}",
            language.t("scrollTestApp"),
            index + 1,
            app.title
        );
        inventory.apps.push(app);
        added += 1;
    }
    eprintln!(
        "Astro scroll preview: {added} placeholder apps appended to {} real apps",
        templates.len()
    );
}
