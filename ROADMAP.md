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
M0 Fundament ✅ ─► M1 Zuverlässiger Download-Kern ✅ ─► M2 Bedienbare TUI ✅* ─► M3 Plugins ✅* ─► M4 Release 🟨
                   (Tor-HTTP, Ranges, Retry,            (Live-Queue,           (statisch,    (CI, Binaries,
                    Resume, Persistenz)                  Pause/Resume, Grabber) dann WASM)    crates.io)
                                                                                  ▲
                                            M5 Härtung & Extras (parallel ab M2) ─┘
```

`✅*` = alles bis auf Nice-to-have-Punkte (🟢) erledigt. Aktueller Stand pro Schritt: [STATUS.md](STATUS.md).

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

## M2 – Bedienbare TUI ✅ 🟠
Abhängig von M1 (Engine liefert echten Fortschritt).
- [x] 🔴 Echtzeit-Update der Queue über Channel (`mpsc`)
- [x] 🔴 Session-Restore: Tracker lädt offene Downloads beim Start und setzt sie fort (nutzt M1-Resume)
- [x] 🟠 Steuerung pro Eintrag: Pause · Resume · Cancel · Remove · Ordner öffnen
- [x] 🟠 Detail-Ansicht: Parts, Speed, ETA, Circuit-Infos
- [x] 🟠 Batch-Import (mehrere URLs, Datei, Clipboard) → bildet die Basis des LinkGrabbers
- [x] 🟠 History/Abgeschlossen-Liste mit Statistiken + Export (CSV/JSON) – Tab „History" mit den beendeten Downloads (Dauer, Ø-Geschwindigkeit, Status), Export per Taste als CSV/JSON in das Data-Verzeichnis
- [x] 🟠 **LinkGrabber-Workflow**: Entkoppeltes Sammeln $\to$ Paketieren/Filtern (Hoster, Dateityp) $\to$ Bestätigen & Übertrag in Download-Queue
- [x] 🟠 **Container- & Listen-Import**: `.dlc`, `.ccf`, Text-Linklisten und `.metalink`
- [ ] 🟢 Themes, globaler Clipboard-Hotkey

## M3 – Plugin-System ✅ 🟠
Statisch zuerst, dynamisch unter Sandboxing.
- [x] 🟠 Beispiel-Hoster-Plugin (Redirect-Auflösung einer konkreten Site) – validiert, ob der Trait taugt
- [x] 🟠 Crawler-Plugin-Typ (weitere Links auf einer Seite) → speist LinkGrabber
- [x] 🟠 Account-Plugin-Typ (Login-Cookies, Session-Management)
- [x] 🟠 Captcha-Plugin-Typ (interaktive Lösung in der TUI)
- [x] 🟠 Berechtigungsmodell definieren (Netzwerk / Dateisystem), **bevor** dynamisches Laden gebaut wird (Design: `src/plugin/PERMISSIONS.md`, Code: `src/plugin/permissions.rs`)
- [x] 🟠 **WASM-Plugin-Laufzeit (Wasmtime/Wasmi)**: Dynamisches Laden von `.wasm`-Plugins zur Laufzeit mit strikter Capability-Sandbox
- [ ] 🟢 Plugin-Marktplatz / signierte Auto-Updates

## M4 – Release 🟨 🟠
CI früh starten (parallel zu M1), Rest nach M2.
- [x] 🔴 **CI jetzt**: GitHub Actions mit `cargo fmt --check`, `clippy -D warnings`, `cargo test` (macOS + Linux)
- [x] 🟠 Prebuilt-Binaries (macOS/Linux/Windows) via GitHub Releases (`.github/workflows/release.yml`)
- [x] 🟠 Manpage + `--help` (clap), README mit Quickstart (Threat-Modell: docs/threat-model.md)
- [ ] 🟠 **v0.1.0 veröffentlichen**: Tag `v0.1.0` liegt lokal; privates Repo `Benjamin-Wegener/rDownloader` ist angelegt, der Push scheitert am fehlenden `workflow`-Scope des GitHub-Tokens. Danach Repo auf öffentlich stellen
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
- [ ] 🟢 Watch-Ordner · RSS-Reader

## Lücken aus der Recherche (2026-10-22)
Abgleich mit aria2, JDownloader, yt-dlp und der Arti-Dokumentation. Reihenfolge = Priorität.

**Sicherheit**
- [x] 🔴 **Zentraler Dateinamen-Bereiniger** für alle Quellen: Namen aus Plugins, `.dlc`/`.metalink` und URL werden ungeprüft an den Download-Ordner gehängt (`main.rs`, `dl_dir.join(&filename)`); nur `Content-Disposition` wird bereinigt. Dazu Windows-Reservenamen und automatisches Umbenennen bei Kollision (core-9)
- [x] 🟠 macOS-Quarantäne-Attribut `com.apple.quarantine` auf fertige Downloads, damit Gatekeeper sie prüft (core-11)
- [x] 🟠 Freien Speicher vor dem Start prüfen und Zieldatei vorab reservieren (core-10)

**Tor**
- [x] 🟠 `.onion`-URLs: Arti-Feature `onion-service-client` aktiv, mit Live-Test gegen einen echten Onion-Dienst (net-8)
- [x] 🟢 Vanguards (Schutz gegen Guard-Entdeckung): Arti-Feature `vanguards` aktiviert; Modus ist Arti-Standard (net-8)

**Download-Kern**
- [x] 🟠 Langsame Verbindungen kappen und neu aufbauen (`lowest-speed-limit`-Prinzip), bei Tor besonders nützlich (core-12)
- [x] 🟢 Bedingtes Laden (Vergleich mit `Last-Modified` aus dem Größen-Request, `conditional_get`) (core-12)
- [x] 🟠 Mirrors: mehrere URLs pro Datei (Verteilung auf die Parts, Auswahl nach gemessener Geschwindigkeit, Ausfall-/Falsch-Erkennung); Metalink-Stückprüfsummen mit Reparatur defekter Stücke (core-13)
- [x] 🟢 Ereignis-Hooks bei Start/Fertig/Fehler (ohne Shell, Umgebungsvariablen, Zeitlimit) und Desktop-Benachrichtigung (macOS/Linux) (core-14)
- [x] 🟢 `~/.netrc`-Zugangsdaten und Cookie-Import aus Browsern (Firefox/Chromium/Netscape, plugins-8)

**Protokolle & Inhalte**
- [x] 🟢 FTP über Tor (unverschlüsselt; Passiv-Modus, Resume, pro Part eigene Isolation) (net-9)
- [ ] 🟢 SFTP über Tor (SSH-Client, Host-Key-Prüfung) und FTPS (net-11)
- [ ] 🟢 HLS/DASH-Segment-Downloads, evtl. über die Rust-Crate `vsd` oder einen yt-dlp-Wrapper (net-10)
- [ ] ⛔ BitTorrent/Magnet: bewusst nicht im Umfang (über Tor unüblich, anderes Bedrohungsmodell)

## M6 – GUI & Headless RPC (optional, nach M4) ⬜ 🟢
- [x] 🟠 **Headless Daemon & RPC/Web-API**: JSON-RPC 2.0 + REST für Fernsteuerung ohne TUI (`src/net/rpc.rs`, `--daemon`)
  - [x] Queue-Liste, Status, Hinzufügen, Pause/Resume/Cancel/Remove über `POST /rpc` und `/api/jobs`
  - [x] Optionaler Bearer-Token (`rpc_token`), Loopback-Bindung ohne Token erzwungen
  - [ ] WebSocket-Unterstützung für Live-Events
- [ ] 🟢 GUI-Entscheidung: egui vs. Tauri vs. Browser-WebUI
- [ ] 🟢 Tray-Integration, Browser-Extension-Brücke

---

## Verifikationsstand (2026-10-22)
Was automatisch geprüft ist und was noch **nicht** am echten System gesehen wurde.

**Geprüft (CI-relevant):** `cargo fmt --check`, `clippy -D warnings`, 240 Unit-Tests + Integrationstests (Harness-Server, Kill + Resume + Hash), Release-Build (macOS arm64). Die Engine ist Ende-zu-Ende gegen den `MockTransport` getestet (Mehrteiler, Server ohne Ranges, Verbindungsabbruch mit Retry, zu kurzer Body). FTP läuft gegen einen lokalen Test-Server.

**Live geprüft:** HEAD auf einen echten `.onion`-Dienst über Tor (`tests/live_onion.rs`, einmalig, 231 s inkl. Bootstrap).

**Noch nicht live geprüft:**
- [ ] 🔴 Kompletter Download (HTTPS, mehrere Parts) über das echte Tor-Netz mit dem Release-Binary. Der Versuch am 2026-10-22 scheiterte schon im Tor-Bootstrap (Verbindungen zu den Verzeichnisservern liefen in Timeouts bzw. wurden abgelehnt) – am Netz der Testumgebung, nicht nachweislich am Code. Die Engine wurde zuvor auf den `Transport`-Vertrag umgebaut (Parts liefen vorher ohne TLS), deshalb ist dieser Test wichtig
- [ ] 🟠 FTP gegen einen echten Server über Tor
- [ ] 🟠 RAR-Entpacken mit `unrar`/`7zz` (auf der Entwicklungsmaschine ist keins installiert)
- [ ] 🟠 TUI im echten Terminal: Detail-Ansicht, Import (`i`/`v`), LinkGrabber
- [ ] 🟠 Linux- und Windows-Build sowie ein echter CI-Lauf auf GitHub (bisher nur lokal auf macOS)
- [ ] 🟢 Kein Telemetrie-Test in CI (siehe M5)

---

## Design-Entscheidungen (mit Empfehlung)
1. **HTTP-Client** – *Hyper/`tokio-rustls` über Arti-Stream.* Eigener Hyper-Client auf dem Arti-Stream, TLS via `tokio-rustls` (`src/net/`).
2. **Plugin-ABI** – *WASM-Sandbox (wasmi, ohne WASI-Imports) statt `cdylib`.* Löst Sicherheit (Capability-Whitelisting) + ABI-Stabilität. Umsetzung in `src/plugin/wasm.rs`, Design in `src/plugin/PERMISSIONS.md`.
3. **GUI vs. Daemon** – TUI + headless Daemon mit RPC-Schnittstelle zuerst; native Desktop-GUI nach Nutzerfeedback.

## Aufräumen in `Cargo.toml`
Beim Abschluss von M1 prüfen und entfernen, was ungenutzt ist.
Erledigt (infra-4): `reqwest` (inkl. `socks`), `indicatif` und `thiserror` entfernt, `tokio` auf genutzte Features reduziert. Dependency-Baum: 602 → 579 Pakete.

## Bekannte Limitationen (ehrlich)
- JDs `.jar`-Plugins lassen sich clean-room **nicht** laden (Java + proprietäre API).
- Manche Hoster brauchen JS zum Link-Auflösen – ohne Browser-Engine nicht sauber lösbar; Workaround: vorberechnete Endpunkte oder manuelle Eingabe.
- Tor ist langsam – 10 Parallel-Circuits helfen nur, wenn der Hoster Ranges erlaubt und nicht der Guard der Flaschenhals ist.
- FTP (`ftp://`) ist unverschlüsselt: Login und Daten sind für das Exit-Relay lesbar. SFTP/FTPS fehlen noch.
- `--no-tor` startet nur die Oberfläche; Downloads laufen ohne Tor bewusst nicht (kein direkter Fallback).
- Arti hat noch keine volle C-Tor-Parität (Relay-Mode etc.); als Client produktionsreif (v2.7+).

---

*Zuletzt aktualisiert: 2026-10-22 (Gesamtprüfung)*
