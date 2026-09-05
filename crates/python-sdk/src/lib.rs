mod serde;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use ::serde::Deserialize;
use bytes::Bytes;
use futures::stream;
use http_body::Frame;
use isola::{
    host::{
        BoxError, Host, HttpBodyStream, HttpRequest, HttpResponse, LogLevel, OutputEvent,
        OutputTarget,
    },
    sandbox::{Arg, FsPerms, Sandbox, SandboxOptions, SandboxTemplate},
    value::Value,
};
use parking_lot::Mutex;
use pyo3::{
    create_exception,
    exceptions::PyException,
    prelude::*,
    types::{PyAnyMethods, PyBytes, PyDict, PyModule},
};
use pyo3_async_runtimes::TaskLocals;

use crate::serde::{py_to_value, value_to_py};

const DEFAULT_STREAM_CAPACITY: usize = 1024;
const DEFAULT_HTTP_STREAM_CAPACITY: usize = 8;

create_exception!(_isola, IsolaError, PyException);
create_exception!(_isola, InvalidArgumentError, IsolaError);
create_exception!(_isola, InternalError, IsolaError);
create_exception!(_isola, StreamFullError, IsolaError);
create_exception!(_isola, StreamClosedError, IsolaError);

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Stream is full")]
    StreamFull,

    #[error("Stream is closed")]
    StreamClosed,
}

type Result<T> = std::result::Result<T, Error>;

fn to_py_err(err: Error) -> PyErr {
    match err {
        Error::InvalidArgument(msg) => InvalidArgumentError::new_err(msg),
        Error::Internal(msg) => InternalError::new_err(msg),
        Error::StreamFull => StreamFullError::new_err("Stream is full"),
        Error::StreamClosed => StreamClosedError::new_err("Stream is closed"),
    }
}

fn invalid_argument(msg: impl Into<String>) -> Error {
    Error::InvalidArgument(msg.into())
}

