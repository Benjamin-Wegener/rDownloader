//! String catalog for all user-visible text (UI, help, user-facing errors).
//!
//! Keys are stable dotted identifiers (`cli.about`), placeholders are named (`{name}`).
//! English is built in and is the fallback. Further languages are plain TOML files
//! (`key = "text"`) loaded at runtime with [`load_catalog`], so adding a translation
//! needs no code change.
//!
//! ```ignore
//! t!("queue.title");
//! t!("status.started", name = filename);
//! ```

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// Built-in English catalog (the source of truth for keys).
const EN: &[(&str, &str)] = &[
    (
        "cli.about",
        "Clean-room Rust download manager with Tor (Arti) and multi-part downloads",
    ),
    (
        "cli.arg.url",
        "URL to download (without a URL the TUI starts)",
    ),
    (
        "cli.arg.tor_clients",
        "Deprecated: a single Tor client isolates every download part",
    ),
    ("cli.arg.parts", "Number of parts per download (1..=10)"),
    ("cli.arg.output", "Target path (file or directory)"),
    (
        "cli.arg.headless",
        "CLI mode only (no TUI), log continuously",
    ),
    (
        "cli.arg.no_tor",
        "Disable Tor completely (testing only - unsafe!)",
    ),
    (
        "ui.tab.downloads",
        "Downloads",
    ),
    (
        "ui.tab.linkgrabber",
        "LinkGrabber",
    ),
    (
        "ui.tab.plugins",
        "Plugins",
    ),
    (
        "ui.tab.settings",
        "Settings",
    ),
    (
        "ui.status.help",
        "rdownloader | q=Quit · a=Add · p=Pause · r=Resume · c=Cancel · d=Remove · Tab=Switch · ↑↓=Select",
    ),
    (
        "ui.status.queued_url",
        "URL queued: {url}",
    ),
    (
        "ui.status.pause_requested",
        "Pause requested: {file}",
    ),
    (
        "ui.status.resume_requested",
        "Resume requested: {file}",
    ),
    (
        "ui.status.remove_requested",
        "Removed: {file}",
    ),
    (
        "ui.status.cancel_requested",
        "Cancel requested: {file}",
    ),
    (
        "ui.status.no_plugin",
        "No plugin for {url}",
    ),
    (
        "ui.status.resolve_error",
        "Resolve error: {e}",
    ),
    (
        "ui.status.invalid_url",
        "Invalid URL",
    ),
    (
        "ui.status.input_url",
        "Enter URL: {buffer}_",
    ),
    (
        "ui.status.session_restored",
        "Restored {count} download(s) from the previous session",
    ),
    (
        "ui.queue.empty",
        "No downloads – press 'a' to add a URL.",
    ),
    (
        "ui.queue.title",
        "Download Queue",
    ),
    (
        "ui.queue.failed",
        " FAILED({e})",
    ),
    (
        "ui.queue.done",
        " DONE",
    ),
    (
        "ui.queue.paused",
        " PAUSED",
    ),
    (
        "ui.queue.cancelled",
        " CANCELLED",
    ),
    (
        "ui.progress.title",
        "Progress",
    ),
    (
        "ui.progress.label",
        "{done} / {total} ({parts} parts)",
    ),
    (
        "ui.linkgrabber.title",
        "LinkGrabber",
    ),
    (
        "ui.linkgrabber.body",
        "LinkGrabber (roadmap phase):\n\n- Clipboard integration (auto-capture)\n- Batch import (text file, M3U, etc.)\n- Crawler plugins (find more links on a page)\n- Preview before enqueue\n\nNot yet implemented – see ROADMAP.md",
    ),
    (
        "ui.plugins.title",
        "Plugins (clean-room Rust, not JD .jar)",
    ),
    (
        "ui.plugins.item",
        "[direct] Direct Download v0.1.0 – fallback, accepts any URL",
    ),
    (
        "ui.settings.title",
        "Settings",
    ),
];

struct State {
    language: String,
    catalogs: HashMap<String, HashMap<String, String>>,
}

fn state() -> &'static RwLock<State> {
    static STATE: OnceLock<RwLock<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        let en = EN
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        RwLock::new(State {
            language: "en".into(),
            catalogs: HashMap::from([("en".to_string(), en)]),
        })
    })
}

/// Registers (or replaces) the catalog for `lang` from TOML text (`key = "text"`).
/// Missing keys fall back to English at lookup time.
pub fn load_catalog(lang: &str, toml_text: &str) -> Result<()> {
    let map: HashMap<String, String> = toml::from_str(toml_text)
        .with_context(|| format!("invalid catalog for language {lang}"))?;
    state()
        .write()
        .unwrap()
        .catalogs
        .insert(lang.to_string(), map);
    Ok(())
}

/// Selects the active language; returns `false` (and keeps the current one) if unknown.
pub fn set_language(lang: &str) -> bool {
    let mut st = state().write().unwrap();
    if st.catalogs.contains_key(lang) {
        st.language = lang.to_string();
        true
    } else {
        false
    }
}

pub fn language() -> String {
    state().read().unwrap().language.clone()
}

/// Looks up `key` in the active language, then English, then returns the key itself.
pub fn tr(key: &str) -> String {
    let st = state().read().unwrap();
    st.catalogs
        .get(&st.language)
        .and_then(|c| c.get(key))
        .or_else(|| st.catalogs.get("en").and_then(|c| c.get(key)))
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

/// Like [`tr`], replacing each `{name}` with the matching argument.
/// Unknown placeholders are left untouched.
pub fn tr_args(key: &str, args: &[(&str, String)]) -> String {
    substitute(&tr(key), args)
}

fn substitute(text: &str, args: &[(&str, String)]) -> String {
    let mut text = text.to_string();
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

#[macro_export]
macro_rules! t {
    ($key:expr) => {
        $crate::i18n::tr($key)
    };
    ($key:expr, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::i18n::tr_args($key, &[$((stringify!($name), $value.to_string())),+])
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_is_default() {
        assert!(tr("cli.arg.parts").contains("parts"));
    }

    #[test]
    fn unknown_key_returns_key() {
        assert_eq!(tr("no.such.key"), "no.such.key");
    }

    #[test]
    fn named_placeholders_are_replaced() {
        let args = [("name", "a.bin".to_string()), ("n", "4".to_string())];
        assert_eq!(
            substitute("Started {name} ({n} parts)", &args),
            "Started a.bin (4 parts)"
        );
        assert_eq!(substitute("{unknown} stays", &args), "{unknown} stays");
        assert_eq!(t!("no.such.key", name = "x"), "no.such.key");
    }

    #[test]
    fn translation_with_fallback() {
        // One test mutates the global language; keep all language switching here.
        load_catalog("xx", "\"cli.arg.parts\" = \"Teile: {n}\"").unwrap();
        assert!(set_language("xx"));
        assert_eq!(t!("cli.arg.parts", n = 3), "Teile: 3");
        // Key missing in "xx" falls back to English.
        assert_eq!(
            tr("cli.arg.url"),
            EN.iter().find(|(k, _)| *k == "cli.arg.url").unwrap().1
        );
        assert!(!set_language("does-not-exist"));
        assert_eq!(language(), "xx");
        set_language("en");
    }

    #[test]
    fn invalid_catalog_is_an_error() {
        assert!(load_catalog("bad", "this is = = not toml").is_err());
    }

    #[test]
    fn english_keys_are_unique_and_nonempty() {
        let mut seen = std::collections::HashSet::new();
        for (k, v) in EN {
            assert!(seen.insert(*k), "duplicate key {k}");
            assert!(!v.is_empty(), "empty text for {k}");
        }
    }
}
