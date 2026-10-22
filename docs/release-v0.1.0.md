# rDownloader 0.1.0 (alpha) - release notes

**This is an alpha release. Many bugs are still to be expected; features may change or break.**

The first public release. rDownloader downloads through the Tor network, splits files into parts that
use separate circuits, resumes after crashes and comes with a desktop window, a terminal interface and
a headless daemon. It is written from scratch; the workflow is inspired by classic download managers, but no
code, icons, images or texts of other programs are used.

## What you can rely on

Checked automatically on every change: formatting, `clippy -D warnings`, about 370 unit and integration tests
(including kill-and-resume with hash check), a guard against telemetry and against local DNS resolution.

Checked by hand over the real Tor network with the release binary (October 2026): an HTTPS download of unknown size, an HTTPS download
with four parts and byte ranges, an FTP download, and a request to an onion service. All downloaded files were
byte-identical to a direct download.

## Known limitations

- **Anonymity is only as good as Tor.** Plain `http://` and `ftp://` are visible to the exit relay. Read `docs/threat-model.md`.
- **Not yet checked on real systems:** RAR extraction (needs `unrar`, `7zz` or `bsdtar`; only ZIP and 7z are unpacked in-process), SFTP/FTPS/HLS/DASH against live servers, the Linux and Windows builds, and mouse-driven use of the window beyond the automated render tests.
- **FTP over Tor:** some servers accept the data connection only from the control connection's IP; Tor may use two exits, so a retry or two can be needed.
- **HLS/DASH:** encrypted HLS and DRM-protected DASH are refused; progress for a stream shows only when it has finished.
- **No direct connection:** `--no-tor` only starts the interface; downloads need Tor.
- **Local state is not encrypted** (download list, resume data, partial files).
- **Plugins** compiled into the program are fully trusted; WebAssembly plugins run in a sandbox.
- **Some hosters need a browser** (JavaScript, complex captchas); these are out of reach for now.
- Pluggable transports (obfs4, snowflake), RSS feeds and a system tray icon are not part of 0.1.0.

## Installing

Download the file for your system from the releases page and verify it with `SHA256SUMS.txt`. Or build from source:
`cargo build --release` (see the README for the macOS toolchain note).

## Reporting problems

Security problems: privately through GitHub (see `SECURITY.md`). Everything else: a normal issue.
