# rDownloader

A clean-room download manager written in Rust. Every download goes through the
Tor network (embedded [Arti](https://gitlab.torproject.org/tpo/core/arti), no
external Tor daemon), is split into parts that each use their own circuit,
and can be resumed after a crash. It has a desktop window, a terminal interface, a headless daemon and a small plugin system.

Inspired by the concepts of JDownloader (queue, link grabber, hoster plugins);
no code was taken from it. JDownloader's Java plugins cannot be loaded.

> **Alpha software.** Version 0.1.0 is an early alpha: expect many bugs and rough edges, and do not rely on it for anything important yet.
> It is the first release. Read the [known limitations](#known-limitations) and the
> [threat model](docs/threat-model.md) before relying on it. See [CHANGELOG.md](CHANGELOG.md),
> [ROADMAP.md](ROADMAP.md) for what is planned.

## Features

- Downloads over Tor with per-part stream isolation (one Tor client, one isolation group per part)
- Multi-part downloads (1-10 parts) with automatic fallback to one connection when the server has no `Accept-Ranges`
- Retries with backoff per part, and resume of partial downloads (ETag / Last-Modified are validated)
- Optional checksum verification (MD5, SHA-1, SHA-256, SHA-512) when a hash is known
- Mirrors: a Metalink file lists several URLs per file; parts are spread over them, faster mirrors are preferred, dead or wrong ones are dropped. Metalink piece checksums find damaged chunks, which are repaired from another mirror
- Global speed limit (`max_speed_kib`)
- Desktop window (egui; drag link lists onto it, shortcuts Ctrl/Cmd+N, Ctrl/Cmd+, and F1): toolbar (resume / pause / stop all, move up and down, clear finished), queue with live progress, speed and ETA, per-download details, search with scope and state filter, an overview of totals (size, loaded, speed, time left, connections), add-links dialog, history and Tor status. It is the default when a display is available
- Terminal UI (`--tui`): queue with live progress, per-job details (parts, speed, ETA, Tor state), pause / resume / cancel / remove
- Batch import of links from a text or M3U file, or from the clipboard
- Link grabber: collect links separately (manually, from a file, the clipboard or a crawled page), filter them by host and file type, then confirm them into the download queue
- Session restore: unfinished downloads come back after a restart, paused (a setting continues them on start)
- Plugin types: hoster, crawler, account (cookie sessions) and interactive captcha
- Tor bridges for censored networks
- FTP, FTPS and SFTP downloads over Tor, with resume: `ftp://` (plain, see the threat model), `ftps://` (implicit TLS, port 990), `ftpes://` (explicit TLS, port 21) and `sftp://` (SSH, port 22). SFTP verifies the server key against `~/.ssh/known_hosts` and refuses unknown or changed keys; without a password in the URL it uses an unencrypted key from `~/.ssh` (`id_ed25519`, `id_ecdsa`, `id_rsa`). FTPS checks certificates against the WebPKI roots. Some FTP servers only accept the data connection from the same IP as the control connection and need a retry or two over Tor
- HLS/DASH streams: a `.m3u8` or `.mpd` link goes to the segment engine, with own manifest parsers (master playlists, `SegmentTemplate`/`SegmentList`/`SegmentBase`), parallel fragment downloads over Tor and ordered merge; encrypted HLS and DRM-protected DASH are rejected
- `.onion` addresses (onion services) work like any other URL; plain `http://` is fine there because the connection is end-to-end encrypted by Tor
- Interface text comes from a string catalog (English built in, translations can be added without code changes)

## Install

Download the archive for your system from the releases page and check it against `SHA256SUMS.txt`
(`shasum -a 256 -c SHA256SUMS.txt`). Packaging recipes for Homebrew, AUR and Scoop are in `packaging/`.

### First start: the downloads are not signed

The 0.1.0 files are not code-signed or notarized (that needs paid developer accounts). Your system will warn you, which is expected:

- **macOS:** unzip `rDownloader.app`, move it to Applications, then right-click it, choose **Open** and confirm once. If it still says the app is damaged, run `xattr -dr com.apple.quarantine /Applications/rDownloader.app` in the Terminal.
- **Windows:** SmartScreen shows "Windows protected your PC": click **More info**, then **Run anyway**.
- **Linux:** make the file executable (`chmod +x rdownloader`). The window needs an X11 or Wayland session; without one the terminal interface starts.

Check the download against `SHA256SUMS.txt` before you run it. The Windows, Linux and Intel-Mac builds were built automatically but have not been started by hand yet.

## Quick start

1. Start `rdownloader`. The window opens; the Tor state is shown at the bottom right, downloads begin as soon as it reads "connected".
2. Press **Add links** (Ctrl/Cmd+N), paste one or more links and confirm. Or drop a link list, `.dlc` or `.metalink` file onto the window: the links go to the **Link grabber** first, where you pick what to download.
3. Watch progress in the list; open **Details** on a download for parts, speed and Tor state. Pause, resume, cancel and remove with the buttons or Space and Delete.

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
rdownloader                         # open the window (terminal UI if there is no display)
rdownloader --tui                   # force the terminal UI
rdownloader URL                     # download a URL, showing progress in the TUI
rdownloader --headless -p 4 -o ~/Downloads URL
rdownloader --daemon                # JSON-RPC/REST control API (no TUI)
```

| Option | Meaning |
| --- | --- |
| `-p, --parts N` | parts per download (1-10) |
| `-o, --output PATH` | target file or directory |
| `--gui` / `--tui` | open the window (default with a display) / use the terminal UI |
| `--headless` | no UI, log to the terminal |
| `--daemon` | no TUI; serve the JSON-RPC/REST control API |
| `-t, --tor-clients N` | deprecated and ignored: one Tor client isolates every part |
| `--no-tor` | do not start Tor (testing only; downloads need Tor and will not run) |

Terminal UI keys (`--tui`):

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
| `max_parts` | `3` | parts per download |
| `max_retries` | `3` | retries per part |
| `max_speed_kib` | `0` | global speed limit in KiB/s, `0` = unlimited |
| `[[schedule]]` | none | time windows (`days`, `from`, `to`, `speed_kib` or `pause = true`, local time; first match wins) that change the speed limit or pause downloads |
| `restart_slow_parts` | `true` | restart a single connection that stays far below the other parts of the same download, on a new circuit and rationed (cool-down, one at a time per download, global budget) |
| `min_speed_kib` | `1` | connections slower than this (over 30 s of network time) are dropped and rebuilt on a fresh circuit, `0` = off |
| `read_timeout_secs` | `60` | a connection that delivers no data for this long is dropped |
| `conditional_get` | `false` | skip a download when the existing file has the server's size and is not older than its `Last-Modified` |
| `max_concurrent_jobs`, `max_jobs_per_host` | `3`, `2` | jobs running at once, overall and per host (`0` = unlimited) |
| `auto_extract`, `extract_delete_archives` | `false` | unpack finished `.zip` / `.7z` / `.rar` (also multi-part) next to the download, optionally delete the archives afterwards |
| `bridges` | `[]` | Tor bridge lines |
| `download_dir`, `temp_dir`, `tor_data_dir` | platform defaults | where files and Tor state live; partial downloads go to a permanent folder in the data directory (`.../rdownloader/partial`), not the OS temp folder, so a half-finished download survives a reboot |
| `user_agent` | `rdownloader/0.1` | HTTP user agent |
| `autostart` | `false` | start the window when you log in (also a switch in the settings dialog) |
| `[hooks]` | none | programs to run on start, completion and error, and `notify` (see below) |
| `log_level` | `info` | log level (`RUST_LOG` overrides it) |
| `language` | `en` | interface language: `en` or `de` (`RDOWNLOADER_LANG` overrides it) |
| `rpc_listen` | `127.0.0.1:8765` | listen address of the headless RPC server |
| `rpc_token` | unset | optional bearer token; required for non-loopback addresses |

English is the built-in default; the German catalog ships in `lang/de.toml`. Set `language = "de"` in the config or `RDOWNLOADER_LANG=de` (the environment variable wins). The catalog is embedded at build time, so installed binaries need no extra files.

## Headless daemon & RPC

`rdownloader --daemon` runs without the TUI and exposes the download queue over
HTTP. It listens on `127.0.0.1:8765` by default; set `rpc_listen` to change the
address and `rpc_token` to require `Authorization: Bearer <token>`. Binding to a
non-loopback address without a token is refused.

REST endpoints:

```sh
curl http://127.0.0.1:8765/api/jobs
curl -X POST http://127.0.0.1:8765/api/jobs \
  -H 'Content-Type: application/json' -d '{"url":"https://example.com/file.bin"}'
curl -X POST http://127.0.0.1:8765/api/jobs/<id>/pause
curl -X POST http://127.0.0.1:8765/api/jobs/<id>/resume
curl -X POST http://127.0.0.1:8765/api/jobs/<id>/cancel
curl -X DELETE http://127.0.0.1:8765/api/jobs/<id>
curl -N http://127.0.0.1:8765/api/events
```

`GET /api/events` is a [Server-Sent Events](https://html.spec.whatwg.org/multipage/server-sent-events.html)
stream. It first sends the whole queue (`event: jobs`, JSON array, same shape as
`GET /api/jobs`) and then a new event whenever a job is added, changes state or
is removed. Comment lines keep the connection alive; browsers reconnect
automatically (`retry: 3000`). With `rpc_token`, send the bearer header as usual
(`-H 'Authorization: Bearer <token>'`).

JSON-RPC 2.0 is available at `POST /rpc`; supported methods are `jobs.list`,
`jobs.get`, `jobs.add`, `jobs.pause`, `jobs.resume`, `jobs.cancel`, `jobs.remove`
and `health`:

```sh
curl -X POST http://127.0.0.1:8765/rpc \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"jobs.add","params":{"url":"https://example.com/file.bin"}}'
```

`GET /health` returns `{"status":"ok"}`. The API has no TLS; keep it on loopback
or tunnel it through Tor/a reverse proxy if remote access is needed.

To keep web pages out, the API applies browser-style guards even without a token:
the `Host` header must name `localhost`, a loopback address, or the concrete
address `rpc_listen` binds to (no other host names, which defeats DNS rebinding);
requests that change state (`POST`, `PUT`, `PATCH`, `DELETE`) are rejected when
their `Origin` header names another site; and a request body must be sent as
`application/json` (`curl` needs the header shown above). Every response carries
`X-Content-Type-Options: nosniff`, `Content-Security-Policy`,
`X-Frame-Options`, `Referrer-Policy` and `Cross-Origin-Resource-Policy`; no CORS
headers are sent.

## Hooks and notifications

Run your own program when a download starts, finishes or fails, and optionally get a desktop notification
(macOS: `osascript`, Linux: `notify-send`). In the config file:

```toml
[hooks]
on_complete = ["/Users/me/bin/after-download.sh"]   # program and fixed arguments, no shell
on_error    = ["/usr/local/bin/alert"]
on_start    = []
notify      = true
```

The program gets the details as environment variables (`RDL_EVENT`, `RDL_JOB_ID`, `RDL_URL`, `RDL_FILE`,
`RDL_FILENAME`, `RDL_SIZE`, `RDL_ERROR`) and as two trailing arguments (job id, file path). It is started
directly, never through a shell. **Quote the variables in your own scripts**: file names come from servers.
Hooks run in the background, are stopped after 60 seconds and never delay a download.

## Known limitations

- Anonymity is only as good as Tor; plain `http://` and `ftp://` are visible to the exit relay.
- Downloads need Tor: `--no-tor` only starts the interface.
- RAR archives are unpacked by an external tool (`unrar`, `7zz`/`7z` or `bsdtar`); it is not tested on this release.
- Encrypted HLS and DRM-protected DASH streams are refused. Some hosters need a browser or a captcha that cannot be solved here.
- FTP over Tor may need a retry or two on servers that tie the data connection to one IP.
- Local state (download list, resume data, partial files) is not encrypted: use full-disk encryption.
- Not part of 0.1.0: pluggable transports (obfs4, snowflake), RSS feeds, a system tray icon.

## Safe use

- Prefer `https://` links; use `http://` and `ftp://` only for files you do not mind being seen.
- Leave `auto_extract` off unless you trust the archives; downloaded files are untrusted input.
- Do not expose the daemon (`--daemon`) beyond your machine without `rpc_token`, and never share the token.
- Treat hook scripts like any program: they receive file names from servers, so quote them.
- Report vulnerabilities privately, see `SECURITY.md`.

## Security

What rDownloader protects, and what it does not, is described in [docs/threat-model.md](docs/threat-model.md).
Plugin sandboxing and the WebAssembly plugin ABI are documented in [src/plugin/PERMISSIONS.md](src/plugin/PERMISSIONS.md) and implemented in `src/plugin/wasm.rs`.
Read the limitations before relying on it for anonymity.
Report vulnerabilities privately via GitHub, see [SECURITY.md](SECURITY.md).

## Contributing

Contributions are welcome. The project is developed in small steps by the maintainer together with several AI coding agents,
each working in its own area and branch; every change must pass `cargo fmt --check`, `clippy -D warnings` and all tests.
Please keep to the clean-room rule: no code, icons, images or texts of other download managers. New interface texts need an
English entry in `src/i18n.rs` and a German one in `lang/de.toml`. See [CONTRIBUTORS.md](CONTRIBUTORS.md).

## License

[MIT](LICENSE). The licenses of the Rust crates used are listed in [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md). rDownloader is written from scratch; it contains no code, icons, images or texts of other download managers.
