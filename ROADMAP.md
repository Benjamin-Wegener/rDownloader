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
M0 Fundament ✅ ─► M1 Zuverlässiger Download-Kern ─► M2 Bedienbare TUI ─► M3 Plugins ─► M4 Release
                   (Tor-HTTP, Ranges, Retry,          (Live-Queue,         (statisch,    (CI, Binaries,
                    Resume, Persistenz)                Pause/Resume)        dann dyn.)    crates.io)
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

## M1 – Zuverlässiger Download-Kern 🟨 🔴
Reihenfolge ist Abhängigkeitsreihenfolge – von oben nach unten.

1. 🔴 **Tor-Transport entscheiden & fertigstellen**
   - [ ] Entscheidung treffen (siehe „Design-Entscheidungen“, Empfehlung: Hyper/`reqwest` über eigenen Connector auf dem Arti-Stream statt handgeschriebenem HTTP)
   - [ ] HEAD/GET komplett über Tor-Stream inkl. TLS für `https` (Arti liefert nur rohe TCP-Streams → TLS-Layer nötig)
   - [ ] Redirects, Chunked-Encoding und `Content-Disposition`-Dateiname korrekt behandeln
   - [ ] Eigener Header-Parser (`find_subsequence`) entfällt dadurch
2. ✅ **Range-Support-Prüfung** (`Accept-Ranges`, Test-GET `bytes=0-0`) → Fallback Single-Connection
3. 🔴 **Retries mit Backoff pro Part** (Part-Fehler darf nicht den ganzen Job killen)
4. 🔴 **Resume**: Part-Fortschritt persistieren (Offset je Part, ETag/Last-Modified zur Validierung)
5. 🟠 **Stream-Isolation pro Part** sicherstellen (kein Circuit-Sharing)
   - [ ] Prüfen, ob ein einzelner `TorClient` + `isolated_client()` pro Part statt 10 getrennter Clients reicht – spart 9× Bootstrap und Data-Dirs
6. 🟠 **Health-Check / Auto-Reconnect** bei Circuit-Failure (baut auf Retry auf)
7. 🟠 Checksummen-Verifikation (nur wenn Server/Plugin Hash liefert)
8. 🟢 Bandbreiten-Limit (globales Token-Bucket)

**Definition of Done M1:** 1-GB-Testdatei über Tor, Prozess mitten im Download killen, neu starten → Download setzt fort, Hash stimmt. Als Integrationstest automatisiert.

## M2 – Bedienbare TUI 🟨 🟠
Abhängig von M1 (Engine muss echten Fortschritt liefern).
- [ ] 🔴 Echtzeit-Update der Queue über Channel (`tokio::sync::watch`/`mpsc`) statt Stubs
- [ ] 🔴 Session-Restore: Tracker lädt offene Downloads beim Start und setzt sie fort (nutzt M1-Resume)
- [ ] 🟠 Steuerung pro Eintrag: Pause · Resume · Cancel · Remove · Ordner öffnen
- [ ] 🟠 Detail-Ansicht: Parts, Speed, ETA, Circuit-Infos
- [ ] 🟠 Batch-Import (mehrere URLs, Datei, Clipboard) → bildet die Basis des LinkGrabbers
- [ ] 🟠 History/Abgeschlossen-Liste mit Statistiken + Export (CSV/JSON)
- [ ] 🟢 Themes, globaler Clipboard-Hotkey

## M3 – Plugin-System 🟨 🟠
Statisch zuerst, dynamisch erst bei echtem Bedarf.
- [ ] 🟠 Beispiel-Hoster-Plugin (Redirect-Auflösung einer konkreten Site) – validiert, ob der Trait taugt
- [ ] 🟠 Crawler-Plugin-Typ (weitere Links auf einer Seite) → speist LinkGrabber
- [ ] 🟠 Account-Plugin-Typ (Login-Cookies, Session-Management)
- [ ] 🟠 Captcha-Plugin-Typ (interaktive Lösung in der TUI)
- [ ] 🟠 Berechtigungsmodell definieren (Netzwerk / Dateisystem), **bevor** dynamisches Laden gebaut wird
- [ ] 🟢 Dynamisches Laden (`cdylib`, stabile C-ABI oder WASM – siehe Design-Entscheidung 2)
- [ ] 🟢 Plugin-Marktplatz / signierte Auto-Updates

