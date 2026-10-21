# rdownloader status

Generated 2026-10-22 at commit bd5dbe3 - regenerate with `scripts/status-md.sh`

`[###############-----]` **36/47** steps done (77%), 3 in progress, 8 open
Roadmap items: 42 checked, 27 open

## core (9/14)

- [x] `core-9` Zentraler Dateinamen-Bereiniger fuer alle Quellen (Plugin/Container/URL): kein Pfad-Ausbruch, Windows-Reservenamen, Kollision -> automatisch umbenennen
- [x] `core-1` src/contract.rs: Verträge A (Transport) + B (JobEvent/JobCommand), MockTransport, FakeJob
- [x] `core-2` M1.2 Range-Prüfung (Accept-Ranges) + Single-Connection-Fallback
- [x] `core-3` M1.3 Retries mit Backoff pro Part
- [x] `core-4` M1.4 Resume (Part-Offsets persistieren, ETag/Last-Modified)
- [x] `core-5` M1.7 Checksummen-Verifikation
- [x] `core-6` M1.8 Bandbreiten-Limit
- [x] `core-7` Job-Scheduler & Per-Host-Limits (parallele Jobs, Hoster-Drosselung)
- [x] `core-8` Auto-Extract-Engine fuer Multi-Part-Archive (.rar, .zip, .7z)
- [ ] `core-10` Freier-Speicher-Pruefung vor dem Start + Vorab-Reservierung der Zieldatei
- [ ] `core-11` macOS-Quarantaene-Attribut (com.apple.quarantine) auf fertige Downloads setzen
- [ ] `core-12` Langsame Verbindungen kappen (lowest-speed-limit) + bedingtes Laden (If-Modified-Since)
- [ ] `core-13` Mirrors (mehrere URLs pro Datei, Auswahl nach Geschwindigkeit) + Metalink-Block-Pruefsummen
- [ ] `core-14` Ereignis-Hooks (Start/Fertig/Fehler -> eigener Befehl) und Desktop-Benachrichtigung

## i18n (2/2)

- [x] `i18n-1` src/i18n.rs: String-Katalog (en), Schlüssel + benannte Platzhalter, Tests; Clap-Help darüber
- [x] `i18n-2` Bestehende deutsche Strings/Kommentare/Docs im Code auf Englisch, UI-Texte in den Katalog

## net (7/10)

- [x] `net-1` TLS über Arti-Stream (https-Connector, vertragsfrei)
- [x] `net-2` TorTransport implementiert Vertrag A
- [x] `net-3` M1.1 Redirects, Chunked, Content-Disposition
- [x] `net-4` M1.5 Stream-Isolation (Single-Client + isolated_client() prüfen)
- [x] `net-5` M1.6 Health-Check / Auto-Reconnect
- [x] `net-6` M5 DNS-Leak-Check, Bridges
- [x] `net-7` Headless-Daemon & RPC-API (JSON-RPC/REST-Steuerung)
- [~] `net-8` Onion-Dienste: .onion-URLs (Arti-Feature onion-service-client) + Vanguards - **in progress**
- [ ] `net-9` FTP/SFTP ueber Tor
- [ ] `net-10` HLS/DASH: Segment-Downloads (parallele Fragmente, Zusammenfuegen)

## plugins (6/8)

- [x] `plugins-1` Beispiel-Hoster-Plugin (Redirect-Auflösung)
- [x] `plugins-2` Crawler-Plugin-Typ
- [x] `plugins-3` Account-Plugin-Typ
- [x] `plugins-4` Captcha-Plugin-Typ (interaktive Lösung in TUI)
- [x] `plugins-5` Berechtigungsmodell (Design-Dokument)
- [x] `plugins-6` Container- & Listen-Import (.dlc, .ccf, Textlisten, .metalink)
- [~] `plugins-7` WASM-Plugin-Laufzeit (Wasmtime-Sandbox unter Berechtigungsmodell) - **in progress**
- [ ] `plugins-8` netrc-Zugangsdaten + Cookie-Import (Firefox/Chromium/Netscape-Format)

## infra (6/6)

- [x] `infra-1` CI: GitHub Actions fmt, clippy -D warnings, test
- [x] `infra-2` Test-Harness: lokaler HTTP-Server mit Range-Support + Abbruch-Simulation
- [x] `infra-3` Integrationstest: Kill + Resume + Hash (Definition of Done M1)
- [x] `infra-4` Cargo.toml-Aufräumen (ungenutzte Dependencies)
- [x] `infra-5` Threat-Modell-Doku, README, Manpage
- [x] `infra-6` Release-Binaries (GitHub Releases)

## ui (6/7)

- [x] `ui-1` Queue-Rendering gegen FakeJob (Live-Updates über Vertrag B)
- [x] `ui-2` Steuerung pro Eintrag: Pause/Resume/Cancel/Remove
- [x] `ui-3` Session-Restore im Tracker
- [x] `ui-4` Detail-Ansicht (Parts, Speed, ETA, Circuit)
- [x] `ui-5` Batch-Import (Datei, Clipboard)
- [x] `ui-6` History + Export (CSV/JSON)
- [~] `ui-7` LinkGrabber-Workflow: Entkoppeltes Sammeln, Filtern & Bestaetigen - **in progress**
