use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use isola::sandbox::{FsPerms, SandboxOptions, SandboxTemplate};
use napi_derive::napi;
use parking_lot::Mutex;
use serde::Deserialize;

use crate::{
    env::Env,
    error::{Error, Result, invalid_argument},
    sandbox::SandboxCore,
};

// ---------------------------------------------------------------------------
// Config types (mirrors Python SDK patterns)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ConfiguredMount {
    host: PathBuf,
    guest: String,
    perms: FsPerms,
}

#[derive(Clone, Copy, Debug)]
enum RuntimeFlavor {
    Python,
    Js,
}

impl RuntimeFlavor {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "python" => Ok(Self::Python),
            "js" => Ok(Self::Js),
            _ => Err(invalid_argument(format!(
                "unsupported runtime '{name}', expected 'python' or 'js'"
            ))),
        }
    }

    const fn bundle_file(self) -> &'static str {
        match self {
            Self::Python => "python.wasm",
            Self::Js => "js.wasm",
        }
    }

    const fn uses_runtime_lib_mount(self) -> bool {
        matches!(self, Self::Python)
    }
}

#[derive(Clone, Debug)]
enum CacheDirConfig {
    Auto,
    Disabled,
    Custom(PathBuf),
}

#[derive(Clone, Debug)]
struct ContextConfig {
    cache_dir: CacheDirConfig,
    max_memory: Option<usize>,
    prelude: Option<String>,
    runtime_lib_dir: Option<PathBuf>,
    mounts: Vec<ConfiguredMount>,
    env: Vec<(String, String)>,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            cache_dir: CacheDirConfig::Auto,
            max_memory: None,
            prelude: None,
            runtime_lib_dir: None,
            mounts: Vec::new(),
            env: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PendingSandboxConfig {
    pub(crate) max_memory: Option<usize>,
    pub(crate) mounts: Vec<ConfiguredMount>,
    pub(crate) env: Vec<(String, String)>,
}

impl PendingSandboxConfig {
    pub(crate) fn to_options(&self) -> SandboxOptions {
        let mut options = SandboxOptions::default();
        if let Some(max_memory) = self.max_memory {
            options = options.max_memory(max_memory);
        }
        for mapping in &self.mounts {
            options = options.mount(&mapping.host, &mapping.guest, mapping.perms);
        }
        for (k, v) in &self.env {
            options = options.env(k, v);
        }
        options
    }

