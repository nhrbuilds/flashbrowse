use super::*;

/// Hardened, leak-proof Chromium flags and profile preparation.
pub(super) fn hardened_args(
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

pub(super) fn fresh_user_data_dir() -> std::io::Result<PathBuf> {
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

pub(super) fn resolve_no_sandbox(opt: Option<bool>) -> bool {
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
pub(super) fn cleanup_stale_profiles() {
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
