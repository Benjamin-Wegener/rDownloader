# rdownloader status

Generated 2026-10-22 at commit 233181a - regenerate with `scripts/status-md.sh`

`[#################---]` **59/67** steps done (88%), 2 in progress, 6 open
Roadmap items: 66 checked, 19 open

## core (16/19)

- [x] `core-9` Zentraler Dateinamen-Bereiniger fuer alle Quellen (Plugin/Container/URL): kein Pfad-Ausbruch, Windows-Reservenamen, Kollision -> automatisch umbenennen
- [x] `core-1` src/contract.rs: Verträge A (Transport) + B (JobEvent/JobCommand), MockTransport, FakeJob
- [x] `core-2` M1.2 Range-Prüfung (Accept-Ranges) + Single-Connection-Fallback
- [x] `core-3` M1.3 Retries mit Backoff pro Part
- [x] `core-4` M1.4 Resume (Part-Offsets persistieren, ETag/Last-Modified)
- [x] `core-5` M1.7 Checksummen-Verifikation
- [x] `core-6` M1.8 Bandbreiten-Limit
- [x] `core-7` Job-Scheduler & Per-Host-Limits (parallele Jobs, Hoster-Drosselung)
- [x] `core-8` Auto-Extract-Engine fuer Multi-Part-Archive (.rar, .zip, .7z)
- [x] `core-10` Freier-Speicher-Pruefung vor dem Start + Vorab-Reservierung der Zieldatei
- [x] `core-11` macOS-Quarantaene-Attribut (com.apple.quarantine) auf fertige Downloads setzen
- [x] `core-12` Langsame Verbindungen kappen (lowest-speed-limit) + bedingtes Laden (If-Modified-Since)
- [x] `core-13` Mirrors (mehrere URLs pro Datei, Auswahl nach Geschwindigkeit) + Metalink-Block-Pruefsummen
- [x] `core-14` Ereignis-Hooks (Start/Fertig/Fehler -> eigener Befehl) und Desktop-Benachrichtigung
- [x] `core-15` HLS/DASH in die Engine einbinden: Manifest-URLs (.m3u8/.mpd) an net::SegmentDownloader routen (Fortschritt, Abbruch, Zielendung .ts/.mp4), toten Parser find_subsequence entfernen
- [x] `core-16` Scheduler-Zeitfenster und zeitabhaengige Geschwindigkeitslimits
- [ ] `core-17` Watch-Ordner: Linklisten, .dlc und .metalink in einem Ordner werden importiert (Link-Sammler, nie direkt Download)
- [ ] `core-18` Download-Regeln: Zielordner und Umbenennen nach Dateityp, Host oder Muster
- [ ] `core-19` Duplikat-Erkennung (schon geladen per URL oder History) im Link-Sammler

## i18n (3/3)

- [x] `i18n-1` src/i18n.rs: String-Katalog (en), Schlüssel + benannte Platzhalter, Tests; Clap-Help darüber
- [x] `i18n-2` Bestehende deutsche Strings/Kommentare/Docs im Code auf Englisch, UI-Texte in den Katalog
- [x] `i18n-3` Deutsche Uebersetzung als Sprachpaket (lang/de.toml), Sprache per Config/Umgebung, Test auf Vollstaendigkeit aller Schluessel

## net (12/13)

- [x] `net-1` TLS über Arti-Stream (https-Connector, vertragsfrei)
- [x] `net-2` TorTransport implementiert Vertrag A
- [x] `net-3` M1.1 Redirects, Chunked, Content-Disposition
- [x] `net-4` M1.5 Stream-Isolation (Single-Client + isolated_client() prüfen)
- [x] `net-5` M1.6 Health-Check / Auto-Reconnect
- [x] `net-6` M5 DNS-Leak-Check, Bridges
- [x] `net-7` Headless-Daemon & RPC-API (JSON-RPC/REST-Steuerung)
- [x] `net-8` Onion-Dienste: .onion-URLs (Arti-Feature onion-service-client) + Vanguards
- [x] `net-9` FTP/SFTP ueber Tor
- [x] `net-10` HLS/DASH: Segment-Downloads (parallele Fragmente, Zusammenfuegen)
- [x] `net-12` Daemon-API absichern gegen fremde Webseiten (CSRF/DNS-Rebinding): Host-/Origin-/Content-Type-Pruefung ohne Token, Sicherheits-Header, Tests
- [x] `net-11` SFTP (SSH-Client mit Host-Key-Pruefung) und FTPS ueber Tor
- [~] `net-17` Live-Events im Daemon per Server-Sent Events - **in progress**

