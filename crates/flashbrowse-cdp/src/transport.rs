//! WebSocket Transport & Correlation Engine.
//!
//! One `CdpClient` per browser WebSocket. Multiplexes any number of
//! `sessionId` (flatten-mode) RPCs over the same socket:
//! - Outgoing frames serialize to a single writer task via `mpsc::UnboundedSender`.
//! - Incoming frames with `id` resolve the matching `oneshot` pending entry.
//! - Incoming frames with `method` (no `id`) are routed to [`EventEmitter`].
//!
//! Every [`CdpClient::send`] call is bounded by `CALL_TIMEOUT` and returns a
//! typed [`CdpError::Timeout`] instead of hanging a dead socket forever.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::error::CdpError;
use crate::events::EventEmitter;

/// Per-RPC timeout.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Max accepted WebSocket text frame (32 MiB - screenshots/AXTree batches).
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
/// `connect_async` budget before socket is usable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

type PendingMap = DashMap<u64, (String, oneshot::Sender<Result<Value, CdpError>>)>;

/// Check before invoking Tokio APIs that require an entered runtime.
pub(crate) fn require_runtime() -> Result<(), CdpError> {
    tokio::runtime::Handle::try_current()
        .map(|_| ())
        .map_err(|_| {
            std::io::Error::other("CDP requires a Tokio runtime with I/O and time enabled").into()
        })
}

/// Validate library values before recursive JSON operations.
pub fn validate_json_depth(value: &Value) -> Result<(), CdpError> {
    const MAX_DEPTH: usize = 120;
    let mut pending = vec![(value, 0usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth >= MAX_DEPTH {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "JSON nesting exceeds the CDP depth limit",
            )
            .into());
        }
        let child_depth = depth.saturating_add(1);
        match value {
            Value::Array(items) => pending.extend(items.iter().map(|item| (item, child_depth))),
            Value::Object(items) => pending.extend(items.values().map(|item| (item, child_depth))),
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn drop_json_iteratively(root: Value) {
    let mut pending = vec![root];
    while let Some(value) = pending.pop() {
        match value {
            Value::Array(items) => pending.extend(items),
            Value::Object(items) => pending.extend(items.into_values()),
            _ => {}
        }
    }
}

#[derive(Default)]
struct BoundedJsonWriter {
    bytes: Vec<u8>,
    depth: usize,
    in_string: bool,
    escaped: bool,
}

impl std::io::Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for byte in bytes {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if *byte == b'\\' {
                    self.escaped = true;
                } else if *byte == b'"' {
                    self.in_string = false;
                }
            } else {
                match byte {
                    b'"' => self.in_string = true,
                    b'{' | b'[' => {
                        self.depth = self.depth.saturating_add(1);
                        if self.depth >= 120 {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "JSON nesting exceeds the CDP depth limit",
                            ));
                        }
                    }
                    b'}' | b']' => self.depth = self.depth.saturating_sub(1),
                    _ => {}
                }
            }
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn next_request_id(counter: &AtomicU64, method: &str) -> Result<u64, CdpError> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let Some(next) = current.checked_add(1) else {
            return Err(CdpError::ProtocolError {
                method: method.to_string(),
                code: -1,
                message: "CDP request id space exhausted".to_string(),
            });
        };
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(current),
            Err(actual) => current = actual,
        }
    }
}

/// Raw CDP client over a single browser WebSocket.
///
/// Clone is not provided intentionally - share via `Arc<CdpClient>`.
pub struct CdpClient {
    next_id: AtomicU64,
    pending: Arc<PendingMap>,
    sender: Mutex<Option<mpsc::UnboundedSender<String>>>,
    events: Arc<EventEmitter>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    connected: Arc<AtomicBool>,
}

fn fail_pending_oversize(pending: &PendingMap) {
    let keys: Vec<u64> = pending.iter().map(|e| *e.key()).collect();
    for key in keys {
        if let Some((_, (method, sender))) = pending.remove(&key) {
            let _ = sender.send(Err(CdpError::ProtocolError {
                method,
                code: -1,
                message: format!("CDP response exceeded {MAX_FRAME_BYTES} byte limit"),
            }));
        }
    }
}