#[derive(Clone, Debug)]
struct ConfiguredMount {
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
struct PendingSandboxConfig {
    max_memory: Option<usize>,
    mounts: Vec<ConfiguredMount>,
    env: Vec<(String, String)>,
}

impl PendingSandboxConfig {
    fn to_options(&self) -> SandboxOptions {
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

    fn apply_patch(&mut self, patch: SandboxConfigPatch) -> Result<()> {
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

struct ContextState {
    config: ContextConfig,
    template: Option<Arc<SandboxTemplate>>,
}

struct ContextInner {
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

    async fn instantiate_sandbox(&self, options: SandboxOptions, env: Env) -> Result<Sandbox<Env>> {
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
        let state = self.state.lock();
        state.template.is_some()
    }
}

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
struct MountConfigInput {
    host: String,
    guest: String,
    #[serde(default)]
    dir_perms: Option<PermissionConfig>,
    #[serde(default)]
    file_perms: Option<PermissionConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(clippy::option_option, reason = "JSON patch needs tri-state fields")]
struct SandboxConfigPatch {
    #[serde(default)]
    max_memory: Option<Option<u64>>,
    #[serde(default)]
    mounts: Option<Vec<MountConfigInput>>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
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

fn resolve_runtime_lib_dir(template_parent: &Path, runtime_lib_dir: Option<PathBuf>) -> PathBuf {
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

struct PyCallback {
    callback: Py<PyAny>,
}

impl PyCallback {
    fn emit(&self, event: CallbackEvent, data: Option<&str>) {
        Python::attach(|py| {
            let callback = self.callback.bind(py);
            if let Err(err) = callback.call1((event.as_str(), data)) {
                err.write_unraisable(py, Some(callback));
            }
        });
    }

    fn emit_value(&self, event: CallbackEvent, data: Option<&Value>) {
        Python::attach(|py| {
            let py_data: Py<PyAny> = match data {
                Some(v) => match value_to_py(py, v) {
                    Ok(obj) => obj,
                    Err(err) => {
                        err.write_unraisable(py, None::<&Bound<'_, PyAny>>);
                        return;
                    }
                },
                None => py.None(),
            };
            let callback = self.callback.bind(py);
            if let Err(err) = callback.call1((event.as_str(), py_data)) {
                err.write_unraisable(py, Some(callback));
            }
        });
    }
}

struct PyHttpHandler {
    callback: Py<PyAny>,
    event_loop: Py<PyAny>,
}

struct PyHostcallHandler {
    callback: Py<PyAny>,
    event_loop: Py<PyAny>,
}

enum HttpResponseBody {
    Empty,
    Buffered(Bytes),
    Stream(Py<PyAny>),
}

fn io_error(msg: impl Into<String>) -> BoxError {
    Box::new(std::io::Error::other(msg.into()))
}

fn py_error_to_box_error(prefix: &str, err: &PyErr) -> BoxError {
    io_error(format!("{prefix}: {err}"))
}

fn py_bytes_to_rust_bytes(value: &Bound<'_, PyAny>) -> std::result::Result<Bytes, BoxError> {
    if let Ok(py_bytes) = value.cast::<PyBytes>() {
        return Ok(Bytes::copy_from_slice(py_bytes.as_bytes()));
    }

    let py = value.py();
    let builtins = py
        .import("builtins")
        .map_err(|e| py_error_to_box_error("builtins", &e))?;
    let coerced = builtins
        .call_method1("bytes", (value,))
        .map_err(|e| py_error_to_box_error("body chunk must be bytes-like", &e))?;
    let py_bytes = coerced
        .cast::<PyBytes>()
        .map_err(|e| io_error(format!("body coercion failed: {e}")))?;
    Ok(Bytes::copy_from_slice(py_bytes.as_bytes()))
}

async fn await_python_coroutine(
    event_loop: &Py<PyAny>,
    create_coro: impl for<'py> FnOnce(Python<'py>) -> PyResult<Bound<'py, PyAny>> + Send + 'static,
) -> std::result::Result<Py<PyAny>, BoxError> {
    let event_loop = Python::attach(|py| event_loop.clone_ref(py));
    let py_future = Python::attach(|py| {
        let coro = create_coro(py).map_err(|e| py_error_to_box_error("invalid coroutine", &e))?;
        let locals = TaskLocals::new(event_loop.bind(py).clone());
        pyo3_async_runtimes::into_future_with_locals(&locals, coro)
            .map_err(|e| py_error_to_box_error("failed to schedule coroutine", &e))
    })?;

    py_future
        .await
        .map_err(|e| py_error_to_box_error("python coroutine failed", &e))
}

impl PyHttpHandler {
    const fn new(callback: Py<PyAny>, event_loop: Py<PyAny>) -> Self {
        Self {
            callback,
            event_loop,
        }
    }

    async fn await_coroutine(
        &self,
        create_coro: impl for<'py> FnOnce(Python<'py>) -> PyResult<Bound<'py, PyAny>> + Send + 'static,
    ) -> std::result::Result<Py<PyAny>, BoxError> {
        await_python_coroutine(&self.event_loop, create_coro).await
    }

    async fn invoke_http_handler(
        &self,
        incoming: &HttpRequest,
    ) -> std::result::Result<(http::response::Parts, HttpResponseBody), BoxError> {
        let method = incoming.method().as_str().to_owned();
        let url = incoming.uri().to_string();
        let headers = incoming
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|value| (k.as_str().to_string(), value.to_string()))
            })
            .collect::<Vec<_>>();
        let body = incoming.body().clone();
        let callback = Python::attach(|py| self.callback.clone_ref(py));

        let result = self
            .await_coroutine(move |py| {
                let headers_dict = PyDict::new(py);
                for (k, v) in headers {
                    headers_dict.set_item(k, v)?;
                }
                let body_obj = body.as_ref().map_or_else(
                    || py.None().into_bound(py),
                    |bytes| PyBytes::new(py, bytes).into_any(),
                );
                let callback = callback.bind(py);
                callback.call1((method, url, headers_dict, body_obj))
            })
            .await?;

        Python::attach(|py| {
            let tuple = result
                .bind(py)
                .extract::<(u16, Bound<'_, PyDict>, String, Py<PyAny>)>()
                .map_err(|e| py_error_to_box_error("invalid http response payload", &e))?;
            let (status, headers_dict, body_mode, body_payload) = tuple;

            let mut builder = http::Response::builder().status(status);
            for (k, v) in headers_dict.iter() {
                let name = k
                    .extract::<String>()
                    .map_err(|e| py_error_to_box_error("response header name must be str", &e))?;
                let value = v
                    .extract::<String>()
                    .map_err(|e| py_error_to_box_error("response header value must be str", &e))?;
                builder = builder.header(name, value);
            }
            let response = builder
                .body(())
                .map_err(|e| io_error(format!("invalid response metadata: {e}")))?;
            let (parts, ()) = response.into_parts();

            let body = match body_mode.as_str() {
                "none" => HttpResponseBody::Empty,
                "bytes" => {
                    let payload = body_payload.bind(py);
                    let bytes = py_bytes_to_rust_bytes(payload)?;
                    HttpResponseBody::Buffered(bytes)
                }
                "stream" => HttpResponseBody::Stream(body_payload),
                _ => return Err(io_error(format!("unknown body mode: {body_mode}"))),
            };

            Ok((parts, body))
        })
    }

    fn stream_from_async_iter(
        &self,
        iterator: Py<PyAny>,
    ) -> tokio::sync::mpsc::Receiver<std::result::Result<Frame<Bytes>, BoxError>> {
        let (tx, rx) = tokio::sync::mpsc::channel(DEFAULT_HTTP_STREAM_CAPACITY);
        let event_loop = Python::attach(|py| self.event_loop.clone_ref(py));
        let iterator = Arc::new(iterator);
        let anext = Python::attach(|py| -> PyResult<Py<PyAny>> {
            py.import("builtins")?
                .getattr("anext")
                .map(pyo3::Bound::unbind)
        })
        .map_err(|e| py_error_to_box_error("failed to access anext", &e));
        tokio::spawn(async move {
            let anext = match anext {
                Ok(anext) => anext,
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    return;
                }
            };
            loop {
                if tx.is_closed() {
                    break;
                }

                let iterator = Arc::clone(&iterator);
                let next_item = match Python::attach(|py| {
                    let coro = anext.bind(py).call1((iterator.bind(py),)).map_err(|e| {
                        py_error_to_box_error("failed to create anext coroutine", &e)
                    })?;
                    let locals = TaskLocals::new(event_loop.bind(py).clone());
                    pyo3_async_runtimes::into_future_with_locals(&locals, coro)
                        .map_err(|e| py_error_to_box_error("failed to schedule anext", &e))
                }) {
                    Ok(future) => future.await,
                    Err(err) => {
                        let _ = tx.send(Err(err)).await;
                        break;
                    }
                };

                match next_item {
                    Ok(value) => {
                        let bytes = Python::attach(|py| py_bytes_to_rust_bytes(value.bind(py)));
                        match bytes {
                            Ok(bytes) => {
                                if tx.send(Ok(Frame::data(bytes))).await.is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                let _ = tx.send(Err(err)).await;
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        let is_end_of_stream = Python::attach(|py| {
                            err.is_instance_of::<pyo3::exceptions::PyStopAsyncIteration>(py)
                        });
                        if is_end_of_stream {
                            break;
                        }
                        let stream_error = py_error_to_box_error("stream iterator failed", &err);
                        let _ = tx.send(Err(stream_error)).await;
                        break;
                    }
                }
            }
        });

        rx
    }
}

impl PyHostcallHandler {
    const fn new(callback: Py<PyAny>, event_loop: Py<PyAny>) -> Self {
        Self {
            callback,
            event_loop,
        }
    }

    async fn await_coroutine(
        &self,
        create_coro: impl for<'py> FnOnce(Python<'py>) -> PyResult<Bound<'py, PyAny>> + Send + 'static,
    ) -> std::result::Result<Py<PyAny>, BoxError> {
        await_python_coroutine(&self.event_loop, create_coro).await
    }

    async fn invoke_hostcall(
        &self,
        call_type: &str,
        payload: Value,
    ) -> std::result::Result<Value, BoxError> {
        let call_type = call_type.to_owned();
        let py_payload = Python::attach(|py| value_to_py(py, &payload))
            .map_err(|e| io_error(format!("failed to encode hostcall payload: {e}")))?;
        let callback = Python::attach(|py| self.callback.clone_ref(py));

        let result = self
            .await_coroutine(move |py| {
                let callback = callback.bind(py);
                callback.call1((call_type, py_payload.bind(py)))
            })
            .await?;

        Python::attach(|py| {
            py_to_value(result.bind(py))
                .map_err(|e| io_error(format!("invalid hostcall response: {e}")))
        })
    }
}

#[derive(Clone, Copy)]
enum CallbackEvent {
    Result,
    End,
    Stdout,
    Stderr,
    Error,
    Log,
}

impl CallbackEvent {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Result => "result",
            Self::End => "end",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Error => "error",
            Self::Log => "log",
        }
    }
}

#[derive(Default)]
struct OutputData {
    result_json: Vec<String>,
    final_json: Option<String>,
    stdout: Vec<String>,
    stderr: Vec<String>,
    logs: Vec<String>,
    errors: Vec<String>,
}

#[derive(Clone)]
struct OutputCollector {
    callback: Option<Arc<PyCallback>>,
    data: Arc<Mutex<OutputData>>,
}

impl OutputCollector {
    fn new(callback: Option<Arc<PyCallback>>) -> Self {
        Self {
            callback,
            data: Arc::new(Mutex::new(OutputData::default())),
        }
    }

