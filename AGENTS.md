# Orchestrierung – beliebig viele Coding-Agents

Quelle der Wahrheit für *was*: [ROADMAP.md](ROADMAP.md). Die startbaren Schritte stehen in [QUEUE.tsv](QUEUE.tsv). Dieses Dokument regelt *wie*.

Alle Agents werden **im Hauptordner** geöffnet und bekommen nur den Prompt „mach den nächsten Schritt“.
Der Hauptordner bleibt dabei **unverändert** – gearbeitet wird in einem Worktree, den das Skript pro Schritt anlegt.

## Protokoll für „mach den nächsten Schritt“
1. Im Hauptordner ausführen: `scripts/agent.sh next`
   - Wählt den obersten startbaren Schritt aus QUEUE.tsv, sperrt ihn (und seinen Bereich) atomar, legt Worktree `../rdl-<id>` auf Branch `step/<id>` an und gibt Schritt + Pfad aus.
   - Meldet es „NICHTS STARTBAR“: dem Nutzer sagen (Grund via `scripts/agent.sh status`) und **stoppen**.
2. In den ausgegebenen Arbeitsordner wechseln und **ausschließlich dort** arbeiten. Nie Dateien im Hauptordner ändern.
3. Schritt umsetzen (Beschreibung: QUEUE.tsv + passender ROADMAP-Abschnitt) mit Tests; `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` grün. ROADMAP-Checkboxen der erledigten Punkte auf `[x]`.
4. Auf dem Branch committen, dann `scripts/agent.sh finish <id>` (vom Hauptordner aus: rebase auf master, Fast-Forward-Merge unter Lock, Worktree aufräumen, Schritt als erledigt markieren).
   - Konflikt/Fehler: nicht raten. Eintrag in `HANDOFF.md`, `scripts/agent.sh abort <id>` falls aufgegeben, Nutzer informieren.
5. Dem Nutzer in 2–3 Sätzen berichten: was fertig, was als Nächstes startbar ist. Pro Aufruf **ein** Schritt, dann stoppen.

## Wie es sich skaliert
- Es gibt keine festen Agent-Rollen. Jeder Agent nimmt, was gerade startbar ist; Sperren sind atomar (`mkdir` im gemeinsamen `.git/orch/`).
- Pro **Bereich** (core, net, ui, plugins, infra) läuft höchstens ein Schritt gleichzeitig, und Schritte eines Bereichs laufen in Queue-Reihenfolge. Das verhindert Konflikte, weil jeder Bereich eigene Dateien besitzt (core=`src/download/`, net=`src/tor/`+`src/net/`, ui=`src/ui/`+`src/tracker/`, plugins=`src/plugin/`+`plugins/`+`examples/`, infra=`.github/`+`tests/`+`docs/`+Cargo-Aufräumen).
- Daraus folgt: bis zu **5 Agents** arbeiten gleichzeitig; ein 6. meldet „nichts startbar“. Weniger Agents arbeiten die Queue einfach nacheinander ab. Mehr Parallelität würde das Aufteilen eines Bereichs in Teilbereiche in QUEUE.tsv erfordern.
- Cross-Bereich-Abhängigkeiten stehen in der Spalte „braucht“ von QUEUE.tsv. Ein Schritt wird erst startbar, wenn die Voraussetzungen erledigt (gemergt) sind.

## Regeln
- Berührt ein Schritt Dateien außerhalb seines Bereichs (typisch: `Cargo.toml`, `main.rs`): nur minimal ändern, früh mergen.
- Verträge (`src/contract.rs`, Schritt core-1) sind danach stabil. Änderungen daran brauchen einen `HANDOFF.md`-Eintrag und Absprache mit dem Nutzer.
- Review-Wunsch oder Blocker an andere Bereiche: `HANDOFF.md` (append-only), Format `[von → an] Anliegen – Status`.
- Das Skript prüft, dass der Hauptordner sauber ist. Dort nie direkt arbeiten.
- `finish` aktualisiert `STATUS.md` automatisch (Skript `scripts/status-md.sh`) und committet es auf master.