fn fail_pending_disconnected(pending: &PendingMap) {
    let keys: Vec<u64> = pending.iter().map(|e| *e.key()).collect();
    for key in keys {
        if let Some((_, (method, sender))) = pending.remove(&key) {
            let _ = sender.send(Err(CdpError::Disconnected { method }));
        }
    }
}

struct TransportTask {
    connected: Arc<AtomicBool>,
    pending: Arc<PendingMap>,
    stop: tokio::sync::watch::Sender<bool>,
}

impl Drop for TransportTask {
    fn drop(&mut self) {
        self.connected.store(false, Ordering::SeqCst);
        let _ = self.stop.send_replace(true);
        fail_pending_disconnected(&self.pending);
    }
}

impl CdpClient {
    /// Connect to a freshly launched [`crate::ChromeProcess`], preferring POSIX
    /// pipe transport when available and falling back to WebSocket.
    pub async fn connect_chrome(chrome: &mut crate::ChromeProcess) -> Result<Self, CdpError> {
        crate::runtime::run(Self::connect_chrome_inner(chrome)).await?
    }

    async fn connect_chrome_inner(chrome: &mut crate::ChromeProcess) -> Result<Self, CdpError> {
        require_runtime()?;
        #[cfg(unix)]
        if let Some(pipes) = chrome.take_pipe_handles() {
            return Self::connect_pipe(pipes.read_pipe, pipes.write_pipe).await;
        }
        Self::connect(chrome.browser_ws_url()).await
    }

