//! Guard: rdownloader sends no telemetry, analytics or crash reports.
//!
//! This cannot prove the absence of every possible phone-home, but it stops the
//! common ways one sneaks in: a telemetry crate in `Cargo.toml` or (transitively) in
//! `Cargo.lock`, or a well-known analytics host name in the sources. A new entry on
//! one of the lists below is cheap; a real finding must be removed, not allowed.

use std::path::{Path, PathBuf};

/// Crates whose job is to report data about the running program.
const TELEMETRY_CRATES: &[&str] = &[
    "sentry",
    "opentelemetry",
    "tracing-opentelemetry",
    "segment",
    "posthog-rs",
    "mixpanel",
    "amplitude",
    "datadog-tracing",
    "dogstatsd",
    "newrelic",
    "bugsnag",
    "rollbar",
    "honeycomb",
    "metrics-exporter-prometheus",
    "appcenter",
    "google-analytics",
    "matomo",
    "plausible",
    "crashreport",
    "human-panic",
    "color-backtrace-report",
];

/// Host names of analytics and crash-reporting services.
const TELEMETRY_HOSTS: &[&str] = &[
    "sentry.io",
    "google-analytics.com",
    "googletagmanager.com",
    "segment.io",
    "segment.com",
    "mixpanel.com",
    "amplitude.com",
    "posthog.com",
    "datadoghq.com",
    "bugsnag.com",
    "rollbar.com",
    "hotjar.com",
    "plausible.io",
    "matomo.cloud",
    "newrelic.com",
    "nr-data.net",
    "applicationinsights.azure.com",
    "clarity.ms",
    "honeycomb.io",
    "crashlytics.com",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read source directory") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Package names of a `Cargo.lock`.
fn locked_packages(lock: &str) -> Vec<String> {
    lock.lines()
        .filter_map(|l| l.strip_prefix("name = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .map(str::to_string)
        .collect()
}

#[test]
fn no_telemetry_crates_in_the_dependency_tree() {
    let lock = std::fs::read_to_string(root().join("Cargo.lock")).expect("Cargo.lock");
    let found: Vec<String> = locked_packages(&lock)
        .into_iter()
        .filter(|name| TELEMETRY_CRATES.contains(&name.as_str()))
        .collect();
    assert!(
        found.is_empty(),
        "telemetry crates in Cargo.lock: {found:?}"
    );

    let manifest = std::fs::read_to_string(root().join("Cargo.toml")).expect("Cargo.toml");
    let direct: Vec<&&str> = TELEMETRY_CRATES
        .iter()
        .filter(|c| {
            manifest.lines().any(|l| {
                l.trim_start().starts_with(&format!("{c} "))
                    || l.trim_start().starts_with(&format!("{c}="))
            })
        })
        .collect();
    assert!(
        direct.is_empty(),
        "telemetry crates in Cargo.toml: {direct:?}"
    );
}

#[test]
fn no_analytics_hosts_in_the_sources() {
    let mut files = Vec::new();
    rust_files(&root().join("src"), &mut files);
    let mut offenders = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read source file");
        for (n, line) in text.lines().enumerate() {
            for host in TELEMETRY_HOSTS {
                if line.contains(host) {
                    offenders.push(format!("{}:{} ({host})", path.display(), n + 1));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "analytics hosts in the sources: {offenders:#?}"
    );
}

#[test]
fn the_guard_itself_detects_what_it_looks_for() {
    // A guard that never fires is worthless: check the matching logic on samples.
    let lock = "[[package]]\nname = \"serde\"\n\n[[package]]\nname = \"sentry\"\n";
    let names = locked_packages(lock);
    assert_eq!(names, ["serde", "sentry"]);
    assert!(names.iter().any(|n| TELEMETRY_CRATES.contains(&n.as_str())));
    assert!(TELEMETRY_HOSTS
        .iter()
        .any(|h| "https://o1.ingest.sentry.io/api".contains(h)));
}