    fn record<F>(&self, f: F)
    where
        F: FnOnce(&mut OutputData),
    {
        let mut data = self.data.lock();
        f(&mut data);
    }

    fn emit(&self, event: CallbackEvent, payload: Option<&str>) {
        if let Some(callback) = &self.callback {
            callback.emit(event, payload);
        }
    }

    fn emit_error_message(&self, message: &str) {
        self.record(|data| data.errors.push(message.to_owned()));
        self.emit(CallbackEvent::Error, Some(message));
    }

    fn into_result(self) -> PyRunResult {
        let data = std::mem::take(&mut *self.data.lock());
        PyRunResult {
            result_json: data.result_json,
            final_json: data.final_json,
            stdout: data.stdout,
            stderr: data.stderr,
            logs: data.logs,
            errors: data.errors,
        }
    }
}

impl OutputCollector {
    fn target(&self) -> OutputTarget {
        let collector = self.clone();
        OutputTarget::synchronous(move |event| collector.handle_event(event))
    }

    fn handle_event(&self, event: OutputEvent) -> std::result::Result<(), BoxError> {
        match event {
            OutputEvent::Item(item) => {
                let text = item.to_json_str().map_err(|e| -> BoxError {
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                })?;
                self.record(|data| data.result_json.push(text));
                if let Some(callback) = &self.callback {
                    callback.emit_value(CallbackEvent::Result, Some(&item));
                }
            }
            OutputEvent::Complete(item) => {
                if let Some(item) = item {
                    let text = item.to_json_str().map_err(|e| -> BoxError {
                        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                    })?;
                    self.record(|data| data.final_json = Some(text));
                    if let Some(callback) = &self.callback {
                        callback.emit_value(CallbackEvent::End, Some(&item));
                    }
                } else {
                    self.emit(CallbackEvent::End, None);
                }
            }
            OutputEvent::Log { level, message, .. } => {
                let callback_event = match level {
                    LogLevel::Stdout => CallbackEvent::Stdout,
                    LogLevel::Stderr => CallbackEvent::Stderr,
                    _ => CallbackEvent::Log,
                };
                self.emit(callback_event, Some(&message));
                self.record(|data| match level {
                    LogLevel::Stdout => data.stdout.push(message),
                    LogLevel::Stderr => data.stderr.push(message),
                    _ => data.logs.push(message),
                });
            }
            _ => {}
        }
        Ok(())
    }
}

enum SandboxInner {
    Uninitialized,
    Pending {
        config: PendingSandboxConfig,
        callback: Option<Arc<PyCallback>>,
        http_handler: Option<Arc<PyHttpHandler>>,
        hostcall_handler: Option<Arc<PyHostcallHandler>>,
    },
    Running {
        sandbox: Option<Sandbox<Env>>,
        callback: Option<Arc<PyCallback>>,
    },
}

struct RunningSandboxLease {
    inner: Arc<Mutex<SandboxInner>>,
    sandbox: Option<Sandbox<Env>>,
}

impl RunningSandboxLease {
    const fn new(inner: Arc<Mutex<SandboxInner>>, sandbox: Sandbox<Env>) -> Self {
        Self {
            inner,
            sandbox: Some(sandbox),
        }
    }

