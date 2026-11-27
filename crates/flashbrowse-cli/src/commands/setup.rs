//! `flashbrowse setup` - download Chrome-for-Testing headless shell.
//!
//! Installs to `~/.cache/flashbrowse/chrome/` (Linux/macOS) or
//! `%LOCALAPPDATA%\flashbrowse\chrome\` (Windows). No manual Chrome
//! install needed afterwards - `find_chrome()` falls back to this dir.
//!
//! Pin the CFT version with env `FLASHBROWSE_CHROME_VERSION` (e.g. `131.0.6778.85`).

use std::path::PathBuf;

use futures_util::StreamExt;
use serde_json::Value;

/// Chrome-for-Testing stable metadata.
const VERSIONS_URL: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";

/// User-data/cache dir for the managed browser.
pub fn managed_chrome_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(local).join("flashbrowse").join("chrome");
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("flashbrowse").join("chrome");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".cache")
        .join("flashbrowse")
        .join("chrome")
}

/// Locate an already-installed managed binary, if any.
pub fn find_managed_binary() -> Option<PathBuf> {
    let root = managed_chrome_dir();
    // Search root and one level deep (extracted zip layout).
    let mut candidates = vec![
        root.join("chrome-headless-shell"),
        root.join("chrome-headless-shell.exe"),
    ];
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            for sub in [
                "chrome-headless-shell",
                "chrome-headless-shell.exe",
                "chrome",
                "chrome.exe",
            ] {
                candidates.push(entry.path().join(sub));
            }
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn platform_key() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "mac-arm64"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "mac-x64"
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "win64"
    }
    #[cfg(all(target_os = "windows", target_arch = "x86"))]
    {
        "win32"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "linux-arm64"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "linux64"
    }
    #[cfg(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "x86"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64")
    )))]
    {
        "unsupported"
    }
}

/// Pick the `chrome-headless-shell` download URL for this platform,
/// falling back to full `chrome` when headless-shell is absent.
/// Returns `(url, version)`.
async fn resolve_download(client: &reqwest::Client) -> anyhow::Result<(String, String)> {
    let meta: Value = client
        .get(VERSIONS_URL)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let pinned = std::env::var("FLASHBROWSE_CHROME_VERSION")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let (version, downloads) = if let Some(ref want) = pinned {
        let versions = meta
            .get("versions")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("no versions[] in metadata (pin {want})"))?;
        let entry = versions
            .iter()
            .find(|v| v.get("version").and_then(|x| x.as_str()) == Some(want.as_str()))
            .ok_or_else(|| anyhow::anyhow!("Chrome version {want} not in CFT metadata"))?;
        let ver = entry
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or(want.as_str())
            .to_string();
        let dl = entry
            .get("downloads")
            .ok_or_else(|| anyhow::anyhow!("no downloads for pinned version {want}"))?;
        (ver, dl.clone())
    } else {
        let stable = meta
            .get("channels")
            .and_then(|c| c.get("Stable"))
            .ok_or_else(|| anyhow::anyhow!("unexpected metadata shape"))?;
        let version = stable
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let downloads = stable
            .get("downloads")
            .ok_or_else(|| anyhow::anyhow!("no downloads in metadata"))?
            .clone();
        (version, downloads)
    };
    let platform = platform_key();
    if platform == "unsupported" {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        anyhow::bail!(
            "unsupported platform for managed Chrome ({os}/{arch}). \
             Install Chrome manually and pass --chrome-path."
        );
    }
    // Metadata layout: downloads[kind][] where each entry has `platform` + `url`.
    // Prefer headless shell (small, headless-only - exactly flashbrowse's needs).
    for kind in ["chrome-headless-shell", "chrome"] {
        if let Some(arr) = downloads.get(kind).and_then(|v| v.as_array()) {
            for d in arr {
                if d.get("platform").and_then(|p| p.as_str()) == Some(platform) {
                    if let Some(url) = d.get("url").and_then(|u| u.as_str()) {
                        return Ok((url.to_string(), version));
                    }
                }
            }
        }
    }
    anyhow::bail!("no chrome download for {platform} (version {version})")
}

