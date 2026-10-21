# rdownloader

A clean-room download manager written in Rust. Every download goes through the
Tor network (embedded [Arti](https://gitlab.torproject.org/tpo/core/arti), no
external Tor daemon), is split into parts that each use their own circuit,
and can be resumed after a crash. It has a terminal UI and a small plugin system.

Inspired by the concepts of JDownloader (queue, link grabber, hoster plugins);
no code was taken from it. JDownloader's Java plugins cannot be loaded.

> Status: early development. See [STATUS.md](STATUS.md) for what is done and
> [ROADMAP.md](ROADMAP.md) for what is planned.

## Features

- Downloads over Tor with per-part stream isolation (one Tor client, one isolation group per part)
- Multi-part downloads (1-10 parts) with automatic fallback to one connection when the server has no `Accept-Ranges`
- Retries with backoff per part, and resume of partial downloads (ETag / Last-Modified are validated)
- Optional checksum verification (MD5, SHA-1, SHA-256, SHA-512) when a hash is known
- Global speed limit (`max_speed_kib`)
- Terminal UI: queue with live progress, per-job details (parts, speed, ETA, Tor state), pause / resume / cancel / remove
- Batch import of links from a text or M3U file, or from the clipboard
- Session restore: unfinished downloads come back after a restart
- Plugin types: hoster, crawler, account (cookie sessions) and interactive captcha
- Tor bridges for censored networks
- Interface text comes from a string catalog (English built in, translations can be added without code changes)

## Build

Requires a recent stable Rust toolchain and a C toolchain (clang) for some dependencies.

```sh
cargo build --release
./target/release/rdownloader --help
```

On a Mac without Xcode Command Line Tools, `source scripts/env-macos.sh` first; it points
the build at Homebrew LLVM and a standalone macOS SDK.

## Usage

```sh
rdownloader                         # start the TUI
rdownloader URL                     # download a URL, showing progress in the TUI
rdownloader --headless -p 4 -o ~/Downloads URL
```

| Option | Meaning |
| --- | --- |
| `-p, --parts N` | parts per download (1-10) |
| `-o, --output PATH` | target file or directory |
| `--headless` | no TUI, log to the terminal |
| `-t, --tor-clients N` | deprecated and ignored: one Tor client isolates every part |
| `--no-tor` | do not start Tor (testing only; downloads need Tor and will not run) |

TUI keys:

| Key | Action |
| --- | --- |
| `a` | add a URL |
| `i` / `v` | import links from a file / from the clipboard |
| `Enter` | toggle the detail panel |
| `p` `r` `c` `d` | pause, resume, cancel, remove the selected download |
| `Tab` / `Shift+Tab` | switch tab |
| `q` | quit |

## Configuration

The config file is created on first start in the platform config directory
(`~/.config/rdownloader/config.toml` on Linux, `~/Library/Application Support/rdownloader/config.toml` on macOS).
All keys are optional.

| Key | Default | Meaning |
| --- | --- | --- |
| `max_parts` | `10` | parts per download |
| `max_retries` | `3` | retries per part |
| `max_speed_kib` | `0` | global speed limit in KiB/s, `0` = unlimited |
| `bridges` | `[]` | Tor bridge lines |
| `download_dir`, `temp_dir`, `tor_data_dir` | platform defaults | where files and Tor state live |
| `user_agent`, `connect_timeout_secs`, `read_timeout_secs` | | HTTP behaviour |
| `log_level` | `info` | log level (`RUST_LOG` overrides it) |

`RDOWNLOADER_LANG=<code>` selects the interface language when a catalog for it is loaded.

## Security

What rdownloader protects, and what it does not, is described in [docs/threat-model.md](docs/threat-model.md).
Plugin sandboxing is designed in [src/plugin/PERMISSIONS.md](src/plugin/PERMISSIONS.md).
Read the limitations before relying on it for anonymity.

## Contributing

Work is organised through [AGENTS.md](AGENTS.md) (steps, areas, build environment). See [CONTRIBUTORS.md](CONTRIBUTORS.md).
