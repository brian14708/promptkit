use std::{future::Future, sync::Arc};

use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt as _;
use tokio::time::timeout;
use tracing::Instrument;
use wasmtime::{
    Engine, Store,
    component::{Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    Error as HttpError, RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks,
    WasiHttpView,
};

use super::bindings::{EmitValue, HostView, add_to_linker};
use crate::{
    host::{Host, HttpRequest, LogContext, LogLevel, OutputTarget},
    internal::{
        resource::MemoryLimiter,
        trace_output::{LogTargetStore, TraceOutput, new_log_target_store, set_log_target},
        wasm,
    },
    sandbox::DirectoryMapping,
    value::Value,
};

pub struct InstanceState<H: Host> {
    pub limiter: MemoryLimiter,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    host: Arc<H>,
    http_hooks: InstanceHttpHooks<H>,

    output_target: Option<OutputTarget>,
    pub(crate) capture_output: Option<crate::sandbox::CallOutput>,
    log_target_store: LogTargetStore,
    output_buffer: OutputBuffer,
}

struct InstanceHttpHooks<H: Host> {
    host: Arc<H>,
}

type HttpSendResult = Result<
    (
        http::Response<WasiBody>,
        Box<dyn Future<Output = Result<(), HttpError>> + Send>,
    ),
    HttpError,
>;

const MAX_OUTGOING_HTTP_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_OUTGOING_HTTP_BODY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_BUFFERED_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

async fn collect_outgoing_http_body(
    body: WasiBody,
    max_bytes: usize,
    read_timeout: std::time::Duration,
    content_length: Option<usize>,
) -> Result<Option<Bytes>, HttpError> {
    let mut body = body;
    let bytes = timeout(read_timeout, async {
        let mut buf = BytesMut::with_capacity(content_length.unwrap_or(0).min(max_bytes));
        while let Some(frame) = http_body_util::BodyExt::frame(&mut body).await {
            let frame = frame.map_err(|e| {
                HttpError::InternalError(Some(format!("request body read error: {e:?}")))
            })?;

            if let Ok(data) = frame.into_data() {
                if buf.len().saturating_add(data.len()) > max_bytes {
                    return Err(HttpError::HttpRequestBodySize(Some(
                        u64::try_from(max_bytes).unwrap_or(u64::MAX),
                    )));
                }
                buf.extend_from_slice(data.as_ref());
            }
        }

        Ok::<Bytes, HttpError>(buf.freeze())
    })
    .await
    .map_err(|_e| HttpError::ConnectionWriteTimeout)??;

    Ok(if bytes.is_empty() { None } else { Some(bytes) })
}

impl<H: Host> InstanceState<H> {
    /// Creates a new linker for the sandbox state.
    ///
    /// # Errors
    ///
    /// Returns an error if any of the WASI components fail to link.
    pub fn new_linker(engine: &Engine) -> wasmtime::Result<Linker<Self>> {
        let mut linker = Linker::<Self>::new(engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
        wasm::logging::add_to_linker(&mut linker)?;
        add_to_linker(&mut linker)?;
        Ok(linker)
    }

    /// Creates a new sandbox state with the specified configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the preopened directories cannot be added to the
    /// WASI context.
    pub fn new(
        engine: &Engine,
        directory_mappings: &[DirectoryMapping],
        env: &[(String, String)],
        max_memory: usize,
        host: H,
    ) -> wasmtime::Result<Store<Self>> {
        let log_target_store = new_log_target_store();
        let mut builder = WasiCtxBuilder::new();

        for mapping in directory_mappings {
            builder
                .preopened_dir(&mapping.host, &mapping.guest, mapping.perms)
                .map_err(|e| {
                    wasmtime::Error::msg(format!(
                        "Failed to add directory mapping '{}' -> '{}': {e}",
                        mapping.host.display(),
                        mapping.guest
                    ))
                })?;
        }
        for (k, v) in env {
            builder.env(k, v);
        }
        let wasi = builder
            .allow_tcp(false)
            .allow_udp(false)
            .stdout(TraceOutput::new(
                LogLevel::Stdout,
                LogContext::Stdout,
                Arc::clone(&log_target_store),
            ))
            .stderr(TraceOutput::new(
                LogLevel::Stderr,
                LogContext::Stderr,
                Arc::clone(&log_target_store),
            ))
            .build();
        let limiter = MemoryLimiter::new(max_memory);
        let host = Arc::new(host);

        let mut s = Store::new(
            engine,
            Self {
                limiter,
                wasi,
                http: WasiHttpCtx::new(),
                table: ResourceTable::new(),
                host: Arc::clone(&host),
                http_hooks: InstanceHttpHooks { host },
                output_target: None,
                capture_output: None,
                log_target_store,
                output_buffer: OutputBuffer::new(),
            },
        );
        s.limiter(|s| &mut s.limiter);
        Ok(s)
    }

    pub fn set_output_target(&mut self, target: Option<OutputTarget>) {
        // Prevent cross-call output leakage and avoid retaining large buffers if
        // the call traps or is interrupted mid-output.
        self.output_buffer.reset();
        set_log_target(&self.log_target_store, target.clone());
        self.output_target = target;
    }

    #[expect(
        clippy::needless_pass_by_ref_mut,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "the async boundary is kept consistent with call cleanup hooks"
    )]
    pub async fn flush_logs(&mut self) -> wasmtime::Result<()> {
        Ok(())
    }

    #[cfg(test)]
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
    ) -> std::pin::Pin<Box<dyn Future<Output = HttpSendResult> + Send>> {
        Box::into_pin(
            self.http_hooks
                .send_request(request, options, Box::new(async { Ok(()) })),
        )
    }
}

