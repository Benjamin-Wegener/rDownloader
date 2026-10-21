//! Sandboxed WebAssembly plugin runtime.
//!
//! Dynamic plugins are compiled to WebAssembly and executed by [`wasmi`], a
//! pure-Rust interpreter. Guests get **zero ambient authority**: no WASI, no
//! filesystem, no sockets, no clock. Only the narrow host API below is linked,
//! and every call is checked against the plugin's [`PluginPermissions`].
//!
//! # Guest ABI
//!
//! A guest module must export a linear `memory` and these functions:
//!
//! ```text
//! rdl_alloc(len: i32) -> i32
//!     Allocate `len` bytes inside the guest and return a pointer.
//!
//! rdl_plugin_meta(out_ptr: i32, out_cap: i32) -> i32
//!     Write the `PluginMeta` JSON into `out_ptr`, return its byte length.
//!
//! rdl_resolve(url_ptr: i32, url_len: i32, out_ptr: i32, out_cap: i32) -> i32
//!     Resolve the UTF-8 URL and write a `ResolvedLink` JSON into `out_ptr`,
//!     return its byte length.
//! ```
//!
//! A return value `< 0` is treated as a plugin error. The host allocates the
//! output buffers through `rdl_alloc`, so guests never hand out raw pointers
//! for results.
//!
//! # Host ABI (imports, namespace `rdl`)
//!
//! ```text
//! log(level: i32, ptr: i32, len: i32)
//!     Emit a UTF-8 log line (1 = info, 2 = warn, 3 = error).
//!
//! http_get(url_ptr: i32, url_len: i32, out_ptr: i32, out_cap: i32) -> i32
//!     Fetch a URL through Contract A. Returns the number of body bytes
//!     written, or `-(needed)` when `out_cap` is too small. Permission
//!     violations trap immediately and terminate the guest.
//! ```
//!
//! Network access is only possible through `http_get`, which checks the
//! declared domains first and then delegates to the injected [`WasmHttpClient`]
//! (over Tor in the application). The URL returned by `rdl_resolve` is checked
//! against the same domain allowlist.
//!
//! # Resource limits
//!
//! Fuel metering bounds CPU time, [`StoreLimits`] bounds linear memory, and
//! every guest output/response is capped. Unknown imports (for example WASI)
//! make instantiation fail instead of silently gaining capabilities.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use url::Url;

use wasmi::{
    Caller, Engine, Extern, Instance, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder,
};

use super::permissions::PermissionDenied;
use super::{PluginMeta, ResolvedLink};
use crate::contract::Transport;
use crate::plugin::DownloadPlugin;
use crate::t;
use async_trait::async_trait;

/// Guest export names of the plugin ABI.
pub mod abi {
    pub const MEMORY: &str = "memory";
    pub const ALLOC: &str = "rdl_alloc";
    pub const PLUGIN_META: &str = "rdl_plugin_meta";
    pub const RESOLVE: &str = "rdl_resolve";

    pub const HOST_MODULE: &str = "rdl";
    pub const LOG: &str = "log";
    pub const HTTP_GET: &str = "http_get";
}

const META_BUFFER_BYTES: usize = 16 * 1024;
const MAX_LOG_BYTES: usize = 4 * 1024;
const MAX_URL_BYTES: usize = 8 * 1024;

/// Resource limits of the WebAssembly sandbox.
#[derive(Debug, Clone)]
pub struct WasmRuntimeConfig {
    /// Maximum size of the guest linear memory.
    pub max_memory_bytes: usize,
    /// Fuel granted to every guest call (one unit is a few instructions).
    pub fuel: u64,
    /// Maximum size of a guest JSON result.
    pub max_output_bytes: usize,
    /// Maximum size of a response body returned by `rdl.http_get`.
    pub max_response_bytes: usize,
}

impl Default for WasmRuntimeConfig {
    fn default() -> Self {
        Self {
            max_memory_bytes: 64 * 1024 * 1024,
            fuel: 50_000_000,
            max_output_bytes: 64 * 1024,
            max_response_bytes: 1024 * 1024,
        }
    }
}

