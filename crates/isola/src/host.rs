use std::{future::Future, pin::Pin, sync::Arc};

use bytes::Bytes;
use http_body::Frame;

use crate::value::Value;

/// Thread-safe error returned by host callbacks and output sinks.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Streaming body of an HTTP response returned to a guest.
///
/// Each stream item is either an HTTP body [`Frame`] or an error. Data frames
/// are forwarded without buffering the complete response in host memory.
pub type HttpBodyStream =
    Pin<Box<dyn futures::Stream<Item = core::result::Result<Frame<Bytes>, BoxError>> + Send>>;

/// HTTP request forwarded from a guest to [`Host::http_request`].
///
/// The request body is fully buffered. `None` represents an empty body.
pub type HttpRequest = http::Request<Option<Bytes>>;

/// HTTP response returned by [`Host::http_request`].
///
/// The response body is streamed back to the guest through [`HttpBodyStream`].
pub type HttpResponse = http::Response<HttpBodyStream>;

/// Severity or output channel associated with a guest log message.
///
/// Converting from a string recognizes the lowercase names returned by
/// [`LogLevel::as_str`]; an unrecognized name becomes [`LogLevel::Info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Trace-level diagnostic message.
    Trace,
    /// Debug-level diagnostic message.
    Debug,
    /// Informational message.
    Info,
    /// Warning message.
    Warn,
    /// Error message.
    Error,
    /// Critical or fatal diagnostic message.
    Critical,
    /// Text written to the guest's standard output stream.
    Stdout,
    /// Text written to the guest's standard error stream.
    Stderr,
}

impl LogLevel {
    /// Return the stable lowercase name of this level.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Critical => "critical",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

impl From<&str> for LogLevel {
    fn from(context: &str) -> Self {
        match context {
            "trace" => Self::Trace,
            "debug" => Self::Debug,
            "warn" => Self::Warn,
            "error" => Self::Error,
            "critical" => Self::Critical,
            "stdout" => Self::Stdout,
            "stderr" => Self::Stderr,
            _ => Self::Info,
        }
    }
}

/// Source context supplied with a guest log message.
///
/// Standard output and standard error have dedicated variants so consumers do
/// not need to compare context strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogContext<'a> {
    /// The guest's standard output stream.
    Stdout,
    /// The guest's standard error stream.
    Stderr,
    /// A language-runtime-defined logging context.
    Other(&'a str),
}

/// Owned source context carried by an [`OutputEvent::Log`] event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedLogContext {
    /// The guest's standard output stream.
    Stdout,
    /// The guest's standard error stream.
    Stderr,
    /// A language-runtime-defined logging context.
    Other(String),
}

impl From<LogContext<'_>> for OwnedLogContext {
    fn from(context: LogContext<'_>) -> Self {
        match context {
            LogContext::Stdout => Self::Stdout,
            LogContext::Stderr => Self::Stderr,
            LogContext::Other(context) => Self::Other(context.to_owned()),
        }
    }
}

/// One owned value, completion, or log record sent to an output channel or
/// synchronous output callback.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum OutputEvent {
    /// A value yielded or explicitly emitted by guest code.
    Item(Value),
    /// The final return value after guest execution completes.
    Complete(Option<Value>),
    /// One guest log record.
    Log {
        /// Severity or output channel.
        level: LogLevel,
        /// Source context supplied by the guest runtime.
        context: OwnedLogContext,
        /// Log message text.
        message: String,
    },
}

/// Receives values and logs produced by one guest operation.
///
/// The runtime awaits each callback. Returning an error aborts the current
/// operation and surfaces the error from the corresponding
/// [`Sandbox`](crate::sandbox::Sandbox) method.
pub trait OutputSink: Send + Sync + 'static {
    /// Receive one value yielded or explicitly emitted by guest code.
    ///
    /// # Errors
    ///
    /// Returning an error aborts the current guest operation.
    fn on_item(
        &self,
        value: Value,
    ) -> impl Future<Output = core::result::Result<(), BoxError>> + Send;

    /// Receive the final return value after guest execution completes.
    ///
    /// `None` means the guest completed without an encoded return value. This
    /// callback is distinct from [`OutputSink::on_item`], which may be invoked
    /// zero or more times first.
    ///
    /// # Errors
    ///
    /// Returning an error makes the current guest operation fail.
    fn on_complete(
        &self,
        value: Option<Value>,
    ) -> impl Future<Output = core::result::Result<(), BoxError>> + Send;

    /// Receive one guest log record.
    ///
    /// The default implementation ignores the record.
    ///
    /// # Errors
    ///
    /// Returns an error to fail the current guest execution when log delivery
    /// fails.
    fn on_log(
        &self,
        _level: LogLevel,
        _log_context: LogContext<'_>,
        _message: &str,
    ) -> impl Future<Output = core::result::Result<(), BoxError>> + Send {
        std::future::ready(Ok(()))
    }
}

