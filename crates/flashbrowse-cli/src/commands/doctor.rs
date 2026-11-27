//! `flashbrowse doctor` - environment diagnostics.
//!
//! Answers the questions an operator asks before a run: is Chrome there,
//! is the cache writable, is there disk/memory headroom, are there stale
//! sessions or orphaned browsers.

use std::path::PathBuf;

use serde_json::json;

use super::session_file::{flashbrowse_cache_dir, sessions_dir};

/// One diagnostic line.
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
    /// `ok` (good), `warn` (usable but noteworthy), `fail` (blocking).
    level: &'static str,
}

/// Run all diagnostics and print them. Returns `true` when nothing failed.
pub async fn run(json_output: bool) -> anyhow::Result<bool> {
    let mut checks: Vec<Check> = Vec::new();

    // --- browser ---------------------------------------------------------
    let chrome = flashbrowse_cdp::find_chrome(None);
    match &chrome {
        Ok(path) => {
            let version = std::process::Command::new(path)
                .arg("--version")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            checks.push(Check {
                name: "chromium",
                ok: true,
                level: "ok",
                detail: format!("{} ({version})", path.display()),
            });
        }
        Err(e) => checks.push(Check {
            name: "chromium",
            ok: false,
            level: "fail",
            detail: format!("not found: {e}. Run 'flashbrowse setup'."),
        }),
    }

    // --- Firefox (future BiDi; informational) ---------------------------
    //
    // The version probe spawns the binary, so it is skipped unless a
    // Firefox is actually installed - `doctor` must stay fast and must not
    // launch a browser the user did not ask for.
    match flashbrowse_bidi::find_firefox(None) {
        Ok(path) => {
            checks.push(Check {
                name: "firefox",
                ok: true,
                level: "warn",
                detail: format!(
                    "{} - experimental engine; chrome required for open/act today",
                    path.display()
                ),
            });
        }
        Err(e) => checks.push(Check {
            name: "firefox",
            ok: true,
            level: "warn",
            detail: format!("not found ({e}) - optional until BiDi milestone"),
        }),
    }

    // --- managed install -------------------------------------------------
    let managed = super::setup::managed_chrome_dir();
    let managed_ok = managed.join("chrome-headless-shell").is_file()
        || managed.join("chrome-headless-shell.exe").is_file();
    checks.push(Check {
        name: "managed-chrome",
        ok: true,
        level: if managed_ok { "ok" } else { "warn" },
        detail: if managed_ok {
            format!("installed at {}", managed.display())
        } else {
            format!(
                "not installed ({}); system Chrome will be used",
                managed.display()
            )
        },
    });

    // --- cache dirs ------------------------------------------------------
    let cache = flashbrowse_cache_dir();
    let writable = std::fs::create_dir_all(&cache).is_ok()
        && std::fs::write(cache.join(".write-test"), b"").is_ok();
    let _ = std::fs::remove_file(cache.join(".write-test"));
    checks.push(Check {
        name: "cache-dir",
        ok: writable,
        level: if writable { "ok" } else { "fail" },
        detail: format!("{} (writable={writable})", cache.display()),
    });

    // --- disk space ------------------------------------------------------
    let tmp = std::env::temp_dir();
    let tmp_free = fs_free_mb(&tmp);
    checks.push(match tmp_free {
        Some(free) if free < 200 => Check {
            name: "temp-space",
            ok: false,
            level: "fail",
            detail: format!("only {free} MB free in {}", tmp.display()),
        },
        Some(free) => Check {
            name: "temp-space",
            ok: true,
            level: "ok",
            detail: format!("{free} MB free in {}", tmp.display()),
        },
        None => Check {
            name: "temp-space",
            ok: true,
            level: "warn",
            detail: "unknown".to_string(),
        },
    });

    // --- memory ----------------------------------------------------------
    let mem = mem_available_mb();
    checks.push(match mem {
        Some(m) if m < 512 => Check {
            name: "memory",
            ok: false,
            level: "fail",
            detail: format!("only {m} MB available"),
        },
        Some(m) => Check {
            name: "memory",
            ok: true,
            level: "ok",
            detail: format!("{m} MB available"),
        },
        None => Check {
            name: "memory",
            ok: true,
            level: "warn",
            detail: "unknown".to_string(),
        },
    });

    // --- sessions --------------------------------------------------------
    let sessions = std::fs::read_dir(sessions_dir())
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .count()
        })
        .unwrap_or(0);
    checks.push(Check {
        name: "sessions",
        ok: true,
        level: if sessions > 0 { "warn" } else { "ok" },
        detail: format!("{sessions} stored (flashbrowse close --session-id <id> to stop)"),
    });

    // --- orphaned browsers ----------------------------------------------
    let orphans = count_orphan_browsers();
    checks.push(Check {
        name: "orphan-browsers",
        ok: orphans == 0,
        level: if orphans == 0 { "ok" } else { "warn" },
        detail: format!("{orphans} flashbrowse-managed browser(s) still running"),
    });

    // --- report ----------------------------------------------------------
    let failed = checks.iter().any(|c| c.level == "fail");
    if json_output {
        let items: Vec<_> = checks
            .iter()
            .map(|c| json!({"check": c.name, "level": c.level, "ok": c.ok, "detail": c.detail}))
            .collect();
        let output = serde_json::to_string_pretty(&json!({"ok": !failed, "checks": items}))?;
        crate::write_stdout(format_args!("{output}"))?;
    } else {
        crate::write_stdout(format_args!(
            "flashbrowse doctor  (v{})",
            env!("CARGO_PKG_VERSION")
        ))?;
        for c in &checks {
            let mark = match c.level {
                "ok" => "PASS",
                "warn" => "WARN",
                _ => "FAIL",
            };
            crate::write_stdout(format_args!("  [{mark}] {:<16} {}", c.name, c.detail))?;
        }
        crate::write_stdout(format_args!(
            "\n{}",
            if failed {
                "one or more checks failed - fix them before running agents"
            } else {
                "all checks passed"
            }
        ))?;
    }
    Ok(!failed)
}

