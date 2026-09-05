//! Template construction and sandbox lifecycle APIs.
//!
//! Typical flow:
//! 1. Build a [`SandboxTemplate`](crate::sandbox::SandboxTemplate) with
//!    [`SandboxTemplateBuilder`](crate::sandbox::SandboxTemplateBuilder).
//! 2. Instantiate a [`Sandbox`](crate::sandbox::Sandbox) from that template and
//!    a [`Host`](crate::host::Host) implementation.
//! 3. Evaluate scripts or call guest functions with
//!    [`Arg`](crate::sandbox::Arg) inputs.
//!
//! [`SandboxOptions`](crate::sandbox::SandboxOptions) controls
//! per-instantiation options (for example mount/env overrides and per-sandbox
//! memory cap), while template defaults are configured through
//! [`SandboxTemplateBuilder`](crate::sandbox::SandboxTemplateBuilder). Source
//! loaded into a sandbox and guest globals created by calls remain available
//! for that sandbox's lifetime.

#[cfg(feature = "serde")]
mod args_macro;

use std::{
    any::{Any, TypeId},
    collections::HashMap,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use futures::Stream;
use parking_lot::Mutex;
use wasmtime::{
    Engine, Store,
    component::{Component, InstancePre},
};
pub use wasmtime_wasi::FsPerms;

#[cfg(feature = "serde")]
pub use crate::args;
use crate::{
    host::{BoxError, Host, OutputTarget},
    internal::{
        module::{
            ModuleConfig as InternalModuleConfig,
            call::CallCleanup,
            compile::load_or_compile_component,
            configure::configure_engine,
            epoch::{EpochTickerRegistration, global_epoch_ticker},
        },
        sandbox::{
            HostView as _, InstanceState, Sandbox as WasmSandbox, SandboxPre, ValueIterator,
            exports::{self, Argument as RawArgument, Value as WasmValue},
        },
    },
    value::Value,
};

/// Result type used by `isola::sandbox` APIs.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Error produced while building or executing a sandbox.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Guest/user-code failure.
    #[error("{message}")]
    UserCode {
        /// Error text supplied by the guest language runtime.
        message: String,
    },

    /// Failure from Wasmtime APIs.
    #[error("wasm error: {0}")]
    Wasm(#[from] wasmtime::Error),

    /// Filesystem or OS-level runtime failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Other host/runtime failures.
    #[error("runtime error: {0}")]
    Other(#[from] BoxError),
}

impl From<exports::Error> for Error {
    fn from(value: exports::Error) -> Self {
        let exports::Error { code, message } = value;
        match code {
            exports::ErrorCode::Aborted => Self::UserCode { message },
            exports::ErrorCode::Internal => {
                Self::Other(std::io::Error::other(format!("[{code:?}] {message}")).into())
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DirectoryMapping {
    pub(crate) host: PathBuf,
    pub(crate) guest: String,
    pub(crate) perms: FsPerms,
}

impl DirectoryMapping {
    pub fn new(host: impl Into<PathBuf>, guest: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            guest: guest.into(),
            perms: FsPerms::ReadOnly,
        }
    }

    pub const fn with_permissions(mut self, perms: FsPerms) -> Self {
        self.perms = perms;
        self
    }
}

/// Function argument passed to guest `call-func`.
pub enum Arg {
    /// Positional argument containing one encoded value.
    Positional(Value),
    /// Named argument containing its guest-visible name and encoded value.
    Named(String, Value),
    /// Positional argument read lazily from an asynchronous value stream.
    PositionalStream(Pin<Box<dyn Stream<Item = Value> + Send + 'static>>),
    /// Named argument read lazily from an asynchronous value stream.
    NamedStream(String, Pin<Box<dyn Stream<Item = Value> + Send + 'static>>),
}

impl core::fmt::Debug for Arg {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Positional(value) => f
                .debug_struct("Arg::Positional")
                .field("value", value)
                .finish(),
            Self::Named(name, value) => f
                .debug_struct("Arg::Named")
                .field("name", name)
                .field("value", value)
                .finish(),
            Self::PositionalStream(..) => f
                .debug_struct("Arg::PositionalStream")
                .field("stream", &"<stream>")
                .finish(),
            Self::NamedStream(name, ..) => f
                .debug_struct("Arg::NamedStream")
                .field("name", name)
                .field("stream", &"<stream>")
                .finish(),
        }
    }
}

/// Builder for compiling a reusable [`SandboxTemplate`].
///
/// `SandboxTemplateBuilder` configures template-level defaults shared by every
/// sandbox instantiated from the resulting [`SandboxTemplate`], including base
/// mount/env settings.
#[derive(Default)]
pub struct SandboxTemplateBuilder {
    pub(crate) cache: Option<PathBuf>,
    pub(crate) base_options: SandboxOptions,
    pub(crate) prelude: Option<String>,
}

/// Compiled sandbox template that can instantiate multiple sandboxes.
///
/// A `SandboxTemplate` is an immutable, reusable compiled artifact.
/// Instantiate it to create one or more independent [`Sandbox`] values with
/// isolated runtime state.
pub struct SandboxTemplate {
    pub(crate) base_options: SandboxOptions,
    pub(crate) engine: Engine,
    pub(crate) component: Component,
    pub(crate) ticker: Arc<EpochTickerRegistration>,
    pre_instances: Mutex<HashMap<TypeId, Box<dyn Any + Send + Sync>>>,
}

/// Live guest instance with mutable execution state.
///
/// A `Sandbox` belongs to a single instantiation of a [`SandboxTemplate`]. It
/// carries the guest state for eval/call operations and is isolated from other
/// sandboxes created from the same template.
pub struct Sandbox<H: Host> {
    pub(crate) store: Store<InstanceState<H>>,
    pub(crate) bindings: WasmSandbox,
    /// Keeps the epoch ticker alive for the lifetime of this sandbox.
    pub(crate) _ticker: Arc<EpochTickerRegistration>,
}

/// Per-instantiation policy overrides for a [`Sandbox`].
///
/// Default options inherit every setting from the [`SandboxTemplate`]. Values
/// set here are merged with the template defaults during
/// [`SandboxTemplate::instantiate`].
#[derive(Clone, Debug, Default)]
pub struct SandboxOptions {
    pub(crate) max_memory: Option<usize>,
    pub(crate) directory_mappings: Vec<DirectoryMapping>,
    pub(crate) env: Vec<(String, String)>,
}

impl SandboxOptions {
    /// Override the memory hard limit for this sandbox.
    #[must_use]
    pub const fn max_memory(mut self, max_memory: usize) -> Self {
        self.max_memory = Some(max_memory);
        self
    }

    /// Mount a host directory into this sandbox instance.
    ///
    /// If a guest path duplicates a module-level mount, this mount replaces it
    /// for that instance.
    #[must_use]
    pub fn mount(
        mut self,
        host_path: impl AsRef<Path>,
        guest_path: impl AsRef<str>,
        perms: FsPerms,
    ) -> Self {
        self.directory_mappings.push(
            DirectoryMapping::new(host_path.as_ref(), guest_path.as_ref()).with_permissions(perms),
        );
        self
    }

    /// Add an environment variable for this sandbox instance.
    ///
    /// If the same key is set multiple times, the last value wins.
    #[must_use]
    pub fn env(mut self, k: impl AsRef<str>, v: impl AsRef<str>) -> Self {
        self.env
            .push((k.as_ref().to_string(), v.as_ref().to_string()));
        self
    }

    /// Merge `overrides` into this options value and return the merged result.
    ///
    /// Merge behavior:
    /// - `max_memory`: override wins when set.
    /// - mounts: override entries replace on guest-path collision.
    /// - `env`: override values replace by matching key.
    #[must_use]
    pub fn merged_with(&self, overrides: &Self) -> Self {
        self.merged_with_owned(overrides.clone())
    }

    fn merged_with_owned(&self, overrides: Self) -> Self {
        let mut merged = self.clone();

        if let Some(max_memory) = overrides.max_memory {
            merged.max_memory = Some(max_memory);
        }

        let mut mount_indices =
            std::collections::HashMap::with_capacity(merged.directory_mappings.len());
        for (index, mapping) in merged.directory_mappings.iter().enumerate() {
            mount_indices.insert(mapping.guest.clone(), index);
        }
        for mapping in overrides.directory_mappings {
            if let Some(&index) = mount_indices.get(&mapping.guest) {
                merged.directory_mappings[index] = mapping;
            } else {
                mount_indices.insert(mapping.guest.clone(), merged.directory_mappings.len());
                merged.directory_mappings.push(mapping);
            }
        }
        let mut env_indices = std::collections::HashMap::with_capacity(merged.env.len());
        for (index, (key, _)) in merged.env.iter().enumerate() {
            env_indices.insert(key.clone(), index);
        }
        for (key, value) in overrides.env {
            if let Some(&index) = env_indices.get(&key) {
                merged.env[index].1 = value;
            } else {
                env_indices.insert(key.clone(), merged.env.len());
                merged.env.push((key, value));
            }
        }

        merged
    }
}

/// Collected output from
/// [`Sandbox::call`](crate::sandbox::Sandbox::call).
#[derive(Debug, Default)]
pub struct CallOutput {
    /// Values yielded or explicitly emitted by the guest, in emission order.
    pub items: Vec<Value>,
    /// Final guest return value, or `None` when no final value was encoded.
    ///
    /// A guest-language `None` or `null` is an encoded CBOR value and is
    /// therefore represented as `Some(Value)`.
    pub result: Option<Value>,
}

impl SandboxTemplateBuilder {
    /// Set the optional component cache directory.
    ///
    /// When set, compiled artifacts are cached on disk and reused across
    /// builds.
    #[must_use]
    pub fn cache(mut self, cache: Option<std::path::PathBuf>) -> Self {
        self.cache = cache;
        self
    }

    /// Set the per-sandbox memory hard limit.
    ///
    /// Defaults to unlimited (`usize::MAX`).
    #[must_use]
    pub const fn max_memory(mut self, max_memory: usize) -> Self {
        self.base_options.max_memory = Some(max_memory);
        self
    }

    /// Set base directory mappings shared by all sandboxes from this template.
    ///
    /// These mappings can be extended or overridden per instantiation via
    /// [`SandboxOptions::mount`](crate::sandbox::SandboxOptions::mount).
    ///
    /// This API matches the WASI-style preopen configuration shape.
    #[must_use]
    pub fn mount(
        mut self,
        host_path: impl AsRef<Path>,
        guest_path: impl AsRef<str>,
        perms: FsPerms,
    ) -> Self {
        self.base_options = self.base_options.mount(host_path, guest_path, perms);
        self
    }

    /// Add an environment variable that will be present in sandbox WASI env.
    ///
    /// If the same key is set multiple times, the last value wins.
    #[must_use]
    pub fn env(mut self, k: impl AsRef<str>, v: impl AsRef<str>) -> Self {
        self.base_options = self.base_options.env(k, v);
        self
    }

    /// Set optional guest prelude code executed during template initialization.
    ///
    /// Prelude state is captured in the compiled template and is therefore
    /// present in every sandbox instantiated from it. `None` disables the
    /// prelude.
    #[must_use]
    pub fn prelude(mut self, prelude: Option<String>) -> Self {
        self.prelude = prelude;
        self
    }

    /// Compile and initialize a reusable template from an Isola runtime
    /// component.
    ///
    /// `wasm` must identify a Python or JavaScript runtime component compatible
    /// with this version of Isola. Configured mounts and environment variables
    /// are available while the component is initialized and snapshotted.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be resolved or read, a mount cannot
    /// be opened, the component is incompatible, initialization fails, or a
    /// compiled artifact cannot be cached.
    pub async fn build(self, wasm: impl AsRef<Path>) -> Result<SandboxTemplate> {
        let wasm_path = std::fs::canonicalize(wasm.as_ref()).map_err(Error::from)?;
        let base_options = self.base_options;
        let cfg = InternalModuleConfig {
            cache: self.cache.clone(),
            max_memory: base_options.max_memory.unwrap_or(usize::MAX),
            directory_mappings: base_options.directory_mappings.clone(),
            env: base_options.env.clone(),
            prelude: self.prelude.clone(),
        };

        let mut engine_cfg = wasmtime::Config::default();
        configure_engine(&mut engine_cfg);
        let engine = Engine::new(&engine_cfg).map_err(Error::Wasm)?;

        let component =
            load_or_compile_component(&engine, &wasm_path, &cfg.directory_mappings, &cfg).await?;
        Engine::tls_eager_initialize();
        let ticker = global_epoch_ticker()
            .map_err(Error::from)?
            .register(engine.clone());

        Ok(SandboxTemplate {
            base_options,
            engine,
            component,
            ticker,
            pre_instances: Mutex::new(HashMap::new()),
        })
    }
}

impl SandboxTemplate {
    /// Create a builder for a reusable sandbox template.
    #[must_use]
    pub fn builder() -> SandboxTemplateBuilder {
        SandboxTemplateBuilder::default()
    }

    /// Create a new sandbox instance from this compiled template.
    ///
    /// Each sandbox has isolated mutable guest state. Per-sandbox
    /// [`SandboxOptions`] are merged with the template defaults configured on
    /// [`SandboxTemplateBuilder`].
    ///
    /// # Errors
    /// Returns an error if instantiation fails.
    pub async fn instantiate<H: Host>(
        &self,
        host: H,
        options: SandboxOptions,
    ) -> Result<Sandbox<H>> {
        let ticker = Arc::clone(&self.ticker);
        let merged = self.base_options.merged_with_owned(options);

        let mut store = InstanceState::new(
            &self.engine,
            &merged.directory_mappings,
            &merged.env,
            merged.max_memory.unwrap_or(usize::MAX),
            host,
        )
        .map_err(Error::Wasm)?;
        store.epoch_deadline_async_yield_and_update(1);

        let pre = {
            let mut cached = self.pre_instances.lock();
            let host_type = TypeId::of::<H>();
            if let std::collections::hash_map::Entry::Vacant(entry) = cached.entry(host_type) {
                let linker = InstanceState::<H>::new_linker(&self.engine).map_err(Error::Wasm)?;
                let pre = linker
                    .instantiate_pre(&self.component)
                    .map_err(Error::Wasm)?;
                entry.insert(Box::new(pre));
            }
            cached
                .get(&host_type)
                .and_then(|pre| pre.downcast_ref::<InstancePre<InstanceState<H>>>())
                .ok_or_else(|| {
                    Error::Other(
                        std::io::Error::other(
                            "pre-instantiation cache type did not match its TypeId key",
                        )
                        .into(),
                    )
                })?
                .clone()
        };
        let bindings = SandboxPre::new(pre)
            .map_err(Error::Wasm)?
            .instantiate_async(&mut store)
            .await
            .map_err(Error::Wasm)?;

        Ok(Sandbox {
            store,
            bindings,
            _ticker: ticker,
        })
    }
}

impl<H: Host> Sandbox<H> {
    /// Evaluate source code in this sandbox's persistent guest scope.
    ///
    /// Guest logs and any explicitly emitted values are delivered to `target`.
    /// Definitions created by the script remain available to later calls on
    /// this sandbox.
    ///
    /// # Errors
    ///
    /// Returns an error if the guest rejects or fails while evaluating the
    /// script, output delivery fails, or the WebAssembly runtime traps.
    pub async fn eval_script(
        &mut self,
        code: impl AsRef<str>,
        target: impl Into<OutputTarget>,
    ) -> Result<()> {
        self.eval_script_impl(code.as_ref(), target.into()).await
    }

    async fn eval_script_impl(&mut self, code: &str, target: OutputTarget) -> Result<()> {
        let mut store = CallCleanup::new(&mut self.store);
        store.set_output_target(target);
        let result = self
            .bindings
            .isola_script_runtime()
            .func_eval_script()
            .call_async(&mut store, (code.to_string(),))
            .await;
        let flush_result = store.data_mut().flush_logs().await.map_err(Error::Wasm);
        result.map_err(Error::Wasm)?.0?;
        flush_result?;
        Ok(())
    }

    /// Evaluate a file using its exact guest-visible path string.
    ///
    /// The file must be visible through a mount configured on the template or
    /// this sandbox instance. Definitions created by the file remain available
    /// to later calls.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be loaded by the guest, evaluation
    /// fails, output delivery fails, or the WebAssembly runtime traps.
    pub async fn eval_file(
        &mut self,
        guest_path: &str,
        target: impl Into<OutputTarget>,
    ) -> Result<()> {
        self.eval_file_impl(guest_path, target.into()).await
    }

    async fn eval_file_impl(&mut self, guest_path: &str, target: OutputTarget) -> Result<()> {
        let mut store = CallCleanup::new(&mut self.store);
        store.set_output_target(target);
        let result = self
            .bindings
            .isola_script_runtime()
            .func_eval_file()
            .call_async(&mut store, (guest_path.to_string(),))
            .await;
        let flush_result = store.data_mut().flush_logs().await.map_err(Error::Wasm);
        result.map_err(Error::Wasm)?.0?;
        flush_result?;
        Ok(())
    }

    /// Call a guest function and deliver output incrementally to a target.
    ///
    /// Values yielded or explicitly emitted by the guest are delivered as item
    /// events. The final return value is delivered as a completion event.
    ///
    /// # Errors
    ///
    /// Returns an error if the function is missing, guest execution fails,
    /// output delivery fails, or the WebAssembly runtime traps.
    pub async fn call_with_sink<I>(
        &mut self,
        function: &str,
        args: I,
        target: impl Into<OutputTarget>,
    ) -> Result<()>
    where
        I: IntoIterator<Item = Arg>,
    {
        self.call_impl(function, args, target.into()).await
    }

    /// Call a guest function and collect emitted items/final result.
    ///
    /// This is the collecting counterpart to [`Sandbox::call_with_sink`]. See
    /// [`CallOutput`] for how yielded and final values are represented.
    ///
    /// # Errors
    ///
    /// Returns an error if the function is missing, guest execution fails, or
    /// the WebAssembly runtime traps.
    pub async fn call<I>(&mut self, function: &str, args: I) -> Result<CallOutput>
    where
        I: IntoIterator<Item = Arg>,
    {
        self.store.data_mut().capture_output = Some(CallOutput::default());
        let target = OutputTarget::capture();
        self.call_impl(function, args, target).await?;
        Ok(self
            .store
            .data_mut()
            .capture_output
            .take()
            .unwrap_or_default())
    }

    async fn call_impl<I>(&mut self, function: &str, args: I, target: OutputTarget) -> Result<()>
    where
        I: IntoIterator<Item = Arg>,
    {
        let mut store = CallCleanup::new(&mut self.store);
        let internal_args = args
            .into_iter()
            .map(|arg| match arg {
                Arg::Positional(value) => Ok(RawArgument {
                    name: None,
                    value: WasmValue::Cbor(value.into_cbor().into()),
                }),
                Arg::Named(name, value) => Ok(RawArgument {
                    name: Some(name),
                    value: WasmValue::Cbor(value.into_cbor().into()),
                }),
                Arg::PositionalStream(stream_arg) => {
                    let iter = store
                        .data_mut()
                        .table()
                        .push(ValueIterator::new(stream_arg))
                        .map_err(|e| Error::Other(e.into()))?;
                    Ok(RawArgument {
                        name: None,
                        value: WasmValue::CborIterator(iter),
                    })
                }
                Arg::NamedStream(name, stream_arg) => {
                    let iter = store
                        .data_mut()
                        .table()
                        .push(ValueIterator::new(stream_arg))
                        .map_err(|e| Error::Other(e.into()))?;
                    Ok(RawArgument {
                        name: Some(name),
                        value: WasmValue::CborIterator(iter),
                    })
                }
            })
            .collect::<Result<Vec<RawArgument>>>()?;

        store.set_output_target(target);
        let result = self
            .bindings
            .isola_script_runtime()
            .func_call_func()
            .call_async(&mut store, (function.to_string(), internal_args))
            .await;
        let flush_result = store.data_mut().flush_logs().await.map_err(Error::Wasm);
        result.map_err(Error::Wasm)?.0?;
        flush_result?;
        Ok(())
    }

    /// Return the current guest WebAssembly linear-memory allocation in bytes.
    ///
    /// This does not include host-side allocations such as streamed values or
    /// cached components.
    #[must_use]
    pub fn memory_usage(&self) -> usize {
        self.store.data().limiter.current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_configuration_is_fluent() {
        let options = SandboxOptions::default()
            .max_memory(1024)
            .mount("/host", "/guest", FsPerms::ReadOnly)
            .env("KEY", "value");

        assert_eq!(options.max_memory, Some(1024));
        assert_eq!(options.directory_mappings.len(), 1);
        assert_eq!(options.env, [("KEY".to_string(), "value".to_string())]);

        let _builder = SandboxTemplate::builder()
            .max_memory(1024)
            .mount("/host", "/guest", FsPerms::ReadOnly)
            .env("KEY", "value");
    }
}
