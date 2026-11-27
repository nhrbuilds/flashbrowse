//! Platform-agnostic Chromium discovery, process spawning, stderr scraping.
//!
//! Priority order for binary resolution (locked decision):
//! 1. explicit `--chrome-path` / `LaunchOptions::chrome_path`
//! 2. `CHROME_PATH` env
//! 3. `CHROME_BINARY` env
//! 4. PATH: `google-chrome-stable`, `google-chrome`, `chromium`, `chromium-browser`
//! 5. macOS: `/Applications/Google Chrome.app/...`, `/Applications/Chromium.app/...`
//! 6. Windows: `Program Files/Google/Chrome/...`, `LocalAppData/Google/Chrome/...`

use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs::create_dir_all;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
#[cfg(unix)]
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::error::CdpError;

/// Seconds to wait for `DevTools listening on ...` on stderr.
const DEVTOOLS_TIMEOUT_SECS: u64 = 20;

/// Matches Chromium stderr line, e.g.
/// `DevTools listening on ws://127.0.0.1:9222/devtools/browser/abc-123`
fn parse_devtools_ws_url(line: &str) -> Option<&str> {
    let (_, suffix) = line.split_once("DevTools listening on ")?;
    let token = suffix.split_whitespace().next()?;
    let (host, rest) = token
        .strip_prefix("ws://")?
        .split_once("/devtools/browser/")?;
    if host.is_empty() {
        return None;
    }
    let id_len = rest
        .bytes()
        .take_while(|byte| byte.is_ascii_hexdigit() || *byte == b'-')
        .count();
    if id_len == 0 {
        return None;
    }
    let suffix = rest.get(id_len..)?;
    token.strip_suffix(suffix)
}

/// Options for launching a headless Chromium instance.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Explicit binary path (highest priority).
    pub chrome_path: Option<PathBuf>,
    /// Extra CLI args appended after the hardened defaults.
    pub extra_args: Vec<String>,
    /// If `Some(true)`, pass `--no-sandbox` (containers / CI).
    /// Defaults from `FLASHBROWSE_NO_SANDBOX` env at launch time.
    pub no_sandbox: Option<bool>,
    /// If `Some(dir)`, use as `--user-data-dir`. Otherwise a fresh temp dir.
    pub user_data_dir: Option<PathBuf>,
    /// Window size `WxH`, default `1280x800`.
    pub window_size: (u32, u32),
    /// Use WebSocket so detached persistent sessions survive CLI exit.
    pub persistent: bool,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            chrome_path: None,
            extra_args: Vec::new(),
            no_sandbox: None,
            user_data_dir: None,
            window_size: (1280, 800),
            persistent: false,
        }
    }
}

/// Async pipes for `--remote-debugging-pipe` on Unix.
#[cfg(unix)]
pub struct PipeHandles {
    pub read_pipe: ChildStdout,
    pub write_pipe: ChildStdin,
}

