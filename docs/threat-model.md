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
- **Transport security**: `https` uses rustls with the WebPKI root set; certificates are validated. Plain `http` is allowed and is exposed to the exit relay: use `https` URLs.
- **Hostile servers**:
  - `Content-Disposition` names are reduced to a safe base name, so a server cannot write outside the download directory *(verified by tests in `src/net/disposition.rs`)*.
  - Crawled pages are limited to 2 MiB; batch import files to 4 MiB and 10 000 links.
  - Redirects are followed up to a fixed limit.
  - Resume only continues when ETag / Last-Modified still match.
- **Archive extraction** (`auto_extract`, off by default): entry names are validated (no absolute paths, no `..`), symlinks in archives are never created, existing files are never overwritten, archive permissions are ignored, and entry count and unpacked size are capped (64 GiB, 100 000 entries) *(verified by tests with a path-traversal ZIP and a size-limit bomb)*. RAR is unpacked by an external tool (`unrar`, `7zz`/`7z` or `bsdtar`); its result is checked afterwards, but the size cap can only be enforced after the tool has finished.
- **Integrity**: when a hash is known (SHA-256 recommended), the finished file is verified and deleted on mismatch *(verified)*. Without a hash, integrity relies on TLS only.
- **Credentials**: `Secret` and cookie values are redacted in `Debug` output and are not serialisable, so they do not reach logs or the tracker file by accident *(verified)*. Cookies are only attached to requests for the matching domain.
- **Plugins**: a permission model (network allow-list per plugin, file system limits, credential scoping) is specified in `src/plugin/PERMISSIONS.md`. Plugins are static Rust code today, so they are trusted code; the model matters once dynamic (WASM) plugins exist.

## Not protected / known limitations

- **Anonymity is only as good as Tor.** Traffic correlation by a global observer, and exit-relay attacks on plain `http`, are out of scope.
- **The destination learns what you ask for.** If you log in (account plugin) or send cookies, the hoster can identify you regardless of Tor.
- **Local state is not encrypted.** `downloads.json` (URLs, file names, progress), the resume data, partial files in the temp directory and the Tor state directory are plain files protected only by the OS user account and umask. Anyone who can read your home directory can see what you downloaded. Use full-disk encryption.
- **Cookies for the account plugin are held in memory only** and must be supplied again after a restart.
- **Plugins are fully trusted today.** A malicious plugin compiled into the binary can do anything the process can.
- **Clipboard import** reads the clipboard through `pbpaste` / `wl-paste` / `xclip` / `xsel`; whatever is on the clipboard is parsed for links (only `http`/`https`).
- **Captcha**: interactive solving shows the challenge in the terminal; no automatic solving or third-party solver is used.
- **Bridges**: configured bridge lines are passed to Arti; their trustworthiness is the user's responsibility.
- **Timing and size**: part sizes and timing can fingerprint a download for an observer who sees both ends.
- **No update mechanism yet**: install updates yourself; there is no signature verification for releases until release builds exist.

## Reporting

Report vulnerabilities privately through GitHub (Security tab, "Report a vulnerability"); see [SECURITY.md](../SECURITY.md). Do not use public issues.