    pub(crate) fn apply_patch(&mut self, patch: SandboxConfigPatch) -> Result<()> {
        if let Some(max_memory) = patch.max_memory {
            self.max_memory = max_memory
                .map(|value| {
                    value
                        .try_into()
                        .map_err(|_| invalid_argument("max_memory exceeds usize"))
                })
                .transpose()?;
        }
        if let Some(mounts) = patch.mounts {
            self.mounts = parse_mounts(mounts)?;
        }
        if let Some(env) = patch.env {
            self.env = env.into_iter().collect();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Config patch types (serde)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(clippy::option_option, reason = "JSON patch needs tri-state fields")]
struct ContextConfigPatch {
    #[serde(default)]
    cache_dir: Option<Option<String>>,
    #[serde(default)]
    max_memory: Option<Option<u64>>,
    #[serde(default)]
    prelude: Option<Option<String>>,
    #[serde(default)]
    runtime_lib_dir: Option<Option<String>>,
    #[serde(default)]
    mounts: Option<Vec<MountConfigInput>>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(clippy::option_option, reason = "JSON patch needs tri-state fields")]
pub struct SandboxConfigPatch {
    #[serde(default)]
    pub(crate) max_memory: Option<Option<u64>>,
    #[serde(default)]
    pub(crate) mounts: Option<Vec<MountConfigInput>>,
    #[serde(default)]
    pub(crate) env: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
enum PermissionConfig {
    #[serde(rename = "read")]
    Read,
    #[serde(rename = "write")]
    Write,
    #[serde(rename = "read-write", alias = "read_write", alias = "rw")]
    ReadWrite,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountConfigInput {
    host: String,
    guest: String,
    #[serde(default)]
    dir_perms: Option<PermissionConfig>,
    #[serde(default)]
    file_perms: Option<PermissionConfig>,
}

fn parse_mounts(mounts: Vec<MountConfigInput>) -> Result<Vec<ConfiguredMount>> {
    let mut parsed = Vec::with_capacity(mounts.len());
    for mount in mounts {
        if mount.host.is_empty() {
            return Err(invalid_argument("mount host path must not be empty"));
        }
        if mount.guest.is_empty() {
            return Err(invalid_argument("mount guest path must not be empty"));
        }
        let perms = if matches!(
            mount.dir_perms,
            Some(PermissionConfig::Write | PermissionConfig::ReadWrite)
        ) || matches!(
            mount.file_perms,
            Some(PermissionConfig::Write | PermissionConfig::ReadWrite)
        ) {
            FsPerms::ReadWrite
        } else {
            FsPerms::ReadOnly
        };
        parsed.push(ConfiguredMount {
            host: PathBuf::from(mount.host),
            guest: mount.guest,
            perms,
        });
    }
    Ok(parsed)
}

fn dedupe_mounts_by_guest(mounts: Vec<ConfiguredMount>) -> Vec<ConfiguredMount> {
    let mut deduped = Vec::with_capacity(mounts.len());
    let mut indices = std::collections::HashMap::with_capacity(mounts.len());
    for mount in mounts {
        if let Some(&index) = indices.get(&mount.guest) {
            deduped[index] = mount;
        } else {
            indices.insert(mount.guest.clone(), deduped.len());
            deduped.push(mount);
        }
    }
    deduped
}

fn resolve_runtime_lib_dir(
    template_parent: &std::path::Path,
    runtime_lib_dir: Option<PathBuf>,
) -> PathBuf {
    if let Some(path) = runtime_lib_dir {
        return path;
    }
    std::env::var("WASI_PYTHON_RUNTIME").map_or_else(
        |_| {
            let mut lib_dir = template_parent.to_owned();
            lib_dir.push("wasm32-wasip2");
            lib_dir.push("wasi-deps");
            lib_dir.push("usr");
            lib_dir.push("local");
            lib_dir.push("lib");
            lib_dir
        },
        |runtime| {
            let mut lib_dir = PathBuf::from(runtime);
            lib_dir.push("lib");
            lib_dir
        },
    )
}

// ---------------------------------------------------------------------------
// ContextInner
// ---------------------------------------------------------------------------

struct ContextState {
    config: ContextConfig,
    template: Option<Arc<SandboxTemplate>>,
}

pub struct ContextInner {
    state: Mutex<ContextState>,
}

impl ContextInner {
    fn new() -> Self {
        Self {
            state: Mutex::new(ContextState {
                config: ContextConfig::default(),
                template: None,
            }),
        }
    }

    async fn initialize_template(
        &self,
        runtime_path: PathBuf,
        runtime: RuntimeFlavor,
    ) -> Result<()> {
        let mut wasm_path = runtime_path;
        wasm_path.push(runtime.bundle_file());

        let parent = wasm_path
            .parent()
            .ok_or_else(|| Error::Internal("Wasm path has no parent directory".to_string()))?
            .to_owned();

        let config = {
            let state = self.state.lock();
            if state.template.is_some() {
                return Err(invalid_argument("context template already initialized"));
            }
            state.config.clone()
        };

        let mut builder = SandboxTemplate::builder();
        builder = builder.prelude(config.prelude.clone());
        if let Some(max_memory) = config.max_memory {
            builder = builder.max_memory(max_memory);
        }

        builder = match config.cache_dir {
            CacheDirConfig::Auto => builder.cache(Some(parent.join("cache"))),
            CacheDirConfig::Disabled => builder.cache(None),
            CacheDirConfig::Custom(path) => builder.cache(Some(path)),
        };

        let mut mounts = config.mounts;
        if runtime.uses_runtime_lib_mount() {
            mounts.insert(
                0,
                ConfiguredMount {
                    host: resolve_runtime_lib_dir(&parent, config.runtime_lib_dir),
                    guest: "/lib".to_string(),
                    perms: FsPerms::ReadOnly,
                },
            );
        }
        mounts = dedupe_mounts_by_guest(mounts);

        for mapping in &mounts {
            builder = builder.mount(&mapping.host, &mapping.guest, mapping.perms);
        }

        for (k, v) in &config.env {
            builder = builder.env(k, v);
        }

        let template = builder
            .build(&wasm_path)
            .await
            .map_err(|e| Error::Internal(format!("Failed to load runtime template: {e}")))?;

        let mut state = self.state.lock();
        if state.template.is_some() {
            return Err(invalid_argument("context template already initialized"));
        }
        state.template = Some(Arc::new(template));
        drop(state);
        Ok(())
    }

    pub(crate) async fn instantiate_sandbox(
        &self,
        options: SandboxOptions,
        env: Env,
    ) -> Result<isola::sandbox::Sandbox<Env>> {
        let template = {
            let state = self.state.lock();
            state
                .template
                .clone()
                .ok_or_else(|| invalid_argument("runtime template not initialized"))?
        };
        template
            .instantiate(env, options)
            .await
            .map_err(|e| Error::Internal(format!("Failed to create instance: {e}")))
    }

    fn has_template(&self) -> bool {
        self.state.lock().template.is_some()
    }
}

impl ContextConfig {
    fn apply_patch(&mut self, patch: ContextConfigPatch) -> Result<()> {
        if let Some(cache_dir) = patch.cache_dir {
            self.cache_dir = cache_dir.map_or(CacheDirConfig::Disabled, |path| {
                CacheDirConfig::Custom(PathBuf::from(path))
            });
        }
        if let Some(max_memory) = patch.max_memory {
            self.max_memory = max_memory
                .map(|value| {
                    value
                        .try_into()
                        .map_err(|_| invalid_argument("max_memory exceeds usize"))
                })
                .transpose()?;
        }
        if let Some(prelude) = patch.prelude {
            self.prelude = prelude;
        }
        if let Some(runtime_lib_dir) = patch.runtime_lib_dir {
            self.runtime_lib_dir = runtime_lib_dir.map(PathBuf::from);
        }
        if let Some(mounts) = patch.mounts {
            self.mounts = parse_mounts(mounts)?;
        }
        if let Some(env) = patch.env {
            self.env = env.into_iter().collect();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// N-API class: ContextCore
// ---------------------------------------------------------------------------

#[napi]
pub struct ContextCore {
    inner: Option<Arc<ContextInner>>,
}

impl ContextCore {
    fn inner_ref(&self) -> Result<&Arc<ContextInner>> {
        self.inner
            .as_ref()
            .ok_or_else(|| invalid_argument("context is closed"))
    }
}

#[napi]
impl ContextCore {
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            inner: Some(Arc::new(ContextInner::new())),
        }
    }

    #[napi]
    pub fn configure(&self, config: serde_json::Value) -> napi::Result<()> {
        let patch: ContextConfigPatch = serde_json::from_value(config)
            .map_err(|e| napi::Error::from(invalid_argument(format!("invalid config: {e}"))))?;
        let inner = self.inner_ref().map_err(napi::Error::from)?;
        let mut state = inner.state.lock();
        if state.template.is_some() {
            return Err(napi::Error::from(invalid_argument(
                "context template already initialized",
            )));
        }
        state.config.apply_patch(patch).map_err(napi::Error::from)
    }

    #[napi]
    pub async fn initialize_template(
        &self,
        runtime_path: String,
        runtime_name: Option<String>,
    ) -> napi::Result<()> {
        let inner = Arc::clone(self.inner_ref().map_err(napi::Error::from)?);
        let runtime_path = PathBuf::from(runtime_path);
        let runtime_name = runtime_name.as_deref().unwrap_or("python");
        let runtime = RuntimeFlavor::parse(runtime_name).map_err(napi::Error::from)?;
        inner
            .initialize_template(runtime_path, runtime)
            .await
            .map_err(napi::Error::from)
    }

    #[napi]
    pub fn instantiate(&self) -> napi::Result<SandboxCore> {
        let inner = Arc::clone(self.inner_ref().map_err(napi::Error::from)?);
        if !inner.has_template() {
            return Err(napi::Error::from(invalid_argument(
                "runtime template not initialized",
            )));
        }
        Ok(SandboxCore::new(inner))
    }

    #[napi]
    pub fn close(&mut self) {
        self.inner = None;
    }
}
