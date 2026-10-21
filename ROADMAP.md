# rdownloader – Roadmap

Clean-room Rust Download-Manager mit Tor (Arti), Multi-Part, eigenem Plugin-System & TUI.
Inspiriert von JDownloader (Konzept/UX), **kein Code übernommen** – alles von Grund auf in Rust.

> **Warum kein „echtes“ JDownloader-Plugin?**
> JDownloader-Plugins sind Java-`.jar`s, gebunden an JDownloads interne, nicht-public API.
> Eine clean-room Rust-Implementierung kann diese nicht laden, ohne JVM + proprietäre Laufzeit.
> Stattdessen: **eigenes, sicheres Plugin-System** mit gleichem Konzept (Hoster-Resolver, Captcha, Accounts, Crawler).

**Legende:** ⬜ offen · 🟨 in Arbeit · ✅ fertig · ⛔ verworfen
**Priorität:** 🔴 Blocker für MVP · 🟠 wichtig · 🟢 Nice-to-have

---

## Reihenfolge auf einen Blick

```
M0 Fundament ✅ ─► M1 Zuverlässiger Download-Kern ✅ ─► M2 Bedienbare TUI ─► M3 Plugins ─► M4 Release
                   (Tor-HTTP, Ranges, Retry,            (Live-Queue,           (statisch,    (CI, Binaries,
                    Resume, Persistenz)                  Pause/Resume, Grabber) dann WASM)    crates.io)
                                                                                  ▲
                                            M5 Härtung & Extras (parallel ab M2) ─┘
```

Regel: Nichts aus M3+ anfangen, bevor M1 ein Testdownload mit Abbruch + Resume besteht.
Der Kern (M1) ist das Risiko; Plugins/GUI sind ohne ihn wertlos.

---

## M0 – Fundament ✅
- [x] Rust-Toolchain, Cargo-Projekt, Modulstruktur (`tor/`, `download/`, `plugin/`, `tracker/`, `ui/`, `config.rs`)
- [x] Zentrale Config (TOML, `~/.config/rdownloader/config.toml`)
- [x] Tracker-Store (JSON) mit Upsert + Status-Updates
- [x] Erste Multi-Part-Engine: HEAD, Range-Splitting (bis 10), Temp-Parts + Merge
- [x] `DownloadPlugin`-Trait, `PluginRegistry`, `DirectDownloadPlugin`
- [x] TUI-Grundgerüst: Tabs, Queue mit Farben/Gauge, `a`/`q`/Tab/↑↓
- [x] rustls statt OpenSSL

## M1 – Zuverlässiger Download-Kern ✅ 🔴
Reihenfolge ist Abhängigkeitsreihenfolge – von oben nach unten.

1. 🔴 **Tor-Transport entscheiden & fertigstellen**
   - [x] Entscheidung treffen (Hyper-Client über eigenen Connector auf dem Arti-Stream)
   - [x] HEAD/GET komplett über Tor-Stream inkl. TLS für `https` (Arti liefert rohe TCP-Streams → TLS-Layer via `tokio-rustls`)
   - [x] Redirects, Chunked-Encoding und `Content-Disposition`-Dateiname korrekt behandeln
   - [x] HEAD/GET-Routing komplett über `TorTransport`
2. ✅ **Range-Support-Prüfung** (`Accept-Ranges`, Test-GET `bytes=0-0`) → Fallback Single-Connection
3. [x] 🔴 **Retries mit Backoff pro Part** (Part-Fehler darf nicht den ganzen Job killen)
4. [x] 🔴 **Resume**: Part-Fortschritt persistieren (Offset je Part, ETag/Last-Modified zur Validierung)
5. 🟠 **Stream-Isolation pro Part** sicherstellen (kein Circuit-Sharing)
   - [x] Ein `TorClient` + `IsolationToken` je Part – spart Bootstrap & Data-Dirs
