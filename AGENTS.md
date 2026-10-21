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

## Agent-Start-Prompt (pro Lane kopieren, `<LANE>` ersetzen)
> Du bist Lane `<LANE>` im Projekt rdownloader. Lies `AGENTS.md` und `ROADMAP.md`. Arbeite nur in den Dateien deiner Lane, in deinem Worktree `lane/<LANE>`. Nimm den obersten offenen Punkt deiner Lane, claime ihn in der Roadmap, implementiere ihn mit Tests, halte fmt/clippy/test grün, merge klein. Bei Abhängigkeiten von anderen Lanes: `HANDOFF.md`, nicht deren Dateien ändern.
