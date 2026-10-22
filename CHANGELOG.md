# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [Semantic Versioning](https://semver.org/).

## [0.1.0] - first release (alpha)

> Alpha: expect many bugs. Please report what you find.

First public version. A clean-room download manager in Rust: everything goes through the Tor network.

### Downloads
- Multi-part downloads (1-10 parts) over Tor, one isolation group per part, with automatic fallback to a single connection when the server has no byte ranges or reports no size
- Retries with backoff, circuit rotation after failures, resume of partial downloads (ETag / Last-Modified validated), slow or stalled connections are dropped (`min_speed_kib`, `read_timeout_secs`)
- Checksum verification (MD5, SHA-1, SHA-256, SHA-512), Metalink piece checksums with repair of damaged pieces, mirrors with speed-based selection and failover
- Protocols: HTTP and HTTPS, FTP, FTPS (implicit and explicit TLS), SFTP with host-key verification, `.onion` services, HLS and DASH streams (unencrypted only)
- Global speed limit, time windows (`[[schedule]]`) that limit or pause downloads, limits for parallel jobs and per host, conditional download, free-space check and preallocation
- Auto-extract of `.zip`, `.7z` and `.rar` (also multi-part), off by default and hardened against path traversal and zip bombs
- Event hooks and desktop notifications
- macOS quarantine attribute on finished downloads, safe file names for every source (no path traversal, no reserved Windows names, no overwriting)

### Interface
- Desktop window (egui) with toolbar, packages and sortable columns, search and filters, totals, link grabber, history, settings, context menus, drag and drop of link lists, shortcuts and an about dialog
- Terminal interface (`--tui`) with the same queue, link grabber and history
- Headless daemon (`--daemon`) with JSON-RPC 2.0 and REST, protected against other web pages
- English and German interface; further languages as plain TOML catalogs

### Importing
- Link lists, M3U playlists, `.dlc`, `.ccf`, `.metalink`, the clipboard, and crawling a web page for links; everything is collected in the link grabber first

### Plugins
- Hoster, crawler, account (cookie sessions, netrc, browser cookie import) and captcha plugin types; WebAssembly plugins in a capability-checked sandbox

### Security and privacy
- No telemetry (guarded by a test), no local DNS resolution outside Tor (guarded by a test), secrets redacted in logs, threat model in `docs/threat-model.md`

### Fixed after the alpha tag
- Pause really stops the transfer and stays paused (it used to resume a second later); cancel and remove stop the connections instead of only hiding the download
- Single connections that stay far below their siblings are restarted on a new circuit, rationed so the network is not flooded

- Stop closes the connections and keeps the partial data; a stopped download survives a restart and continues with Resume (a failed one can be tried again); adding the same link continues it; Cancel and Remove delete the partial data; unused partial folders can be deleted from the settings
- A stopped or cancelled download is no longer shown as "Failed", and the shown speed drops to zero at once
- The speed limit is entered with a unit (KB/s or MB/s) and applies immediately to running downloads; the footer shows the active limit

### Known limitations
See "Known limitations" in the README and in `docs/release-v0.1.0.md`.