    const fn sandbox_mut(&mut self) -> &mut Sandbox<Env> {
        self.sandbox
            .as_mut()
            .expect("running sandbox lease must contain sandbox")
    }
}

impl Drop for RunningSandboxLease {
    fn drop(&mut self) {
        let Some(sandbox) = self.sandbox.take() else {
            return;
        };

        let mut guard = self.inner.lock();
        if let SandboxInner::Running { sandbox: slot, .. } = &mut *guard
            && slot.is_none()
        {
            *slot = Some(sandbox);
        }
    }
}

#[pyclass(name = "_ContextCore")]
struct PyContext {
    inner: Option<Arc<ContextInner>>,
}

impl PyContext {
    fn inner_ref(&self) -> Result<&Arc<ContextInner>> {
        self.inner
            .as_ref()
            .ok_or_else(|| invalid_argument("context is closed"))
    }
}

#[pymethods]
impl PyContext {
    #[new]
    fn new() -> Self {
        let inner = ContextInner::new();
        Self {
            inner: Some(Arc::new(inner)),
        }
    }

    fn configure(&self, config: &Bound<'_, PyAny>) -> PyResult<()> {
        let patch: ContextConfigPatch = crate::serde::py_to_serde(config).map_err(to_py_err)?;
        let inner = Arc::clone(self.inner_ref().map_err(to_py_err)?);
        let mut state = inner.state.lock();
        if state.template.is_some() {
            return Err(to_py_err(invalid_argument(
                "context template already initialized",
            )));
        }
        state.config.apply_patch(patch).map_err(to_py_err)
    }