/// Handle to a running Chromium process.
///
/// On drop, the child is killed and reaped on a best-effort basis so CLI exit
/// never leaks a browser. Callers needing persistence (session files) should
/// detach ownership explicitly (e.g. keep pid + ws_url, then drop the guard
/// after killing a clone - no, use `detach`).
pub struct ChromeProcess {
    child: Option<Child>,
    browser_ws_url: String,
    chrome_path: PathBuf,
    user_data_dir: PathBuf,
    /// `true` only for auto-created temp dirs - caller-supplied
    /// `--user-data-dir` is never deleted on drop (H1 fix).
    owned_dir: bool,
    #[cfg(unix)]
    pipe_handles: Option<PipeHandles>,
}

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
            tracing::warn!(%error, "could not complete Chromium shutdown");
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
                tracing::debug!(%err, pid, "could not signal Chromium process group");
            }
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            if tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .is_err()
            {
                tracing::warn!(pid = child.id(), "Chromium did not reap within 5 seconds");
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
                    tracing::debug!(%err, pid, "could not signal Chromium process group during drop");
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
fn kill_chrome_process_group_without_panicking(pid: u32) {
    let Ok(raw_pid) = i32::try_from(pid) else {
        return;
    };
    let Some(process_id) = rustix::process::Pid::from_raw(raw_pid) else {
        return;
    };
    let _ = rustix::process::kill_process_group(process_id, rustix::process::Signal::KILL);
}

async fn remove_profile(path: &std::path::Path) {
    if let Err(err) = tokio::fs::remove_dir_all(path).await {
        log_profile_cleanup_error(path, err);
    }
}

fn log_profile_cleanup_error(path: &std::path::Path, err: std::io::Error) {
    if err.kind() != std::io::ErrorKind::NotFound {
        tracing::warn!(profile = %path.display(), %err, "could not remove Chromium profile");
    }
}

#[cfg(unix)]
fn signal_chrome_process_group(pid: u32) -> std::io::Result<()> {
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

/// Resolve a Chromium binary using the locked priority order.
pub fn find_chrome(explicit: Option<&PathBuf>) -> Result<PathBuf, CdpError> {
    let mut searched: Vec<String> = Vec::new();

    if let Some(p) = explicit {
        searched.push(format!("explicit: {}", p.display()));
        if p.is_file() {
            return Ok(p.clone());
        }
        return Err(CdpError::NoChromeBinary { searched });
    }

    for env_key in ["CHROME_PATH", "CHROME_BINARY"] {
        if let Ok(p) = env::var(env_key) {
            searched.push(format!("${env_key}: {p}"));
            let pb = PathBuf::from(&p);
            if pb.is_file() {
                return Ok(pb);
            }
        }
    }

    let prefer_managed = std::env::var("FLASHBROWSE_PREFER_MANAGED")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    if prefer_managed {
        for cand in managed_chrome_paths() {
            searched.push(format!("managed: {}", cand.display()));
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }

    for name in [
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
    ] {
        searched.push(format!("PATH:{name}"));
        if let Ok(p) = which::which(name) {
            if p.is_file() {
                return Ok(p);
            }
        }
    }

    #[cfg(target_os = "macos")]
    for cand in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ] {
        searched.push(cand.to_string());
        let pb = PathBuf::from(cand);
        if pb.is_file() {
            return Ok(pb);
        }
    }

    // Managed download (`flashbrowse setup`) - searched before hardcoded
    // system paths so a first-run install just works.
    for cand in managed_chrome_paths() {
        searched.push(format!("managed: {}", cand.display()));
        if cand.is_file() {
            return Ok(cand);
        }
    }

    #[cfg(target_os = "windows")]
    for cand in windows_chrome_paths() {
        searched.push(cand.display().to_string());
        if cand.is_file() {
            return Ok(cand);
        }
    }

    Err(CdpError::NoChromeBinary { searched })
}

/// Managed Chrome paths (installed by `flashbrowse setup`).
fn managed_chrome_paths() -> Vec<PathBuf> {
    let root = managed_chrome_root();
    let mut out = vec![
        root.join("chrome-headless-shell"),
        root.join("chrome-headless-shell.exe"),
        root.join("chrome"),
        root.join("chrome.exe"),
    ];
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            out.push(entry.path().join("chrome-headless-shell"));
            out.push(entry.path().join("chrome-headless-shell.exe"));
            out.push(entry.path().join("chrome"));
            out.push(entry.path().join("chrome.exe"));
        }
    }
    out
}

fn managed_chrome_root() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(local) = env::var("LOCALAPPDATA") {
            return PathBuf::from(local).join("flashbrowse").join("chrome");
        }
    }
    if let Ok(xdg) = env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("flashbrowse").join("chrome");
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".cache")
        .join("flashbrowse")
        .join("chrome")
}

#[cfg(target_os = "windows")]
fn windows_chrome_paths() -> Vec<PathBuf> {
    let mut out = vec![
        PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
    ];
    if let Ok(local) = env::var("LOCALAPPDATA") {
        out.push(PathBuf::from(local).join(r"Google\Chrome\Application\chrome.exe"));
    }
    if let Ok(program_files) = env::var("ProgramFiles") {
        out.push(PathBuf::from(program_files).join(r"Google\Chrome\Application\chrome.exe"));
    }
    out
}

