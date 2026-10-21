# rdownloader status

Generated 2026-10-22 at commit 04ee215 - regenerate with `scripts/status-md.sh`

`[###########---------]` **17/31** steps done (55%), 4 in progress, 10 open
Roadmap items: 19 checked, 24 open

## i18n (1/2)

- [x] `i18n-1` src/i18n.rs: String-Katalog (en), Schlüssel + benannte Platzhalter, Tests; Clap-Help darüber
- [ ] `i18n-2` Bestehende deutsche Strings/Kommentare/Docs im Code auf Englisch, UI-Texte in den Katalog

## core (6/6)

- [x] `core-1` src/contract.rs: Verträge A (Transport) + B (JobEvent/JobCommand), MockTransport, FakeJob
- [x] `core-2` M1.2 Range-Prüfung (Accept-Ranges) + Single-Connection-Fallback
- [x] `core-3` M1.3 Retries mit Backoff pro Part
- [x] `core-4` M1.4 Resume (Part-Offsets persistieren, ETag/Last-Modified)
- [x] `core-5` M1.7 Checksummen-Verifikation
- [x] `core-6` M1.8 Bandbreiten-Limit

## net (4/6)

- [x] `net-1` TLS über Arti-Stream (https-Connector, vertragsfrei)
- [x] `net-2` TorTransport implementiert Vertrag A
- [x] `net-3` M1.1 Redirects, Chunked, Content-Disposition
- [x] `net-4` M1.5 Stream-Isolation (Single-Client + isolated_client() prüfen)
- [~] `net-5` M1.6 Health-Check / Auto-Reconnect - **in progress**
- [ ] `net-6` M5 DNS-Leak-Check, Bridges

## plugins (3/5)

- [x] `plugins-1` Beispiel-Hoster-Plugin (Redirect-Auflösung)
- [x] `plugins-2` Crawler-Plugin-Typ
- [x] `plugins-3` Account-Plugin-Typ
- [~] `plugins-4` Captcha-Plugin-Typ (interaktive Lösung in TUI) - **in progress**
- [ ] `plugins-5` Berechtigungsmodell (Design-Dokument)

## infra (1/6)

- [x] `infra-1` CI: GitHub Actions fmt, clippy -D warnings, test
- [~] `infra-2` Test-Harness: lokaler HTTP-Server mit Range-Support + Abbruch-Simulation - **in progress**
- [ ] `infra-3` Integrationstest: Kill + Resume + Hash (Definition of Done M1)
- [ ] `infra-4` Cargo.toml-Aufräumen (ungenutzte Dependencies)
- [ ] `infra-5` Threat-Modell-Doku, README, Manpage
- [ ] `infra-6` Release-Binaries (GitHub Releases)

## ui (2/6)

- [x] `ui-1` Queue-Rendering gegen FakeJob (Live-Updates über Vertrag B)
- [x] `ui-2` Steuerung pro Eintrag: Pause/Resume/Cancel/Remove
- [~] `ui-3` Session-Restore im Tracker - **in progress**
- [ ] `ui-4` Detail-Ansicht (Parts, Speed, ETA, Circuit)
- [ ] `ui-5` Batch-Import (Datei, Clipboard)
- [ ] `ui-6` History + Export (CSV/JSON)
