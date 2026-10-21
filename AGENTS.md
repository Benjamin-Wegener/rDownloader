# Orchestrierung – 5 Coding-Agents

Quelle der Wahrheit für *was*: [ROADMAP.md](ROADMAP.md). Dieses Dokument regelt *wer, wo, in welcher Reihenfolge*.

## Grundregeln
1. **Ein Agent = eine Lane = ein Verzeichnis-Besitz.** Dateien außerhalb der eigenen Lane nicht ändern; stattdessen Änderungswunsch in `HANDOFF.md` eintragen.
2. **Eigener Git-Worktree + Branch pro Agent** (`lane/<name>`), z. B. `git worktree add ../rdl-core lane/core`. Kein Arbeiten im selben Checkout.
3. **Claimen in der Roadmap:** Beim Start einer Aufgabe `[ ]` → `[~ @lane]`, bei Abschluss `[x]`. Nur die eigenen Zeilen anfassen (vermeidet Merge-Konflikte).
4. **Kleine PRs/Merges** in `master`, je Roadmap-Punkt einer. Vor Merge: `cargo fmt`, `cargo clippy -D warnings`, `cargo test` grün.
5. **Interfaces zuerst.** Lanes sprechen nur über die Verträge unten, nie über Interna anderer Lanes.
6. Cargo.toml und `main.rs` sind Konfliktstellen: Änderungen nur klein, früh mergen, sofort rebasen.

## Lanes

| Lane | Besitz | Roadmap-Punkte | Startet |
|---|---|---|---|
| **net** | `src/tor/`, `src/net/` (neu) | M1.1 Tor-Transport + TLS, M1.5 Isolation, M1.6 Reconnect, M5 DNS/Bridges | sofort |
| **core** | `src/download/` | M1.2 Ranges, M1.3 Retry, M1.4 Resume, M1.7 Hash, M1.8 Limit | nach Vertrag A (siehe unten), davor gegen Mock |
| **ui** | `src/ui/`, `src/tracker/`, `main.rs` (nur UI-Verdrahtung) | M2 komplett | nach Vertrag B, davor gegen Fake-Events |
| **plugins** | `src/plugin/`, `plugins/`, `examples/` | M3 (statisch) | sofort |
| **infra** | `.github/`, `tests/`, `docs/`, `Cargo.toml`-Aufräumen | M4 CI, Integrationstest-Harness (lokaler Range-Server), Threat-Modell, README/Manpage | sofort |

## Verträge (Schritt 0 – vor allem anderen, ca. 1 Agent, <1 Stunde)
Diese zwei Schnittstellen als Trait/Typen in `src/contract.rs` festlegen und nach `master` mergen. Erst danach arbeiten die abhängigen Lanes parallel.

- **Vertrag A – Transport (net → core):**
  `trait Transport { async fn head(&self, url) -> Meta; async fn get_range(&self, url, range, isolation: PartId) -> ByteStream; }`
  `Meta { size, accepts_ranges, etag, last_modified, filename }`
  core entwickelt gegen eine `MockTransport`-Implementierung, net liefert `TorTransport`.
- **Vertrag B – Fortschritt (core → ui):**
  `JobEvent { id, state, bytes_done, bytes_total, parts: Vec<PartState>, speed }` über `tokio::sync::watch`/`mpsc`; Steuerung rückwärts über `JobCommand { Pause, Resume, Cancel }`.
  ui entwickelt gegen einen `FakeJob`-Generator.

Vorschlag: **infra** schreibt Schritt 0 nicht, sondern **core** (kennt die Engine am besten); net/ui/plugins starten parallel mit ihren vertragsfreien Teilen.

## Phasen & Synchronisationspunkte
1. **Welle 0:** Verträge mergen (core). Parallel: infra setzt CI + Test-Server auf, plugins arbeitet am Hoster-Beispiel, net beginnt TLS-über-Arti.
2. **Welle 1:** Alle fünf parallel gegen die Verträge. Sync-Punkt: `TorTransport` + core-Resume laufen zusammen → **Definition of Done M1** (Kill + Resume + Hash) als Integrationstest von infra.
3. **Welle 2:** ui verdrahtet echte Events; plugins hängt Crawler/Account/Captcha an LinkGrabber (ui-Hook über `HANDOFF.md` abstimmen); infra: Binaries + Doku.
4. **Review-Regel:** Jeder Merge wird von einer *anderen* Lane per `/code-review` geprüft (net↔core, ui↔plugins, infra reviewt Rest).

