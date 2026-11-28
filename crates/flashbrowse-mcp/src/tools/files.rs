use super::*;

pub(super) fn is_windows_reserved_stem(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

pub(super) async fn call_pdf(
    arguments: &Value,
    handle: &SessionHandle,
    default_config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let name = arg_string(arguments, "name")?;
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || is_windows_reserved_stem(&name)
    {
        return Err(McpError::InvalidParams(
            "pdf 'name' must use only letters, digits, '-' or '_' and not be a reserved device name"
                .to_string(),
        ));
    }
    let root = mcp_file_root(default_config)?;
    let pdf_dir = root.join("pdfs");
    tokio::fs::create_dir_all(&pdf_dir)
        .await
        .map_err(|e| McpError::Internal(format!("cannot create pdf directory: {e}")))?;
    let pdf_dir = tokio::fs::canonicalize(&pdf_dir)
        .await
        .map_err(|e| McpError::Internal(format!("cannot resolve pdf directory: {e}")))?;
    if !pdf_dir.starts_with(&root) {
        return Err(McpError::InvalidParams(
            "pdf directory resolves outside the configured root".to_string(),
        ));
    }
    let path = pdf_dir.join(format!("{name}.pdf"));

    let scale = arguments
        .get("scale")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    if !scale.is_finite() || !(0.1..=2.0).contains(&scale) {
        return Err(McpError::InvalidParams(
            "pdf 'scale' must be a number in 0.1..=2.0".to_string(),
        ));
    }
    let opts = PdfOptions {
        landscape: arg_bool(arguments, "landscape"),
        print_background: arguments
            .get("print_background")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        scale,
    };
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    match session.print_to_pdf(opts).await {
        Ok(bytes) => {
            // Write to a new file and replace the destination atomically.
            // Renaming replaces a destination symlink instead of following it.
            let size = bytes.len();
            let result = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
                use std::io::Write;
                let mut temp = tempfile::NamedTempFile::new_in(pdf_dir)?;
                temp.write_all(&bytes)?;
                temp.persist(&path).map_err(|error| error.error)?;
                Ok(path)
            })
            .await
            .map_err(|e| McpError::Internal(format!("pdf write task failed: {e}")))?;
            let path = result.map_err(|e| McpError::Internal(format!("cannot write pdf: {e}")))?;
            let resp = serde_json::json!({
                "path": path.to_string_lossy(),
                "bytes": size,
            });
            Ok(ToolResult::text(resp.to_string()))
        }
        Err(e) => Ok(ToolResult::error(e.to_string())),
    }
}

pub(super) fn storage_state_path(
    config: &SessionConfig,
    name: &str,
) -> Result<std::path::PathBuf, McpError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || name.ends_with(".json")
        || is_windows_reserved_stem(name)
    {
        return Err(McpError::InvalidParams(
            "name must be a simple file name without a path or extension and not a reserved device name"
                .into(),
        ));
    }
    let root = config
        .storage_state_root
        .clone()
        .unwrap_or_else(|| flashbrowse_core::paths::cache_dir().join("storage-state"));
    std::fs::create_dir_all(&root).map_err(|error| McpError::Internal(error.to_string()))?;
    let root =
        std::fs::canonicalize(&root).map_err(|error| McpError::Internal(error.to_string()))?;
    let path = root.join(format!("{name}.json"));
    if path.exists() {
        let canonical =
            std::fs::canonicalize(&path).map_err(|error| McpError::Internal(error.to_string()))?;
        if !canonical.starts_with(&root) {
            return Err(McpError::InvalidParams(
                "storage-state file resolves outside the configured root".into(),
            ));
        }
    }
    Ok(path)
}

pub(super) fn mcp_file_root(config: &SessionConfig) -> Result<std::path::PathBuf, McpError> {
    let root = config
        .mcp_file_root
        .clone()
        .unwrap_or_else(|| flashbrowse_core::paths::cache_dir().join("files"));
    std::fs::create_dir_all(&root).map_err(|error| McpError::Internal(error.to_string()))?;
    std::fs::canonicalize(root).map_err(|error| McpError::Internal(error.to_string()))
}