type BoxSinkFuture<'a> =
    Pin<Box<dyn Future<Output = core::result::Result<(), BoxError>> + Send + 'a>>;

trait ErasedOutputSink: Send + Sync + 'static {
    fn on_item(&self, value: Value) -> BoxSinkFuture<'_>;

    fn on_complete(&self, value: Option<Value>) -> BoxSinkFuture<'_>;

    fn on_log<'a>(
        &'a self,
        level: LogLevel,
        log_context: LogContext<'a>,
        message: &'a str,
    ) -> BoxSinkFuture<'a>;
}

impl<T: OutputSink> ErasedOutputSink for T {
    fn on_item(&self, value: Value) -> BoxSinkFuture<'_> {
        Box::pin(OutputSink::on_item(self, value))
    }

    fn on_complete(&self, value: Option<Value>) -> BoxSinkFuture<'_> {
        Box::pin(OutputSink::on_complete(self, value))
    }

    fn on_log<'a>(
        &'a self,
        level: LogLevel,
        log_context: LogContext<'a>,
        message: &'a str,
    ) -> BoxSinkFuture<'a> {
        Box::pin(OutputSink::on_log(self, level, log_context, message))
    }
}

type SyncOutputCallback =
    dyn Fn(OutputEvent) -> core::result::Result<(), BoxError> + Send + Sync + 'static;

#[derive(Clone)]
enum OutputTargetKind {
    Discard,
    Capture,
    Bounded(tokio::sync::mpsc::Sender<OutputEvent>),
    Unbounded(tokio::sync::mpsc::UnboundedSender<OutputEvent>),
    Sync(Arc<SyncOutputCallback>),
    Async(Arc<dyn ErasedOutputSink>),
}

/// Run-scoped destination for guest values, completion, and log records.
///
/// Bounded and unbounded Tokio channels, synchronous callbacks, and built-in
/// collection avoid allocating a boxed future for each event. Arbitrary
/// [`OutputSink`] implementations use the asynchronous fallback.
#[derive(Clone)]
pub struct OutputTarget {
    kind: OutputTargetKind,
}

impl OutputTarget {
    /// Construct a target that discards all output.
    #[must_use]
    pub const fn discard() -> Self {
        Self {
            kind: OutputTargetKind::Discard,
        }
    }

    /// Construct a target backed by a bounded Tokio channel.
    ///
    /// Delivery waits for channel capacity, so the receiver must be driven
    /// concurrently when a run can produce more events than the channel holds.
    #[must_use]
    pub const fn bounded(sender: tokio::sync::mpsc::Sender<OutputEvent>) -> Self {
        Self {
            kind: OutputTargetKind::Bounded(sender),
        }
    }

    /// Construct a target backed by an unbounded Tokio channel.
    ///
    /// Delivery never waits for capacity; consumers must bound retained output
    /// according to their workload.
    #[must_use]
    pub const fn unbounded(sender: tokio::sync::mpsc::UnboundedSender<OutputEvent>) -> Self {
        Self {
            kind: OutputTargetKind::Unbounded(sender),
        }
    }

