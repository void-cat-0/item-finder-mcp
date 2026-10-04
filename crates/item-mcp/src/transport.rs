//! Request backpressure around the official SDK transport (no custom framing).
//!
//! Four admitted requests include handler work, queued responses and writes. At
//! capacity we read at most one extra request, retaining it without handing it
//! to the SDK service. Notifications ahead of that lookahead still flow, so a
//! cancellation immediately following the four requests can free a slot. A
//! cancellation behind a fifth request cannot bypass it on a single byte stream.

use std::{
    collections::HashMap,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use rmcp::{
    RoleServer,
    model::{ClientNotification, JsonRpcMessage, RequestId},
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{Transport, async_rw::AsyncRwTransport},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Mutex, Notify, OwnedMutexGuard},
};
use tokio_util::sync::CancellationToken;

const REQUEST_BUDGET: usize = 4;
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Requests {
    // true means send was called; cancellation must not free a queued write.
    ids: HashMap<RequestId, bool>,
}

#[derive(Default)]
struct Shared {
    requests: std::sync::Mutex<Requests>,
    changed: Notify,
    stopped: CancellationToken,
    send_gate: Arc<Mutex<()>>,
}

impl Shared {
    fn clear(&self) {
        self.requests.lock().unwrap().ids.clear();
        self.changed.notify_waiters();
    }

    fn terminate(&self) {
        self.stopped.cancel();
        self.clear();
    }
}

/// A server-only SDK transport wrapper with a four-request lifetime budget.
pub struct BoundedTransport<T> {
    inner: T,
    shared: Arc<Shared>,
    pending: Option<RxJsonRpcMessage<RoleServer>>,
    eof: bool,
    write_timeout: Duration,
}

impl<T> BoundedTransport<T> {
    pub fn new(inner: T) -> Self {
        Self::with_write_timeout(inner, WRITE_TIMEOUT)
    }

    /// Configure the independent write deadline (including writer queue wait).
    pub fn with_write_timeout(inner: T, write_timeout: Duration) -> Self {
        Self {
            inner,
            shared: Arc::default(),
            pending: None,
            eof: false,
            write_timeout,
        }
    }
}

/// Construct the official byte-stream transport with terminal-aware writes.
/// This also protects protocol-error writes made internally by SDK receive.
pub fn from_io<R, W>(
    read: R,
    write: W,
    write_timeout: Duration,
) -> BoundedTransport<impl Transport<RoleServer, Error = io::Error> + 'static>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let shared = Arc::new(Shared::default());
    let writer = TerminalWriter {
        inner: write,
        shared: shared.clone(),
        deadline: None,
        write_timeout,
    };
    BoundedTransport {
        inner: AsyncRwTransport::new_server(read, writer),
        shared,
        pending: None,
        eof: false,
        write_timeout,
    }
}

/// Production stdio, using rmcp's reader, writer, codec and protocol handling.
pub fn stdio() -> BoundedTransport<impl Transport<RoleServer, Error = io::Error> + 'static> {
    let (read, write) = rmcp::transport::stdio();
    from_io(read, write, WRITE_TIMEOUT)
}

struct TerminalWriter<W> {
    inner: W,
    shared: Arc<Shared>,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    write_timeout: Duration,
}

impl<W> TerminalWriter<W> {
    fn check(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if self.shared.stopped.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "MCP writer terminated",
            ));
        }
        let deadline = self
            .deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(self.write_timeout)));
        if deadline.as_mut().poll(cx).is_ready() {
            self.shared.terminate();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "MCP writer deadline exceeded",
            ));
        }
        Ok(())
    }

    fn checked_result<T>(&mut self, result: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        if matches!(result, Poll::Ready(Err(_))) {
            self.shared.terminate();
        }
        result
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for TerminalWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.check(cx)?;
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        if matches!(result, Poll::Ready(Ok(0))) && !bytes.is_empty() {
            self.shared.terminate();
        }
        self.checked_result(result)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check(cx)?;
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.deadline = None;
        }
        self.checked_result(result)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// A dropped send may have written only a prefix of a JSON message. Never poll
// another SDK send on that connection, including sends already waiting in line.
struct SendGuard {
    shared: Arc<Shared>,
    id: Option<RequestId>,
    completed: bool,
    // Drop runs before this field is unlocked, so a partial write is marked
    // terminal before any other send can acquire the gate.
    gate: Option<OwnedMutexGuard<()>>,
}

