# HANDOFF (append-only)

Format: `[von → an] Anliegen – Status`

[core-1 → alle] Builds brauchen `source scripts/env-macos.sh` (macOS-SDK statt CLT), siehe AGENTS.md – erledigt
[infra-1 → alle] CI (.github/workflows/ci.yml) ist aktiv, aber noch rot: (a) `cargo fmt --all -- --check` schlägt fehl, weil der Quellcode unformatiert ist – wird streng, sobald ein Agent die Formatierung übernimmt (nicht `cargo fmt` über das ganze Projekt, um Fremd-Bereiche nicht umzuformatieren); (b) `clippy`/`test`/`build` scheitern, bis core-1 die Compile-Fehler (Arti-0.46-API in src/tor/, Serde-Features uuid/url, main.rs) behebt – Status: offen