    #[pyo3(signature = (runtime_path, runtime_name = "python"))]
    fn initialize_template<'py>(
        &self,
        py: Python<'py>,
        runtime_path: &str,
        runtime_name: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(self.inner_ref().map_err(to_py_err)?);
        let runtime_path = PathBuf::from(runtime_path);
        let runtime = RuntimeFlavor::parse(runtime_name).map_err(to_py_err)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            inner
                .initialize_template(runtime_path, runtime)
                .await
                .map_err(to_py_err)?;
            Ok(())
        })
    }

    fn instantiate<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(self.inner_ref().map_err(to_py_err)?);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let has_template = inner.has_template();
            if !has_template {
                return Err(to_py_err(invalid_argument(
                    "runtime template not initialized",
                )));
            }

            Python::attach(|py| {
                Py::new(
                    py,
                    PySandbox {
                        ctx: inner,
                        inner: Arc::new(Mutex::new(SandboxInner::Pending {
                            config: PendingSandboxConfig::default(),
                            callback: None,
                            http_handler: None,
                            hostcall_handler: None,
                        })),
                    },
                )
            })
        })
    }

    fn close(&mut self) {
        self.inner = None;
    }
}

type WireArgument = (String, Option<String>, Py<PyAny>);

enum RawArgument {
    Json(Option<String>, Value),
    JsonStream(Option<String>, tokio::sync::mpsc::Receiver<Value>),
}

fn parse_run_args(py: Python<'_>, args: Vec<WireArgument>) -> Result<Vec<RawArgument>> {
    let mut parsed = Vec::with_capacity(args.len());

    for (kind, name, payload) in args {
        match kind.as_str() {
            "json" => {
                let value = py_to_value(payload.bind(py))
                    .map_err(|e| invalid_argument(format!("invalid argument value: {e}")))?;
                parsed.push(RawArgument::Json(name, value));
            }
            "stream" => {
                let stream: PyRef<'_, StreamHandle> = payload
                    .bind(py)
                    .extract()
                    .map_err(|_| invalid_argument("stream payload must be a Stream handle"))?;
                let receiver = stream.take_receiver()?;
                parsed.push(RawArgument::JsonStream(name, receiver));
            }
            _ => {
                return Err(invalid_argument(format!(
                    "unsupported argument kind: {kind}"
                )));
            }
        }
    }

    Ok(parsed)
}

#[pyclass(name = "_SandboxCore")]
struct PySandbox {
    ctx: Arc<ContextInner>,
    inner: Arc<Mutex<SandboxInner>>,
}

