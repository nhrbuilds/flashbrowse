//! `flashbrowse update` - check for and install a newer release (update channel aware).
//!
//! Channels (env `FLASHBROWSE_UPDATE_CHANNEL`, default `stable`):
//!   * `stable`  - latest `v*` release
//!   * `beta`    - latest `v*-beta*` release
//!   * `nightly` - latest `v*-nightly*` release
//!   * `off`     - never check
//!
//! Without `--check`, downloads the matching platform archive from GitHub
//! Releases and replaces the running executable in place. With `--check`,
//! prints whether a newer version is available without modifying anything.

use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;

use anyhow::Context;
use serde_json::Value;

/// Update channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Beta,
    Nightly,
    Off,
}

impl Channel {
    /// Parse a channel name.
    pub fn parse(s: &str) -> Channel {
        match s.trim().to_ascii_lowercase().as_str() {
            "beta" | "preview" => Channel::Beta,
            "nightly" | "dev" => Channel::Nightly,
            "off" | "none" | "disabled" => Channel::Off,
            _ => Channel::Stable,
        }
    }

    /// Read the channel from the environment (default `stable`).
    pub fn from_env() -> Channel {
        Channel::parse(
            &std::env::var("FLASHBROWSE_UPDATE_CHANNEL").unwrap_or_else(|_| "stable".into()),
        )
    }

    /// Does a release tag match this channel?
    pub fn matches(&self, tag: &str) -> bool {
        match self {
            Channel::Stable => {
                !tag.contains("beta") && !tag.contains("nightly") && !tag.contains("rc")
            }
            Channel::Beta => tag.contains("beta") || tag.contains("rc"),
            Channel::Nightly => tag.contains("nightly"),
            Channel::Off => false,
        }
    }

    /// Name of the channel.
    pub fn name(&self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
            Channel::Nightly => "nightly",
            Channel::Off => "off",
        }
    }
}

/// Attached release asset.
#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    /// Asset filename, e.g. `flashbrowse-linux-x86_64.tar.gz`.
    pub name: String,
    /// Direct download URL.
    pub download_url: String,
}

/// Latest release info.
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    /// Release tag, e.g. `v1.2.3`.
    pub tag: String,
    /// Release URL.
    pub url: String,
    /// Attached release assets.
    pub assets: Vec<ReleaseAsset>,
}

fn build_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("flashbrowse-update/1.0")
        .build()
        .map_err(|e| e.to_string())
}

/// Fetch the latest release matching `channel` from the GitHub API.
pub async fn latest_release(channel: Channel) -> Result<ReleaseInfo, String> {
    if channel == Channel::Off {
        return Err("update channel is 'off'".to_string());
    }
    let client = build_http_client()?;
    let url = "https://api.github.com/repos/nhrbuilds/flashbrowse/releases";
    let list: Vec<Value> = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    for rel in list {
        let Some(tag) = rel.get("tag_name").and_then(|t| t.as_str()) else {
            continue;
        };
        if rel.get("draft").and_then(|d| d.as_bool()).unwrap_or(false) {
            continue;
        }
        if !channel.matches(tag) {
            continue;
        }
        let html = rel
            .get("html_url")
            .and_then(|u| u.as_str())
            .unwrap_or("https://github.com/nhrbuilds/flashbrowse/releases")
            .to_string();
        let assets = rel
            .get("assets")
            .and_then(|a| a.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let name = item.get("name")?.as_str()?.to_string();
                        let download_url = item.get("browser_download_url")?.as_str()?.to_string();
                        Some(ReleaseAsset { name, download_url })
                    })
                    .collect()
            })
            .unwrap_or_default();
        return Ok(ReleaseInfo {
            tag: tag.to_string(),
            url: html,
            assets,
        });
    }
    Err(format!("no {} release found", channel.name()))
}

