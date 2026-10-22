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
        "cli.arg.gui",
        "Open the desktop window (default when a display is available)",
    ),
    (
        "cli.arg.tui",
        "Use the terminal interface instead of the window",
    ),
    (
        "cli.arg.no_tor",
        "Disable Tor completely (testing only - unsafe!)",
    ),
    (
        "cli.arg.daemon",
        "Headless mode: serve the JSON-RPC/REST control API instead of the TUI",
    ),
    (
        "ui.tab.downloads",
        "Downloads",
    ),
    (
        "ui.tab.history",
        "History",
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
        "rdownloader | q=Quit · a=Add · Enter=Details · i=Import file · v=Paste clipboard · p=Pause · r=Resume · c=Cancel · d=Remove · Tab=Switch · ↑↓=Select",
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
        "ui.status.captcha_prompt",
        "Captcha [{host}]: {prompt} -> {buffer}_",
    ),
    ("ui.status.captcha_title", "Captcha Challenge"),
    (
        "ui.status.captcha_image_default",
        "Solve image captcha",
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
        "ui.history.title",
        "History",
    ),
    (
        "ui.history.empty",
        "No finished downloads yet.",
    ),
    (
        "ui.history.hint",
        "e=Export CSV · E=Export JSON · x=Remove from history",
    ),
    (
        "ui.history.stats",
        "{size} · {duration} · {speed}",
    ),
    (
        "ui.status.export_ok",
        "Exported {count} entries to {path}",
    ),
    (
        "ui.status.export_failed",
        "Export failed: {error}",
    ),
    (
        "ui.status.history_removed",
        "Removed from history: {file}",
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
        "ui.linkgrabber.empty",
        "No links collected yet – press 'a' to add a URL, 'i' for a link list, 'v' for the clipboard or 'c' to crawl a page.",
    ),
    (
        "ui.linkgrabber.hint",
        "Space=Select · p=Package · A=All · f=Host filter · F=Type filter · x=Remove · Enter=Transfer to queue · a=Add · i=File · v=Clipboard · c=Crawl",
    ),
    (
        "ui.linkgrabber.counts",
        "{selected} of {total} link(s) selected · {filtered} filtered out",
    ),
    (
        "ui.linkgrabber.package_manual",
        "Manual links",
    ),
    (
        "ui.linkgrabber.package_clipboard",
        "Clipboard",
    ),
    (
        "ui.linkgrabber.package_file",
        "File: {name}",
    ),
    (
        "ui.linkgrabber.package_crawl",
        "Page: {page}",
    ),
    (
        "ui.linkgrabber.package_count",
        "{count} link(s) marked",
    ),
    (
        "ui.linkgrabber.added",
        "Collected {count} link(s), {skipped} already known",
    ),
    (
        "ui.linkgrabber.duplicate_only",
        "All links are already collected",
    ),
    (
        "ui.linkgrabber.filter_host",
        "Host filter: {host} is {state}",
    ),
    (
        "ui.linkgrabber.filter_type",
        "Type filter: {extension} is {state}",
    ),
    (
        "ui.linkgrabber.filter_no_type",
        "This link has no file type to filter",
    ),
    (
        "ui.linkgrabber.filter_excluded",
        "excluded",
    ),
    (
        "ui.linkgrabber.filter_included",
        "included",
    ),
    (
        "ui.linkgrabber.nothing_selected",
        "No link selected – nothing to transfer",
    ),
    (
        "ui.linkgrabber.transferred",
        "{count} link(s) transferred to the download queue",
    ),
    (
        "ui.linkgrabber.removed",
        "Removed from the link grabber: {file}",
    ),
    (
        "ui.linkgrabber.package_removed",
        "Package removed: {name}",
    ),
    (
        "ui.linkgrabber.crawl_started",
        "Crawling {page} ...",
    ),
    (
        "ui.linkgrabber.crawl_done",
        "Found {count} link(s) on {page}",
    ),
    (
        "ui.linkgrabber.crawl_failed",
        "Crawl of {page} failed: {error}",
    ),
    (
        "ui.linkgrabber.no_crawler",
        "No crawler plugin for {page}",
    ),
    (
        "ui.linkgrabber.crawl_no_transport",
        "Crawling needs a transport (Tor)",
    ),
    (
        "ui.linkgrabber.input_url",
        "URL to collect: {buffer}_",
    ),
    (
        "ui.linkgrabber.input_import",
        "Link list file to collect (~ allowed): {buffer}_",
    ),
    (
        "ui.linkgrabber.input_crawl",
        "Page to crawl: {buffer}_",
    ),
    (
        "ui.linkgrabber.size_unknown",
        "size unknown",
    ),
    (
        "ui.status.enqueued_many",
        "Queued {count} link(s) for download, {failed} failed",
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
        "ui.job.queued",
        "Queued",
    ),
    (
        "ui.job.resolving",
        "Resolving",
    ),
    (
        "ui.job.downloading",
        "Downloading",
    ),
    (
        "ui.job.merging",
        "Merging parts",
    ),
    (
        "ui.job.done",
        "Done",
    ),
    (
        "ui.job.paused",
        "Paused",
    ),
    (
        "ui.job.cancelled",
        "Cancelled",
    ),
    (
        "ui.job.failed",
        "Failed: {error}",
    ),
    (
        "ui.part.pending",
        "pending",
    ),
    (
        "ui.part.downloading",
        "downloading",
    ),
    (
        "ui.part.retrying",
        "retry {attempt}/{max}",
    ),
    (
        "ui.part.done",
        "done",
    ),
    (
        "ui.part.failed",
        "failed: {error}",
    ),
    (
        "ui.detail.title",
        "Details",
    ),
    (
        "ui.detail.url",
        "URL: {url}",
    ),
    (
        "ui.detail.status",
        "Status: {status}",
    ),
    (
        "ui.detail.speed_eta",
        "Speed: {speed}   ETA: {eta}",
    ),
    (
        "ui.detail.circuit",
        "Tor: {circuit}",
    ),
    (
        "ui.detail.parts_header",
        "Parts ({count}):",
    ),
    (
        "ui.detail.part_line",
        "  Part {index}: {done} / {total} ({pct}%) {status}",
    ),
    (
        "ui.detail.unknown",
        "-",
    ),
    (
        "ui.detail.circuit_unknown",
        "not in use",
    ),
    (
        "ui.detail.circuit_ready",
        "connected, circuits ready",
    ),
    (
        "ui.detail.circuit_bootstrapping",
        "bootstrapping ({percent}%)",
    ),
    (
        "ui.detail.circuit_blocked",
        "blocked: {reason}",
    ),
    (
        "ui.detail.hint",
        "Enter = toggle details",
    ),
    (
        "ui.status.import_failed",
        "Import failed: {error}",
    ),
    (
        "ui.status.import_none",
        "No http(s) links found to import",
    ),
    (
        "ui.status.import_done",
        "Imported {count} link(s), {skipped} skipped",
    ),
    (
        "ui.status.input_import",
        "Link list file (~ allowed): {buffer}_",
    ),
    (
        "ui.import.title",
        "Import links from file",
    ),
    (
        "gui.tab.downloads",
        "Downloads",
    ),
    (
        "gui.tab.history",
        "History",
    ),
    (
        "gui.add.hint",
        "Paste one or more links (http, https, ftp)",
    ),
    (
        "gui.add.button",
        "Add",
    ),
    (
        "gui.add.clipboard",
        "Paste from clipboard",
    ),
    (
        "gui.tor.off",
        "Tor: off",
    ),
    (
        "gui.tor.ready",
        "Tor: connected",
    ),
    (
        "gui.tor.starting",
        "Tor: starting ({percent}%)",
    ),
    (
        "gui.tor.blocked",
        "Tor: blocked",
    ),
    (
        "gui.queue.empty",
        "No downloads yet. Paste a link above.",
    ),
    (
        "gui.queue.summary",
        "{active} active · {done} done · {total} total",
    ),
    (
        "gui.action.pause",
        "Pause",
    ),
    (
        "gui.action.resume",
        "Resume",
    ),
    (
        "gui.action.cancel",
        "Cancel",
    ),
    (
        "gui.action.remove",
        "Remove",
    ),
    (
        "gui.job.parts",
        "{n} parts",
    ),
    (
        "gui.job.eta",
        "ETA {eta}",
    ),
    (
        "gui.job.details",
        "Details",
    ),
    (
        "gui.history.remove",
        "Remove from history",
    ),
    (
        "gui.captcha.title",
        "Captcha required",
    ),
    (
        "gui.captcha.asks",
        "{host} asks:",
    ),
    (
        "gui.captcha.submit",
        "Submit",
    ),
    (
        "gui.captcha.cancel",
        "Cancel",
    ),
    (
        "ui.settings.title",
        "Settings",
    ),
    (
        "ui.settings.body",
        "Tor clients: {tor_clients}\nMax parts: {max_parts}\nTor data dir: {tor_data_dir}\nTemp dir: {temp_dir}\nDownload dir: {download_dir}\nUser agent: {user_agent}\nConnect timeout: {connect_timeout}s\nRead timeout: {read_timeout}s\nMax retries: {max_retries}\nLog level: {log_level}",
    ),
    ("ui.status.title", "Status"),
    ("ui.status.input_title", "Add URL"),
    ("ui.status.started", "Download started: {file}"),
    (
        "plugin.direct.description",
        "Direct download without redirect resolution",
    ),
    (
        "plugin.redirect.description",
        "Example hoster plugin for resolving link redirects",
    ),
    (
        "plugin.redirect.no_target",
        "No valid redirect target found in URL '{url}'",
    ),
    (
        "plugin.redirect.resolve_failed",
        "Example hoster plugin could not resolve the download link",
    ),
    (
        "plugin.wasm.invalid_module",
        "WebAssembly plugin {name} could not be loaded: {error}",
    ),
    (
        "plugin.wasm.instantiate_failed",
        "WebAssembly plugin instance could not be started: {error}",
    ),
    (
        "plugin.wasm.missing_export",
        "WebAssembly plugin {name} is missing required export '{export}'",
    ),
    (
        "plugin.wasm.meta_empty",
        "WebAssembly plugin {name} has an empty plugin id",
    ),
    (
        "plugin.wasm.invalid_meta",
        "WebAssembly plugin {name} returned invalid metadata: {error}",
    ),
    (
        "plugin.wasm.invalid_output",
        "WebAssembly plugin {name} returned invalid resolved-link data: {error}",
    ),
    (
        "plugin.wasm.output_too_large",
        "WebAssembly plugin {name} returned more than {limit} bytes",
    ),
    (
        "plugin.wasm.guest_error",
        "WebAssembly plugin {name} failed: {error}",
    ),
    (
        "plugin.wasm.task_failed",
        "WebAssembly plugin {name} could not run: {error}",
    ),
    (
        "download.error.content_length_missing",
        "Content-Length missing",
    ),
    ("download.error.no_host", "URL has no host"),
    (
        "download.error.header_closed",
        "Connection closed before end of headers",
    ),
    ("download.error.header_too_large", "HTTP header too large"),
    (
        "download.error.body_truncated",
        "Connection ended early: received {received} of {expected} bytes",
    ),
    (
        "hook.notify.complete",
        "Download finished",
    ),
    (
        "hook.notify.error",
        "Download failed",
    ),
    (
        "hook.notify.error_body",
        "{file}: {error}",
    ),
    (
        "download.error.pieces_corrupt",
        "{count} damaged piece(s) could not be repaired",
    ),
    (
        "download.error.mirror_mismatch",
        "Mirror serves a different file",
    ),
    (
        "download.error.read_timeout",
        "No data received for {secs} s",
    ),
    (
        "download.error.too_slow",
        "Connection too slow: {speed} B/s (minimum {min} B/s)",
    ),
    (
        "download.error.unexpected_status",
        "Unexpected status: {status}",
    ),
    (
        "download.error.create_part_file",
        "create part file: {path}",
    ),
    (
        "download.error.create_output",
        "create output file: {path}",
    ),
    ("download.error.open_part", "open part file: {path}"),
    ("download.error.part_failed", "At least one part failed"),
    ("download.error.incomplete", "Download incomplete"),
    (
        "net.error.tls_hostname",
        "Invalid TLS hostname: {host}",
    ),
    ("net.error.url_without_scheme", "URL without scheme"),
    (
        "net.error.unsupported_scheme",
        "Unsupported scheme: {scheme}",
    ),
    ("net.error.url_without_port", "URL without port"),
    ("net.error.url_without_host", "URL without host"),
    ("net.error.tls_handshake", "TLS handshake with {host}"),
    ("net.error.stream", "Stream -> {host}:{port}"),
    (
        "net.error.request_uri_without_host",
        "Request URI without host",
    ),
    ("net.error.http_handshake", "HTTP/1.1 handshake"),
    ("net.error.send_request", "send HTTP request"),
    ("net.segment.manifest", "Not an HLS or DASH manifest"),
    (
        "net.segment.empty",
        "Streaming manifest lists no segments",
    ),
    (
        "net.segment.encrypted",
        "Encrypted HLS streams are not supported",
    ),
    (
        "net.segment.drm",
        "DRM-protected DASH streams are not supported",
    ),
    (
        "net.segment.live",
        "Live streaming manifests are not supported",
    ),
    (
        "net.segment.fetch",
        "Fetch streaming manifest: {url}",
    ),
    ("net.segment.read", "Read streaming manifest"),
    ("net.segment.init", "Initialization segment"),
    (
        "net.segment.range_ignored",
        "Server ignored the range request for {url}",
    ),
    ("net.segment.write", "Write segment: {path}"),
    ("net.segment.merge", "Merge segments: {path}"),
    (
        "net.segment.segment",
        "Segment {index}/{total}",
    ),
    (
        "net.segment.invalid",
        "Invalid streaming manifest: {detail}",
    ),
    (
        "net.segment.template",
        "Unsupported DASH layout: {detail}",
    ),
    (
        "net.segment.duration",
        "DASH manifest lacks a presentation duration",
    ),
    (
        "main.error.tor_start",
        "Tor could not be started: {e}",
    ),
    (
        "main.error.headless_without_tor",
        "Headless without Tor is not supported yet",
    ),
    ("main.error.tor_unavailable", "Tor not available"),
    (
        "main.error.rpc_bind",
        "RPC server cannot listen on {addr}",
    ),
    (
        "main.error.rpc_public_without_token",
        "Refusing to expose the RPC server on a non-loopback address without rpc_token",
    ),
    ("rpc.error.invalid_request", "Invalid request"),
    ("rpc.error.invalid_params", "Invalid params"),
    ("rpc.error.invalid_job_id", "Invalid job id: {id}"),
    ("rpc.error.job_not_found", "Job not found: {id}"),
    ("rpc.error.job_not_paused", "Job is not paused"),
    ("rpc.error.job_not_running", "Job is not running"),
    ("rpc.error.unknown_method", "Unknown method: {method}"),
    ("rpc.error.not_found_path", "Unknown endpoint"),
    ("rpc.error.body_too_large", "Request body too large"),
    ("rpc.error.unauthorized", "Unauthorized"),
    (
        "rpc.error.invalid_host",
        "Invalid Host header; use localhost or a loopback address",
    ),
    ("rpc.error.cross_origin", "Cross-origin request rejected"),
    (
        "rpc.error.unsupported_media_type",
        "Unsupported media type; send application/json",
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
        load_catalog("xx", "\"cli.arg.parts\" = \"Piezas: {n}\"").unwrap();
        assert!(set_language("xx"));
        assert_eq!(t!("cli.arg.parts", n = 3), "Piezas: 3");
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