/// Synchronous HTTP backend used by the `rdl.http_get` host import.
///
/// The sandbox runs on a blocking thread, so the backend is synchronous. The
/// application supplies an adapter over Contract A (see [`TransportHttpClient`])
/// and tests supply mocks.
pub trait WasmHttpClient: Send + Sync {
    /// Fetch `url` and return the body, rejecting bodies larger than `max_bytes`.
    fn get(&self, url: &Url, max_bytes: usize) -> Result<Vec<u8>>;
}

/// Default backend: denies every request (no network authority).
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyHttpClient;

impl WasmHttpClient for DenyHttpClient {
    fn get(&self, url: &Url, _max_bytes: usize) -> Result<Vec<u8>> {
        bail!("network access is disabled for WebAssembly plugins ({url})")
    }
}

/// Contract A adapter: runs the async [`Transport`] from the blocking sandbox.
pub struct TransportHttpClient {
    transport: Arc<dyn Transport>,
    handle: tokio::runtime::Handle,
    isolation: u64,
}

impl TransportHttpClient {
    pub fn new(
        transport: Arc<dyn Transport>,
        handle: tokio::runtime::Handle,
        isolation: u64,
    ) -> Self {
        Self {
            transport,
            handle,
            isolation,
        }
    }
}

impl WasmHttpClient for TransportHttpClient {
    fn get(&self, url: &Url, max_bytes: usize) -> Result<Vec<u8>> {
        use futures::StreamExt;

        self.handle.block_on(async {
            let mut response = self.transport.get(url, None, self.isolation).await?;
            let mut body = Vec::new();
            while let Some(chunk) = response.body.next().await {
                let chunk = chunk?;
                if body.len() + chunk.len() > max_bytes {
                    bail!("response body exceeds {max_bytes} bytes");
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
    }
}

/// A host-side violation raised while executing a guest.
#[derive(Debug, Clone)]
pub enum WasmHostViolation {
    Permission(PermissionDenied),
    InvalidUrl(String),
    Http(String),
}

impl fmt::Display for WasmHostViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Permission(p) => write!(f, "{p}"),
            Self::InvalidUrl(url) => write!(f, "plugin requested an invalid URL: {url}"),
            Self::Http(message) => write!(f, "plugin HTTP request failed: {message}"),
        }
    }
}

impl std::error::Error for WasmHostViolation {}

impl wasmi::errors::HostError for WasmHostViolation {}

/// Host state shared by all host imports of one instantiation.
struct HostState {
    permissions: super::PluginPermissions,
    http: Arc<dyn WasmHttpClient>,
    max_response_bytes: usize,
    limits: StoreLimits,
}

impl HostState {
    fn new(
        permissions: super::PluginPermissions,
        http: Arc<dyn WasmHttpClient>,
        config: &WasmRuntimeConfig,
    ) -> Self {
        Self {
            permissions,
            http,
            max_response_bytes: config.max_response_bytes,
            limits: StoreLimitsBuilder::new()
                .memory_size(config.max_memory_bytes)
                .tables(4)
                .memories(1)
                .instances(1)
                .build(),
        }
    }
}

/// A dynamically loaded `.wasm` plugin.
#[derive(Clone)]
pub struct WasmPlugin {
    meta: PluginMeta,
    module: Module,
    permissions: super::PluginPermissions,
    config: WasmRuntimeConfig,
    http: Arc<dyn WasmHttpClient>,
}

impl fmt::Debug for WasmPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmPlugin")
            .field("meta", &self.meta)
            .field("permissions", &self.permissions)
            .finish_non_exhaustive()
    }
}