## HANDOFF.md
Eine Datei, Append-only, Format: `[von-Lane → an-Lane] Anliegen – Status`. Blockiert eine Lane, trägt sie dort ein und wechselt zur nächsten freien Aufgabe der eigenen Lane.

## "Mach den nächsten Schritt" – Protokoll (gilt für jeden Agent)
Der Nutzer öffnet nur den Worktree-Ordner und sagt z. B. „mach den nächsten Schritt“. Dann:
1. **Lane bestimmen:** `git branch --show-current` → `lane/<name>` ergibt deine Lane. Keine `lane/…`-Branch → nachfragen.
2. **Sync:** `git fetch`/`git merge master` (bzw. `git merge master` im Worktree), damit Verträge und fremde Merges da sind. Danach `HANDOFF.md` auf Einträge an deine Lane prüfen.
3. **Schritt wählen:** Der erste Eintrag deiner Lane-Queue unten, der in der Roadmap noch `[ ]` ist und dessen „braucht“-Bedingung erfüllt ist. Ist keiner startbar → `HANDOFF.md` lesen/schreiben und den nächsten startbaren nehmen; gibt es keinen, dem Nutzer sagen, worauf du wartest.
4. **Claimen** in ROADMAP.md (`[~ @lane]`), implementieren mit Tests, `cargo fmt`, `cargo clippy -D warnings`, `cargo test`.
5. **Abschluss:** Roadmap-Zeile auf `[x]`, committen auf dem Lane-Branch, nach `master` mergen (Fast-Forward nach Rebase; bei Konflikt nicht raten, `HANDOFF.md`). Dem Nutzer in 2–3 Sätzen berichten: was fertig, was als Nächstes dran ist.
6. Immer nur **einen** Schritt pro Aufruf, dann stoppen.

## Lane-Queues (Reihenfolge = Priorität)
**core** – 1. `src/contract.rs` (Verträge A+B) + `MockTransport` + `FakeJob`, nach master mergen · 2. M1.2 Range-Prüfung/Fallback (braucht: 1) · 3. M1.3 Retry/Backoff · 4. M1.4 Resume · 5. M1.7 Hash (braucht: Meta.etag/Hash) · 6. M1.8 Bandbreiten-Limit
**net** – 1. TLS über Arti-Stream (https-Connector, vertragsfrei) · 2. `TorTransport` implementiert Vertrag A (braucht: core 1) · 3. M1.1 Redirects/Chunked/Content-Disposition · 4. M1.5 Isolation (Single-Client + `isolated_client()` prüfen) · 5. M1.6 Health-Check/Reconnect · 6. M5 DNS-Leak-Check, Bridges
**ui** – 1. Queue-Rendering gegen `FakeJob` (braucht: core 1) · 2. Steuerung Pause/Resume/Cancel · 3. Session-Restore im Tracker (braucht: core 4) · 4. Detail-Ansicht · 5. Batch-Import · 6. History + Export · 7. Themes/Hotkey
**plugins** – 1. Beispiel-Hoster-Plugin (Redirect-Auflösung) · 2. Crawler-Plugin-Typ · 3. Account-Plugin-Typ · 4. Captcha-Plugin-Typ · 5. Berechtigungsmodell (Design-Dokument) · 6. dynamisches Laden/WASM (nur nach Freigabe durch den Nutzer)
**infra** – 1. CI (fmt, clippy, test) · 2. Test-Harness: lokaler HTTP-Server mit Range-Support, Abbruch-Simulation · 3. Integrationstest „Kill + Resume + Hash“ (braucht: core 4, net 2) · 4. Cargo.toml-Aufräumen (nach M1) · 5. Threat-Modell-Doku, README, Manpage · 6. Release-Binaries

Tipp: Der Nutzer startet am besten zuerst **core** (Schritt 1 entsperrt net/ui), die anderen drei können sofort parallel laufen.
