//! Runtime used for flashbrowse-owned asynchronous operations.

use std::cell::Cell;
use std::future::{poll_fn, Future};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, OnceLock};
use std::task::Poll;

use crate::CdpError;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static INITIALIZING: Mutex<()> = Mutex::new(());

tokio::task_local! {
    static CONTAINED_PANIC: Cell<bool>;
}

/// Observe panics contained by nested runtime calls while polling this future.
#[doc(hidden)]
pub async fn track_contained_panics<F: Future>(future: F) -> (F::Output, bool) {
    CONTAINED_PANIC
        .scope(Cell::new(false), async {
            let result = future.await;
            (result, CONTAINED_PANIC.with(Cell::get))
        })
        .await
}

/// Contain panics from caller-provided asynchronous I/O polling and destructors.
#[doc(hidden)]
pub struct PanicSafeIo<T>(Option<T>);

impl<T> PanicSafeIo<T> {
    /// Wrap an I/O object so polling or destruction cannot unwind into its owner.
    #[doc(hidden)]
    pub fn new(value: T) -> Self {
        Self(Some(value))
    }
}

impl<T> Drop for PanicSafeIo<T> {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
                discard_panic_payload(payload);
            }
        }
    }
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for PanicSafeIo<T> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        contain_io_poll_panic(|| match self.get_mut().0.as_mut() {
            Some(reader) => {
                tokio::io::AsyncRead::poll_read(std::pin::Pin::new(reader), context, buffer)
            }
            None => Poll::Ready(Err(std::io::Error::other(
                "asynchronous reader was already dropped",
            ))),
        })
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for PanicSafeIo<T> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        contain_io_poll_panic(|| match self.get_mut().0.as_mut() {
            Some(writer) => {
                tokio::io::AsyncWrite::poll_write(std::pin::Pin::new(writer), context, buffer)
            }
            None => Poll::Ready(Err(std::io::Error::other(
                "asynchronous writer was already dropped",
            ))),
        })
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        contain_io_poll_panic(|| match self.get_mut().0.as_mut() {
            Some(writer) => tokio::io::AsyncWrite::poll_flush(std::pin::Pin::new(writer), context),
            None => Poll::Ready(Err(std::io::Error::other(
                "asynchronous writer was already dropped",
            ))),
        })
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        contain_io_poll_panic(|| match self.get_mut().0.as_mut() {
            Some(writer) => {
                tokio::io::AsyncWrite::poll_shutdown(std::pin::Pin::new(writer), context)
            }
            None => Poll::Ready(Err(std::io::Error::other(
                "asynchronous writer was already dropped",
            ))),
        })
    }
}

fn contain_io_poll_panic<T>(
    poll: impl FnOnce() -> Poll<std::io::Result<T>>,
) -> Poll<std::io::Result<T>> {
    match catch_unwind(AssertUnwindSafe(poll)) {
        Ok(result) => result,
        Err(payload) => {
            discard_panic_payload(payload);
            Poll::Ready(Err(std::io::Error::other(IoPollPanic)))
        }
    }
}

#[derive(Debug)]
struct IoPollPanic;

impl std::fmt::Display for IoPollPanic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("asynchronous I/O implementation panicked during poll")
    }
}

impl std::error::Error for IoPollPanic {}

/// Return true when an I/O error was created after containing a poll panic.
#[doc(hidden)]
pub fn is_io_poll_panic(error: &std::io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.downcast_ref::<IoPollPanic>().is_some())
}

fn runtime() -> Result<&'static tokio::runtime::Runtime, CdpError> {
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let _guard = INITIALIZING
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let built = catch_unwind(AssertUnwindSafe(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
    }));
    let runtime = match built {
        Ok(Ok(runtime)) => runtime,
        Ok(Err(error)) => {
            return Err(CdpError::Io(std::io::Error::other(format!(
                "cannot start flashbrowse runtime: {error}"
            ))));
        }
        Err(payload) => {
            discard_panic_payload(payload);
            return Err(internal_panic_error());
        }
    };
    if let Err(runtime) = RUNTIME.set(runtime) {
        runtime.shutdown_background();
    }
    RUNTIME
        .get()
        .ok_or_else(|| CdpError::Io(std::io::Error::other("cannot retain flashbrowse runtime")))
}

/// Start a task on the flashbrowse runtime.
pub fn spawn<F>(future: F) -> Result<tokio::task::JoinHandle<F::Output>, CdpError>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    Ok(runtime()?.spawn(future))
}