#[pymethods]
impl PySandbox {
    fn configure(&self, config: &Bound<'_, PyAny>) -> PyResult<()> {
        let patch: SandboxConfigPatch = crate::serde::py_to_serde(config).map_err(to_py_err)?;
        let mut guard = self.inner.lock();
        match &mut *guard {
            SandboxInner::Pending { config, .. } => config.apply_patch(patch).map_err(to_py_err),
            SandboxInner::Running { .. } => {
                Err(to_py_err(invalid_argument("sandbox is already running")))
            }
            SandboxInner::Uninitialized => {
                Err(to_py_err(invalid_argument("sandbox is not initialized")))
            }
        }
    }

    fn set_callback(&self, callback: Option<Py<PyAny>>) -> PyResult<()> {
        let callback = callback.map(|callback| Arc::new(PyCallback { callback }));

        let mut inner = self.inner.lock();
        match &mut *inner {
            SandboxInner::Pending { callback: slot, .. }
            | SandboxInner::Running { callback: slot, .. } => {
                *slot = callback;
                Ok(())
            }
            SandboxInner::Uninitialized => {
                Err(to_py_err(invalid_argument("sandbox is not initialized")))
            }
        }
    }

    fn set_http_handler(
        &self,
        callback: Option<Py<PyAny>>,
        event_loop: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        let http_handler = match (callback, event_loop) {
            (Some(callback), Some(event_loop)) => {
                Some(Arc::new(PyHttpHandler::new(callback, event_loop)))
            }
            (None, None) => None,
            _ => {
                return Err(to_py_err(invalid_argument(
                    "callback and event_loop must both be set or both be None",
                )));
            }
        };

        let mut inner = self.inner.lock();
        match &mut *inner {
            SandboxInner::Pending {
                http_handler: slot, ..
            } => {
                *slot = http_handler;
                Ok(())
            }
            SandboxInner::Running { .. } => {
                Err(to_py_err(invalid_argument("sandbox is already running")))
            }
            SandboxInner::Uninitialized => {
                Err(to_py_err(invalid_argument("sandbox is not initialized")))
            }
        }
    }

    fn set_hostcall_handler(
        &self,
        callback: Option<Py<PyAny>>,
        event_loop: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        let hostcall_handler = match (callback, event_loop) {
            (Some(callback), Some(event_loop)) => {
                Some(Arc::new(PyHostcallHandler::new(callback, event_loop)))
            }
            (None, None) => None,
            _ => {
                return Err(to_py_err(invalid_argument(
                    "callback and event_loop must both be set or both be None",
                )));
            }
        };

        let mut inner = self.inner.lock();
        match &mut *inner {
            SandboxInner::Pending {
                hostcall_handler: slot,
                ..
            } => {
                *slot = hostcall_handler;
                Ok(())
            }
            SandboxInner::Running { .. } => {
                Err(to_py_err(invalid_argument("sandbox is already running")))
            }
            SandboxInner::Uninitialized => {
                Err(to_py_err(invalid_argument("sandbox is not initialized")))
            }
        }
    }

    fn start<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        let ctx = Arc::clone(&self.ctx);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let (config, callback, http_handler, hostcall_handler) = {
                let mut guard = inner.lock();
                let current = std::mem::replace(&mut *guard, SandboxInner::Uninitialized);
                match current {
                    SandboxInner::Pending {
                        config,
                        callback,
                        http_handler,
                        hostcall_handler,
                    } => (config, callback, http_handler, hostcall_handler),
                    other => {
                        *guard = other;
                        drop(guard);
                        return Err(to_py_err(invalid_argument(
                            "sandbox is not in pending state",
                        )));
                    }
                }
            };

            let options = config.to_options();
            let env = Env::new(http_handler.clone(), hostcall_handler.clone());