impl WasmPlugin {
    /// Load a plugin from a `.wasm` file.
    pub fn load(
        path: &Path,
        permissions: super::PluginPermissions,
        config: WasmRuntimeConfig,
        http: Arc<dyn WasmHttpClient>,
    ) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading WebAssembly plugin {}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("plugin.wasm");
        Self::from_bytes(name, bytes, permissions, config, http)
    }

    /// Load a plugin from already-read WebAssembly bytes (binary or WAT).
    pub fn from_bytes(
        name: impl Into<String>,
        bytes: impl AsRef<[u8]>,
        permissions: super::PluginPermissions,
        config: WasmRuntimeConfig,
        http: Arc<dyn WasmHttpClient>,
    ) -> Result<Self> {
        let name = name.into();
        let engine = new_engine();
        let module = Module::new(&engine, bytes).map_err(|error| {
            anyhow!(t!(
                "plugin.wasm.invalid_module",
                name = &name,
                error = error
            ))
        })?;
        let meta = read_meta(&name, &module, &permissions, &config, http.clone())?;
        if meta.id.trim().is_empty() {
            bail!(t!("plugin.wasm.meta_empty", name = &name));
        }
        Ok(Self {
            meta,
            module,
            permissions,
            config,
            http,
        })
    }

    /// Metadata declared by the guest at load time.
    pub fn meta(&self) -> &PluginMeta {
        &self.meta
    }

    /// Permissions this instance was granted by the host.
    pub fn permissions(&self) -> &super::PluginPermissions {
        &self.permissions
    }

    /// Synchronous resolution used by the async [`DownloadPlugin`] wrapper.
    pub fn resolve_blocking(&self, url: Url) -> Result<ResolvedLink> {
        let name = self.meta.id.clone();
        let mut resolved = run_resolve(
            &name,
            &self.module,
            &self.permissions,
            &self.config,
            self.http.clone(),
            url,
        )?;
        self.permissions
            .check_network(&resolved.url)
            .map_err(anyhow::Error::new)?;
        if resolved.additional_headers.is_empty() {
            resolved.additional_headers = HashMap::new();
        }
        Ok(resolved)
    }
}

#[async_trait]
impl DownloadPlugin for WasmPlugin {
    fn meta(&self) -> PluginMeta {
        self.meta.clone()
    }

    async fn resolve(&self, url: Url) -> Result<ResolvedLink> {
        let this = self.clone();
        let name = self.meta.id.clone();
        tokio::task::spawn_blocking(move || this.resolve_blocking(url))
            .await
            .map_err(|error| anyhow!(t!("plugin.wasm.task_failed", name = &name, error = error)))?
    }
}

/// Load every `*.wasm` file in `dir` with its permission sidecar.
///
/// The sidecar is a JSON file with the same stem (`hoster.wasm` →
/// `hoster.json`) that deserialises to [`PluginPermissions`](super::PluginPermissions).
/// Without a sidecar a plugin gets no permissions at all.
///
/// Errors are per file so one broken plugin does not stop the others.
pub fn load_dir(
    dir: &Path,
    config: &WasmRuntimeConfig,
    http: Arc<dyn WasmHttpClient>,
) -> Vec<Result<WasmPlugin>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            return vec![Err(anyhow!(
                "reading WebAssembly plugin directory {}: {error}",
                dir.display()
            ))]
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("wasm"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let permissions = load_permissions_sidecar(&path);
            WasmPlugin::load(&path, permissions, config.clone(), http.clone())
        })
        .collect()
}

/// Read `<stem>.json` next to the plugin; missing or invalid files mean "deny all".
pub fn load_permissions_sidecar(wasm_path: &Path) -> super::PluginPermissions {
    let sidecar = wasm_path.with_extension("json");
    if !sidecar.exists() {
        return super::PluginPermissions::default();
    }
    match std::fs::read_to_string(&sidecar)
        .map_err(anyhow::Error::from)
        .and_then(|json| serde_json::from_str(&json).map_err(anyhow::Error::from))
    {
        Ok(permissions) => permissions,
        Err(error) => {
            tracing::warn!(
                path = %sidecar.display(),
                %error,
                "ignoring invalid WebAssembly plugin permission sidecar; denying all permissions"
            );
            super::PluginPermissions::default()
        }
    }
}

fn new_engine() -> Engine {
    let mut engine_config = wasmi::Config::default();
    engine_config.consume_fuel(true);
    Engine::new(&engine_config)
}