impl<H: Host> WasiView for InstanceState<H> {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl<H: Host> WasiHttpView for InstanceState<H> {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

impl<H: Host> WasiHttpHooks for InstanceHttpHooks<H> {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        fut: Box<dyn Future<Output = Result<(), HttpError>> + Send>,
    ) -> Box<dyn Future<Output = HttpSendResult> + Send> {
        let host = Arc::clone(&self.host);

        Box::new(
            async move {
                let (parts, body) = request.into_parts();
                let headers = parts.headers;

                // Fast-path reject based on `Content-Length` if present.
                if let Some(len) = headers.get(http::header::CONTENT_LENGTH)
                    && let Some(len) = len.to_str().ok().and_then(|s| s.parse::<u64>().ok())
                {
                    let max = u64::try_from(MAX_OUTGOING_HTTP_BODY_BYTES).unwrap_or(u64::MAX);
                    if len > max {
                        return Err(HttpError::HttpRequestBodySize(Some(max)));
                    }
                }

                let options = options.unwrap_or_default();
                let body_timeout = options
                    .connect_timeout
                    .unwrap_or(MAX_OUTGOING_HTTP_BODY_READ_TIMEOUT)
                    .min(MAX_OUTGOING_HTTP_BODY_READ_TIMEOUT);
                let content_length = headers
                    .get(http::header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<usize>().ok());
                let body = collect_outgoing_http_body(
                    body,
                    MAX_OUTGOING_HTTP_BODY_BYTES,
                    body_timeout,
                    content_length,
                )
                .await?;

                let mut req = HttpRequest::new(body);
                *req.method_mut() = parts.method;
                *req.uri_mut() = parts.uri;
                *req.headers_mut() = headers;
                let first_byte_timeout = options
                    .first_byte_timeout
                    .unwrap_or(std::time::Duration::from_secs(600));
                let resp = timeout(first_byte_timeout, host.http_request(req))
                    .await
                    .map_err(|_e| HttpError::HttpResponseTimeout)?
                    .map_err(|e| HttpError::InternalError(Some(format!("request error: {e}"))))?;

                let resp = resp.map(|b| {
                    http_body_util::StreamBody::new(futures::StreamExt::map(b, |e| {
                        e.map_err(|e| HttpError::InternalError(Some(e.to_string())))
                    }))
                    .boxed_unsync()
                });

                Ok((resp, fut))
            }
            .in_current_span(),
        )
    }
}

impl<H: Host> HostView for InstanceState<H> {
    type Host = H;

    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    fn host(&mut self) -> &Arc<Self::Host> {
        &self.host
    }