6. [x] 🟠 **Health-Check / Auto-Reconnect** bei Circuit-Failure – `TorManager::health()` liest Arti-Bootstrap-Status; Part-Retries rotieren Circuits
7. ✅ Checksummen-Verifikation (nur wenn Server/Plugin Hash liefert)
8. ✅ Bandbreiten-Limit (globales Token-Bucket)

- [x] **Definition of Done M1:** 1-GB-Testdatei über Tor, Prozess mitten im Download killen, neu starten → Download setzt fort, Hash stimmt. Als Integrationstest automatisiert (`tests/integration_kill_resume_hash.rs`).

## Querschnitt: Englisch + Übersetzbarkeit
- [x] 🟠 `src/i18n.rs`: String-Katalog (Standard `en`, Schlüssel + benannte Platzhalter), später Sprachpakete (i18n-1)
- [x] 🟠 Bestehende deutsche Texte in Code, Hilfe und UI auf Englisch umstellen (i18n-2)

## M2 – Bedienbare TUI 🟨 🟠
Abhängig von M1 (Engine liefert echten Fortschritt).
- [x] 🔴 Echtzeit-Update der Queue über Channel (`mpsc`)
- [x] 🔴 Session-Restore: Tracker lädt offene Downloads beim Start und setzt sie fort (nutzt M1-Resume)
- [x] 🟠 Steuerung pro Eintrag: Pause · Resume · Cancel · Remove · Ordner öffnen
- [x] 🟠 Detail-Ansicht: Parts, Speed, ETA, Circuit-Infos
- [x] 🟠 Batch-Import (mehrere URLs, Datei, Clipboard) → bildet die Basis des LinkGrabbers
- [x] 🟠 History/Abgeschlossen-Liste mit Statistiken + Export (CSV/JSON) – Tab „History" mit den beendeten Downloads (Dauer, Ø-Geschwindigkeit, Status), Export per Taste als CSV/JSON in das Data-Verzeichnis
- [ ] 🟠 **LinkGrabber-Workflow**: Entkoppeltes Sammeln $\to$ Paketieren/Filtern (Hoster, Dateityp) $\to$ Bestätigen & Übertrag in Download-Queue
- [x] 🟠 **Container- & Listen-Import**: `.dlc`, `.ccf`, Text-Linklisten und `.metalink`
- [ ] 🟢 Themes, globaler Clipboard-Hotkey

## M3 – Plugin-System 🟨 🟠
Statisch zuerst, dynamisch unter Sandboxing.
- [x] 🟠 Beispiel-Hoster-Plugin (Redirect-Auflösung einer konkreten Site) – validiert, ob der Trait taugt
- [x] 🟠 Crawler-Plugin-Typ (weitere Links auf einer Seite) → speist LinkGrabber
- [x] 🟠 Account-Plugin-Typ (Login-Cookies, Session-Management)
- [x] 🟠 Captcha-Plugin-Typ (interaktive Lösung in der TUI)
- [x] 🟠 Berechtigungsmodell definieren (Netzwerk / Dateisystem), **bevor** dynamisches Laden gebaut wird (Design: `src/plugin/PERMISSIONS.md`, Code: `src/plugin/permissions.rs`)
- [ ] 🟠 **WASM-Plugin-Laufzeit (Wasmtime/Wasmi)**: Dynamisches Laden von `.wasm`-Plugins zur Laufzeit mit strikter Capability-Sandbox
- [ ] 🟢 Plugin-Marktplatz / signierte Auto-Updates

## M4 – Release 🟨 🟠
CI früh starten (parallel zu M1), Rest nach M2.
- [x] 🔴 **CI jetzt**: GitHub Actions mit `cargo fmt --check`, `clippy -D warnings`, `cargo test` (macOS + Linux)
- [x] 🟠 Prebuilt-Binaries (macOS/Linux/Windows) via GitHub Releases (`.github/workflows/release.yml`)
- [x] 🟠 Manpage + `--help` (clap), README mit Quickstart (Threat-Modell: docs/threat-model.md)
- [ ] 🟢 `cargo install` (crates.io), Homebrew-Tap / AUR / Scoop