            match ctx.instantiate_sandbox(options, env).await {
                Ok(sandbox) => {
                    let mut guard = inner.lock();
                    *guard = SandboxInner::Running {
                        sandbox: Some(sandbox),
                        callback,
                    };
                    drop(guard);
                    Ok(())
                }
                Err(err) => {
                    let mut guard = inner.lock();
                    *guard = SandboxInner::Pending {
                        config,
                        callback,
                        http_handler,
                        hostcall_handler,
                    };
                    drop(guard);
                    Err(to_py_err(err))
                }
            }
        })
    }

    #[pyo3(signature = (code))]
    fn load_script<'py>(&self, py: Python<'py>, code: &str) -> PyResult<Bound<'py, PyAny>> {
        let script = code.to_string();
        let inner = Arc::clone(&self.inner);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let (mut lease, callback) = {
                let mut guard = inner.lock();
                match &mut *guard {
                    SandboxInner::Running { sandbox, callback } => {
                        let sandbox = sandbox
                            .take()
                            .ok_or_else(|| to_py_err(invalid_argument("sandbox is busy")))?;
                        (
                            RunningSandboxLease::new(Arc::clone(&inner), sandbox),
                            callback.clone(),
                        )
                    }
                    _ => return Err(to_py_err(invalid_argument("sandbox is not running"))),
                }
            };

            let collector = OutputCollector::new(callback);
            let sink = collector.target();
            let outcome = lease.sandbox_mut().eval_script(&script, sink).await;
            if let Err(err) = outcome {
                let message = format!("Script loading failed: {err}");
                collector.emit_error_message(&message);
                return Err(to_py_err(Error::Internal(message)));
            }

            Ok(())
        })
    }

    #[pyo3(signature = (func, args = Vec::new()))]
    fn run<'py>(
        &self,
        py: Python<'py>,
        func: String,
        args: Vec<WireArgument>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let (mut lease, callback) = {
                let mut guard = inner.lock();
                match &mut *guard {
                    SandboxInner::Running { sandbox, callback } => {
                        let sandbox = sandbox
                            .take()
                            .ok_or_else(|| to_py_err(invalid_argument("sandbox is busy")))?;
                        (
                            RunningSandboxLease::new(Arc::clone(&inner), sandbox),
                            callback.clone(),
                        )
                    }
                    _ => return Err(to_py_err(invalid_argument("sandbox is not running"))),
                }
            };

            let parsed_args = match Python::attach(|py| parse_run_args(py, args)) {
                Ok(parsed_args) => parsed_args,
                Err(err) => return Err(to_py_err(err)),
            };

            let collector = OutputCollector::new(callback);
            let sink = collector.target();
            let isola_args = parsed_args
                .into_iter()
                .map(|arg| match arg {
                    RawArgument::Json(name, value) => Ok(match name {
                        Some(name) => Arg::Named(name, value),
                        None => Arg::Positional(value),
                    }),
                    RawArgument::JsonStream(name, receiver) => {
                        let stream =
                            Box::pin(tokio_stream::wrappers::ReceiverStream::new(receiver));
                        Ok(match name {
                            Some(name) => Arg::NamedStream(name, stream),
                            None => Arg::PositionalStream(stream),
                        })
                    }
                })
                .collect::<Result<Vec<_>>>()
                .map_err(to_py_err)?;

            let outcome = lease
                .sandbox_mut()
                .call_with_sink(&func, isola_args, sink)
                .await;
            if let Err(err) = outcome {
                let message = format!("Sandbox execution failed: {err}");
                collector.emit_error_message(&message);
                return Err(to_py_err(Error::Internal(message)));
            }

            Ok(collector.into_result())
        })
    }

    fn close(&self) {
        let mut guard = self.inner.lock();
        *guard = SandboxInner::Uninitialized;
        drop(guard);
    }
}

#[pyclass(name = "_RunResultCore")]
struct PyRunResult {
    #[pyo3(get)]
    result_json: Vec<String>,
    #[pyo3(get)]
    final_json: Option<String>,
    #[pyo3(get)]
    stdout: Vec<String>,
    #[pyo3(get)]
    stderr: Vec<String>,
    #[pyo3(get)]
    logs: Vec<String>,
    #[pyo3(get)]
    errors: Vec<String>,
}

#[pyclass(name = "_StreamCore")]
struct StreamHandle {
    sender: Mutex<Option<tokio::sync::mpsc::Sender<Value>>>,
    receiver: Mutex<Option<tokio::sync::mpsc::Receiver<Value>>>,
}

impl StreamHandle {
    fn sender(&self) -> Result<tokio::sync::mpsc::Sender<Value>> {
        self.sender
            .lock()
            .as_ref()
            .cloned()
            .ok_or(Error::StreamClosed)
    }