    async fn emit(&mut self, data: EmitValue) -> wasmtime::Result<()> {
        let Some(target) = self.output_target.as_ref() else {
            return Err(wasmtime::Error::msg("output target missing"));
        };

        match data {
            EmitValue::Continuation(new_data) => {
                self.output_buffer.append(new_data.as_ref())?;
                Ok(())
            }
            EmitValue::End(new_data) => {
                let output = self.output_buffer.finish(new_data)?;
                let output = if output.is_empty() {
                    None
                } else {
                    Some(Value::from(output))
                };
                if target.is_capture() {
                    if let Some(capture) = self.capture_output.as_mut() {
                        capture.result = output;
                    }
                    Ok(())
                } else {
                    target
                        .on_complete(output)
                        .await
                        .map_err(wasmtime::Error::from_boxed)
                }
            }
            EmitValue::PartialResult(new_data) => {
                let output = self.output_buffer.finish(new_data)?;
                let value = Value::from(output);
                if target.is_capture() {
                    if let Some(capture) = self.capture_output.as_mut() {
                        capture.items.push(value);
                    }
                    Ok(())
                } else {
                    target
                        .on_item(value)
                        .await
                        .map_err(wasmtime::Error::from_boxed)
                }
            }
            EmitValue::Abort => {
                self.output_buffer.reset();
                Ok(())
            }
        }
    }
}

impl<H: Host> wasm::logging::HostView for InstanceState<H> {
    async fn emit_log(
        &mut self,
        log_level: wasm::logging::bindings::logging::Level,
        context: &str,
        message: &str,
    ) -> wasmtime::Result<()> {
        let base_level = match log_level {
            wasm::logging::bindings::logging::Level::Trace => LogLevel::Trace,
            wasm::logging::bindings::logging::Level::Debug => LogLevel::Debug,
            wasm::logging::bindings::logging::Level::Info => LogLevel::Info,
            wasm::logging::bindings::logging::Level::Warn => LogLevel::Warn,
            wasm::logging::bindings::logging::Level::Error => LogLevel::Error,
            wasm::logging::bindings::logging::Level::Critical => LogLevel::Critical,
        };
        let output_context = match context {
            "stdout" => LogContext::Stdout,
            "stderr" => LogContext::Stderr,
            _ => LogContext::Other(context),
        };
        let output_level = match output_context {
            LogContext::Stdout => LogLevel::Stdout,
            LogContext::Stderr => LogLevel::Stderr,
            LogContext::Other(_) => base_level,
        };
        if let Some(target) = self.output_target.clone() {
            target
                .on_log(output_level, output_context, message)
                .await
                .map_err(wasmtime::Error::from_boxed)?;
        }
        Ok(())
    }
}

struct OutputBuffer(BytesMut);

impl OutputBuffer {
    fn new() -> Self {
        Self(BytesMut::new())
    }

    #[inline]
    fn reset(&mut self) {
        let _old = std::mem::take(&mut self.0);
    }

    #[inline]
    fn append(&mut self, data: &[u8]) -> wasmtime::Result<()> {
        let new_len = self.0.len().saturating_add(data.len());
        if new_len > MAX_BUFFERED_OUTPUT_BYTES {
            // Drop any already-buffered data to avoid retaining attacker-controlled memory.
            self.reset();
            return Err(wasmtime::Error::msg(format!(
                "output buffer exceeded hard limit ({MAX_BUFFERED_OUTPUT_BYTES} bytes)"
            )));
        }
        self.0.extend_from_slice(data);
        Ok(())
    }