/// `flashbrowse setup [--force]` - download Chrome-for-Testing.
pub async fn run(force: bool) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let dir = managed_chrome_dir();
    if !force {
        if let Some(bin) = find_managed_binary() {
            crate::write_stdout(format_args!(
                "managed Chrome already installed at {}",
                bin.display()
            ))?;
            crate::write_stdout(format_args!(
                "(run 'flashbrowse setup --force' to reinstall)"
            ))?;
            flashbrowse_core::observability::record_op(
                "setup", "cached", started, None, true, None,
            );
            return Ok(());
        }
    }

    let client = reqwest::Client::builder()
        .user_agent("flashbrowse-setup/0.1")
        .build()?;
    crate::write_stderr(format_args!(
        "resolving Chrome-for-Testing stable ({} ...)...",
        platform_key()
    ));
    let (url, version) = resolve_download(&client).await?;
    crate::write_stderr(format_args!("chrome-headless-shell {version} <- {url}"));

    // Download zip streaming to a unique temp file (symlink-safe: O_EXCL
    // semantics via create_new, and a per-pid name so concurrent runs
    // cannot clobber each other).
    let tmp_zip = std::env::temp_dir().join(format!(
        "flashbrowse-chrome-{}-{}.zip",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let cleanup = || {
        let _ = std::fs::remove_file(&tmp_zip);
    };
    let download = async {
        let resp = client.get(&url).send().await?.error_for_status()?;
        let total = resp.content_length().unwrap_or(0);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_zip)?;
        let mut downloaded: u64 = 0;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            use std::io::Write;
            file.write_all(&chunk)?;
            downloaded = downloaded.saturating_add(chunk.len() as u64);
            if total > 0 {
                crate::write_stderr_raw(format_args!(
                    "\r  downloaded {:.1} / {:.1} MB",
                    downloaded as f64 / (1024.0 * 1024.0),
                    total as f64 / (1024.0 * 1024.0)
                ));
            }
        }
        crate::write_stderr(format_args!(""));
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(e) = download {
        cleanup();
        return Err(e);
    }

    // Extract (same-dir temp then rename for atomicity).
    std::fs::create_dir_all(&dir)?;
    let extract_tmp = dir.join(".extract-tmp");
    let _ = std::fs::remove_dir_all(&extract_tmp);
    std::fs::create_dir_all(&extract_tmp)?;
    let file = std::fs::File::open(&tmp_zip)?;
    let mut zip = zip::ZipArchive::new(file)?;
    zip.extract(&extract_tmp)?;
    // Flatten single top-level dir if present (chrome-linux64/ → .)
    let mut staged: Vec<PathBuf> = vec![];
    for entry in std::fs::read_dir(&extract_tmp)? {
        let entry = entry?;
        staged.push(entry.path());
    }
    if let [single] = staged.as_slice() {
        if single.is_dir() {
            for entry in std::fs::read_dir(single)? {
                let entry = entry?;
                let dst = extract_tmp.join(entry.file_name());
                std::fs::rename(entry.path(), &dst)?;
            }
            let _ = std::fs::remove_dir(single);
        }
    }
    // Move staged contents into final dir (replace previous install).
    for entry in std::fs::read_dir(&extract_tmp)? {
        let entry = entry?;
        let dst = dir.join(entry.file_name());
        if dst.exists() {
            let _ = std::fs::remove_dir_all(&dst);
            let _ = std::fs::remove_file(&dst);
        }
        std::fs::rename(entry.path(), &dst)?;
    }
    std::fs::remove_dir(&extract_tmp)?;
    let _ = std::fs::remove_file(&tmp_zip);

    let bin = find_managed_binary().ok_or_else(|| {
        anyhow::anyhow!(
            "download finished but binary not found under {}",
            dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
    }
    // Record version.
    std::fs::write(dir.join("VERSION"), &version)?;
    // Verify it runs.
    let out = std::process::Command::new(&bin).arg("--version").output();
    match out {
        Ok(o) if o.status.success() => {
            crate::write_stdout(format_args!(
                "OK managed Chrome {} at {}",
                String::from_utf8_lossy(&o.stdout).trim(),
                bin.display()
            ))?;
        }
        _ => crate::write_stdout(format_args!(
            "installed at {} (verify manually)",
            bin.display()
        ))?,
    }
    crate::write_stdout(format_args!("try: flashbrowse open https://example.com"))?;
    flashbrowse_core::observability::record_op("setup", &version, started, None, true, None);
    Ok(())
}