fn instantiate(
    module: &Module,
    permissions: &super::PluginPermissions,
    config: &WasmRuntimeConfig,
    http: Arc<dyn WasmHttpClient>,
) -> Result<(Store<HostState>, Instance)> {
    let engine = module.engine();
    let mut store = Store::new(engine, HostState::new(permissions.clone(), http, config));
    store.limiter(|state| &mut state.limits);
    store
        .set_fuel(config.fuel)
        .map_err(|error| anyhow!("enabling fuel metering: {error}"))?;

    let mut linker = Linker::<HostState>::new(engine);
    linker
        .func_wrap(
            abi::HOST_MODULE,
            abi::LOG,
            |caller: Caller<'_, HostState>,
             level: i32,
             ptr: i32,
             len: i32|
             -> Result<(), wasmi::Error> {
                if len < 0 || len as usize > MAX_LOG_BYTES {
                    return Err(wasmi::Error::new("log message exceeds the size limit"));
                }
                let memory = guest_memory(&caller)?;
                let message = read_guest_string(&caller, &memory, ptr, len)?;
                match level {
                    3 => tracing::error!(target: "plugin.wasm", "{message}"),
                    2 => tracing::warn!(target: "plugin.wasm", "{message}"),
                    _ => tracing::info!(target: "plugin.wasm", "{message}"),
                }
                Ok(())
            },
        )
        .map_err(|error| anyhow!("linking host import log: {error}"))?;
    linker
        .func_wrap(
            abi::HOST_MODULE,
            abi::HTTP_GET,
            |mut caller: Caller<'_, HostState>,
             url_ptr: i32,
             url_len: i32,
             out_ptr: i32,
             out_cap: i32|
             -> Result<i32, wasmi::Error> {
                if url_len < 0 || url_len as usize > MAX_URL_BYTES {
                    return Err(wasmi::Error::new("URL exceeds the size limit"));
                }
                if url_len < 0 || url_len as usize > MAX_URL_BYTES {
                    return Err(wasmi::Error::new("URL exceeds the size limit"));
                }
                let memory = guest_memory(&caller)?;
                let url_text = read_guest_string(&caller, &memory, url_ptr, url_len)?;
                let url = Url::parse(&url_text)
                    .map_err(|_| wasmi::Error::host(WasmHostViolation::InvalidUrl(url_text)))?;

                let permissions = caller.data().permissions.clone();
                let http = caller.data().http.clone();
                let max_bytes = caller.data().max_response_bytes;
                permissions
                    .check_network(&url)
                    .map_err(|denied| wasmi::Error::host(WasmHostViolation::Permission(denied)))?;
                let body = http.get(&url, max_bytes).map_err(|error| {
                    wasmi::Error::host(WasmHostViolation::Http(error.to_string()))
                })?;

                let required = body.len();
                if out_cap < 0 || (required > out_cap as usize) {
                    return Ok(-(required as i32));
                }
                memory
                    .write(&mut caller, out_ptr as usize, &body)
                    .map_err(|error| {
                        wasmi::Error::new(format!("guest memory write failed: {error}"))
                    })?;
                Ok(required as i32)
            },
        )
        .map_err(|error| anyhow!("linking host import http_get: {error}"))?;

    let instance = linker
        .instantiate_and_start(&mut store, module)
        .map_err(|error| anyhow!(t!("plugin.wasm.instantiate_failed", error = error)))?;
    Ok((store, instance))
}

