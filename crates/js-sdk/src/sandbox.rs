use std::sync::Arc;

use isola::{
    host::{BoxError, LogLevel, OutputEvent, OutputTarget},
    sandbox::{Arg, Sandbox},
    value::Value,
};
use napi::{
    bindgen_prelude::{Buffer, Function, Promise},
    threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode},
};
use napi_derive::napi;
use parking_lot::Mutex;

use crate::{
    context::{ContextInner, PendingSandboxConfig, SandboxConfigPatch},
    env::{Env, JsHostcallHandler, JsHttpHandler},
    error::{Error, invalid_argument},
    stream::StreamHandle,
};

// Type alias for the callback TSFN, wrapped in Arc for cloning.
// Type params: T, Return, CallJsBackArgs, ErrorStatus, CalleeHandled
type CallbackTsfn = Arc<
    ThreadsafeFunction<(String, Option<String>), (), (String, Option<String>), napi::Status, false>,
>;
type HttpHandlerFunction<'env> =
    Function<'env, (String, String, Buffer, Option<Buffer>), Promise<crate::env::JsHttpResponse>>;

// ---------------------------------------------------------------------------
// RunResult
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct RunResult {
    pub result_json: Vec<String>,
    pub final_json: Option<String>,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub logs: Vec<String>,
    pub errors: Vec<String>,
}

// ---------------------------------------------------------------------------
// OutputCollector
// ---------------------------------------------------------------------------

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
    callback: Option<CallbackTsfn>,
    data: Arc<Mutex<OutputData>>,
}

