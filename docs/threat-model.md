# Threat model

Scope: the `rdownloader` process on a user's machine and the traffic it creates.
This document says what the design is meant to protect, what it relies on, and where it does **not** help.
Statements marked *(verified)* are enforced by code or tests in this repository; the rest are design intent.

## Assets

1. **The user's network identity**: the IP address and location the user downloads from.
2. **Which files the user downloads** (URLs and file names).
3. **Credentials and sessions** used with hoster accounts (cookies).
4. **Integrity of downloaded files.**
5. **The user's local files** (outside the download directory).

## Adversaries

| Adversary | Can see / do |
| --- | --- |
| Local network / ISP | sees that Tor is used, not the destination |
| Destination server / hoster | sees a Tor exit address, the requested URL, any cookies we send |
| Malicious or compromised exit relay | can read and alter plain `http` traffic |
| Malicious server | can send wrong data, odd headers, oversized or hostile file names |
| Malicious plugin | runs in-process with our privileges today |
| Other local users / malware on the machine | can read files our user can read |
| Global passive observer | may correlate entry and exit traffic (out of scope for Tor itself) |

## What is protected, and how

- **IP address**: all download traffic uses an embedded Tor client; the HTTP client is built on Arti streams. There is no direct fallback; with `--no-tor` downloads do not run at all. *(verified: a test scans the sources for local DNS resolution outside Tor, so host names are never resolved locally.)*
- **Linkability between parts**: every part requests its own isolation token, so parts do not share a circuit. After a failure the token is dropped and the retry uses a fresh circuit.
- **FTP** (`ftp://`): works over Tor with the same per-part isolation, but plain FTP has no encryption, so the login and the data are visible to the exit relay. A warning is logged on every login. The passive-mode address sent by a server is ignored (the data connection always goes to the host in the URL), and URLs that would inject extra FTP commands (line breaks in user, password or path) are rejected *(verified by tests)*. Prefer `https`, FTPS or SFTP when available.
- **FTPS** (`ftps://` implicit, `ftpes://` explicit): control and data connections are protected with TLS and the server certificate is verified against the WebPKI roots. Self-signed certificates are rejected. The Tor exit sees only the encrypted stream, but the FTP server still learns the client's request.
- **SFTP** (`sftp://`): the SSH connection runs over Tor and the server host key must match an entry in `~/.ssh/known_hosts`; unknown keys, changed keys and host certificates abort the download *(verified by tests)*. Wildcard and `@revoked` known-hosts lines are not interpreted and therefore fail closed. Passwords and user names from the URL are stored as part of the job in `downloads.json` like every other URL; prefer key authentication. Encrypted private keys are not used because a download job cannot prompt for a passphrase.
- **Onion services**: `.onion` URLs are reached through Tor's onion-service client (no exit relay involved) and Arti's vanguard protection is compiled in. Certificates for `.onion` hosts are rarely valid, so use `http://` there; it is encrypted by the onion protocol itself. *(verified once by a live test, `tests/live_onion.rs`, run manually with `--ignored`)*
- **Transport security**: `https` uses rustls with the WebPKI root set; certificates are validated. Plain `http` is allowed and is exposed to the exit relay: use `https` URLs.
- **Hostile servers**:
  - `Content-Disposition` names are reduced to a safe base name, so a server cannot write outside the download directory *(verified by tests in `src/net/disposition.rs`)*.
  - Crawled pages are limited to 2 MiB; batch import files to 4 MiB and 10 000 links.
  - Redirects are followed up to a fixed limit.
  - Resume only continues when ETag / Last-Modified still match.