fn read_meta(
    name: &str,
    module: &Module,
    permissions: &super::PluginPermissions,
    config: &WasmRuntimeConfig,
    http: Arc<dyn WasmHttpClient>,
) -> Result<PluginMeta> {
    let (mut store, instance) = instantiate(module, permissions, config, http)?;
    let alloc = instance
        .get_typed_func::<i32, i32>(&store, abi::ALLOC)
        .map_err(|_| {
            anyhow!(t!(
                "plugin.wasm.missing_export",
                name = name,
                export = abi::ALLOC
            ))
        })?;
    let meta_fn = instance
        .get_typed_func::<(i32, i32), i32>(&store, abi::PLUGIN_META)
        .map_err(|_| {
            anyhow!(t!(
                "plugin.wasm.missing_export",
                name = name,
                export = abi::PLUGIN_META
            ))
        })?;
    let memory = guest_memory_required(&instance, &store, name)?;

    let out_ptr = alloc
        .call(&mut store, META_BUFFER_BYTES as i32)
        .map_err(|error| map_wasmi_error(name, error))?;
    let written = meta_fn
        .call(&mut store, (out_ptr, META_BUFFER_BYTES as i32))
        .map_err(|error| map_wasmi_error(name, error))?;
    let bytes = read_guest_output(name, &store, &memory, out_ptr, written, META_BUFFER_BYTES)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| anyhow!(t!("plugin.wasm.invalid_meta", name = name, error = error)))
}

fn run_resolve(
    name: &str,
    module: &Module,
    permissions: &super::PluginPermissions,
    config: &WasmRuntimeConfig,
    http: Arc<dyn WasmHttpClient>,
    url: Url,
) -> Result<ResolvedLink> {
    let (mut store, instance) = instantiate(module, permissions, config, http)?;
    let alloc = instance
        .get_typed_func::<i32, i32>(&store, abi::ALLOC)
        .map_err(|_| {
            anyhow!(t!(
                "plugin.wasm.missing_export",
                name = name,
                export = abi::ALLOC
            ))
        })?;
    let resolve_fn = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&store, abi::RESOLVE)
        .map_err(|_| {
            anyhow!(t!(
                "plugin.wasm.missing_export",
                name = name,
                export = abi::RESOLVE
            ))
        })?;
    let memory = guest_memory_required(&instance, &store, name)?;
    store
        .set_fuel(config.fuel)
        .map_err(|error| anyhow!("resetting fuel: {error}"))?;

    let url_text = url.as_str().as_bytes();
    let url_ptr = alloc
        .call(&mut store, url_text.len() as i32)
        .map_err(|error| map_wasmi_error(name, error))?;
    memory
        .write(&mut store, url_ptr as usize, url_text)
        .context("writing the URL into guest memory")?;

    let out_cap = config.max_output_bytes;
    let out_ptr = alloc
        .call(&mut store, out_cap as i32)
        .map_err(|error| map_wasmi_error(name, error))?;
    let written = resolve_fn
        .call(
            &mut store,
            (url_ptr, url_text.len() as i32, out_ptr, out_cap as i32),
        )
        .map_err(|error| map_wasmi_error(name, error))?;
    let bytes = read_guest_output(name, &store, &memory, out_ptr, written, out_cap)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| anyhow!(t!("plugin.wasm.invalid_output", name = name, error = error)))
}

fn guest_memory_required(
    instance: &Instance,
    store: &Store<HostState>,
    name: &str,
) -> Result<Memory> {
    instance.get_memory(store, abi::MEMORY).ok_or_else(|| {
        anyhow!(t!(
            "plugin.wasm.missing_export",
            name = name,
            export = abi::MEMORY
        ))
    })
}

fn guest_memory(caller: &Caller<'_, HostState>) -> Result<Memory, wasmi::Error> {
    caller
        .get_export(abi::MEMORY)
        .and_then(Extern::into_memory)
        .ok_or_else(|| wasmi::Error::new(format!("guest does not export '{}'", abi::MEMORY)))
}

fn read_guest_string(
    caller: &Caller<'_, HostState>,
    memory: &Memory,
    ptr: i32,
    len: i32,
) -> Result<String, wasmi::Error> {
    if ptr < 0 || len < 0 {
        return Err(wasmi::Error::new(
            "guest passed a negative pointer or length",
        ));
    }
    let mut buffer = vec![0u8; len as usize];
    memory
        .read(caller, ptr as usize, &mut buffer)
        .map_err(|error| wasmi::Error::new(format!("guest memory read failed: {error}")))?;
    String::from_utf8(buffer)
        .map_err(|error| wasmi::Error::new(format!("guest passed invalid UTF-8: {error}")))
}