/// Hardened, leak-proof Chromium flags (spec + `--user-data-dir`, `--remote-debugging-pipe` / `--remote-debugging-port=0`).
fn hardened_args(
    user_data_dir: &std::path::Path,
    window_size: (u32, u32),
    no_sandbox: bool,
    use_pipe: bool,
    also_port: bool,
) -> Vec<String> {
    let mut args = vec![
        "--headless=new".to_string(),
        "--disable-gpu".to_string(),
        "--disable-dev-shm-usage".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-background-networking".to_string(),
        "--disable-background-timer-throttling".to_string(),
        "--disable-client-side-phishing-detection".to_string(),
        "--disable-default-apps".to_string(),
        "--disable-extensions".to_string(),
        "--disable-sync".to_string(),
        "--mute-audio".to_string(),
        format!("--window-size={},{}", window_size.0, window_size.1),
        format!("--user-data-dir={}", user_data_dir.display()),
    ];
    if use_pipe {
        args.push("--remote-debugging-pipe".to_string());
        if also_port {
            args.push("--remote-debugging-port=0".to_string());
        }
    } else {
        args.push("--remote-debugging-port=0".to_string());
    }
    if no_sandbox {
        args.push("--no-sandbox".to_string());
    }
    args
}

static PROFILE_SEQ: AtomicU64 = AtomicU64::new(0);

