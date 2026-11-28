use super::*;

impl ChromeProcess {
    /// Browser-level WebSocket URL: `ws://127.0.0.1:.../devtools/browser/...`
    pub fn browser_ws_url(&self) -> &str {
        &self.browser_ws_url
    }

    /// Resolved Chromium binary path.
    pub fn chrome_path(&self) -> &PathBuf {
        &self.chrome_path
    }

    /// User-data dir used for this instance.
    pub fn user_data_dir(&self) -> &PathBuf {
        &self.user_data_dir
    }

    /// OS process id.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    /// Take the `--remote-debugging-pipe` descriptors if launched in pipe mode.
    #[cfg(unix)]
    pub fn take_pipe_handles(&mut self) -> Option<PipeHandles> {
        self.pipe_handles.take()
    }

    /// Kill the whole process tree (Unix: process group) and wait.
    /// Removes the auto-created profile dir (owned dirs only).
    pub async fn shutdown(&mut self) {
        if let Err(error) = crate::runtime::run(self.shutdown_inner()).await {
            tracing::warn!(target: "flashbrowse_cdp::launcher", %error, "could not complete Chromium shutdown");
        }
    }

    async fn shutdown_inner(&mut self) {
        self.kill_tree().await;
        if std::mem::take(&mut self.owned_dir) {
            remove_profile(&self.user_data_dir).await;
        }
    }

    async fn kill_tree(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid() {
            // Signal the whole process group so zygote and GPU children die.
            if let Err(err) = signal_chrome_process_group(pid) {
                tracing::debug!(target: "flashbrowse_cdp::launcher", %err, pid, "could not signal Chromium process group");
            }
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            if tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .is_err()
            {
                tracing::warn!(target: "flashbrowse_cdp::launcher", pid = child.id(), "Chromium did not reap within 5 seconds");
            }
        }
    }

    /// Detach: release the child process (it keeps running), suppressing
    /// the `Drop` kill. Returns the raw child handle plus the profile dir,
    /// which the caller now owns (session-file owner removes on `close`).
    ///
    /// Returns `None` if the process was already shut down.
    pub fn detach(mut self) -> Option<(Child, PathBuf)> {
        let child = self.child.take()?;
        let dir = std::mem::take(&mut self.user_data_dir);
        std::mem::forget(self);
        Some((child, dir))
    }
}

impl Drop for ChromeProcess {
    fn drop(&mut self) {
        if std::thread::panicking() {
            #[cfg(unix)]
            if let Some(pid) = self.pid() {
                kill_chrome_process_group_without_panicking(pid);
            }
            if self.owned_dir {
                let _ = std::fs::remove_dir_all(&self.user_data_dir);
            }
            return;
        }
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(unix)]
            if let Some(pid) = self.pid() {
                if let Err(err) = signal_chrome_process_group(pid) {
                    tracing::debug!(target: "flashbrowse_cdp::launcher", %err, pid, "could not signal Chromium process group during drop");
                }
            }
            if let Some(child) = self.child.take() {
                // tokio::Child::kill_on_drop + start_kill handles teardown.
                // tokio::process::Child::start_kill() is sync; reaping is
                // best-effort (OS reaps on process exit).
                let mut child = child;
                let _ = child.start_kill();
                drop(child);
            }
            if self.owned_dir {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let path = std::mem::take(&mut self.user_data_dir);
                    runtime.spawn_blocking(move || {
                        if let Err(err) = std::fs::remove_dir_all(&path) {
                            log_profile_cleanup_error(&path, err);
                        }
                    });
                } else if let Err(err) = std::fs::remove_dir_all(&self.user_data_dir) {
                    log_profile_cleanup_error(&self.user_data_dir, err);
                }
            }
        })) {
            // Dropping a panic payload can itself panic, so intentionally leak
            // it rather than allowing a second panic to escape this destructor.
            std::mem::forget(payload);
        }
    }
}

#[cfg(unix)]
pub(super) fn kill_chrome_process_group_without_panicking(pid: u32) {
    let Ok(raw_pid) = i32::try_from(pid) else {
        return;
    };
    let Some(process_id) = rustix::process::Pid::from_raw(raw_pid) else {
        return;
    };
    let _ = rustix::process::kill_process_group(process_id, rustix::process::Signal::KILL);
}

pub(super) async fn remove_profile(path: &std::path::Path) {
    if let Err(err) = tokio::fs::remove_dir_all(path).await {
        log_profile_cleanup_error(path, err);
    }
}

pub(super) fn log_profile_cleanup_error(path: &std::path::Path, err: std::io::Error) {
    if err.kind() != std::io::ErrorKind::NotFound {
        tracing::warn!(target: "flashbrowse_cdp::launcher", profile = %path.display(), %err, "could not remove Chromium profile");
    }
}

#[cfg(unix)]
pub(super) fn signal_chrome_process_group(pid: u32) -> std::io::Result<()> {
    let raw_pid = i32::try_from(pid)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string()))?;
    let process_id = rustix::process::Pid::from_raw(raw_pid).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Chromium process id must be positive",
        )
    })?;
    rustix::process::kill_process_group(process_id, rustix::process::Signal::KILL)
        .map_err(std::io::Error::from)
}