    /// Connect over `\0`-delimited POSIX DevTools pipes (`--remote-debugging-pipe`).
    #[cfg(unix)]
    pub async fn connect_pipe<R, W>(pipe_rx: R, pipe_tx: W) -> Result<Self, CdpError>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        crate::runtime::run(Self::connect_pipe_inner(pipe_rx, pipe_tx)).await?
    }

    #[cfg(unix)]
    async fn connect_pipe_inner<R, W>(pipe_rx: R, pipe_tx: W) -> Result<Self, CdpError>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        require_runtime()?;

        let pipe_rx = crate::runtime::PanicSafeIo::new(pipe_rx);
        let mut pipe_tx = crate::runtime::PanicSafeIo::new(pipe_tx);

        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let events = Arc::new(EventEmitter::new());
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        let connected = Arc::new(AtomicBool::new(true));
        let writer = TransportTask {
            connected: connected.clone(),
            pending: pending.clone(),
            stop: stop_tx.clone(),
        };
        let mut writer_stop = stop_rx.clone();
        let _ = *writer_stop.borrow_and_update();
        tasks.push(tokio::spawn(async move {
            let _writer = writer;
            loop {
                let text = tokio::select! {
                    biased;
                    _ = writer_stop.changed() => break,
                    text = out_rx.recv() => text,
                };
                let Some(text) = text else {
                    break;
                };
                let result = tokio::select! {
                    biased;
                    _ = writer_stop.changed() => break,
                    result = async {
                        pipe_tx.write_all(text.as_bytes()).await?;
                        pipe_tx.write_all(b"\0").await?;
                        pipe_tx.flush().await
                    } => result,
                };
                if result.is_err() {
                    tracing::warn!("CDP pipe writer disconnected");
                    break;
                }
            }
        }));

        let reader = TransportTask {
            connected: connected.clone(),
            pending: pending.clone(),
            stop: stop_tx,
        };
        let events_rx = events.clone();
        let mut reader_stop = stop_rx;
        let _ = *reader_stop.borrow_and_update();
        tasks.push(tokio::spawn(async move {
            let mut buffer = Vec::with_capacity(64 * 1024);
            let mut input = BufReader::with_capacity(16 * 1024, pipe_rx);
            loop {
                let result = tokio::select! {
                    biased;
                    _ = reader_stop.changed() => break,
                    result = input.fill_buf() => result,
                };
                let Ok(bytes) = result else {
                    break;
                };
                if bytes.is_empty() {
                    break;
                }
                let delimiter = bytes.iter().position(|byte| *byte == 0);
                let Some(frame) = bytes.get(..delimiter.unwrap_or(bytes.len())) else {
                    break;
                };
                if frame.len() > MAX_FRAME_BYTES.saturating_sub(buffer.len()) {
                    reader.connected.store(false, Ordering::SeqCst);
                    fail_pending_oversize(&reader.pending);
                    break;
                }
                buffer.extend_from_slice(frame);
                let consumed = frame.len().saturating_add(usize::from(delimiter.is_some()));
                input.consume(consumed);
                if delimiter.is_some() {
                    if let Ok(value) = serde_json::from_slice::<Value>(&buffer) {
                        dispatch_frame(&value, &reader.pending, &events_rx);
                    }
                    buffer.clear();
                }
            }
        }));

        Ok(Self {
            next_id: AtomicU64::new(1),
            pending,
            sender: Mutex::new(Some(out_tx)),
            events,
            tasks: Mutex::new(tasks),
            connected,
        })
    }

    /// Connect to a browser-level DevTools WebSocket URL and spawn
    /// reader/writer background tasks.
    pub async fn connect(ws_url: &str) -> Result<Self, CdpError> {
        crate::runtime::run(Self::connect_inner(ws_url)).await?
    }

    async fn connect_inner(ws_url: &str) -> Result<Self, CdpError> {
        require_runtime()?;
        const ATTEMPTS: u32 = 6;
        let mut last_err = CdpError::WsConnectFailed {
            url: ws_url.to_string(),
            detail: "connect failed".to_string(),
        };
        let mut socket = None;
        for attempt in 0..ATTEMPTS {
            match tokio::time::timeout(
                CONNECT_TIMEOUT,
                tokio_tungstenite::connect_async_with_config(
                    ws_url,
                    Some(tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
                        max_message_size: Some(MAX_FRAME_BYTES),
                        max_frame_size: Some(MAX_FRAME_BYTES),
                        ..Default::default()
                    }),
                    false,
                ),
            )
            .await
            {
                Ok(Ok(pair)) => {
                    socket = Some(pair.0);
                    break;
                }
                Ok(Err(e)) => {
                    last_err = CdpError::WsConnectFailed {
                        url: ws_url.to_string(),
                        detail: e.to_string(),
                    };
                }
                Err(_) => {
                    last_err = CdpError::WsConnectFailed {
                        url: ws_url.to_string(),
                        detail: "connect timed out".to_string(),
                    };
                }
            }
            let completed = attempt.saturating_add(1);
            if completed < ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(
                    250u64.saturating_mul(completed.into()),
                ))
                .await;
            }
        }
        let socket = socket.ok_or(last_err)?;

        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let events = Arc::new(EventEmitter::new());
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

        let (mut write, mut read) = socket.split();
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        // Writer loop
        let connected = Arc::new(AtomicBool::new(true));
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let writer = TransportTask {
            connected: connected.clone(),
            pending: pending.clone(),
            stop: stop_tx.clone(),
        };
        let mut writer_stop = stop_rx.clone();
        let _ = *writer_stop.borrow_and_update();
        tasks.push(tokio::spawn(async move {
            let _writer = writer;
            loop {
                let text = tokio::select! {
                    biased;
                    _ = writer_stop.changed() => break,
                    text = out_rx.recv() => text,
                };
                let Some(text) = text else {
                    break;
                };
                let result = tokio::select! {
                    biased;
                    _ = writer_stop.changed() => break,
                    result = write.send(Message::Text(text)) => result,
                };
                if result.is_err() {
                    tracing::warn!("CDP WebSocket writer disconnected");
                    break;
                }
            }
        }));

        // Reader loop
        let reader = TransportTask {
            connected: connected.clone(),
            pending: pending.clone(),
            stop: stop_tx,
        };
        let events_rx = events.clone();
        let mut reader_stop = stop_rx;
        let _ = *reader_stop.borrow_and_update();
        tasks.push(tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    biased;
                    _ = reader_stop.changed() => break,
                    frame = read.next() => frame,
                };
                let Some(frame) = frame else {
                    break;
                };
                let text = match frame {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Close(_)) => break,
                    Ok(_) => continue,
                    Err(tokio_tungstenite::tungstenite::Error::Capacity(_)) => {
                        reader.connected.store(false, Ordering::SeqCst);
                        fail_pending_oversize(&reader.pending);
                        break;
                    }
                    Err(_) => break,
                };
                // Max-frame cap (M1): oversize frames are dropped, not
                // parsed - protects the reader task from OOM per page.
                if text.len() > MAX_FRAME_BYTES {
                    tracing::warn!(
                        bytes = text.len(),
                        "oversize CDP frame - failing pending calls"
                    );
                    reader.connected.store(false, Ordering::SeqCst);
                    fail_pending_oversize(&reader.pending);
                    break;
                }
                let value: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                dispatch_frame(&value, &reader.pending, &events_rx);
            }
        }));

        Ok(Self {
            next_id: AtomicU64::new(1),
            pending,
            sender: Mutex::new(Some(out_tx)),
            events,
            tasks: Mutex::new(tasks),
            connected,
        })
    }

    /// Shared event router for `connect`ed transports.
    pub fn events(&self) -> Arc<EventEmitter> {
        self.events.clone()
    }

    /// Whether both transport halves are still usable.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Fail all still-pending calls, close writer, abort I/O tasks.
    ///
    /// Socket halves close once the writer task exits and this client is
    /// dropped. Pair with `ChromeProcess::shutdown()` in `BrowserSession`.
    pub fn shutdown(&self) {
        self.connected.store(false, Ordering::SeqCst);
        // Take the sender so writer loop sees channel close and exits.
        let mut slot = match self.sender.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = slot.take();
        drop(slot);
        let keys: Vec<u64> = self.pending.iter().map(|e| *e.key()).collect();
        for key in keys {
            if let Some((_, (method, sender))) = self.pending.remove(&key) {
                let _ = sender.send(Err(CdpError::Disconnected { method }));
            }
        }
        let mut tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(poisoned) => poisoned.into_inner(),
        };
        for h in tasks.drain(..) {
            h.abort();
        }
        tracing::debug!("CdpClient shutdown: pending calls drained");
    }

    fn shutdown_during_unwind(&mut self) {
        self.connected.store(false, Ordering::SeqCst);
        match self.sender.get_mut() {
            Ok(sender) => {
                drop(sender.take());
            }
            Err(poisoned) => {
                drop(poisoned.into_inner().take());
            }
        }
        match self.tasks.get_mut() {
            Ok(tasks) => {
                for task in tasks.drain(..) {
                    task.abort();
                }
            }
            Err(poisoned) => {
                for task in poisoned.into_inner().drain(..) {
                    task.abort();
                }
            }
        }
    }

    /// Send a JSON-RPC `value` payload and await its `result`.
    ///
    /// `session_id` is included as top-level `sessionId` when `Some` (flatten
    /// mode). `method` is e.g. `"Page.navigate"`.
    pub fn send<'a>(
        &'a self,
        session_id: Option<&'a str>,
        method: &'a str,
        params: Value,
    ) -> impl std::future::Future<Output = Result<Value, CdpError>> + 'a {
        let params = match validate_json_depth(&params) {
            Ok(()) => Ok(params),
            Err(error) => {
                drop_json_iteratively(params);
                Err(error)
            }
        };
        async move { crate::runtime::run(self.send_inner(session_id, method, params?)).await? }
    }

    async fn send_inner(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, CdpError> {
        require_runtime()?;
        let id = next_request_id(&self.next_id, method)?;
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, (method.to_string(), tx));

        // Guard the pending entry: if the caller cancels `send` mid-flight
        // (select/timeout), the entry is dropped - no unbounded growth (M2).
        struct PendingGuard<'a> {
            pending: &'a PendingMap,
            id: u64,
            armed: bool,
        }
        impl Drop for PendingGuard<'_> {
            fn drop(&mut self) {
                if self.armed {
                    self.pending.remove(&self.id);
                }
            }
        }
        let mut guard = PendingGuard {
            pending: &self.pending,
            id,
            armed: true,
        };

        let mut frame = serde_json::Map::from_iter([
            ("id".into(), Value::from(id)),
            ("method".into(), Value::from(method)),
            ("params".into(), params),
        ]);
        if let Some(sid) = session_id {
            frame.insert("sessionId".into(), Value::from(sid));
        }
        let frame = Value::Object(frame);
        if !self.connected.load(Ordering::SeqCst) {
            return Err(CdpError::Disconnected {
                method: method.to_string(),
            });
        }
        let sent: Result<(), ()> = match self.sender.lock() {
            Ok(slot) => match slot.as_ref() {
                Some(tx) => tx.send(frame.to_string()).map_err(|_| ()),
                None => Err(()),
            },
            Err(_) => Err(()),
        };
        if sent.is_err() {
            return Err(CdpError::Disconnected {
                method: method.to_string(),
            });
        }

        let rx_result = tokio::time::timeout(CALL_TIMEOUT, rx).await;
        match rx_result {
            Err(_) => Err(CdpError::Timeout {
                method: method.to_string(),
            }),
            Ok(Err(_)) => Err(CdpError::Disconnected {
                method: method.to_string(),
            }),
            Ok(Ok(inner)) => {
                guard.armed = false;
                inner.map_err(|e| match e {
                    CdpError::ProtocolError { code, message, .. } => CdpError::ProtocolError {
                        method: method.to_string(),
                        code,
                        message,
                    },
                    CdpError::Disconnected { .. } => CdpError::Disconnected {
                        method: method.to_string(),
                    },
                    other => other,
                })
            }
        }
    }

    /// Typed variant of [`send`]: serializes `P` to params, deserializes result to `R`.
    pub async fn send_typed<P, R>(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: &P,
    ) -> Result<R, CdpError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let mut writer = BoundedJsonWriter::default();
        let serialized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            serde_json::to_writer(&mut writer, params)
        }));
        let serialized = match serialized {
            Ok(result) => result,
            Err(payload) => {
                crate::runtime::discard_panic_payload(payload);
                return Err(CdpError::Io(std::io::Error::other(
                    "CDP parameter serialization failed unexpectedly",
                )));
            }
        };
        serialized?;
        let params_value = serde_json::from_slice(&writer.bytes)?;
        let result = self.send(session_id, method, params_value).await?;
        let deserialized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            serde_json::from_value(result)
        }));
        match deserialized {
            Ok(result) => result.map_err(CdpError::from),
            Err(payload) => {
                crate::runtime::discard_panic_payload(payload);
                Err(CdpError::Io(std::io::Error::other(
                    "CDP response deserialization failed unexpectedly",
                )))
            }
        }
    }
}