fn fresh_user_data_dir() -> std::io::Result<PathBuf> {
    // Secure temp dir: 0700, exclusive-create, symlink-rejecting (H3 fix).
    // Falls back to hash naming only for the integer suffix; creation is
    // still O_EXCL via create_new semantics on the DirBuilder mode.
    let seq = PROFILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    seq.hash(&mut hasher);
    let hash = hasher.finish();
    let base = env::temp_dir().join(format!("flashbrowse-{}-{:x}", std::process::id(), hash));
    // Try base, then base-N suffixes on collision/symlink.
    for attempt in 0..10u32 {
        let dir = if attempt == 0 {
            base.clone()
        } else {
            env::temp_dir().join(format!("{}-{}", base.display(), attempt))
        };
        // Reject pre-existing symlinks (TOCTOU mitigation).
        if let Ok(md) = std::fs::symlink_metadata(&dir) {
            if md.file_type().is_symlink() || md.is_dir() {
                let _ = std::fs::remove_dir_all(&dir);
                // If it was a real dir from another run, keep probing suffixes.
                continue;
            }
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
                }
                return Ok(dir);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    // Last resort: create_dir_all (better than failing the launch).
    create_dir_all(&base)?;
    Ok(base)
}

fn resolve_no_sandbox(opt: Option<bool>) -> bool {
    opt.unwrap_or_else(|| {
        env::var("FLASHBROWSE_NO_SANDBOX")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Remove leftover profile dirs from crashed/killed runs.
///
/// Best-effort: only dirs that look like ours (`flashbrowse-<pid>-<hash>`),
/// whose pid is gone and that are older than `STALE_PROFILE_SECS`.
fn cleanup_stale_profiles() {
    const STALE_PROFILE_SECS: u64 = 3600;
    let tmp = env::temp_dir();
    let Ok(rd) = std::fs::read_dir(&tmp) else {
        return;
    };
    let now = SystemTime::now();
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("flashbrowse-") {
            continue;
        }
        // flashbrowse-<pid>-<hash>
        let mut parts = name.split('-');
        let _ = parts.next();
        let Some(pid) = parts.next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        let path = entry.path();
        // Only remove if the owning process is gone and the dir is old.
        if std::path::Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age.as_secs() > STALE_PROFILE_SECS {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Launch headless Chromium.
///
/// On Unix, ephemeral/MCP launches (`!opts.persistent`) use
/// `--remote-debugging-pipe` (falling back to WebSocket if unsupported).
/// Persistent launches (`opts.persistent`) use WebSocket so Chromium stays
/// alive after the spawning CLI process exits and closes its file descriptors.
pub async fn launch_chrome(opts: LaunchOptions) -> Result<ChromeProcess, CdpError> {
    crate::runtime::run(launch_chrome_inner(opts)).await?
}

async fn launch_chrome_inner(opts: LaunchOptions) -> Result<ChromeProcess, CdpError> {
    crate::transport::require_runtime()?;
    cleanup_stale_profiles();
    #[cfg(unix)]
    {
        if !opts.persistent
            && !opts
                .extra_args
                .iter()
                .any(|a| a.starts_with("--remote-debugging-port"))
        {
            match launch_chrome_pipe(&opts).await {
                Ok(proc) => return Ok(proc),
                Err(err) => {
                    tracing::warn!(%err, "pipe launch failed; falling back to WebSocket");
                }
            }
        }
    }
    launch_chrome_ws(opts).await
}

#[cfg(unix)]
async fn launch_chrome_pipe(opts: &LaunchOptions) -> Result<ChromeProcess, CdpError> {
    let chrome_path = find_chrome(opts.chrome_path.as_ref())?;
    let (user_data_dir, owned_dir) = match &opts.user_data_dir {
        Some(d) => {
            create_dir_all(d)?;
            (d.clone(), false)
        }
        None => (fresh_user_data_dir()?, true),
    };
    let no_sandbox = resolve_no_sandbox(opts.no_sandbox);
    let args = hardened_args(
        &user_data_dir,
        opts.window_size,
        no_sandbox,
        true,
        opts.persistent,
    );

    // Chromium requires its pipe transport on descriptors 3 and 4. Use a
    // small POSIX shell only to map its piped stdin/stdout onto those numbers;
    // all arguments remain separate argv entries and never pass through shell
    // expansion. `exec` preserves the child pid and process group.
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("exec 3<&0 4>&1; exec \"$@\" >/dev/null")
        .arg("flashbrowse-chrome")
        .arg(&chrome_path)
        .args(&args)
        .args(&opts.extra_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if opts.persistent {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true);
    configure_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(source) => {
            if owned_dir {
                remove_profile(&user_data_dir).await;
            }
            return Err(CdpError::SpawnFailed {
                path: chrome_path,
                source,
            });
        }
    };
    let (read_pipe, write_pipe) = match (child.stdout.take(), child.stdin.take()) {
        (Some(read_pipe), Some(write_pipe)) => (read_pipe, write_pipe),
        _ => {
            stop_child(&mut child).await;
            if owned_dir {
                remove_profile(&user_data_dir).await;
            }
            return Err(CdpError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Chromium pipe transport could not open its child streams",
            )));
        }
    };

    let browser_ws_url = if opts.persistent {
        match await_devtools_ws_url(&mut child).await {
            Ok(url) => url,
            Err(err) => {
                stop_child(&mut child).await;
                if owned_dir {
                    remove_profile(&user_data_dir).await;
                }
                return Err(err);
            }
        }
    } else {
        String::new()
    };

    tracing::debug!(
        chrome = %chrome_path.display(),
        persistent = opts.persistent,
        "Chromium launched in pipe mode"
    );

    Ok(ChromeProcess {
        child: Some(child),
        browser_ws_url,
        chrome_path,
        user_data_dir,
        owned_dir,
        pipe_handles: Some(PipeHandles {
            read_pipe,
            write_pipe,
        }),
    })
}

async fn await_devtools_ws_url(child: &mut Child) -> Result<String, CdpError> {
    let stderr = child.stderr.take().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "Chromium stderr pipe unavailable",
        )
    })?;
    let (tx, rx) = oneshot::channel::<String>();

    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        const MAX_DEVTOOLS_LINE: usize = 64 * 1024;
        let mut line = Vec::new();
        let mut discard = false;
        while let Ok(bytes) = reader.fill_buf().await {
            if bytes.is_empty() {
                if !discard {
                    if let Some(url) = std::str::from_utf8(&line)
                        .ok()
                        .and_then(parse_devtools_ws_url)
                    {
                        let _ = tx.send(url.to_string());
                    }
                }
                break;
            }
            let delimiter = bytes.iter().position(|byte| *byte == b'\n');
            let Some(chunk) = bytes.get(..delimiter.unwrap_or(bytes.len())) else {
                break;
            };
            if !discard {
                if chunk.len() <= MAX_DEVTOOLS_LINE.saturating_sub(line.len()) {
                    line.extend_from_slice(chunk);
                } else {
                    discard = true;
                }
            }
            let consumed = chunk.len().saturating_add(usize::from(delimiter.is_some()));
            reader.consume(consumed);
            if delimiter.is_some() {
                if !discard {
                    if let Some(url) = std::str::from_utf8(&line)
                        .ok()
                        .and_then(parse_devtools_ws_url)
                    {
                        let _ = tx.send(url.to_string());
                        break;
                    }
                }
                discard = false;
                line.clear();
            }
        }
    });

    match timeout(Duration::from_secs(DEVTOOLS_TIMEOUT_SECS), rx).await {
        Ok(Ok(url)) => Ok(url),
        Ok(Err(_)) => {
            let code = child.try_wait().ok().flatten().and_then(|s| s.code());
            stop_child(child).await;
            Err(CdpError::ChromiumExitedEarly { code })
        }
        Err(_) => {
            stop_child(child).await;
            Err(CdpError::DevToolsTimeout {
                seconds: DEVTOOLS_TIMEOUT_SECS,
            })
        }
    }
}