## M4 – Release 🟠
CI früh starten (parallel zu M1), Rest nach M2.
- [x] 🔴 **CI jetzt**: GitHub Actions mit `cargo fmt --check`, `clippy -D warnings`, `cargo test` (macOS + Linux)
- [ ] 🟠 Prebuilt-Binaries (macOS/Linux/Windows) via GitHub Releases
- [ ] 🟠 Manpage + `--help` (clap), README mit Quickstart
- [ ] 🟢 `cargo install` (crates.io), Homebrew-Tap / AUR / Scoop

## M5 – Härtung & Extras (parallel, nicht blockierend) 🟢
**Sicherheit/Privacy**
- [ ] 🟠 Threat-Modell-Doku: was Tor schützt / was nicht (Fingerprints, Endpoints, Exit-Sicht bei HTTP)
- [ ] 🟠 DNS-Leak-Prüfung (alle Auflösungen über Tor, auch in Plugins)
- [ ] 🟠 Keine Telemetrie – als Test/Grep-Check in CI absichern
- [ ] 🟢 Bridges / Pluggable Transports (obfs4, snowflake)
- [ ] 🟢 Erkennung eines laufenden System-`tor` (Ports nutzen statt eigene Clients)
- [ ] 🟢 Certificate-Pinning-Option

**Integrationen**
- [ ] 🟢 Watch-Ordner · RSS-Reader · Scheduler · Desktop-Notifications

## M6 – GUI (optional, erst nach M4) ⬜ 🟢
- [ ] Entscheidung egui vs. Tauri vs. nur TUI – erst entscheiden, wenn M2 steht und Nutzerfeedback vorliegt
- [ ] Tray-Integration, Browser-Extension-Brücke

---

## Design-Entscheidungen (mit Empfehlung)
1. **HTTP-Client** – *Empfehlung: Hyper/`reqwest` mit eigenem Connector über den Arti-Stream (+ `tokio-rustls`).* Kein Handparsing von Headern, trotzdem kein externer Tor-Daemon nötig. Das `socks`-Feature von reqwest wäre nur bei externem Tor sinnvoll und kann dann entfallen. **Offen bis Ende M1-Schritt 1.**
2. **Plugin-ABI** – *Empfehlung: statisch eingebettete Rust-Plugins (`Box<dyn Trait>`) bis M3 abgeschlossen; danach WASM-Sandbox statt `cdylib`.* WASM löst Sicherheit + ABI-Stabilität gleichzeitig, `cdylib` bräuchte `unsafe` und hat keine Isolation.
3. **GUI** – verschoben nach M6; TUI zuerst.

## Aufräumen in `Cargo.toml`
Beim Abschluss von M1 prüfen und entfernen, was ungenutzt ist (z. B. `tor-circmgr`, `tor-dirmgr`, `tor-rtcompat` falls nicht direkt importiert, `reqwest/socks`, `indicatif` neben ratatui). Kürzere Build-Zeit und kleinere Angriffsfläche.

## Bekannte Limitationen (ehrlich)
- JDs `.jar`-Plugins lassen sich clean-room **nicht** laden (Java + proprietäre API).
- Manche Hoster brauchen JS zum Link-Auflösen – ohne Browser-Engine nicht sauber lösbar; Workaround: vorberechnete Endpunkte oder manuelle Eingabe.
- Tor ist langsam – 10 Parallel-Circuits helfen nur, wenn der Hoster Ranges erlaubt und nicht der Guard der Flaschenhals ist. **Vor M2 messen**, ob 10 Parts gegenüber z. B. 4 überhaupt etwas bringen, und den Default entsprechend setzen.
- Arti hat noch keine volle C-Tor-Parität (Relay-Mode etc.); als Client produktionsreif (v2.7+).

---

*Zuletzt aktualisiert: 2026-10-21*