/// Compare semver-ish tags. Returns `true` when `candidate` is newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim()
            .trim_start_matches('v')
            .split(['.', '-'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let a = parse(candidate);
    let b = parse(current);
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// Return `(archive_name, binary_name)` for the target OS and architecture.
pub fn platform_asset(os: &str, arch: &str) -> Result<(&'static str, &'static str), String> {
    match (os, arch) {
        ("linux", "x86_64") => Ok(("flashbrowse-linux-x86_64.tar.gz", "flashbrowse")),
        ("linux", "aarch64") => Ok(("flashbrowse-linux-aarch64.tar.gz", "flashbrowse")),
        ("macos", "aarch64") => Ok(("flashbrowse-macos-aarch64.tar.gz", "flashbrowse")),
        ("macos", "x86_64") => Ok(("flashbrowse-macos-x86_64.tar.gz", "flashbrowse")),
        ("windows", "x86_64") => Ok(("flashbrowse-windows-x86_64.zip", "flashbrowse.exe")),
        _ => Err(format!(
            "unsupported platform {os}/{arch}; build from source with: cargo install --git https://github.com/nhrbuilds/flashbrowse flashbrowse-cli"
        )),
    }
}

/// Extract `binary_name` from a `.tar.gz` archive buffer.
pub fn extract_from_tar_gz(archive_bytes: &[u8], binary_name: &str) -> anyhow::Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);
    let expected = OsStr::new(binary_name);
    for entry in archive.entries().context("failed to read tar entries")? {
        let mut entry = entry.context("invalid tar entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().context("invalid tar entry path")?;
        if path.file_name() == Some(expected) {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .context("failed to read binary from tar archive")?;
            return Ok(buf);
        }
    }
    anyhow::bail!("binary '{binary_name}' not found in tar.gz archive")
}

/// Extract `binary_name` from a `.zip` archive buffer.
pub fn extract_from_zip(archive_bytes: &[u8], binary_name: &str) -> anyhow::Result<Vec<u8>> {
    let reader = std::io::Cursor::new(archive_bytes);
    let mut archive = zip::ZipArchive::new(reader).context("failed to open zip archive")?;
    let expected = OsStr::new(binary_name);
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).context("invalid zip entry")?;
        if file.is_dir() {
            continue;
        }
        let name = file.name().context("invalid zip entry name")?;
        if Path::new(name.as_ref()).file_name() == Some(expected) {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)
                .context("failed to read binary from zip archive")?;
            return Ok(buf);
        }
    }
    anyhow::bail!("binary '{binary_name}' not found in zip archive")
}

fn extract_binary(
    archive_name: &str,
    archive_bytes: &[u8],
    binary_name: &str,
) -> anyhow::Result<Vec<u8>> {
    if archive_name.ends_with(".tar.gz") {
        extract_from_tar_gz(archive_bytes, binary_name)
    } else if archive_name.ends_with(".zip") {
        extract_from_zip(archive_bytes, binary_name)
    } else {
        anyhow::bail!("unsupported archive format: {archive_name}")
    }
}

/// Atomically replace `target` on disk with `new_bytes`.
pub fn replace_binary_at(target: &Path, new_bytes: &[u8]) -> anyhow::Result<()> {
    let parent = target
        .parent()
        .context("executable path has no parent directory")?;
    let tmp_path = parent.join(format!(".flashbrowse-update-{}.tmp", std::process::id()));

    let write_res = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(new_bytes)?;
        f.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow::bail!(
                "permission denied writing to {}; re-run with elevated privileges (e.g. sudo)",
                parent.display()
            );
        }
        return Err(e).with_context(|| format!("failed to write {}", tmp_path.display()));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))
        {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e).context("failed to mark updated binary executable");
        }
        if let Err(e) = std::fs::rename(&tmp_path, target) {
            let _ = std::fs::remove_file(&tmp_path);
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                anyhow::bail!(
                    "permission denied replacing {}; re-run with elevated privileges (e.g. sudo)",
                    target.display()
                );
            }
            return Err(e).with_context(|| format!("failed to replace {}", target.display()));
        }
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("xattr")
                .args(["-dr", "com.apple.quarantine"])
                .arg(target)
                .status();
        }
    }

    #[cfg(windows)]
    {
        let old_path = target.with_extension("old.exe");
        let _ = std::fs::remove_file(&old_path);
        if let Err(e) = std::fs::rename(target, &old_path) {
            let _ = std::fs::remove_file(&tmp_path);
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                anyhow::bail!(
                    "permission denied replacing {}; re-run from an elevated terminal",
                    target.display()
                );
            }
            return Err(e).with_context(|| format!("failed to stage {}", target.display()));
        }
        if let Err(e) = std::fs::rename(&tmp_path, target) {
            let _ = std::fs::rename(&old_path, target);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e).with_context(|| format!("failed to install {}", target.display()));
        }
        let _ = std::fs::remove_file(&old_path);
    }

    Ok(())
}