fn read_guest_output(
    name: &str,
    store: &Store<HostState>,
    memory: &Memory,
    ptr: i32,
    written: i32,
    max_len: usize,
) -> Result<Vec<u8>> {
    if written < 0 {
        bail!(t!(
            "plugin.wasm.guest_error",
            name = name,
            error = format!("returned {written}")
        ));
    }
    let written = written as usize;
    if ptr < 0 || written > max_len {
        bail!(t!(
            "plugin.wasm.output_too_large",
            name = name,
            limit = max_len
        ));
    }
    let mut buffer = vec![0u8; written];
    memory
        .read(store, ptr as usize, &mut buffer)
        .context("reading the guest result")?;
    Ok(buffer)
}

fn map_wasmi_error(name: &str, error: wasmi::Error) -> anyhow::Error {
    if let Some(violation) = error.downcast_ref::<WasmHostViolation>() {
        return match violation {
            WasmHostViolation::Permission(denied) => anyhow::Error::new(denied.clone()),
            other => anyhow!(t!("plugin.wasm.guest_error", name = name, error = other)),
        };
    }
    anyhow!(t!("plugin.wasm.guest_error", name = name, error = error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::PluginPermissions;
    use std::sync::Mutex;

    const META: &str = r#"{"id":"demo.wasm","name":"Demo wasm","version":"1.0.0","author":"tests","description":"test guest","kind":"Hoster","domains":["demo.test"]}"#;
    const RESOLVED: &str = r#"{"url":"https://demo.test/file.bin","filename":"file.bin","filesize":null,"supports_ranges":true,"additional_headers":{}}"#;

    fn config() -> WasmRuntimeConfig {
        WasmRuntimeConfig {
            max_memory_bytes: 1024 * 1024,
            fuel: 1_000_000,
            max_output_bytes: 4096,
            max_response_bytes: 4096,
        }
    }

    fn wat_escape(json: &str) -> String {
        json.replace('\\', "\\\\").replace('"', "\\\"")
    }

    fn static_guest(meta: &str, resolved: &str) -> String {
        format!(
            r#"(module
                (memory (export "memory") 1)
                (global $heap (mut i32) (i32.const 16384))
                (data (i32.const 1024) "{meta}")
                (data (i32.const 8192) "{resolved}")
                (func (export "rdl_alloc") (param $len i32) (result i32)
                    (local $p i32)
                    (local.set $p (global.get $heap))
                    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
                    (local.get $p))
                (func $copy (param $src i32) (param $len i32) (param $dst i32) (param $cap i32) (result i32)
                    (if (i32.gt_u (local.get $len) (local.get $cap))
                        (then (return (i32.const -1))))
                    (memory.copy (local.get $dst) (local.get $src) (local.get $len))
                    (local.get $len))
                (func (export "rdl_plugin_meta") (param $out i32) (param $cap i32) (result i32)
                    (call $copy (i32.const 1024) (i32.const {meta_len}) (local.get $out) (local.get $cap)))
                (func (export "rdl_resolve") (param $u i32) (param $ul i32) (param $out i32) (param $cap i32) (result i32)
                    (call $copy (i32.const 8192) (i32.const {res_len}) (local.get $out) (local.get $cap))))"#,
            meta = wat_escape(meta),
            meta_len = meta.len(),
            resolved = wat_escape(resolved),
            res_len = resolved.len(),
        )
    }

    fn http_guest(meta: &str) -> String {
        format!(
            r#"(module
                (import "rdl" "http_get" (func $http_get (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (global $heap (mut i32) (i32.const 16384))
                (data (i32.const 1024) "{meta}")
                (func (export "rdl_alloc") (param $len i32) (result i32)
                    (local $p i32)
                    (local.set $p (global.get $heap))
                    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
                    (local.get $p))
                (func (export "rdl_plugin_meta") (param $out i32) (param $cap i32) (result i32)
                    (memory.copy (local.get $out) (i32.const 1024) (i32.const {meta_len}))
                    (i32.const {meta_len}))
                (func (export "rdl_resolve") (param $u i32) (param $ul i32) (param $out i32) (param $cap i32) (result i32)
                    (call $http_get (local.get $u) (local.get $ul) (local.get $out) (local.get $cap))))"#,
            meta = wat_escape(meta),
            meta_len = meta.len(),
        )
    }

    fn loop_guest(meta: &str) -> String {
        format!(
            r#"(module
                (memory (export "memory") 1)
                (global $heap (mut i32) (i32.const 16384))
                (data (i32.const 1024) "{meta}")
                (func (export "rdl_alloc") (param $len i32) (result i32)
                    (local $p i32)
                    (local.set $p (global.get $heap))
                    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
                    (local.get $p))
                (func (export "rdl_plugin_meta") (param $out i32) (param $cap i32) (result i32)
                    (memory.copy (local.get $out) (i32.const 1024) (i32.const {meta_len}))
                    (i32.const {meta_len}))
                (func (export "rdl_resolve") (param $u i32) (param $ul i32) (param $out i32) (param $cap i32) (result i32)
                    (loop $forever (br $forever))
                    (i32.const 0)))"#,
            meta = wat_escape(meta),
            meta_len = meta.len(),
        )
    }

    struct MockHttp {
        body: Vec<u8>,
        calls: Mutex<Vec<String>>,
    }

    impl MockHttp {
        fn new(body: impl Into<Vec<u8>>) -> Self {
            Self {
                body: body.into(),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl WasmHttpClient for MockHttp {
        fn get(&self, url: &Url, max_bytes: usize) -> Result<Vec<u8>> {
            self.calls.lock().unwrap().push(url.to_string());
            if self.body.len() > max_bytes {
                bail!("mock response too large");
            }
            Ok(self.body.clone())
        }
    }

    fn permissions(domain: &str) -> PluginPermissions {
        PluginPermissions::new().with_network_domain(domain)
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn loads_meta_and_resolves() {
        let plugin = WasmPlugin::from_bytes(
            "demo.wasm",
            static_guest(META, RESOLVED),
            permissions("demo.test"),
            config(),
            Arc::new(DenyHttpClient),
        )
        .unwrap();
        assert_eq!(plugin.meta().id, "demo.wasm");
        assert_eq!(plugin.meta().domains, vec!["demo.test".to_string()]);

        let resolved = plugin
            .resolve_blocking(url("https://demo.test/start"))
            .unwrap();
        assert_eq!(resolved.url.as_str(), "https://demo.test/file.bin");
        assert_eq!(resolved.filename.as_deref(), Some("file.bin"));
    }

    #[test]
    fn rejects_resolved_url_outside_allowlist() {
        let plugin = WasmPlugin::from_bytes(
            "demo.wasm",
            static_guest(META, RESOLVED),
            permissions("other.test"),
            config(),
            Arc::new(DenyHttpClient),
        )
        .unwrap();
        let error = plugin
            .resolve_blocking(url("https://other.test/start"))
            .unwrap_err();
        assert!(error.downcast_ref::<PermissionDenied>().is_some());
    }

    #[test]
    fn http_get_is_check_permitted() {
        let mock = Arc::new(MockHttp::new(RESOLVED));
        let perms = permissions("cdn.test").with_network_domain("demo.test");
        let plugin =
            WasmPlugin::from_bytes("demo.wasm", http_guest(META), perms, config(), mock.clone())
                .unwrap();
        let resolved = plugin
            .resolve_blocking(url("https://cdn.test/start"))
            .unwrap();
        assert_eq!(resolved.url.as_str(), "https://demo.test/file.bin");
        assert_eq!(mock.calls(), vec!["https://cdn.test/start".to_string()]);
    }

    #[test]
    fn http_get_outside_allowlist_never_reaches_network() {
        let mock = Arc::new(MockHttp::new(RESOLVED));
        let plugin = WasmPlugin::from_bytes(
            "demo.wasm",
            http_guest(META),
            permissions("cdn.test"),
            config(),
            mock.clone(),
        )
        .unwrap();
        let error = plugin
            .resolve_blocking(url("https://evil.test/steal"))
            .unwrap_err();
        assert!(error.downcast_ref::<PermissionDenied>().is_some());
        assert!(mock.calls().is_empty());
    }

    #[test]
    fn fuel_limit_stops_endless_guest() {
        let mut cfg = config();
        cfg.fuel = 100_000;
        let plugin = WasmPlugin::from_bytes(
            "demo.wasm",
            loop_guest(META),
            permissions("demo.test"),
            cfg,
            Arc::new(DenyHttpClient),
        )
        .unwrap();
        let error = plugin
            .resolve_blocking(url("https://demo.test/start"))
            .unwrap_err();
        assert!(error.to_string().to_lowercase().contains("fuel"));
    }

    #[test]
    fn wasi_imports_are_rejected() {
        let wat = format!(
            r#"(module
                (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
                (memory (export "memory") 1)
                (global $heap (mut i32) (i32.const 16384))
                (data (i32.const 1024) "{meta}")
                (func (export "rdl_alloc") (param $len i32) (result i32)
                    (local.get $len))
                (func (export "rdl_plugin_meta") (param $out i32) (param $cap i32) (result i32)
                    (i32.const 0))
                (func (export "rdl_resolve") (param i32 i32 i32 i32) (result i32)
                    (i32.const 0)))"#,
            meta = wat_escape(META)
        );
        let error = WasmPlugin::from_bytes(
            "demo.wasm",
            wat,
            permissions("demo.test"),
            config(),
            Arc::new(DenyHttpClient),
        )
        .unwrap_err();
        assert!(error.to_string().to_lowercase().contains("instance"));
    }

    #[test]
    fn memory_limit_is_enforced() {
        let guest = static_guest(META, RESOLVED).replace(
            "(memory (export \"memory\") 1)",
            "(memory (export \"memory\") 2)",
        );
        let mut cfg = config();
        cfg.max_memory_bytes = 64 * 1024;
        let error = WasmPlugin::from_bytes(
            "demo.wasm",
            guest,
            permissions("demo.test"),
            cfg,
            Arc::new(DenyHttpClient),
        )
        .unwrap_err();
        assert!(error.to_string().to_lowercase().contains("instance"));
    }

    #[test]
    fn invalid_meta_json_is_rejected() {
        let error = WasmPlugin::from_bytes(
            "demo.wasm",
            static_guest("not json", RESOLVED),
            permissions("demo.test"),
            config(),
            Arc::new(DenyHttpClient),
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid metadata"));
    }

    #[test]
    fn load_dir_reads_permission_sidecar() {
        let dir = std::env::temp_dir().join(format!("rdl-wasm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("demo.wasm"), static_guest(META, RESOLVED)).unwrap();
        let sidecar = serde_json::to_string(&permissions("demo.test")).unwrap();
        std::fs::write(dir.join("demo.json"), sidecar).unwrap();

        let loaded = load_dir(&dir, &config(), Arc::new(DenyHttpClient));
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.len(), 1);
        let plugin = loaded.into_iter().next().unwrap().unwrap();
        assert_eq!(plugin.meta().id, "demo.wasm");
        assert!(plugin
            .resolve_blocking(url("https://demo.test/start"))
            .is_ok());
    }

    #[test]
    fn missing_sidecar_denies_all_permissions() {
        let dir = std::env::temp_dir().join(format!("rdl-wasm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("demo.wasm"), static_guest(META, RESOLVED)).unwrap();

        let loaded = load_dir(&dir, &config(), Arc::new(DenyHttpClient));
        std::fs::remove_dir_all(&dir).unwrap();

        let plugin = loaded.into_iter().next().unwrap().unwrap();
        assert!(plugin.permissions().network_domains.is_empty());
        assert!(plugin
            .resolve_blocking(url("https://demo.test/start"))
            .is_err());
    }
}