    #[inline]
    fn finish(&mut self, data: Bytes) -> wasmtime::Result<Bytes> {
        if self.0.is_empty() {
            if data.len() > MAX_BUFFERED_OUTPUT_BYTES {
                return Err(wasmtime::Error::msg(format!(
                    "output buffer exceeded hard limit ({MAX_BUFFERED_OUTPUT_BYTES} bytes)"
                )));
            }
            return Ok(data);
        }

        self.append(data.as_ref())?;
        Ok(self.take())
    }

    #[inline]
    fn take(&mut self) -> Bytes {
        std::mem::take(&mut self.0).freeze()
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use http_body::Frame;
    use parking_lot::Mutex;

    use super::*;
    use crate::host::{BoxError, Host, HttpBodyStream, HttpRequest, HttpResponse};

    #[derive(Clone, Default)]
    struct ScriptedHost {
        calls: Arc<Mutex<Vec<HttpRequest>>>,
    }

    impl ScriptedHost {
        fn calls(&self) -> Vec<HttpRequest> {
            self.calls.lock().clone()
        }
    }

    fn empty_body() -> HttpBodyStream {
        Box::pin(futures::stream::empty::<Result<Frame<Bytes>, BoxError>>())
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "the test host implements the asynchronous callback contract"
    )]
    impl Host for ScriptedHost {
        async fn hostcall(
            &self,
            _call_type: &str,
            _payload: Value,
        ) -> core::result::Result<Value, BoxError> {
            Err(std::io::Error::other("unsupported").into())
        }

        async fn http_request(
            &self,
            req: HttpRequest,
        ) -> core::result::Result<HttpResponse, BoxError> {
            self.calls.lock().push(req.clone());

            let uri = req.uri().to_string();
            let resp = match uri.as_str() {
                "http://a.example/" => http::Response::builder()
                    .status(http::StatusCode::FOUND)
                    .header(http::header::LOCATION, "http://b.example/next")
                    .body(empty_body())
                    .expect("response build"),
                "http://b.example/next" => http::Response::builder()
                    .status(http::StatusCode::OK)
                    .body(empty_body())
                    .expect("response build"),
                _ => {
                    return Err(std::io::Error::other(format!("unexpected uri: {uri}")).into());
                }
            };
            Ok(resp)
        }
    }

