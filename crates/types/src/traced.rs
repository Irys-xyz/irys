use std::fmt;
use tokio::sync::mpsc::{self, UnboundedSender};

/// A value bundled with a parent tracing span for cross-service propagation.
///
/// Wraps service messages so that span context flows automatically through
/// channels without requiring each message variant to carry its own span field.
///
/// # Example
/// ```ignore
/// // Sending — captures the current span automatically:
/// sender.send_traced(MyServiceMessage::DoWork { data })?;
///
/// // Receiving — destructure in the service loop:
/// let Traced { inner: msg, span } = rx.recv().await?;
/// self.handle(msg).instrument(span).await;
/// ```
pub struct Traced<T> {
    pub inner: T,
    pub span: tracing::Span,
}

impl<T> Traced<T> {
    /// Capture the current span and wrap `inner`.
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            span: tracing::Span::current(),
        }
    }

    /// Wrap with an explicit span (for forwarding an existing parent).
    pub fn with_span(inner: T, span: tracing::Span) -> Self {
        Self { inner, span }
    }

    /// Destructure into (inner, span).
    pub fn into_parts(self) -> (T, tracing::Span) {
        (self.inner, self.span)
    }

    /// Enter the span on the current thread and return the inner value.
    ///
    /// The returned [`EnteredSpan`] guard keeps the span active on TLS.
    /// All tracing calls made while the guard is alive are recorded under
    /// this span. The span exits when the guard is dropped.
    pub fn into_inner(self) -> (T, tracing::span::EnteredSpan) {
        (self.inner, self.span.entered())
    }
}

impl<T> From<T> for Traced<T> {
    fn from(inner: T) -> Self {
        Self::new(inner)
    }
}

impl<T> From<Traced<T>> for (T, tracing::Span) {
    fn from(traced: Traced<T>) -> Self {
        (traced.inner, traced.span)
    }
}

impl<T: fmt::Debug> fmt::Debug for Traced<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Traced")
            .field("inner", &self.inner)
            .finish()
    }
}

/// Extension trait for sending traced messages through unbounded channels.
pub trait SendTraced<T> {
    /// Wrap `msg` with the current tracing span and send it.
    fn send_traced(&self, msg: T) -> Result<(), mpsc::error::SendError<Traced<T>>>;

    /// Wrap `msg` with an explicit parent span and send it.
    fn send_with_span(
        &self,
        msg: T,
        span: tracing::Span,
    ) -> Result<(), mpsc::error::SendError<Traced<T>>>;
}

impl<T> SendTraced<T> for UnboundedSender<Traced<T>> {
    fn send_traced(&self, msg: T) -> Result<(), mpsc::error::SendError<Traced<T>>> {
        self.send(Traced::new(msg))
    }

    fn send_with_span(
        &self,
        msg: T,
        span: tracing::Span,
    ) -> Result<(), mpsc::error::SendError<Traced<T>>> {
        self.send(Traced::with_span(msg, span))
    }
}