- **Archive extraction** (`auto_extract`, off by default): entry names are validated (no absolute paths, no `..`), symlinks in archives are never created, existing files are never overwritten, archive permissions are ignored, and entry count and unpacked size are capped (64 GiB, 100 000 entries) *(verified by tests with a path-traversal ZIP and a size-limit bomb)*. RAR is unpacked by an external tool (`unrar`, `7zz`/`7z` or `bsdtar`); its result is checked afterwards, but the size cap can only be enforced after the tool has finished.
- **Mirrors**: a mirror that reports a different file size is banned and none of its bytes are used. Metalink piece checksums are verified after merging; a damaged piece is only replaced by data whose hash matches, otherwise the job fails and the file is deleted. Mirrors come from the same import file as the primary URL, so they are as trusted as that file; without a hash in the import there is no protection against a mirror that serves a same-sized wrong file.
- **Hooks and notifications**: hook commands come from the user's own config and are started without a shell; download details are passed as environment variables and arguments, so a hostile file name or error text cannot inject commands into the hook launcher. The hook script itself must quote them. Notification text is passed as program arguments, stripped of control characters and truncated *(verified: a crafted file name did not run a command through `osascript`)*. Hooks are time-limited and at most four run at once.
- **Integrity**: when a hash is known (SHA-256 recommended), the finished file is verified and deleted on mismatch *(verified)*. Without a hash, integrity relies on TLS only.
- **Credentials**: `Secret` and cookie values are redacted in `Debug` output and are not serialisable, so they do not reach logs or the tracker file by accident *(verified)*. Cookies are only attached to requests for the matching domain.
- **Plugins**: a permission model (network allow-list per plugin, file system limits, credential scoping) is specified in `src/plugin/PERMISSIONS.md`. Plugins are static Rust code today, so they are trusted code; the model matters once dynamic (WASM) plugins exist.

- **No telemetry**: rDownloader sends no usage data or crash reports. `tests/no_telemetry.rs` fails the build if a known telemetry crate enters the dependency tree (`Cargo.lock`, including transitive ones) or a known analytics host appears in the sources. This is a guard against accidents, not a proof: an unlisted service or a dynamically built address would not be caught.

## Not protected / known limitations

- **Anonymity is only as good as Tor.** Traffic correlation by a global observer, and exit-relay attacks on plain `http`, are out of scope.
- **The destination learns what you ask for.** If you log in (account plugin) or send cookies, the hoster can identify you regardless of Tor.
- **Local state is not encrypted.** `downloads.json` (URLs, file names, progress), the resume data, partial files in the temp directory and the Tor state directory are plain files protected only by the OS user account and umask. Anyone who can read your home directory can see what you downloaded. Use full-disk encryption.
- **Cookies for the account plugin are held in memory only** and must be supplied again after a restart.
- **Plugins are fully trusted today.** A malicious plugin compiled into the binary can do anything the process can.
- **Clipboard import** reads the clipboard through `pbpaste` / `wl-paste` / `xclip` / `xsel`; whatever is on the clipboard is parsed for links (only `http`/`https`).
- **Captcha**: interactive solving shows the challenge in the terminal; no automatic solving or third-party solver is used.
- **Bridges**: configured bridge lines are passed to Arti; their trustworthiness is the user's responsibility.
- **Local control API**: `--daemon` serves JSON-RPC/REST without TLS. Anything that can reach the port can control the download queue. Binding to a non-loopback address requires `rpc_token`, but the token travels in clear text; use a tunnel or reverse proxy with TLS for remote access.
- **Browser-driven requests against the API**: the server accepts only local `Host` values, rejects cross-origin state changes (`Origin`) and requires `application/json` request bodies, so a page the user visits cannot queue or inspect downloads through the browser. `rpc_token` remains the stronger protection once the port is reachable from elsewhere.
- **Timing and size**: part sizes and timing can fingerprint a download for an observer who sees both ends.
- **No update mechanism yet**: install updates yourself; there is no signature verification for releases until release builds exist.

## Reporting

Report vulnerabilities privately through GitHub (Security tab, "Report a vulnerability"); see [SECURITY.md](../SECURITY.md). Do not use public issues.