/// Free space in MB on the filesystem holding `path`.
///
/// Uses `df -m` (1 MiB blocks), which is portable across GNU coreutils and
/// BSD/macOS, and locates the "Available"/"Avail" column from the header
/// instead of assuming a fixed index.
fn fs_free_mb(path: &PathBuf) -> Option<u64> {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("df")
            .arg("-m")
            .arg(path)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let header = lines.next()?;
        let row = lines.next()?;
        let cols: Vec<&str> = header.split_whitespace().collect();
        let idx = cols
            .iter()
            .position(|c| c.eq_ignore_ascii_case("available") || c.eq_ignore_ascii_case("avail"))?;
        let cell = row.split_whitespace().nth(idx)?;
        let digits: String = cell.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse::<u64>().ok()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn mem_available_mb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                let kb: u64 = rest.trim().trim_end_matches(" kB").trim().parse().ok()?;
                return Some(kb / 1024);
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        mem_available_mb_macos()
    }
    #[cfg(target_os = "windows")]
    {
        mem_available_mb_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn mem_available_mb_macos() -> Option<u64> {
    let out = std::process::Command::new("vm_stat").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let page_size: u64 = std::process::Command::new("sysctl")
        .args(["-n", "hw.pagesize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(4096);
    let mut free_pages: u64 = 0;
    for line in text.lines() {
        let (label, rest) = line.split_once(':')?;
        let label = label.trim();
        if label == "Pages free" || label == "Pages inactive" {
            let n: u64 = rest.trim().trim_end_matches('.').parse().ok()?;
            free_pages = free_pages.saturating_add(n);
        }
    }
    if free_pages == 0 {
        return None;
    }
    Some((free_pages.saturating_mul(page_size)) / (1024 * 1024))
}

#[cfg(target_os = "windows")]
fn mem_available_mb_windows() -> Option<u64> {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory / 1KB",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<u64>().ok()
}

// Only the `ps`-based orphan scan uses this; Windows counts via PowerShell.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn chrome_cmdline_has_flashbrowse_profile(cmdline: &str) -> bool {
    let lower = cmdline.to_ascii_lowercase();
    (lower.contains("chrome") || lower.contains("chromium")) && lower.contains("flashbrowse-")
}

fn count_orphan_browsers() -> usize {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let Ok(out) = std::process::Command::new("ps")
            .args(["-eo", "args"])
            .output()
        else {
            return 0;
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| chrome_cmdline_has_flashbrowse_profile(l))
            .count()
    }
    #[cfg(target_os = "windows")]
    {
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'flashbrowse-' -and ($_.CommandLine -match 'chrome' -or $_.CommandLine -match 'chromium') } | Measure-Object | Select-Object -ExpandProperty Count",
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<usize>()
                .unwrap_or(0),
            _ => 0,
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        0
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::chrome_cmdline_has_flashbrowse_profile;

    #[test]
    fn recognizes_flashbrowse_chrome_cmdline() {
        assert!(chrome_cmdline_has_flashbrowse_profile(
            "/usr/bin/chrome-headless-shell --user-data-dir=/tmp/flashbrowse-42-deadbeef"
        ));
        assert!(!chrome_cmdline_has_flashbrowse_profile(
            "/usr/bin/chrome --user-data-dir=/tmp/my-profile"
        ));
    }
}