    #[tokio::test]
    async fn send_request_body_timeout_is_enforced() {
        let host = ScriptedHost::default();
        let host = Arc::new(host.clone());

        let mut state = InstanceState {
            limiter: MemoryLimiter::new(1024),
            wasi: WasiCtxBuilder::new().build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            host: Arc::clone(&host),
            http_hooks: InstanceHttpHooks {
                host: Arc::clone(&host),
            },
            output_target: None,
            capture_output: None,
            log_target_store: Arc::new(Mutex::new(None)),
            output_buffer: OutputBuffer::new(),
        };

        // A body that never completes.
        let body = http_body_util::StreamBody::new(futures::stream::pending::<
            Result<Frame<Bytes>, HttpError>,
        >())
        .boxed_unsync();

        let req = hyper::Request::builder()
            .method(http::Method::POST)
            .uri("http://a.example/")
            .body(body)
            .expect("request build");

        let options = RequestOptions {
            connect_timeout: Some(Duration::from_millis(20)),
            first_byte_timeout: Some(Duration::from_secs(1)),
            between_bytes_timeout: Some(Duration::from_secs(1)),
        };

        let result = timeout(
            Duration::from_millis(500),
            state.send_request(req, Some(options)),
        )
        .await
        .expect("ready in time");

        let Err(err) = result else {
            panic!("expected timeout");
        };
        assert!(matches!(err, HttpError::ConnectionWriteTimeout));
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn outgoing_http_body_is_capped() {
        let body = http_body_util::StreamBody::new(futures::stream::iter([
            Ok::<_, HttpError>(Frame::data(Bytes::from_static(b"abcd"))),
            Ok::<_, HttpError>(Frame::data(Bytes::from_static(b"e"))),
        ]))
        .boxed_unsync();

        let err = collect_outgoing_http_body(body, 4, Duration::from_secs(1), None)
            .await
            .expect_err("expected cap error");
        assert!(matches!(err, HttpError::HttpRequestBodySize(Some(4))));
    }

    #[tokio::test]
    async fn send_request_delegates_redirect_and_host_handling_to_host() {
        let host = ScriptedHost::default();
        let host = Arc::new(host.clone());

        let mut state = InstanceState {
            limiter: MemoryLimiter::new(1024),
            wasi: WasiCtxBuilder::new().build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            host: Arc::clone(&host),
            http_hooks: InstanceHttpHooks {
                host: Arc::clone(&host),
            },
            output_target: None,
            capture_output: None,
            log_target_store: Arc::new(Mutex::new(None)),
            output_buffer: OutputBuffer::new(),
        };

        let body = http_body_util::StreamBody::new(futures::stream::empty::<
            Result<Frame<Bytes>, HttpError>,
        >())
        .boxed_unsync();

        let req = hyper::Request::builder()
            .method(http::Method::POST)
            .uri("http://a.example/")
            .header(http::header::HOST, "a.example")
            .header(http::header::AUTHORIZATION, "Bearer secret")
            .header(http::header::COOKIE, "a=b")
            .header("x-other", "keep")
            .body(body)
            .expect("request build");

        let options = RequestOptions {
            connect_timeout: Some(Duration::from_secs(1)),
            first_byte_timeout: Some(Duration::from_secs(1)),
            between_bytes_timeout: Some(Duration::from_secs(1)),
        };

        let (incoming, _io) = timeout(
            Duration::from_millis(500),
            state.send_request(req, Some(options)),
        )
        .await
        .expect("ready in time")
        .expect("expected response");
        assert_eq!(incoming.status(), http::StatusCode::FOUND);

        let calls = host.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method(), http::Method::POST);
        assert_eq!(calls[0].uri(), "http://a.example/");
        assert_eq!(calls[0].body().as_deref(), None);

        assert_eq!(
            calls[0]
                .headers()
                .get("x-other")
                .expect("x-other forwarded")
                .to_str()
                .expect("valid header value"),
            "keep"
        );
        assert_eq!(
            calls[0]
                .headers()
                .get(http::header::HOST)
                .expect("host forwarded")
                .to_str()
                .expect("valid header value"),
            "a.example"
        );
    }

    #[test]
    fn output_buffer_take_resets() {
        let mut buf = OutputBuffer::new();
        buf.append(b"hello").expect("append within limit");
        assert_eq!(&buf.take()[..], b"hello");
        assert!(buf.take().is_empty());
    }

    #[test]
    fn output_buffer_finish_reuses_single_chunk() {
        let mut buf = OutputBuffer::new();
        let chunk = Bytes::from(vec![1_u8, 2, 3]);
        let chunk_ptr = chunk.as_ptr();
        let output = buf.finish(chunk).expect("finish within limit");
        assert_eq!(output.as_ptr(), chunk_ptr);
        assert_eq!(&output[..], &[1, 2, 3]);
    }

    #[test]
    fn output_buffer_finish_combines_continuations() {
        let mut buf = OutputBuffer::new();
        buf.append(b"hello ").expect("append within limit");
        let output = buf
            .finish(Bytes::from_static(b"world"))
            .expect("finish within limit");
        assert_eq!(&output[..], b"hello world");
    }

    #[test]
    fn output_buffer_hard_cap_resets_buffer() {
        let mut buf = OutputBuffer::new();
        let at_limit = vec![0_u8; MAX_BUFFERED_OUTPUT_BYTES];
        buf.append(&at_limit).expect("append at hard limit");
        assert!(buf.append(b"x").is_err());
        assert!(buf.take().is_empty());
    }
}