async fn install_update(info: &ReleaseInfo, current: &str) -> anyhow::Result<()> {
    let (archive_name, binary_name) = platform_asset(std::env::consts::OS, std::env::consts::ARCH)
        .map_err(|e| anyhow::anyhow!(e))?;
    let asset = info
        .assets
        .iter()
        .find(|a| a.name == archive_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release {} does not include {} yet (CI may still be building)",
                info.tag,
                archive_name
            )
        })?;

    crate::write_stdout(format_args!(
        "updating flashbrowse {current} -> {} ...",
        info.tag
    ))?;
    let client = build_http_client().map_err(|e| anyhow::anyhow!(e))?;
    let resp = client
        .get(&asset.download_url)
        .send()
        .await
        .context("failed to download release archive")?
        .error_for_status()
        .context("release archive download returned an error")?;
    let archive_bytes = resp
        .bytes()
        .await
        .context("failed to read release archive")?;

    let binary_bytes = extract_binary(archive_name, &archive_bytes, binary_name)?;
    let exe_path = std::env::current_exe().context("failed to locate current executable")?;
    replace_binary_at(&exe_path, &binary_bytes)?;
    crate::write_stdout(format_args!(
        "updated {} to {}",
        exe_path.display(),
        info.tag
    ))?;
    Ok(())
}

/// `flashbrowse update [--check]`.
pub async fn run(check_only: bool) -> anyhow::Result<()> {
    let channel = Channel::from_env();
    let current = env!("CARGO_PKG_VERSION");
    if channel == Channel::Off {
        crate::write_stdout(format_args!(
            "update channel is 'off' (FLASHBROWSE_UPDATE_CHANNEL=off)"
        ))?;
        return Ok(());
    }
    match latest_release(channel).await {
        Ok(info) => {
            if is_newer(&info.tag, current) {
                if check_only {
                    crate::write_stdout(format_args!(
                        "update available: {current} -> {}",
                        info.tag
                    ))?;
                    crate::write_stdout(format_args!("  {}", info.url))?;
                } else {
                    install_update(&info, current).await?;
                }
            } else {
                crate::write_stdout(format_args!(
                    "flashbrowse {current} is up to date (channel: {})",
                    channel.name()
                ))?;
            }
        }
        Err(e) => {
            if check_only {
                // Never fail a check command when offline or rate-limited.
                crate::write_stderr(format_args!("update check skipped: {e}"));
            } else {
                anyhow::bail!("update failed: {e}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::{
        extract_from_tar_gz, extract_from_zip, is_newer, platform_asset, replace_binary_at, Channel,
    };

    #[test]
    fn channel_matching() {
        let stable = Channel::Stable;
        assert!(stable.matches("v1.2.3"));
        assert!(!stable.matches("v1.2.3-beta.1"));
        assert!(!stable.matches("v1.2.3-nightly"));
        assert!(Channel::Beta.matches("v1.2.3-beta.1"));
        assert!(Channel::Nightly.matches("v1.2.3-nightly"));
        assert!(!Channel::Off.matches("v1.2.3"));
    }

    #[test]
    fn version_comparison() {
        assert!(is_newer("v1.0.1", "1.0.0"));
        assert!(is_newer("v2.0.0", "v1.9.9"));
        assert!(!is_newer("v1.0.0", "v1.0.0"));
        assert!(!is_newer("v0.9.0", "v1.0.0"));
    }

    #[test]
    fn platform_asset_maps_supported_targets() {
        assert_eq!(
            platform_asset("linux", "x86_64").unwrap(),
            ("flashbrowse-linux-x86_64.tar.gz", "flashbrowse")
        );
        assert_eq!(
            platform_asset("macos", "aarch64").unwrap(),
            ("flashbrowse-macos-aarch64.tar.gz", "flashbrowse")
        );
        assert_eq!(
            platform_asset("windows", "x86_64").unwrap(),
            ("flashbrowse-windows-x86_64.zip", "flashbrowse.exe")
        );
        assert!(platform_asset("freebsd", "x86_64").is_err());
    }

    #[test]
    fn extracts_binary_from_tar_gz() {
        let payload = b"#!/bin/sh\necho ok\n";
        let gz_buf = Vec::new();
        let encoder = flate2::write::GzEncoder::new(gz_buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);

        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "flashbrowse", &payload[..])
            .unwrap();
        let encoder = tar.into_inner().unwrap();
        let bytes = encoder.finish().unwrap();

        let extracted = extract_from_tar_gz(&bytes, "flashbrowse").unwrap();
        assert_eq!(extracted, payload);
    }

    #[test]
    fn extracts_binary_from_zip() {
        let payload = b"MZ-dummy-exe";
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();
            zw.start_file("flashbrowse.exe", options).unwrap();
            zw.write_all(payload).unwrap();
            zw.finish().unwrap();
        }
        let bytes = buf.into_inner();

        let extracted = extract_from_zip(&bytes, "flashbrowse.exe").unwrap();
        assert_eq!(extracted, payload);
    }

    #[test]
    fn replace_binary_swaps_contents_atomically() {
        let dir =
            std::env::temp_dir().join(format!("flashbrowse-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("flashbrowse");
        std::fs::write(&target, b"old-binary").unwrap();

        replace_binary_at(&target, b"new-binary").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new-binary");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
