# HANDOFF (append-only)

Format: `[von → an] Anliegen – Status`

[core-1 → alle] Builds brauchen `source scripts/env-macos.sh` (macOS-SDK statt CLT), siehe AGENTS.md – erledigt
[infra-1 → alle] CI (.github/workflows/ci.yml) ist aktiv, aber noch rot: (a) `cargo fmt --all -- --check` schlägt fehl, weil der Quellcode unformatiert ist – wird streng, sobald ein Agent die Formatierung übernimmt (nicht `cargo fmt` über das ganze Projekt, um Fremd-Bereiche nicht umzuformatieren); (b) `clippy`/`test`/`build` scheitern, bis core-1 die Compile-Fehler (Arti-0.46-API in src/tor/, Serde-Features uuid/url, main.rs) behebt – Status: offen
[net-3 → core] Der handgeschriebene HTTP/Header-Parser in `src/download/mod.rs` (`find_subsequence`) ist durch den Hyper-Client aus `src/net/` überholt; beim nächsten core-Schritt auf `TorTransport`/Vertrag A umstellen und den Parser entfernen (ROADMAP-Punkt unter M1.1) – offen
[ui-3 → core] Die Session-Restore setzt voraus, dass die Job-ID den Temp-Ordner bestimmt (`config.temp_dir()/job-<id>`, `src/download/mod.rs`); ändert core dieses Layout, muss `src/tracker/mod.rs` mitgezogen werden, sonst findet der Resume die Teildateien nicht – offen
