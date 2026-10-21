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

### Dynamic Sandboxed Plugins (Next Phase / WASM)
When dynamic plugins are introduced:
- **WebAssembly (Wasmtime / Wasmi)**: Dynamic plugins will be compiled to `wasm32-wasip1` / component model.
- **Zero Ambient Authority**: WASI system calls for filesystem (`fd_read`, `fd_write`) are pre-opened exclusively for the plugin's isolated scratch folder.
- **Host Imports**: Network operations are exposed via a narrow Host API implementing Contract A, rejecting any host outside the permitted domains.
- **Resource Limits**: Memory limits (e.g. 64 MB maximum linear memory) and CPU fuel consumption to prevent CPU starvation.

---

## 4. User Consent & Manifest Verification

- On plugin registration, permissions are inspected.
- Wildcard network access (`*`) or sensitive permissions (account access, filesystem writes) require explicit consent or declaration in configuration.
- Any violation terminates the plugin execution immediately with a typed [`PermissionDenied`](file:///Users/user/Desktop/rdownloader/src/plugin/permissions.rs) error.