## M5 – Härtung, Automation & Headless (parallel) 🟢
**Sicherheit/Privacy**
- [x] 🟠 Threat-Modell-Doku: was Tor schützt / was nicht (`docs/threat-model.md`)
- [x] 🟠 DNS-Leak-Prüfung (alle Auflösungen über Tor, auch in Plugins) – Guard-Test verbietet lokale Resolver im Code
- [ ] 🟠 Keine Telemetrie – als Test/Grep-Check in CI absichern
- [ ] 🟢 Bridges / Pluggable Transports (obfs4, snowflake)
  - [x] Direkte Bridge-Lines (Config-Feld `bridges`, in Arti geladen; `Bridge `-Präfix optional)
  - [ ] Pluggable Transports (obfs4, snowflake) – braucht `pt-client`-Feature plus externe PT-Binaries
- [ ] 🟢 Erkennung eines laufenden System-`tor` (Ports nutzen statt eigene Clients)
- [ ] 🟢 Certificate-Pinning-Option

**Automation, Limits & Post-Processing**
- [x] 🟠 **Auto-Extract Engine**: Automatisches Entpacken von Multi-Part-Archiven (`.rar`, `.zip`, `.7z`) nach Download & Hash-Verifikation
- [x] 🟠 **Job-Scheduler & Per-Host Limits**: Max. gleichzeitige Jobs und parallele Jobs pro Hoster (`max_concurrent_jobs`, `max_jobs_per_host`)
- [ ] 🟢 Scheduler-Zeitfenster (Downloads nur zu bestimmten Uhrzeiten)
- [ ] 🟢 Watch-Ordner · RSS-Reader · Desktop-Notifications

## M6 – GUI & Headless RPC (optional, nach M4) ⬜ 🟢
- [ ] 🟠 **Headless Daemon & RPC/Web-API**: JSON-RPC / REST / WebSocket für Fernsteuerung ohne TUI
- [ ] 🟢 GUI-Entscheidung: egui vs. Tauri vs. Browser-WebUI
- [ ] 🟢 Tray-Integration, Browser-Extension-Brücke

---

## Design-Entscheidungen (mit Empfehlung)
1. **HTTP-Client** – *Hyper/`tokio-rustls` über Arti-Stream.* Eigener Hyper-Client auf dem Arti-Stream, TLS via `tokio-rustls` (`src/net/`).
2. **Plugin-ABI** – *WASM-Sandbox (`wasm32-wasip1`) statt `cdylib`.* Löst Sicherheit (Capability-Whitelisting) + ABI-Stabilität. Design fertig in `src/plugin/PERMISSIONS.md`.
3. **GUI vs. Daemon** – TUI + headless Daemon mit RPC-Schnittstelle zuerst; native Desktop-GUI nach Nutzerfeedback.

## Aufräumen in `Cargo.toml`
Beim Abschluss von M1 prüfen und entfernen, was ungenutzt ist.
Erledigt (infra-4): `reqwest` (inkl. `socks`), `indicatif` und `thiserror` entfernt, `tokio` auf genutzte Features reduziert. Dependency-Baum: 602 → 579 Pakete.

## Bekannte Limitationen (ehrlich)
- JDs `.jar`-Plugins lassen sich clean-room **nicht** laden (Java + proprietäre API).
- Manche Hoster brauchen JS zum Link-Auflösen – ohne Browser-Engine nicht sauber lösbar; Workaround: vorberechnete Endpunkte oder manuelle Eingabe.
- Tor ist langsam – 10 Parallel-Circuits helfen nur, wenn der Hoster Ranges erlaubt und nicht der Guard der Flaschenhals ist.
- Arti hat noch keine volle C-Tor-Parität (Relay-Mode etc.); als Client produktionsreif (v2.7+).

---

*Zuletzt aktualisiert: 2026-10-22*