impl Drop for SendGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.shared.terminate();
        } else if let Some(id) = &self.id {
            self.shared.requests.lock().unwrap().ids.remove(id);
            self.shared.changed.notify_waiters();
        }
    }
}

impl<T> Drop for BoundedTransport<T> {
    fn drop(&mut self) {
        self.shared.terminate();
    }
}

impl<T> Transport<RoleServer> for BoundedTransport<T>
where
    T: Transport<RoleServer, Error = io::Error> + 'static,
{
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let id = match &item {
            JsonRpcMessage::Response(response) => Some(response.id.clone()),
            JsonRpcMessage::Error(error) => error.id.clone(),
            _ => None,
        };
        if let Some(id) = &id
            && let Some(sending) = self.shared.requests.lock().unwrap().ids.get_mut(id)
        {
            *sending = true;
        }
        let mut guard = SendGuard {
            shared: self.shared.clone(),
            id,
            completed: false,
            gate: None,
        };
        let send = self.inner.send(item);
        let deadline = tokio::time::Instant::now() + self.write_timeout;
        async move {
            let shared = guard.shared.clone();
            let result = tokio::select! {
                biased;
                _ = shared.stopped.cancelled() => Err(io::Error::new(io::ErrorKind::NotConnected, "MCP transport terminated")),
                result = tokio::time::timeout_at(deadline, async {
                    guard.gate = Some(shared.send_gate.clone().lock_owned().await);
                    if shared.stopped.is_cancelled() {
                        return Err(io::Error::new(io::ErrorKind::NotConnected, "MCP transport terminated"));
                    }
                    send.await
                }) => result.unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "MCP stdout write deadline exceeded"))),
            };
            if result.is_err() {
                shared.terminate();
            }
            guard.completed = true;
            drop(guard);
            result
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        loop {
            if self.shared.stopped.is_cancelled() {
                self.pending = None;
                let _ = self.inner.close().await;
                return None;
            }
            if self.eof {
                return None;
            }
            if self.pending.is_some() {
                let changed = self.shared.changed.notified();
                tokio::pin!(changed);
                // Register before checking capacity: send completion can run on
                // another thread while receive is inside the SDK's select loop.
                changed.as_mut().enable();
                let admitted = {
                    let mut requests = self.shared.requests.lock().unwrap();
                    if requests.ids.len() < REQUEST_BUDGET {
                        let Some(JsonRpcMessage::Request(request)) = &self.pending else {
                            unreachable!()
                        };
                        requests.ids.insert(request.id.clone(), false);
                        true
                    } else {
                        false
                    }
                };
                if admitted {
                    return self.pending.take();
                }
                tokio::select! {
                    _ = self.shared.stopped.cancelled() => {},
                    _ = changed => {},
                }
                continue;
            }
            let message = tokio::select! {
                biased;
                _ = self.shared.stopped.cancelled() => continue,
                message = self.inner.receive() => message,
            };
            let Some(message) = message else {
                self.eof = true;
                // Preserve the SDK's EOF response drain, but no more admissions.
                self.shared.clear();
                return None;
            };
            match &message {
                JsonRpcMessage::Request(request) => {
                    let duplicate = self
                        .shared
                        .requests
                        .lock()
                        .unwrap()
                        .ids
                        .contains_key(&request.id);
                    if duplicate {
                        tracing::error!(id = %request.id, "duplicate in-flight MCP request ID; terminating transport");
                        self.shared.terminate();
                        continue;
                    }
                    self.pending = Some(message);
                }
                JsonRpcMessage::Notification(notification) => {
                    if let ClientNotification::CancelledNotification(cancelled) =
                        &notification.notification
                        && let Some(id) = &cancelled.params.request_id
                    {
                        let mut requests = self.shared.requests.lock().unwrap();
                        if requests.ids.get(id) == Some(&false) {
                            requests.ids.remove(id);
                            self.shared.changed.notify_waiters();
                        }
                    }
                    return Some(message);
                }
                _ => return Some(message),
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.shared.terminate();
        self.pending = None;
        self.eof = true;
        self.inner.close().await
    }
}