    fn take_receiver(&self) -> Result<tokio::sync::mpsc::Receiver<Value>> {
        self.receiver
            .lock()
            .take()
            .ok_or_else(|| invalid_argument("stream receiver already taken"))
    }

    fn try_send(&self, value: Value) -> Result<()> {
        let sender_guard = self.sender.lock();
        let sender = sender_guard.as_ref().ok_or(Error::StreamClosed)?;
        let result = sender.try_send(value);
        drop(sender_guard);
        match result {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Err(Error::StreamFull),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(Error::StreamClosed),
        }
    }

    fn close_sender(&self) {
        self.sender.lock().take();
    }
}

#[pymethods]
impl StreamHandle {
    #[new]
    #[pyo3(signature = (capacity = DEFAULT_STREAM_CAPACITY))]
    fn new(capacity: usize) -> PyResult<Self> {
        if capacity == 0 {
            return Err(to_py_err(invalid_argument(
                "stream capacity must be greater than 0",
            )));
        }

        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        Ok(Self {
            sender: Mutex::new(Some(sender)),
            receiver: Mutex::new(Some(receiver)),
        })
    }

    #[pyo3(signature = (value, blocking = false))]
    fn push(&self, py: Python<'_>, value: &Bound<'_, PyAny>, blocking: bool) -> PyResult<()> {
        let value = py_to_value(value).map_err(to_py_err)?;

        if blocking {
            let sender = self.sender().map_err(to_py_err)?;
            let result = py.detach(move || sender.blocking_send(value));
            match result {
                Ok(()) => Ok(()),
                Err(_) => Err(to_py_err(Error::StreamClosed)),
            }
        } else {
            self.try_send(value).map_err(to_py_err)
        }
    }

    fn push_async<'py>(
        &self,
        py: Python<'py>,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let value = py_to_value(value).map_err(to_py_err)?;
        let sender = self.sender().map_err(to_py_err)?;

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            sender
                .send(value)
                .await
                .map_err(|_| to_py_err(Error::StreamClosed))?;
            Ok(())
        })
    }

    fn end(&self) {
        self.close_sender();
    }
}

#[derive(Clone)]
struct Env {
    http_handler: Option<Arc<PyHttpHandler>>,
    hostcall_handler: Option<Arc<PyHostcallHandler>>,
}

impl Env {
    const fn new(
        http_handler: Option<Arc<PyHttpHandler>>,
        hostcall_handler: Option<Arc<PyHostcallHandler>>,
    ) -> Self {
        Self {
            http_handler,
            hostcall_handler,
        }
    }
}

impl Host for Env {
    async fn hostcall(
        &self,
        call_type: &str,
        payload: Value,
    ) -> std::result::Result<Value, BoxError> {
        let Some(handler) = &self.hostcall_handler else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("unsupported hostcall: {call_type}"),
            )
            .into());
        };

        handler.invoke_hostcall(call_type, payload).await
    }

    async fn http_request(
        &self,
        incoming: HttpRequest,
    ) -> std::result::Result<HttpResponse, BoxError> {
        let Some(handler) = &self.http_handler else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "unsupported http_request",
            )
            .into());
        };

        let (parts, body) = handler.invoke_http_handler(&incoming).await?;
        let stream: HttpBodyStream = match body {
            HttpResponseBody::Empty => Box::pin(stream::empty()),
            HttpResponseBody::Buffered(bytes) => {
                Box::pin(stream::once(async move { Ok(Frame::data(bytes)) }))
            }
            HttpResponseBody::Stream(iterator) => {
                let rx = handler.stream_from_async_iter(iterator);
                Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
            }
        };

        Ok(HttpResponse::from_parts(parts, stream))
    }
}

#[pymodule]
fn _isola(py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("IsolaError", py.get_type::<IsolaError>())?;
    module.add(
        "InvalidArgumentError",
        py.get_type::<InvalidArgumentError>(),
    )?;
    module.add("InternalError", py.get_type::<InternalError>())?;
    module.add("StreamFullError", py.get_type::<StreamFullError>())?;
    module.add("StreamClosedError", py.get_type::<StreamClosedError>())?;

    module.add_class::<PyContext>()?;
    module.add_class::<PySandbox>()?;
    module.add_class::<PyRunResult>()?;
    module.add_class::<StreamHandle>()?;
    Ok(())
}
