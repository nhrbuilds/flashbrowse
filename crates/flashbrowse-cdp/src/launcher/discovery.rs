use super::*;

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