    /// Construct a target that invokes a synchronous callback for each event.
    ///
    /// The callback runs inline and must not block the sandbox executor.
    #[must_use]
    pub fn synchronous(
        callback: impl Fn(OutputEvent) -> core::result::Result<(), BoxError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: OutputTargetKind::Sync(Arc::new(callback)),
        }
    }

    /// Construct a target backed by a native-async [`OutputSink`].
    #[must_use]
    pub fn asynchronous<T: OutputSink>(sink: Arc<T>) -> Self {
        Self {
            kind: OutputTargetKind::Async(sink),
        }
    }

    pub(crate) const fn capture() -> Self {
        Self {
            kind: OutputTargetKind::Capture,
        }
    }

    pub(crate) const fn is_capture(&self) -> bool {
        matches!(self.kind, OutputTargetKind::Capture)
    }

    pub(crate) async fn on_item(&self, value: Value) -> core::result::Result<(), BoxError> {
        match &self.kind {
            OutputTargetKind::Discard | OutputTargetKind::Capture => Ok(()),
            OutputTargetKind::Bounded(sender) => sender
                .send(OutputEvent::Item(value))
                .await
                .map_err(|_| output_channel_closed()),
            OutputTargetKind::Unbounded(sender) => sender
                .send(OutputEvent::Item(value))
                .map_err(|_| output_channel_closed()),
            OutputTargetKind::Sync(callback) => callback(OutputEvent::Item(value)),
            OutputTargetKind::Async(sink) => sink.on_item(value).await,
        }
    }

    pub(crate) async fn on_complete(
        &self,
        value: Option<Value>,
    ) -> core::result::Result<(), BoxError> {
        match &self.kind {
            OutputTargetKind::Discard | OutputTargetKind::Capture => Ok(()),
            OutputTargetKind::Bounded(sender) => sender
                .send(OutputEvent::Complete(value))
                .await
                .map_err(|_| output_channel_closed()),
            OutputTargetKind::Unbounded(sender) => sender
                .send(OutputEvent::Complete(value))
                .map_err(|_| output_channel_closed()),
            OutputTargetKind::Sync(callback) => callback(OutputEvent::Complete(value)),
            OutputTargetKind::Async(sink) => sink.on_complete(value).await,
        }
    }

    pub(crate) async fn on_log(
        &self,
        level: LogLevel,
        context: LogContext<'_>,
        message: &str,
    ) -> core::result::Result<(), BoxError> {
        match &self.kind {
            OutputTargetKind::Discard | OutputTargetKind::Capture => Ok(()),
            OutputTargetKind::Bounded(sender) => sender
                .send(output_log_event(level, context, message))
                .await
                .map_err(|_| output_channel_closed()),
            OutputTargetKind::Unbounded(sender) => sender
                .send(output_log_event(level, context, message))
                .map_err(|_| output_channel_closed()),
            OutputTargetKind::Sync(callback) => callback(output_log_event(level, context, message)),
            OutputTargetKind::Async(sink) => sink.on_log(level, context, message).await,
        }
    }
}

impl<T: OutputSink> From<Arc<T>> for OutputTarget {
    fn from(sink: Arc<T>) -> Self {
        Self::asynchronous(sink)
    }
}

impl From<tokio::sync::mpsc::Sender<OutputEvent>> for OutputTarget {
    fn from(sender: tokio::sync::mpsc::Sender<OutputEvent>) -> Self {
        Self::bounded(sender)
    }
}

impl From<tokio::sync::mpsc::UnboundedSender<OutputEvent>> for OutputTarget {
    fn from(sender: tokio::sync::mpsc::UnboundedSender<OutputEvent>) -> Self {
        Self::unbounded(sender)
    }
}

fn output_log_event(level: LogLevel, context: LogContext<'_>, message: &str) -> OutputEvent {
    OutputEvent::Log {
        level,
        context: context.into(),
        message: message.to_owned(),
    }
}

fn output_channel_closed() -> BoxError {
    std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "output channel receiver dropped",
    )
    .into()
}