impl Drop for CdpClient {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.shutdown_during_unwind();
            return;
        }
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.shutdown();
        })) {
            if let Err(secondary_payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
            {
                std::mem::forget(secondary_payload);
            }
        }
    }
}

pub(crate) fn dispatch_frame(value: &Value, pending: &PendingMap, events: &EventEmitter) {
    if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
        if let Some((_, (method, sender))) = pending.remove(&id) {
            if let Some(err) = value.get("error") {
                let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
                let message = err
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown CDP error")
                    .to_string();
                let _ = sender.send(Err(CdpError::ProtocolError {
                    method,
                    code,
                    message,
                }));
            } else if let Some(result) = value.get("result") {
                let _ = sender.send(Ok(result.clone()));
            } else {
                let _ = sender.send(Err(CdpError::ProtocolError {
                    method,
                    code: -1,
                    message: "response missing result and error".to_string(),
                }));
            }
            return;
        }
        // Unknown id: ignore (late response after timeout).
        return;
    }

    if let Some(method) = value.get("method").and_then(|v| v.as_str()) {
        let session_id = value
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        events.emit_with_session(method, session_id, params);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn pipe_reader_poll_and_destructor_panics_disconnect_without_process_panic() {
        struct PanicReader;
        impl tokio::io::AsyncRead for PanicReader {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                panic!("injected CDP pipe reader panic");
            }
        }
        impl Drop for PanicReader {
            fn drop(&mut self) {
                panic!("injected CDP pipe reader destructor panic");
            }
        }

        let client = CdpClient::connect_pipe(PanicReader, tokio::io::sink())
            .await
            .expect("pipe setup completes before the background reader runs");
        // The background reader runs on the shared flashbrowse runtime, so
        // concurrent tests may delay its first poll. Keep this as a bounded
        // condition wait without treating scheduler contention as a failure.
        tokio::time::timeout(Duration::from_secs(10), async {
            while client.is_connected() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reader failure disconnects the CDP client");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_client_during_unwind_aborts_pipe_reader() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct DropTrackedReader {
            inner: tokio::io::DuplexStream,
            dropped: Arc<AtomicBool>,
        }
        impl tokio::io::AsyncRead for DropTrackedReader {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                context: &mut std::task::Context<'_>,
                buffer: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                tokio::io::AsyncRead::poll_read(
                    std::pin::Pin::new(&mut self.inner),
                    context,
                    buffer,
                )
            }
        }
        impl Drop for DropTrackedReader {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        let (client_read, _peer_write) = tokio::io::duplex(64);
        let reader_dropped = Arc::new(AtomicBool::new(false));
        let client = CdpClient::connect_pipe(
            DropTrackedReader {
                inner: client_read,
                dropped: Arc::clone(&reader_dropped),
            },
            tokio::io::sink(),
        )
        .await
        .expect("connect pipe client");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _client = client;
            panic!("simulate a panic while the browser client is owned");
        }));
        assert!(result.is_err());

        tokio::time::timeout(Duration::from_secs(1), async {
            while !reader_dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("unwinding shutdown drops the pipe reader");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writer_failure_stops_the_pipe_reader_task() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct DropTrackedReader {
            inner: tokio::io::DuplexStream,
            dropped: Arc<AtomicBool>,
        }
        impl tokio::io::AsyncRead for DropTrackedReader {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                context: &mut std::task::Context<'_>,
                buffer: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                tokio::io::AsyncRead::poll_read(
                    std::pin::Pin::new(&mut self.inner),
                    context,
                    buffer,
                )
            }
        }
        impl Drop for DropTrackedReader {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        struct FailingWriter;
        impl tokio::io::AsyncWrite for FailingWriter {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected pipe writer failure",
                )))
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let (client_read, _peer_write) = tokio::io::duplex(64);
        let reader_dropped = Arc::new(AtomicBool::new(false));
        let client = CdpClient::connect_pipe(
            DropTrackedReader {
                inner: client_read,
                dropped: Arc::clone(&reader_dropped),
            },
            FailingWriter,
        )
        .await
        .expect("connect pipe client");

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            client.send(None, "Browser.getVersion", json!({})),
        )
        .await
        .expect("failed writer settles the pending CDP call");
        assert!(matches!(result, Err(CdpError::Disconnected { .. })));

        tokio::time::timeout(Duration::from_secs(1), async {
            while !reader_dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer failure cancels the pipe reader");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pipe_reader_eof_stops_the_writer_task() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct DropTrackedWriter {
            dropped: Arc<AtomicBool>,
        }
        impl tokio::io::AsyncWrite for DropTrackedWriter {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "unexpected write after reader EOF",
                )))
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        impl Drop for DropTrackedWriter {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        let writer_dropped = Arc::new(AtomicBool::new(false));
        let client = CdpClient::connect_pipe(
            tokio::io::empty(),
            DropTrackedWriter {
                dropped: Arc::clone(&writer_dropped),
            },
        )
        .await
        .expect("connect pipe client");

        tokio::time::timeout(Duration::from_secs(1), async {
            while client.is_connected() || !writer_dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reader EOF cancels the pipe writer");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pipe_reader_eof_cancels_a_writer_blocked_inside_io() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct PendingWriter {
            started: Arc<AtomicBool>,
            dropped: Arc<AtomicBool>,
        }
        impl tokio::io::AsyncWrite for PendingWriter {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                self.started.store(true, Ordering::SeqCst);
                std::task::Poll::Pending
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Pending
            }

            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        impl Drop for PendingWriter {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        let (peer_writer, client_reader) = tokio::io::duplex(64);
        let started = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let client = Arc::new(
            CdpClient::connect_pipe(
                client_reader,
                PendingWriter {
                    started: Arc::clone(&started),
                    dropped: Arc::clone(&dropped),
                },
            )
            .await
            .expect("connect pipe client"),
        );
        let pending = tokio::spawn({
            let client = client.clone();
            async move { client.send(None, "Browser.getVersion", json!({})).await }
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer should enter its pending I/O operation");

        drop(peer_writer);
        let result = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .expect("reader EOF should cancel blocked writer I/O")
            .expect("pending send task should finish");
        assert!(matches!(result, Err(CdpError::Disconnected { .. })));
        assert!(dropped.load(Ordering::SeqCst));
        client.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn pipe_send_works_with_no_caller_timer_or_io_driver() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (client_write, peer_read) = tokio::io::duplex(1024);
            let (mut peer_write, client_read) = tokio::io::duplex(1024);
            let client = CdpClient::connect_pipe(client_read, client_write)
                .await
                .unwrap();
            let peer = tokio::spawn(async move {
                let mut reader = BufReader::new(peer_read);
                let mut frame = Vec::new();
                reader.read_until(0, &mut frame).await.unwrap();
                frame.pop();
                let request: Value = serde_json::from_slice(&frame).unwrap();
                let reply = serde_json::json!({"id": request["id"], "result": {"ok": true}});
                peer_write
                    .write_all(reply.to_string().as_bytes())
                    .await
                    .unwrap();
                peer_write.write_all(b"\0").await.unwrap();
            });
            let reply = client
                .send(None, "test", serde_json::json!({}))
                .await
                .unwrap();
            assert_eq!(reply["ok"], true);
            peer.await.unwrap();
        });
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn panicking_parameter_serializer_returns_an_error() {
        struct PanicsOnDrop;

        impl Drop for PanicsOnDrop {
            fn drop(&mut self) {
                panic!("injected panic payload destructor panic")
            }
        }

        struct BadSerializer;
        impl serde::Serialize for BadSerializer {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                std::panic::panic_any(PanicsOnDrop)
            }
        }

        let (write, _read_peer) = tokio::io::duplex(128);
        let (_write_peer, read) = tokio::io::duplex(128);
        let client = CdpClient::connect_pipe(read, write).await.unwrap();
        let error = client
            .send_typed::<_, Value>(None, "test", &BadSerializer)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("serialization failed"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn typed_parameters_reject_deep_nesting_before_serialization_overflows() {
        let (write, _read_peer) = tokio::io::duplex(128);
        let (_write_peer, read) = tokio::io::duplex(128);
        let client = CdpClient::connect_pipe(read, write).await.unwrap();
        let value = (0..10_000).fold(Value::Null, |value, _| Value::Array(vec![value]));
        let error = client
            .send_typed::<_, Value>(None, "test", &value)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("nesting"));
        drop_json_iteratively(value);
        let value = (0..10_000).fold(Value::Null, |value, _| Value::Array(vec![value]));
        drop(client.send(None, "test", value));
    }

    #[test]
    fn deeply_nested_owned_json_is_rejected_and_dropped_iteratively() {
        let value = (0..10_000).fold(Value::Null, |value, _| Value::Array(vec![value]));
        assert!(validate_json_depth(&value).is_err());
        drop_json_iteratively(value);
    }

    #[test]
    fn request_ids_fail_cleanly_instead_of_wrapping() {
        let counter = AtomicU64::new(u64::MAX);
        let err = next_request_id(&counter, "Page.navigate").unwrap_err();
        assert!(matches!(
            err,
            CdpError::ProtocolError {
                method,
                message,
                ..
            } if method == "Page.navigate" && message.contains("exhausted")
        ));
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
    use serde_json::json;
    use tokio::sync::oneshot;

    #[test]
    fn dispatch_resolves_pending_rpc_with_result() {
        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let events = EventEmitter::new();
        let (tx, mut rx) = oneshot::channel();
        pending.insert(7, ("Test.method".to_string(), tx));
        dispatch_frame(&json!({"id": 7, "result": {"ok": true}}), &pending, &events);
        let got = rx.try_recv().expect("sender fired");
        assert!(got.is_ok());
        assert_eq!(got.unwrap()["ok"], true);
    }

    #[test]
    fn dispatch_maps_protocol_error() {
        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let events = EventEmitter::new();
        let (tx, mut rx) = oneshot::channel();
        pending.insert(1, ("Page.navigate".to_string(), tx));
        dispatch_frame(
            &json!({"id": 1, "error": {"code": -32000, "message": "boom"}}),
            &pending,
            &events,
        );
        let err = rx.try_recv().unwrap().unwrap_err();
        assert!(matches!(err, CdpError::ProtocolError { message, .. } if message == "boom"));
    }

    #[test]
    fn dispatch_ignores_unknown_id_without_panicking() {
        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let events = EventEmitter::new();
        dispatch_frame(&json!({"id": 99999, "result": {}}), &pending, &events);
    }

    #[test]
    fn dispatch_emits_events_without_id() {
        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let events = EventEmitter::new();
        let mut sub = events.subscribe("Network.loadingFinished");
        dispatch_frame(
            &json!({"method": "Network.loadingFinished", "params": {"requestId": "x"}}),
            &pending,
            &events,
        );
        let ev = sub.try_recv().expect("event emitted");
        assert_eq!(ev["requestId"], "x");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pipe_rejects_an_oversize_frame_without_a_delimiter() {
        use tokio::io::AsyncReadExt;
        let (client_tx, mut peer_rx) = tokio::io::duplex(256);
        let (mut peer_tx, client_rx) = tokio::io::duplex(64 * 1024);
        let client = CdpClient::connect_pipe(client_rx, client_tx).await.unwrap();
        let peer = tokio::spawn(async move {
            let mut byte = [0];
            peer_rx.read_exact(&mut byte).await.unwrap();
            let mut source = tokio::io::repeat(b' ').take((MAX_FRAME_BYTES + 1) as u64);
            tokio::io::copy(&mut source, &mut peer_tx).await.unwrap();
        });
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            client.send(None, "Browser.getVersion", json!({})),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(
            matches!(error, CdpError::ProtocolError { message, .. } if message.contains("byte limit"))
        );
        assert!(!client.is_connected());
        peer.await.unwrap();
        client.shutdown();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pipe_reader_eof_marks_the_connection_dead() {
        use tokio::io::AsyncReadExt;

        let (client_tx, mut peer_rx) = tokio::io::duplex(256);
        let (peer_tx, client_rx) = tokio::io::duplex(256);
        let client = Arc::new(CdpClient::connect_pipe(client_rx, client_tx).await.unwrap());
        let pending = tokio::spawn({
            let client = client.clone();
            async move { client.send(None, "Browser.getVersion", json!({})).await }
        });
        let mut byte = [0];
        peer_rx.read_exact(&mut byte).await.unwrap();
        drop(peer_tx);
        let err = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .expect("EOF must unblock the pending request")
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, CdpError::Disconnected { .. }));
        assert!(!client.is_connected());
        assert!(matches!(
            client.send(None, "Browser.getVersion", json!({})).await,
            Err(CdpError::Disconnected { .. })
        ));
        client.shutdown();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_client_closes_pipe_background_tasks() {
        use tokio::io::AsyncReadExt;

        let (client_write, mut peer_read) = tokio::io::duplex(64);
        let (_peer_write, client_read) = tokio::io::duplex(64);
        let client = CdpClient::connect_pipe(client_read, client_write)
            .await
            .expect("pipe client");

        drop(client);

        let mut byte = [0];
        let read = tokio::time::timeout(Duration::from_secs(1), peer_read.read(&mut byte))
            .await
            .expect("drop should close the writer pipe")
            .expect("pipe read");
        assert_eq!(read, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pipe_transport_handles_framing_events_and_shutdown() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (client_tx, mut peer_rx) = tokio::io::duplex(64 * 1024);
        let (mut peer_tx, client_rx) = tokio::io::duplex(64 * 1024);
        let client = CdpClient::connect_pipe(client_rx, client_tx)
            .await
            .expect("connect_pipe");
        let mut ev_rx = client.events().subscribe("Target.targetCreated");

        let peer_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = peer_rx.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.iter().position(|&b| b == 0) {
                    let req: Value = serde_json::from_slice(&buf[..pos]).unwrap();
                    let id = req["id"].as_u64().unwrap();
                    let ev = json!({"method": "Target.targetCreated", "params": {"targetInfo": {"targetId": "t1"}}});
                    let reply = json!({"id": id, "result": {"echo": req["method"]}});
                    // Send event + split reply across two writes to test framing.
                    let mut payload = serde_json::to_vec(&ev).unwrap();
                    payload.push(0);
                    let reply_bytes = serde_json::to_vec(&reply).unwrap();
                    let mid = reply_bytes.len() / 2;
                    peer_tx.write_all(&payload).await.unwrap();
                    peer_tx.write_all(&reply_bytes[..mid]).await.unwrap();
                    peer_tx.flush().await.unwrap();
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    peer_tx.write_all(&reply_bytes[mid..]).await.unwrap();
                    peer_tx.write_all(b"\0").await.unwrap();
                    peer_tx.flush().await.unwrap();
                    break;
                }
            }
        });

        let res = client
            .send(None, "Browser.getVersion", json!({}))
            .await
            .expect("pipe rpc");
        assert_eq!(res["echo"], "Browser.getVersion");
        let ev = ev_rx.recv().await.expect("event delivered");
        assert_eq!(ev["targetInfo"]["targetId"], "t1");

        client.shutdown();
        peer_task.await.unwrap();
    }
}