pub(super) fn resolve_upload_files(
    config: &SessionConfig,
    values: &[Value],
) -> Result<Vec<std::path::PathBuf>, McpError> {
    if values.is_empty() || values.len() > 20 {
        return Err(McpError::InvalidParams(
            "files must contain between 1 and 20 file paths".into(),
        ));
    }
    let root = mcp_file_root(config)?;
    values
        .iter()
        .map(|value| {
            let raw = value.as_str().ok_or_else(|| {
                McpError::InvalidParams("each files item must be a path string".into())
            })?;
            let candidate = std::path::Path::new(raw);
            let candidate = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                root.join(candidate)
            };
            let canonical = std::fs::canonicalize(candidate).map_err(|error| {
                McpError::InvalidParams(format!("cannot access upload file: {error}"))
            })?;
            if !canonical.starts_with(&root) || !canonical.is_file() {
                return Err(McpError::InvalidParams(
                    "upload files must be regular files within the configured MCP file root".into(),
                ));
            }
            Ok(canonical)
        })
        .collect()
}

pub(super) async fn call_storage_state(
    arguments: &Value,
    handle: &SessionHandle,
    config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let action = arg_string(arguments, "action")?;
    let name = arg_string(arguments, "name")?;
    let path = storage_state_path(config, &name)?;
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    match action.as_str() {
        "export" => {
            let state = session
                .export_storage_state()
                .await
                .map_err(|error| McpError::Internal(error.to_string()))?;
            let encoded = serde_json::to_vec_pretty(&state)
                .map_err(|error| McpError::Internal(error.to_string()))?;
            tokio::fs::write(&path, encoded)
                .await
                .map_err(|error| McpError::Internal(error.to_string()))?;
            Ok(ToolResult::text(format!(
                "Storage state exported as {name}"
            )))
        }
        "import" => {
            let encoded = tokio::fs::read(&path).await.map_err(|error| {
                McpError::InvalidParams(format!("cannot read storage state: {error}"))
            })?;
            let state: Value = serde_json::from_slice(&encoded).map_err(|error| {
                McpError::InvalidParams(format!("invalid storage-state JSON: {error}"))
            })?;
            session
                .import_storage_state(&state)
                .await
                .map(ToolResult::text)
                .map_err(|error| McpError::Internal(error.to_string()))
        }
        _ => Err(McpError::InvalidParams(
            "action must be import or export".into(),
        )),
    }
}

pub(super) async fn call_upload(
    arguments: &Value,
    handle: &SessionHandle,
    config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let action_id = arg_u32(arguments, "action_id")?;
    let values = arguments
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::InvalidParams("files must be an array of paths".into()))?;
    let files = resolve_upload_files(config, values)?;
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    session
        .upload_files(action_id, &files)
        .await
        .map(ToolResult::text)
        .map_err(|error| McpError::InvalidParams(error.to_string()))
}

pub(super) async fn call_downloads(
    arguments: &Value,
    handle: &SessionHandle,
) -> Result<ToolResult, McpError> {
    let action = arg_opt_string(arguments, "action").unwrap_or_else(|| "list".into());
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    if action == "wait" {
        let guid = arg_string(arguments, "guid")?;
        let timeout_ms = arguments
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(10_000)
            .clamp(100, 20_000);
        let now = tokio::time::Instant::now();
        let deadline = now
            .checked_add(std::time::Duration::from_millis(timeout_ms))
            .unwrap_or(now);
        loop {
            if let Some(download) = session
                .downloads()
                .into_iter()
                .find(|item| item.guid == guid)
            {
                if download.state != "inProgress" || tokio::time::Instant::now() >= deadline {
                    return Ok(ToolResult::text(
                        serde_json::to_string(&download).unwrap_or_default(),
                    ));
                }
            } else if tokio::time::Instant::now() >= deadline {
                return Err(McpError::InvalidParams(format!(
                    "unknown download guid: {guid}"
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    if action != "list" {
        return Err(McpError::InvalidParams(
            "action must be list or wait".into(),
        ));
    }
    let result = serde_json::json!({
        "download_dir": session.download_dir(),
        "downloads": session.downloads(),
    });
    Ok(ToolResult::text(
        serde_json::to_string(&result).unwrap_or_default(),
    ))
}
