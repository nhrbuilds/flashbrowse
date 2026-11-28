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
            tokio::io::AsyncRead::poll_read(std::pin::Pin::new(&mut self.inner), context, buffer)
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
            tokio::io::AsyncRead::poll_read(std::pin::Pin::new(&mut self.inner), context, buffer)
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