/// Capabilities that guest code can request from its host application.
///
/// Both methods reject requests by default, so an empty implementation grants
/// neither hostcalls nor outbound HTTP. Guest code can issue multiple
/// asynchronous requests concurrently; implementations must therefore be safe
/// for overlapping calls.
///
/// # Examples
///
/// A host can expose a small, named hostcall surface while leaving HTTP
/// disabled:
///
/// ```
/// use isola::{
///     host::{BoxError, Host},
///     value::Value,
/// };
///
/// #[derive(Clone)]
/// struct EchoHost;
///
/// impl Host for EchoHost {
///     async fn hostcall(&self, call_type: &str, payload: Value) -> Result<Value, BoxError> {
///         match call_type {
///             "echo" => Ok(payload),
///             _ => Err(std::io::Error::other("unsupported hostcall").into()),
///         }
///     }
/// }
/// ```
pub trait Host: Send + Sync + 'static {
    /// Handle a named request from guest code.
    ///
    /// `payload` contains the CBOR-encoded guest value. An error is reported to
    /// the guest as a failed hostcall.
    ///
    /// The default implementation returns an unsupported-operation error.
    ///
    /// # Errors
    ///
    /// Implementations may return any [`BoxError`] when the request is unknown
    /// or cannot be completed.
    fn hostcall(
        &self,
        call_type: &str,
        payload: Value,
    ) -> impl Future<Output = core::result::Result<Value, BoxError>> + Send {
        async move {
            let _payload = payload;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("unsupported hostcall: {call_type}"),
            )
            .into())
        }
    }

    /// Perform an HTTP request.
    ///
    /// Implementations own redirect behavior and header hygiene. In particular,
    /// remove any caller-supplied `Host` header before dispatching.
    /// The default implementation returns an unsupported-operation error.
    ///
    /// # Errors
    ///
    /// Implementations may return any [`BoxError`] when the request cannot be
    /// dispatched. Errors yielded later by [`HttpBodyStream`] are propagated
    /// while the guest consumes the response body.
    fn http_request(
        &self,
        req: HttpRequest,
    ) -> impl Future<Output = core::result::Result<HttpResponse, BoxError>> + Send {
        async move {
            let _req = req;
            Err(
                std::io::Error::new(std::io::ErrorKind::Unsupported, "unsupported http_request")
                    .into(),
            )
        }
    }
}

impl<T: Host + ?Sized> Host for Arc<T> {
    async fn hostcall(
        &self,
        call_type: &str,
        payload: Value,
    ) -> core::result::Result<Value, BoxError> {
        (**self).hostcall(call_type, payload).await
    }

    async fn http_request(&self, req: HttpRequest) -> core::result::Result<HttpResponse, BoxError> {
        (**self).http_request(req).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn value() -> Value {
        Value::from(Bytes::from_static(&[0x01]))
    }

    #[tokio::test]
    async fn bounded_target_delivers_all_event_kinds() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(3);
        let target = OutputTarget::bounded(sender);

        target.on_item(value()).await.unwrap();
        target.on_complete(None).await.unwrap();
        target
            .on_log(LogLevel::Info, LogContext::Other("runtime"), "message")
            .await
            .unwrap();

        assert!(matches!(receiver.recv().await, Some(OutputEvent::Item(_))));
        assert!(matches!(
            receiver.recv().await,
            Some(OutputEvent::Complete(None))
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(OutputEvent::Log {
                level: LogLevel::Info,
                context: OwnedLogContext::Other(context),
                message,
            }) if context == "runtime" && message == "message"
        ));
    }

    #[tokio::test]
    async fn unbounded_target_reports_closed_receiver() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        drop(receiver);

        let error = OutputTarget::unbounded(sender)
            .on_item(value())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "output channel receiver dropped");
    }

    #[tokio::test]
    async fn synchronous_and_capture_targets_avoid_async_fallback() {
        let event_count = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&event_count);
        let target = OutputTarget::synchronous(move |_event| {
            callback_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        target.on_item(value()).await.unwrap();
        target.on_complete(None).await.unwrap();
        assert_eq!(event_count.load(Ordering::Relaxed), 2);
    }

    struct CountingAsyncSink(AtomicUsize);

    impl OutputSink for CountingAsyncSink {
        fn on_item(
            &self,
            _value: Value,
        ) -> impl Future<Output = core::result::Result<(), BoxError>> + Send {
            self.0.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok(()))
        }

        fn on_complete(
            &self,
            _value: Option<Value>,
        ) -> impl Future<Output = core::result::Result<(), BoxError>> + Send {
            self.0.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn asynchronous_target_uses_private_adapter() {
        let sink = Arc::new(CountingAsyncSink(AtomicUsize::new(0)));
        let target = OutputTarget::from(Arc::clone(&sink));

        target.on_item(value()).await.unwrap();
        target.on_complete(None).await.unwrap();

        assert_eq!(sink.0.load(Ordering::Relaxed), 2);
    }
}