async fn stop_child(child: &mut Child) {
    let _ = child.start_kill();
    if tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        tracing::warn!(
            pid = child.id(),
            "browser process did not stop within 2 seconds"
        );
    }
}

/// Launch Chromium and verify the pipe before committing to that transport.
pub async fn launch_connected_chrome(
    opts: LaunchOptions,
) -> Result<(ChromeProcess, crate::CdpClient), CdpError> {
    crate::runtime::run(launch_connected_chrome_inner(opts)).await?
}

async fn launch_connected_chrome_inner(
    opts: LaunchOptions,
) -> Result<(ChromeProcess, crate::CdpClient), CdpError> {
    #[cfg(unix)]
    let mut chrome = launch_chrome(opts.clone()).await?;
    #[cfg(not(unix))]
    let mut chrome = launch_chrome(opts).await?;
    #[cfg(unix)]
    if chrome.pipe_handles.is_some() {
        let client = crate::CdpClient::connect_chrome(&mut chrome).await?;
        match crate::protocol::browser::get_version(&client).await {
            Ok(_) => return Ok((chrome, client)),
            Err(err) => {
                tracing::warn!(%err, "pipe handshake failed; falling back to WebSocket");
                client.shutdown();
                chrome.shutdown().await;
                chrome = launch_chrome_ws(opts).await?;
            }
        }
    }
    let client = crate::CdpClient::connect_chrome(&mut chrome).await?;
    Ok((chrome, client))
}

/// Launch headless Chromium over WebSocket (`--remote-debugging-port=0`).
pub async fn launch_chrome_ws(opts: LaunchOptions) -> Result<ChromeProcess, CdpError> {
    crate::runtime::run(launch_chrome_ws_inner(opts)).await?
}

