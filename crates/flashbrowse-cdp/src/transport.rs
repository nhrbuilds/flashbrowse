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
#[path = "transport/tests.rs"]
mod tests;