## plugins (8/8)

- [x] `plugins-1` Beispiel-Hoster-Plugin (Redirect-Auflösung)
- [x] `plugins-2` Crawler-Plugin-Typ
- [x] `plugins-3` Account-Plugin-Typ
- [x] `plugins-4` Captcha-Plugin-Typ (interaktive Lösung in TUI)
- [x] `plugins-5` Berechtigungsmodell (Design-Dokument)
- [x] `plugins-6` Container- & Listen-Import (.dlc, .ccf, Textlisten, .metalink)
- [x] `plugins-7` WASM-Plugin-Laufzeit (Wasmtime-Sandbox unter Berechtigungsmodell)
- [x] `plugins-8` netrc-Zugangsdaten + Cookie-Import (Firefox/Chromium/Netscape-Format)

## infra (8/8)

- [x] `infra-1` CI: GitHub Actions fmt, clippy -D warnings, test
- [x] `infra-2` Test-Harness: lokaler HTTP-Server mit Range-Support + Abbruch-Simulation
- [x] `infra-3` Integrationstest: Kill + Resume + Hash (Definition of Done M1)
- [x] `infra-4` Cargo.toml-Aufräumen (ungenutzte Dependencies)
- [x] `infra-5` Threat-Modell-Doku, README, Manpage
- [x] `infra-6` Release-Binaries (GitHub Releases)
- [x] `infra-7` Aufraeumen: ungenutzte Config connect_timeout_secs (umsetzen oder entfernen), irrefuehrende Tor-Log-Zeile (bootstrap 85%, healthy=false), README/Roadmap an den echten Stand anpassen
- [x] `infra-8` Paketierungsvorlagen: Cargo-Metadaten fuer crates.io, Homebrew-Formel, AUR-PKGBUILD, Scoop-Manifest (Veroeffentlichen von Hand)

## ui (7/7)

- [x] `ui-1` Queue-Rendering gegen FakeJob (Live-Updates über Vertrag B)
- [x] `ui-2` Steuerung pro Eintrag: Pause/Resume/Cancel/Remove
- [x] `ui-3` Session-Restore im Tracker
- [x] `ui-4` Detail-Ansicht (Parts, Speed, ETA, Circuit)
- [x] `ui-5` Batch-Import (Datei, Clipboard)
- [x] `ui-6` History + Export (CSV/JSON)
- [x] `ui-7` LinkGrabber-Workflow: Entkoppeltes Sammeln, Filtern & Bestaetigen

## gui (5/9)

- [x] `gui-1` Native GUI-Fenster (egui) mit derselben Engine, Standard wenn ein Display da ist; --tui fuer das Terminal
- [x] `gui-2` GUI-Rahmen nach docs/gui-layout.md: Toolbar, Reiter, Uebersichtspanel, Befehlsleiste, Fusszeile
- [x] `gui-3` GUI-Tabelle: Spalten (Name, Groesse, Host, Verbindung, Status, Tempo, Rest, Geladen, Fortschritt, Speicherort, Kommentar), Sortierung, Spaltenauswahl, Pakete im Datenmodell
- [x] `gui-4` GUI-Reiter Link-Sammler auf dem vorhandenen Sammler-Modell
- [x] `gui-5` GUI-Dialoge und Menues: Links-hinzufuegen-Dialog, Kontextmenue, Einstellungen, Dateiauswahl
- [~] `gui-6` App-Paket: macOS .app mit Icon, Windows/Linux-Pakete, Autostart - **in progress**
- [ ] `gui-7` Themes (hell, dunkel, automatisch, Akzentfarbe) und Einstellungsseite inkl. Sprachwahl
- [ ] `gui-8` Zwischenablage-Ueberwachung (opt-in) speist den Link-Sammler, nie direkt den Download
- [ ] `gui-9` Tray-Symbol, Autostart, Fenster in den Tray schliessen
