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
- Link grabber: collect links separately (manually, from a file, the clipboard or a crawled page), filter them by host and file type, then confirm them into the download queue
- Session restore: unfinished downloads come back after a restart
- Plugin types: hoster, crawler, account (cookie sessions) and interactive captcha
- Tor bridges for censored networks
- FTP downloads (`ftp://`) over Tor, with resume (unencrypted, see the threat model); SFTP is planned
- `.onion` addresses (onion services) work like any other URL; plain `http://` is fine there because the connection is end-to-end encrypted by Tor
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
rdownloader --daemon                # JSON-RPC/REST control API (no TUI)
```

| Option | Meaning |
| --- | --- |
| `-p, --parts N` | parts per download (1-10) |
| `-o, --output PATH` | target file or directory |
| `--headless` | no TUI, log to the terminal |
| `--daemon` | no TUI; serve the JSON-RPC/REST control API |
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

LinkGrabber tab (the links are collected first and downloaded only after the confirmation):

| Key | Action |
| --- | --- |
| `a` / `i` / `v` / `c` | collect a URL, a link list file, the clipboard, or crawl a page for links |
| `Space` / `p` / `A` | mark the link, the whole package, or all links |
| `f` / `F` | exclude the host or the file type of the selected link |
| `x` | remove the selected link or package |
| `Enter` | transfer the marked links into the download queue |

## Configuration

The config file is created on first start in the platform config directory
(`~/.config/rdownloader/config.toml` on Linux, `~/Library/Application Support/rdownloader/config.toml` on macOS).
All keys are optional.

| Key | Default | Meaning |
| --- | --- | --- |
| `max_parts` | `10` | parts per download |
| `max_retries` | `3` | retries per part |
| `max_speed_kib` | `0` | global speed limit in KiB/s, `0` = unlimited |
| `min_speed_kib` | `1` | connections slower than this (over 30 s of network time) are dropped and rebuilt on a fresh circuit, `0` = off |
| `read_timeout_secs` | `60` | a connection that delivers no data for this long is dropped |
| `conditional_get` | `false` | skip a download when the existing file has the server's size and is not older than its `Last-Modified` |
| `max_concurrent_jobs`, `max_jobs_per_host` | `3`, `2` | jobs running at once, overall and per host (`0` = unlimited) |
| `auto_extract`, `extract_delete_archives` | `false` | unpack finished `.zip` / `.7z` / `.rar` (also multi-part) next to the download, optionally delete the archives afterwards |
| `bridges` | `[]` | Tor bridge lines |
| `download_dir`, `temp_dir`, `tor_data_dir` | platform defaults | where files and Tor state live |
| `user_agent`, `connect_timeout_secs`, `read_timeout_secs` | | HTTP behaviour |
| `log_level` | `info` | log level (`RUST_LOG` overrides it) |
| `rpc_listen` | `127.0.0.1:8765` | listen address of the headless RPC server |
| `rpc_token` | unset | optional bearer token; required for non-loopback addresses |

`RDOWNLOADER_LANG=<code>` selects the interface language when a catalog for it is loaded.

## Headless daemon & RPC

`rdownloader --daemon` runs without the TUI and exposes the download queue over
HTTP. It listens on `127.0.0.1:8765` by default; set `rpc_listen` to change the
address and `rpc_token` to require `Authorization: Bearer <token>`. Binding to a
non-loopback address without a token is refused.

REST endpoints:

```sh
curl http://127.0.0.1:8765/api/jobs
curl -X POST http://127.0.0.1:8765/api/jobs -d '{"url":"https://example.com/file.bin"}'
curl -X POST http://127.0.0.1:8765/api/jobs/<id>/pause
curl -X POST http://127.0.0.1:8765/api/jobs/<id>/resume
curl -X POST http://127.0.0.1:8765/api/jobs/<id>/cancel
curl -X DELETE http://127.0.0.1:8765/api/jobs/<id>
```

JSON-RPC 2.0 is available at `POST /rpc`; supported methods are `jobs.list`,
`jobs.get`, `jobs.add`, `jobs.pause`, `jobs.resume`, `jobs.cancel`, `jobs.remove`
and `health`:

```sh
curl -X POST http://127.0.0.1:8765/rpc \
  -d '{"jsonrpc":"2.0","id":1,"method":"jobs.add","params":{"url":"https://example.com/file.bin"}}'
```

`GET /health` returns `{"status":"ok"}`. The API has no TLS; keep it on loopback
or tunnel it through Tor/a reverse proxy if remote access is needed.

## Security

What rdownloader protects, and what it does not, is described in [docs/threat-model.md](docs/threat-model.md).
Plugin sandboxing and the WebAssembly plugin ABI are documented in [src/plugin/PERMISSIONS.md](src/plugin/PERMISSIONS.md) and implemented in `src/plugin/wasm.rs`.
Read the limitations before relying on it for anonymity.
Report vulnerabilities privately via GitHub, see [SECURITY.md](SECURITY.md).

## Contributing

Work is organised through [AGENTS.md](AGENTS.md) (steps, areas, build environment). See [CONTRIBUTORS.md](CONTRIBUTORS.md).

## License

[MIT](LICENSE)