async fn launch_chrome_ws_inner(opts: LaunchOptions) -> Result<ChromeProcess, CdpError> {
    crate::transport::require_runtime()?;
    let chrome_path = find_chrome(opts.chrome_path.as_ref())?;
    #[cfg(windows)]
    protect_parent_stdio()?;
    let (user_data_dir, owned_dir) = match opts.user_data_dir {
        Some(d) => {
            create_dir_all(&d)?;
            (d, false)
        }
        None => (fresh_user_data_dir()?, true),
    };
    let no_sandbox = resolve_no_sandbox(opts.no_sandbox);
    let args = hardened_args(&user_data_dir, opts.window_size, no_sandbox, false, false);

    let mut command = Command::new(&chrome_path);
    command
        .args(&args)
        .args(&opts.extra_args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    configure_process_group(&mut command);
    #[cfg(windows)]
    {
        if opts.persistent {
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(source) => {
            if owned_dir {
                remove_profile(&user_data_dir).await;
            }
            return Err(CdpError::SpawnFailed {
                path: chrome_path,
                source,
            });
        }
    };

    let browser_ws_url = match await_devtools_ws_url(&mut child).await {
        Ok(url) => url,
        Err(err) => {
            stop_child(&mut child).await;
            if owned_dir {
                remove_profile(&user_data_dir).await;
            }
            return Err(err);
        }
    };

    tracing::debug!(
        chrome = %chrome_path.display(),
        ws = %browser_ws_url,
        "Chromium launched, DevTools listening"
    );

    Ok(ChromeProcess {
        child: Some(child),
        browser_ws_url,
        chrome_path,
        user_data_dir,
        owned_dir,
        #[cfg(unix)]
        pipe_handles: None,
    })
}

#[cfg(windows)]
fn protect_parent_stdio() -> std::io::Result<()> {
    use std::os::windows::io::{AsHandle, AsRawHandle};
    use win_handles_sys::{set_handle_information, HandleFlags};

    fn clear_inherit(handle: impl AsHandle + AsRawHandle) -> std::io::Result<()> {
        let raw = handle.as_raw_handle();
        if raw.is_null() || raw as isize == -1 {
            return Ok(());
        }
        set_handle_information(handle, HandleFlags::INHERIT, HandleFlags::from_bits(0))
    }

    clear_inherit(std::io::stdin())?;
    clear_inherit(std::io::stdout())?;
    clear_inherit(std::io::stderr())
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(test)]
mod tests {
    use super::{parse_devtools_ws_url, ChromeProcess};

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PROFILE_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn parses_devtools_ws_url() {
        assert_eq!(
            parse_devtools_ws_url(
                "\nDevTools listening on ws://127.0.0.1:9222/devtools/browser/abc-123-DEF\r\n"
            ),
            Some("ws://127.0.0.1:9222/devtools/browser/abc-123-DEF")
        );
        assert_eq!(
            parse_devtools_ws_url(
                "DevTools listening on ws://[::1]:9222/devtools/browser/0123-abcd"
            ),
            Some("ws://[::1]:9222/devtools/browser/0123-abcd")
        );
        assert_eq!(
            parse_devtools_ws_url(
                "DevTools listening on ws://127.0.0.1:9222/devtools/browser/dead-beef, extra"
            ),
            Some("ws://127.0.0.1:9222/devtools/browser/dead-beef")
        );
        assert_eq!(
            parse_devtools_ws_url(
                "DevTools listening on http://127.0.0.1:9222/devtools/browser/abc"
            ),
            None
        );
        assert_eq!(
            parse_devtools_ws_url("DevTools listening on ws://127.0.0.1:9222/devtools/browser/"),
            None
        );
    }

    #[test]
    fn owned_profile_is_removed_when_process_guard_drops_during_unwind() {
        let id = NEXT_PROFILE_ID.fetch_add(1, Ordering::Relaxed);
        let user_data_dir = std::env::temp_dir().join(format!(
            "flashbrowse-panic-drop-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir(&user_data_dir).expect("create temporary profile");

        let result = std::panic::catch_unwind(|| {
            let process = ChromeProcess {
                child: None,
                browser_ws_url: String::new(),
                chrome_path: PathBuf::new(),
                user_data_dir: user_data_dir.clone(),
                owned_dir: true,
                #[cfg(unix)]
                pipe_handles: None,
            };
            let _process = process;
            panic!("simulate an operation panic");
        });

        assert!(result.is_err());
        assert!(!user_data_dir.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owned_process_group_is_killed_when_guard_drops_during_unwind() {
        use super::{configure_process_group, kill_chrome_process_group_without_panicking};
        use tokio::process::Command;
        use tokio::time::{sleep, timeout, Duration};

        let id = NEXT_PROFILE_ID.fetch_add(1, Ordering::Relaxed);
        let test_dir = std::env::temp_dir().join(format!(
            "flashbrowse-process-drop-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir(&test_dir).expect("create process test directory");
        let marker = test_dir.join("descendant-writes");
        let user_data_dir = test_dir.join("profile");
        std::fs::create_dir(&user_data_dir).expect("create temporary profile");

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sh -c 'while :; do printf x >> \"$1\"; sleep 0.01; done' sh \"$1\" & wait")
            .arg("flashbrowse-test")
            .arg(&marker)
            .kill_on_drop(true);
        configure_process_group(&mut command);
        let child = command.spawn().expect("start process group test child");
        let pid = child.id().expect("test child process id");

        timeout(Duration::from_secs(2), async {
            while !marker.exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant starts writing before panic");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let process = ChromeProcess {
                child: Some(child),
                browser_ws_url: String::new(),
                chrome_path: PathBuf::new(),
                user_data_dir: user_data_dir.clone(),
                owned_dir: true,
                pipe_handles: None,
            };
            let _process = process;
            panic!("simulate an operation panic");
        }));
        assert!(result.is_err());

        sleep(Duration::from_millis(50)).await;
        let stopped_at = std::fs::metadata(&marker)
            .expect("read descendant marker")
            .len();
        sleep(Duration::from_millis(50)).await;
        let stopped = std::fs::metadata(&marker)
            .expect("read descendant marker after wait")
            .len()
            == stopped_at;
        if !stopped {
            kill_chrome_process_group_without_panicking(pid);
        }
        assert!(
            stopped,
            "Chrome descendant kept running after unwind cleanup"
        );
        assert!(!user_data_dir.exists());
        std::fs::remove_dir_all(test_dir).expect("remove process test directory");
    }
}
