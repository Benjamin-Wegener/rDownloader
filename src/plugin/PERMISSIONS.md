# Plugin Permission & Sandboxing Architecture

This design document specifies the security boundary, capability declarations, and enforcement model for `rdownloader` plugins, establishing strict guarantees before dynamic plugin loading (e.g., via WebAssembly) is introduced.

---

## 1. Threat Model & Security Goals

Plugins are responsible for interacting with diverse hosters, crawlers, accounts, and captcha challenges. Because hosters change frequently and community plugins may be added, plugins must run with **least privilege**.

### Threats Mitigated
1. **Network Egress & Exfiltration**: A malicious or compromised plugin resolving a download on `hoster.com` must not be able to send arbitrary HTTP/TCP requests to exfiltrate session cookies, user IPs, or internal network services.
2. **Local File System Compromise**: A plugin must never read private keys, shell histories, system configurations (`/etc/passwd`, `~/.ssh`), or overwrite existing binaries.
3. **Account Credential Leaks**: Credentials stored in the account manager must only be exposed to plugins explicitly authorized for that hoster domain.
4. **Denial of Service / Unprompted Dialog Spam**: Plugins must not initiate infinite captcha challenge loops or flood the user interface without declared permissions.

---

## 2. Capability & Permission Model

Every plugin declares its required capabilities in its manifest (`PluginMeta`):

```rust
pub struct PluginPermissions {
    /// Whitelisted domains the plugin can connect to via Contract A Transport.
    pub network_domains: HashSet<String>,

    /// Specific filesystem paths granted for reading (e.g. plugin cache).
    pub read_paths: HashSet<PathBuf>,

    /// Specific filesystem paths granted for writing.
    pub write_paths: HashSet<PathBuf>,

    /// Permission to retrieve account sessions and credentials.
    pub allow_account_access: bool,

    /// Permission to trigger interactive Captcha prompts in the TUI.
    pub allow_interactive_captcha: bool,
}
```

### Permission Defaults
By default, **all permissions are denied**:
- No network access outside declared domains (`meta.domains`).
- No filesystem access outside the plugin's isolated scratch directory (`~/.local/share/rdownloader/plugins/<plugin-id>/`).
- No credential or captcha dialog access unless explicitly flagged.

---

## 3. Enforcement Layers

### Static Rust Plugins (Current Phase)
- **Contract A (`Transport`) Proxy**: Plugins never access the network directly via standard sockets or external crates. All HTTP operations pass through `Transport` (Contract A, routed over Tor). The transport wrapper checks `check_network(url)` before dispatching requests.
- **Account Manager Guards**: `AccountManager` checks whether the requesting plugin's domain matches the target cookie domain before supplying authentication tokens.
- **Captcha Mediator**: `CaptchaManager` checks `check_captcha_prompt()` before forwarding challenges to the interactive solver.

### Dynamic Sandboxed Plugins (WASM, `src/plugin/wasm.rs`)
Dynamic plugins are WebAssembly modules executed by the pure-Rust `wasmi`
interpreter. They are loaded at runtime from the plugin directory
(`~/.local/share/rdownloader/plugins/*.wasm`):
- **Zero Ambient Authority**: no WASI is linked and no filesystem, clock, or
  socket exports exist. A module importing anything other than the `rdl` host
  namespace fails to instantiate, so there is no ambient authority to escape.
- **Host Imports**: `rdl.log` and `rdl.http_get` only. `rdl.http_get` checks
  `PluginPermissions::check_network` before dispatch and then uses Contract A
  (Tor in the application). The URL returned by `rdl_resolve` is checked against
  the same allowlist.
- **Result ABI**: guests export `memory`, `rdl_alloc`, `rdl_plugin_meta`, and
  `rdl_resolve`. Results are JSON (`PluginMeta` / `ResolvedLink`).
- **Resource Limits**: fuel metering bounds CPU use, `StoreLimits` caps linear
  memory (64 MB by default), outputs are capped (64 KB), and host responses are
  capped (1 MB). Violations trap and terminate the guest.
- **Permissions from the host, not the guest**: `hoster.wasm` reads grants from
  `hoster.json` (serialised `PluginPermissions`). A missing or invalid sidecar
  means deny-all; the guest cannot widen its own permissions.

---

## 4. User Consent & Manifest Verification

- On plugin registration, permissions are inspected.
- Wildcard network access (`*`) or sensitive permissions (account access, filesystem writes) require explicit consent or declaration in configuration.
- Any violation terminates the plugin execution immediately with a typed [`PermissionDenied`](file:///Users/user/Desktop/rdownloader/src/plugin/permissions.rs) error.