## Sprache & Übersetzbarkeit (verbindlich für alle neuen Änderungen)
- **Alles im Programm ist Englisch**: Quellcode (Bezeichner, Kommentare, Doc-Comments, Commit-Messages), `--help`/Clap-Texte, TUI-Oberfläche, Fehlermeldungen, Logs. Interne Orchestrierungs-Dateien (AGENTS.md, ROADMAP.md, QUEUE.tsv, HANDOFF.md) bleiben Deutsch.
- **Keine Nutzer-Texte als Literale im Code.** Sichtbare Texte (UI, Hilfe, Fehler für Nutzer) laufen über den String-Katalog `src/i18n.rs` (Schritt i18n-1): stabile Schlüssel wie `queue.title`, Platzhalter benannt (`{name}`), Standardsprache `en`, später weitere Sprachen als zusätzliche Kataloge ohne Code-Änderung. Bis i18n-1 gemergt ist: Texte Englisch und an einer Stelle pro Modul bündeln (`const`), nicht verstreuen.
- Logs für Entwickler (`tracing`) bleiben Englisch und werden nicht übersetzt.
- Bestehende deutsche Texte im Code werden in Schritt i18n-2 umgestellt; in Dateien, die du ohnehin änderst, direkt mitziehen.

## Clean-Room-Regeln (Ziel: ein JDownloader-ähnlicher Download-Manager, komplett neu geschrieben)
Die Oberfläche und die Bedienkonzepte sollen sich an JDownloader orientieren (Pakete, Link-Sammler, Tabellenansicht, Kontextmenüs, Statusleiste). Damit das ein echter Clean-Room bleibt:
- **Erlaubt:** sichtbares Verhalten, öffentliche Dokumentation, vom Nutzer gelieferte Screenshots als Referenz für Aufbau und Funktionsumfang, eigene Ideen und eigene Tests.
- **Verboten:** JDownloader-Quellcode oder dekompilierte `.jar`s ansehen, Icons/Grafiken/Logos/Themes, Texte oder Übersetzungen kopieren, den Namen oder das Logo „JDownloader“ in Programm, Hilfe oder Paketnamen verwenden (nur beschreibend in Dokumenten wie „inspiriert von“).
- **Eigene Gestaltung:** eigener Name, eigene Icons, eigenes Farbschema, eigene Texte im Katalog. Funktionen werden aus dem *Verhalten* nachgebaut, nicht aus Code.
- Referenz-Screenshots des Nutzers werden **nicht committet** (liegen lokal, z. B. `~/Pictures/rdl-reference/`); in Dokumenten wird nur beschrieben, was daraus gelernt wurde.
- Das Zielbild der Oberfläche steht in `docs/gui-layout.md` (eigene Beschreibung, keine fremden Inhalte).

## Build-Umgebung (macOS ohne Xcode/CLT)
Auf diesem Rechner gibt es keine Xcode Command Line Tools, nur ein macOS-SDK und Homebrew-LLVM. Vor jedem `cargo`-Aufruf (auch fmt, clippy, test):
```
source scripts/env-macos.sh
```
Das Skript setzt SDKROOT (`~/.local/share/SDKs/MacOSX26.0.sdk`), clang/lld aus `/opt/brew` und die Rust-Linker-Flags. Der Sandbox-Modus der Shell muss dafür ggf. abgeschaltet sein (Zugriff auf `/opt/brew` und `~/.local`).
`cargo fmt` nur auf eigene Dateien anwenden (`rustfmt --edition 2021 <datei>`), sonst werden fremde Bereiche umformatiert und es gibt Merge-Konflikte.

## Hilfsbefehle
```
scripts/agent.sh status        # Queue mit Zuständen (offen / IN ARBEIT / ERLEDIGT)
scripts/agent.sh abort <id>    # Schritt aufgeben, Sperre + Worktree entfernen
```
