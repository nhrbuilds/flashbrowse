use super::discovery::find_chrome;
use super::process::remove_profile;
use super::profile::{
    cleanup_stale_profiles, fresh_user_data_dir, hardened_args, resolve_no_sandbox,
};
use super::ChromeProcess;
use super::*;

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
                    tracing::warn!(target: "flashbrowse_cdp::launcher", %err, "pipe launch failed; falling back to WebSocket");
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

    tracing::debug!(target: "flashbrowse_cdp::launcher", chrome = %chrome_path.display(),
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
        tracing::warn!(target: "flashbrowse_cdp::launcher", pid = child.id(),
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
                tracing::warn!(target: "flashbrowse_cdp::launcher", %err, "pipe handshake failed; falling back to WebSocket");
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

    tracing::debug!(target: "flashbrowse_cdp::launcher", chrome = %chrome_path.display(),
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
pub(super) fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}