/// Poll a future with a runtime that has both I/O and timers enabled.
pub async fn run<F: Future>(future: F) -> Result<F::Output, CdpError> {
    let mut future = GuardedFuture {
        future: Some(Box::pin(future)),
    };
    let runtime = runtime()?;
    poll_fn(|context| {
        let Some(active) = future.future.as_mut() else {
            return Poll::Ready(Err(internal_panic_error()));
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = runtime.enter();
            active.as_mut().poll(context)
        }));
        match result {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => {
                let completed = future.future.take();
                if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(completed))) {
                    discard_panic_payload(payload);
                    discard_without_unwinding(output);
                    Poll::Ready(Err(internal_panic_error()))
                } else {
                    Poll::Ready(Ok(output))
                }
            }
            Err(payload) => {
                if let Some(active) = future.future.take() {
                    discard_without_unwinding(active);
                }
                discard_panic_payload(payload);
                Poll::Ready(Err(internal_panic_error()))
            }
        }
    })
    .await
}

struct GuardedFuture<F: Future> {
    future: Option<std::pin::Pin<Box<F>>>,
}

impl<F: Future> Drop for GuardedFuture<F> {
    fn drop(&mut self) {
        if let Some(future) = self.future.take() {
            discard_without_unwinding(future);
        }
    }
}

fn internal_panic_error() -> CdpError {
    let _ = CONTAINED_PANIC.try_with(|caught| caught.set(true));
    CdpError::Io(std::io::Error::other(
        "flashbrowse asynchronous operation failed unexpectedly",
    ))
}

/// A panicking destructor must not replace the original, recovered error.
fn discard_without_unwinding<T>(value: T) {
    if std::thread::panicking() {
        std::mem::forget(value);
        return;
    }
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
        discard_panic_payload(payload);
    }
}

#[doc(hidden)]
pub fn discard_panic_payload(payload: Box<dyn std::any::Any + Send>) {
    if std::thread::panicking() {
        std::mem::forget(payload);
        return;
    }
    if let Err(secondary_payload) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        // A panic payload is caller controlled through `panic_any`; leaking a
        // second payload is safer than allowing its destructor to unwind again.
        std::mem::forget(secondary_payload);
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    #[test]
    fn timer_works_when_caller_has_no_timer() {
        let caller = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        caller.block_on(async {
            let mut value = 0;
            run(async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                value = 1;
            })
            .await
            .unwrap();
            assert_eq!(value, 1);
        });
    }

    #[test]
    fn operation_panic_returns_an_error() {
        let caller = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("caller runtime");
        caller.block_on(async {
            let result = run(async { panic!("injected operation panic") }).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn panicking_future_destructor_does_not_escape_recovery() {
        struct PanicsOnPollAndDrop;

        impl Future for PanicsOnPollAndDrop {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                panic!("injected poll panic")
            }
        }

        impl Drop for PanicsOnPollAndDrop {
            fn drop(&mut self) {
                panic!("injected future destructor panic")
            }
        }

        let caller = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("caller runtime");
        caller.block_on(async {
            let result = run(PanicsOnPollAndDrop).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn destructor_panic_after_ready_returns_an_error() {
        struct PanicsOnDrop;

        impl Future for PanicsOnDrop {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Ready(())
            }
        }

        impl Drop for PanicsOnDrop {
            fn drop(&mut self) {
                panic!("injected future destructor panic")
            }
        }

        let caller = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("caller runtime");
        caller.block_on(async {
            let result = run(PanicsOnDrop).await;
            assert!(result.is_err());
        });
    }

    #[tokio::test]
    async fn cancellation_does_not_escape_a_panicking_future_destructor() {
        struct PendingPanicsOnDrop;

        impl Future for PendingPanicsOnDrop {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Pending
            }
        }

        impl Drop for PendingPanicsOnDrop {
            fn drop(&mut self) {
                panic!("injected cancelled future destructor panic")
            }
        }

        let task = tokio::spawn(run(PendingPanicsOnDrop));
        tokio::task::yield_now().await;
        task.abort();
        let result = task.await;
        assert!(result.is_err_and(|error| error.is_cancelled()));
    }

    #[test]
    fn panicking_panic_payload_destructor_does_not_escape_recovery() {
        struct PanicsOnDrop;

        impl Drop for PanicsOnDrop {
            fn drop(&mut self) {
                panic!("injected panic payload destructor panic")
            }
        }

        let caller = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("caller runtime");
        caller.block_on(async {
            let result = run(async { std::panic::panic_any(PanicsOnDrop) }).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn panic_payload_is_not_dropped_during_an_existing_unwind() {
        struct PanicsOnDrop;

        impl Drop for PanicsOnDrop {
            fn drop(&mut self) {
                panic!("injected panic payload destructor panic")
            }
        }

        struct PayloadGuard(Option<Box<dyn std::any::Any + Send>>);

        impl Drop for PayloadGuard {
            fn drop(&mut self) {
                if let Some(payload) = self.0.take() {
                    super::discard_panic_payload(payload);
                }
            }
        }

        let result = std::panic::catch_unwind(|| {
            let _guard = PayloadGuard(Some(Box::new(PanicsOnDrop)));
            panic!("injected outer panic")
        });
        assert!(result.is_err());
    }
}