impl OutputCollector {
    fn new(callback: Option<CallbackTsfn>) -> Self {
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

    fn emit(&self, event: CallbackEvent, payload: Option<&str>) -> bool {
        if let Some(tsfn) = &self.callback {
            return tsfn.call(
                (event.as_str().to_owned(), payload.map(str::to_owned)),
                ThreadsafeFunctionCallMode::NonBlocking,
            ) == napi::Status::Ok;
        }
        true
    }

    fn emit_error_message(&self, message: &str) {
        self.record(|data| data.errors.push(message.to_owned()));
        let _ = self.emit(CallbackEvent::Error, Some(message));
    }

    fn emit_or_record_error(&self, event: CallbackEvent, payload: Option<&str>, kind: &str) {
        if !self.emit(event, payload) {
            self.record(|data| {
                data.errors
                    .push(format!("output callback rejected a {kind} event"));
            });
        }
    }

    fn into_result(self) -> RunResult {
        let data = std::mem::take(&mut *self.data.lock());
        RunResult {
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
                self.emit_or_record_error(CallbackEvent::Result, Some(&text), "result");
                self.record(|data| data.result_json.push(text));
            }
            OutputEvent::Complete(item) => {
                if let Some(item) = item {
                    let text = item.to_json_str().map_err(|e| -> BoxError {
                        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                    })?;
                    self.emit_or_record_error(CallbackEvent::End, Some(&text), "end");
                    self.record(|data| data.final_json = Some(text));
                } else {
                    self.emit_or_record_error(CallbackEvent::End, None, "end");
                }
            }
            OutputEvent::Log { level, message, .. } => {
                let callback_event = match level {
                    LogLevel::Stdout => CallbackEvent::Stdout,
                    LogLevel::Stderr => CallbackEvent::Stderr,
                    _ => CallbackEvent::Log,
                };
                self.emit_or_record_error(callback_event, Some(&message), "log");
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

// ---------------------------------------------------------------------------
// SandboxInner state machine
// ---------------------------------------------------------------------------

enum SandboxInner {
    Uninitialized,
    Pending {
        config: PendingSandboxConfig,
        callback: Option<CallbackTsfn>,
        http_handler: Option<Arc<JsHttpHandler>>,
        hostcall_handler: Option<Arc<JsHostcallHandler>>,
    },
    Running {
        sandbox: Option<Sandbox<Env>>,
        callback: Option<CallbackTsfn>,
    },
}

// ---------------------------------------------------------------------------
// RunningSandboxLease
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

type WireArgument = (String, Option<String>, serde_json::Value);

enum RawArgument {
    Json(Option<String>, Value),
}

enum ParsedArgument {
    Json(Option<String>, Value),
    JsonStream(Option<String>, usize),
}

fn parse_run_args(args: Vec<WireArgument>) -> crate::error::Result<Vec<RawArgument>> {
    let mut parsed = Vec::with_capacity(args.len());
    for (kind, name, payload) in args {
        match kind.as_str() {
            "json" => {
                let value = Value::from_serde(&payload)
                    .map_err(|e| invalid_argument(format!("invalid argument value: {e}")))?;
                parsed.push(RawArgument::Json(name, value));
            }
            "stream" => {
                return Err(invalid_argument(
                    "stream arguments must use runWithStream method",
                ));
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

fn parse_stream_run_args(
    args: Vec<WireArgument>,
    stream_count: usize,
) -> crate::error::Result<Vec<ParsedArgument>> {
    let mut parsed = Vec::with_capacity(args.len());
    let mut used_streams = vec![false; stream_count];

    for (kind, name, payload) in args {
        match kind.as_str() {
            "json" => {
                let value = Value::from_serde(&payload)
                    .map_err(|e| invalid_argument(format!("invalid argument value: {e}")))?;
                parsed.push(ParsedArgument::Json(name, value));
            }
            "stream" => {
                let index = payload
                    .as_u64()
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(|| invalid_argument("stream argument index must be an integer"))?;
                let Some(used) = used_streams.get_mut(index) else {
                    return Err(invalid_argument("stream argument index is out of bounds"));
                };
                if std::mem::replace(used, true) {
                    return Err(invalid_argument("stream argument is used more than once"));
                }
                parsed.push(ParsedArgument::JsonStream(name, index));
            }
            _ => {
                return Err(invalid_argument(format!(
                    "unsupported argument kind: {kind}"
                )));
            }
        }
    }

    if used_streams.iter().any(|used| !used) {
        return Err(invalid_argument("unreferenced stream argument handle"));
    }

    Ok(parsed)
}

fn take_stream_receivers(
    handles: &[&StreamHandle],
) -> crate::error::Result<Vec<Option<tokio::sync::mpsc::Receiver<Value>>>> {
    let mut receivers = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.take_receiver() {
            Ok(receiver) => receivers.push(Some(receiver)),
            Err(error) => {
                for (handle, receiver) in handles.iter().zip(receivers).rev() {
                    if let Some(receiver) = receiver {
                        handle.restore_receiver(receiver);
                    }
                }
                return Err(error);
            }
        }
    }
    Ok(receivers)
}

// ---------------------------------------------------------------------------
// Helper: take lease from running sandbox
// ---------------------------------------------------------------------------

fn take_running_lease(
    inner: &Arc<Mutex<SandboxInner>>,
) -> napi::Result<(RunningSandboxLease, Option<CallbackTsfn>)> {
    let mut guard = inner.lock();
    match &mut *guard {
        SandboxInner::Running { sandbox, callback } => {
            let sandbox = sandbox
                .take()
                .ok_or_else(|| napi::Error::from(invalid_argument("sandbox is busy")))?;
            Ok((
                RunningSandboxLease::new(Arc::clone(inner), sandbox),
                callback.clone(),
            ))
        }
        _ => Err(napi::Error::from(invalid_argument(
            "sandbox is not running",
        ))),
    }
}

// ---------------------------------------------------------------------------
// N-API class: SandboxCore
// ---------------------------------------------------------------------------

#[napi]
pub struct SandboxCore {
    ctx: Arc<ContextInner>,
    inner: Arc<Mutex<SandboxInner>>,
}

impl SandboxCore {
    pub(crate) fn new(ctx: Arc<ContextInner>) -> Self {
        Self {
            ctx,
            inner: Arc::new(Mutex::new(SandboxInner::Pending {
                config: PendingSandboxConfig::default(),
                callback: None,
                http_handler: None,
                hostcall_handler: None,
            })),
        }
    }
}

#[napi]
impl SandboxCore {
    #[napi]
    pub fn configure(&self, config: serde_json::Value) -> napi::Result<()> {
        let patch: SandboxConfigPatch = serde_json::from_value(config)
            .map_err(|e| napi::Error::from(invalid_argument(format!("invalid config: {e}"))))?;
        let mut guard = self.inner.lock();
        match &mut *guard {
            SandboxInner::Pending { config, .. } => {
                config.apply_patch(patch).map_err(napi::Error::from)
            }
            SandboxInner::Running { .. } => Err(napi::Error::from(invalid_argument(
                "sandbox is already running",
            ))),
            SandboxInner::Uninitialized => Err(napi::Error::from(invalid_argument(
                "sandbox is not initialized",
            ))),
        }
    }

    /// Set the output event callback: (kind: string, data: string | null) =>
    /// void
    #[napi(ts_args_type = "callback: ((kind: string, data: string | null) => void) | null")]
    pub fn set_callback(
        &self,
        callback: Option<Function<(String, Option<String>), ()>>,
    ) -> napi::Result<()> {
        let tsfn: Option<CallbackTsfn> = callback
            .map(|cb| -> napi::Result<CallbackTsfn> {
                Ok(Arc::new(cb.build_threadsafe_function().build()?))
            })
            .transpose()?;

        let mut guard = self.inner.lock();
        match &mut *guard {
            SandboxInner::Pending { callback: slot, .. }
            | SandboxInner::Running { callback: slot, .. } => {
                *slot = tsfn;
                Ok(())
            }
            SandboxInner::Uninitialized => Err(napi::Error::from(invalid_argument(
                "sandbox is not initialized",
            ))),
        }
    }

    /// Set the hostcall handler: (callType: string, payload: Buffer) =>
    /// Promise<Buffer>
    #[napi(
        ts_args_type = "handler: ((callType: string, payload: Buffer) => Promise<Buffer>) | null"
    )]
    pub fn set_hostcall_handler(
        &self,
        handler: Option<
            Function<
                (String, napi::bindgen_prelude::Buffer),
                Promise<napi::bindgen_prelude::Buffer>,
            >,
        >,
    ) -> napi::Result<()> {
        let js_handler = handler
            .map(|cb| {
                let tsfn = cb.build_threadsafe_function().build()?;
                Ok::<_, napi::Error>(Arc::new(JsHostcallHandler::new(tsfn)))
            })
            .transpose()?;

        let mut guard = self.inner.lock();
        match &mut *guard {
            SandboxInner::Pending {
                hostcall_handler: slot,
                ..
            } => {
                *slot = js_handler;
                Ok(())
            }
            SandboxInner::Running { .. } => Err(napi::Error::from(invalid_argument(
                "sandbox is already running",
            ))),
            SandboxInner::Uninitialized => Err(napi::Error::from(invalid_argument(
                "sandbox is not initialized",
            ))),
        }
    }

    /// Set the HTTP handler: (method, url, headers, body) =>
    /// Promise<response>
    #[napi(
        ts_args_type = "handler: ((method: string, url: string, headers: Buffer, body: Buffer | null) => Promise<{ status: number; headers?: Record<string, string>; body?: Buffer | null }>) | null"
    )]
    pub fn set_http_handler(&self, handler: Option<HttpHandlerFunction<'_>>) -> napi::Result<()> {
        let js_handler = handler
            .map(|cb| {
                let tsfn = cb.build_threadsafe_function().build()?;
                Ok::<_, napi::Error>(Arc::new(JsHttpHandler::new(tsfn)))
            })
            .transpose()?;

        let mut guard = self.inner.lock();
        match &mut *guard {
            SandboxInner::Pending {
                http_handler: slot, ..
            } => {
                *slot = js_handler;
                Ok(())
            }
            SandboxInner::Running { .. } => Err(napi::Error::from(invalid_argument(
                "sandbox is already running",
            ))),
            SandboxInner::Uninitialized => Err(napi::Error::from(invalid_argument(
                "sandbox is not initialized",
            ))),
        }
    }

    #[napi]
    pub async fn start(&self) -> napi::Result<()> {
        let inner = Arc::clone(&self.inner);
        let ctx = Arc::clone(&self.ctx);

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
                    return Err(napi::Error::from(invalid_argument(
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
                Err(napi::Error::from(err))
            }
        }
    }

    #[napi]
    pub async fn load_script(&self, code: String) -> napi::Result<()> {
        let inner = Arc::clone(&self.inner);
        let (mut lease, callback) = take_running_lease(&inner)?;

        let collector = OutputCollector::new(callback);
        let sink = collector.target();
        let outcome = lease.sandbox_mut().eval_script(&code, sink).await;
        if let Err(err) = outcome {
            let message = format!("Script loading failed: {err}");
            collector.emit_error_message(&message);
            return Err(napi::Error::from(Error::Internal(message)));
        }
        Ok(())
    }

    #[napi]
    pub async fn run(&self, func: String, args: Vec<WireArgument>) -> napi::Result<RunResult> {
        let inner = Arc::clone(&self.inner);
        let (mut lease, callback) = take_running_lease(&inner)?;

        let parsed_args = parse_run_args(args).map_err(napi::Error::from)?;

        let collector = OutputCollector::new(callback);
        let sink = collector.target();
        let isola_args = parsed_args
            .into_iter()
            .map(|arg| match arg {
                RawArgument::Json(name, value) => Ok(match name {
                    Some(name) => Arg::Named(name, value),
                    None => Arg::Positional(value),
                }),
            })
            .collect::<crate::error::Result<Vec<_>>>()
            .map_err(napi::Error::from)?;

        let outcome = lease
            .sandbox_mut()
            .call_with_sink(&func, isola_args, sink)
            .await;
        if let Err(err) = outcome {
            let message = format!("Sandbox execution failed: {err}");
            collector.emit_error_message(&message);
            return Err(napi::Error::from(Error::Internal(message)));
        }

        Ok(collector.into_result())
    }

    /// Run a function with one or more `StreamHandle` arguments.
    #[napi]
    pub async fn run_with_streams(
        &self,
        func: String,
        args: Vec<WireArgument>,
        stream_args: Vec<&StreamHandle>,
    ) -> napi::Result<RunResult> {
        let inner = Arc::clone(&self.inner);
        let (mut lease, callback) = take_running_lease(&inner)?;

        let parsed = parse_stream_run_args(args, stream_args.len()).map_err(napi::Error::from)?;
        let mut receivers = take_stream_receivers(&stream_args).map_err(napi::Error::from)?;
        let isola_args = parsed
            .into_iter()
            .map(|arg| match arg {
                ParsedArgument::Json(name, value) => Ok(match name {
                    Some(name) => Arg::Named(name, value),
                    None => Arg::Positional(value),
                }),
                ParsedArgument::JsonStream(name, index) => {
                    let receiver = receivers[index].take().ok_or_else(|| {
                        invalid_argument("stream argument receiver was already consumed")
                    })?;
                    let stream = Box::pin(tokio_stream::wrappers::ReceiverStream::new(receiver));
                    Ok(match name {
                        Some(name) => Arg::NamedStream(name, stream),
                        None => Arg::PositionalStream(stream),
                    })
                }
            })
            .collect::<crate::error::Result<Vec<_>>>()
            .map_err(napi::Error::from)?;

        let collector = OutputCollector::new(callback);
        let sink = collector.target();

        let outcome = lease
            .sandbox_mut()
            .call_with_sink(&func, isola_args, sink)
            .await;
        if let Err(err) = outcome {
            let message = format!("Sandbox execution failed: {err}");
            collector.emit_error_message(&message);
            return Err(napi::Error::from(Error::Internal(message)));
        }

        Ok(collector.into_result())
    }

    #[napi]
    pub fn close(&self) {
        let mut guard = self.inner.lock();
        *guard = SandboxInner::Uninitialized;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_argument_indexes_must_be_unique_and_complete() {
        let duplicate = vec![
            ("stream".to_string(), None, serde_json::json!(0)),
            ("stream".to_string(), None, serde_json::json!(0)),
        ];
        assert!(parse_stream_run_args(duplicate, 1).is_err());

        let incomplete = vec![("stream".to_string(), None, serde_json::json!(0))];
        assert!(parse_stream_run_args(incomplete, 2).is_err());
    }

    #[test]
    fn failed_stream_acquisition_restores_prior_receivers() {
        let first = StreamHandle::with_capacity(1);
        let second = StreamHandle::with_capacity(1);
        let held = second.take_receiver().expect("take second receiver");

        assert!(take_stream_receivers(&[&first, &second]).is_err());
        assert!(first.take_receiver().is_ok());
        drop(held);
    }
}
