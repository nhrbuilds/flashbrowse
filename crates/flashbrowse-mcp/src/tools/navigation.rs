use super::*;

pub(super) async fn call_navigate(
    arguments: &Value,
    handle: &SessionHandle,
) -> Result<ToolResult, McpError> {
    let action = arg_string(arguments, "action")?;
    let ignore_cache = arg_bool(arguments, "ignore_cache");
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    let res = match action.as_str() {
        "back" => session.back().await,
        "forward" => session.forward().await,
        "reload" => session.reload(ignore_cache).await,
        other => {
            return Err(McpError::InvalidParams(format!(
                "unknown navigate action '{other}' (expected back|forward|reload)"
            )));
        }
    };
    match res {
        Ok(tree) => Ok(ToolResult::text(tree)),
        Err(e) => Ok(ToolResult::error(e.to_string())),
    }
}

pub(super) async fn call_wait_for(
    arguments: &Value,
    handle: &SessionHandle,
) -> Result<ToolResult, McpError> {
    let text = arg_opt_string(arguments, "text");
    let selector = arg_opt_string(arguments, "selector");
    let url = arg_opt_string(arguments, "url");
    let network_idle_ms = arguments.get("network_idle_ms").and_then(|v| v.as_u64());
    let timeout_ms = arguments
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(10_000);

    let provided = [
        text.is_some(),
        selector.is_some(),
        url.is_some(),
        network_idle_ms.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if provided != 1 {
        return Err(McpError::InvalidParams(
            "browser_wait_for requires exactly one of: text, selector, url, network_idle_ms"
                .to_string(),
        ));
    }
    let condition = if let Some(t) = text {
        WaitCondition::Text(t)
    } else if let Some(s) = selector {
        WaitCondition::Selector(s)
    } else if let Some(u) = url {
        WaitCondition::UrlContains(u)
    } else {
        WaitCondition::NetworkIdle {
            quiet_ms: network_idle_ms.unwrap_or(200),
        }
    };

    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    match session.wait_for(condition, timeout_ms).await {
        Ok(tree) => Ok(ToolResult::text(tree)),
        Err(e) => Ok(ToolResult::error(e.to_string())),
    }
}

pub(super) async fn call_console(
    arguments: &Value,
    handle: &SessionHandle,
) -> Result<ToolResult, McpError> {
    let action = arg_opt_string(arguments, "action").unwrap_or_else(|| "list".to_string());
    let level = arg_opt_string(arguments, "level");
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    match action.as_str() {
        "list" => {
            let entries = session.console_entries(level.as_deref());
            let json =
                serde_json::to_string(&entries).map_err(|e| McpError::Internal(e.to_string()))?;
            Ok(ToolResult::text(json))
        }
        "clear" => {
            session.clear_console();
            Ok(ToolResult::text("console cleared".to_string()))
        }
        other => Err(McpError::InvalidParams(format!(
            "unknown console action '{other}' (expected list|clear)"
        ))),
    }
}

pub(super) async fn call_network(
    arguments: &Value,
    handle: &SessionHandle,
) -> Result<ToolResult, McpError> {
    let action = arg_opt_string(arguments, "action").unwrap_or_else(|| "list".to_string());
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    match action.as_str() {
        "list" => {
            let failed_only = arg_bool(arguments, "failed_only");
            let entries = session.network_entries(failed_only);
            let json =
                serde_json::to_string(&entries).map_err(|e| McpError::Internal(e.to_string()))?;
            Ok(ToolResult::text(json))
        }
        "body" => {
            let request_id = arg_string(arguments, "request_id")?;
            match session.network_response_body(&request_id).await {
                Ok(body) => Ok(ToolResult::text(body)),
                Err(e) => Ok(ToolResult::error(e.to_string())),
            }
        }
        "clear" => {
            session.clear_network();
            Ok(ToolResult::text("network log cleared".to_string()))
        }
        "set_headers" => {
            let obj = arguments
                .get("headers")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    McpError::InvalidParams("set_headers requires 'headers' object".to_string())
                })?;
            let mut map = std::collections::HashMap::with_capacity(obj.len());
            for (k, v) in obj {
                let s = v.as_str().ok_or_else(|| {
                    McpError::InvalidParams(format!("header '{k}' value must be a string"))
                })?;
                if k.is_empty()
                    || !k.is_ascii()
                    || k.bytes().any(|b| b.is_ascii_control() || b == b':')
                {
                    return Err(McpError::InvalidParams(format!(
                        "header name '{k}' must be non-empty ASCII without control characters or ':'"
                    )));
                }
                if !s.is_ascii() || s.bytes().any(|b| b.is_ascii_control()) {
                    return Err(McpError::InvalidParams(format!(
                        "header '{k}' value must be ASCII without control characters"
                    )));
                }
                map.insert(k.clone(), s.to_string());
            }
            match session.set_extra_headers(map).await {
                Ok(()) => Ok(ToolResult::text("extra headers updated".to_string())),
                Err(e) => Ok(ToolResult::error(e.to_string())),
            }
        }
        other => Err(McpError::InvalidParams(format!(
            "unknown network action '{other}' (expected list|body|clear|set_headers)"
        ))),
    }
}

pub(super) async fn call_route(
    arguments: &Value,
    handle: &SessionHandle,
) -> Result<ToolResult, McpError> {
    let action = arg_opt_string(arguments, "action").unwrap_or_else(|| "list".to_string());
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    match action.as_str() {
        "add" => {
            let url_pattern = arg_string(arguments, "url_pattern")?;
            if url_pattern.trim().is_empty() {
                return Err(McpError::InvalidParams(
                    "url_pattern must not be empty".to_string(),
                ));
            }
            let status = match arguments.get("status") {
                None => 200,
                Some(v) => v
                    .as_u64()
                    .and_then(|n| u16::try_from(n).ok())
                    .filter(|&code| (100..=599).contains(&code))
                    .ok_or_else(|| {
                        McpError::InvalidParams(
                            "route 'status' must be an HTTP status integer in 100..=599"
                                .to_string(),
                        )
                    })?,
            };
            let content_type = arg_opt_string(arguments, "content_type")
                .unwrap_or_else(|| "application/json".to_string());
            let content_type = content_type.trim().to_string();
            if content_type.is_empty()
                || !content_type.is_ascii()
                || content_type.bytes().any(|b| b.is_ascii_control())
            {
                return Err(McpError::InvalidParams(
                    "route 'content_type' must be non-empty ASCII without control characters"
                        .to_string(),
                ));
            }
            let body = arg_opt_string(arguments, "body").unwrap_or_default();
            let abort = arg_bool(arguments, "abort");
            let rule = RouteRule {
                url_pattern,
                status,
                content_type,
                body,
                abort,
            };
            match session.add_route(rule).await {
                Ok(()) => Ok(ToolResult::text("route rule added".to_string())),
                Err(e) => Ok(ToolResult::error(e.to_string())),
            }
        }
        "list" => {
            let rules = session.list_routes();
            let json =
                serde_json::to_string(&rules).map_err(|e| McpError::Internal(e.to_string()))?;
            Ok(ToolResult::text(json))
        }
        "clear" => match session.clear_routes().await {
            Ok(()) => Ok(ToolResult::text("routes cleared".to_string())),
            Err(e) => Ok(ToolResult::error(e.to_string())),
        },
        other => Err(McpError::InvalidParams(format!(
            "unknown route action '{other}' (expected add|list|clear)"
        ))),
    }
}

pub(super) async fn call_contexts(
    arguments: &Value,
    handle: &SessionHandle,
    default_config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let action = arg_opt_string(arguments, "action").unwrap_or_else(|| "list".to_string());
    match action.as_str() {
        "list" => {
            let guard = handle.lock().await;
            let mut items: Vec<Value> = guard
                .sessions
                .iter()
                .map(|(name, sess)| {
                    serde_json::json!({
                        "name": name,
                        "active": name == guard.active_name(),
                        "url": sess.current_url(),
                        "browser_context_id": sess.browser_context_id(),
                    })
                })
                .collect();
            items.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            let json =
                serde_json::to_string(&items).map_err(|e| McpError::Internal(e.to_string()))?;
            Ok(ToolResult::text(json))
        }
        "create" => {
            let name = arg_string(arguments, "name")?;
            if name.trim().is_empty() || name == "default" {
                return Err(McpError::InvalidParams(
                    "context name must be non-empty and not 'default'".to_string(),
                ));
            }
            let mut guard = handle.lock().await;
            if guard.sessions.contains_key(&name) {
                return Ok(ToolResult::error(format!(
                    "browser context '{name}' already exists"
                )));
            }
            if !guard.sessions.contains_key("default") {
                let fresh = BrowserSession::launch(default_config.clone())
                    .await
                    .map_err(|e| McpError::Internal(e.to_string()))?;
                guard.sessions.insert("default".to_string(), fresh);
            }
            let Some(default_sess) = guard.sessions.get("default") else {
                return Err(McpError::Internal(
                    "default browser session was not created".to_string(),
                ));
            };
            let sess = default_sess
                .create_isolated_context(default_config)
                .await
                .map_err(|e| McpError::Internal(e.to_string()))?;
            guard.sessions.insert(name.clone(), sess);
            guard.active = name.clone();
            Ok(ToolResult::text(format!(
                "created and selected browser context '{name}'"
            )))
        }
        "select" => {
            let name = arg_string(arguments, "name")?;
            let mut guard = handle.lock().await;
            if !guard.sessions.contains_key(&name) {
                return Ok(ToolResult::error(format!(
                    "unknown browser context '{name}'"
                )));
            }
            guard.active = name.clone();
            let tree = guard
                .sessions
                .get(&name)
                .map(|s| s.last_tree().to_string())
                .unwrap_or_default();
            Ok(ToolResult::text(if tree.is_empty() {
                format!("selected browser context '{name}'")
            } else {
                tree
            }))
        }
        "close" => {
            let name = arg_string(arguments, "name")?;
            if name == "default" {
                return Ok(ToolResult::error(
                    "cannot close the 'default' browser context".to_string(),
                ));
            }
            let mut guard = handle.lock().await;
            let Some(sess) = guard.sessions.remove(&name) else {
                return Ok(ToolResult::error(format!(
                    "unknown browser context '{name}'"
                )));
            };
            if guard.active == name {
                guard.active = "default".to_string();
            }
            if let Err(error) = sess.close_isolated_context().await {
                return Ok(ToolResult::error(format!(
                    "failed to close browser context '{name}': {error}"
                )));
            }
            Ok(ToolResult::text(format!("closed browser context '{name}'")))
        }
        other => Err(McpError::InvalidParams(format!(
            "unknown contexts action '{other}' (expected list|create|select|close)"
        ))),
    }
}

pub(super) async fn call_emulate(
    arguments: &Value,
    handle: &SessionHandle,
) -> Result<ToolResult, McpError> {
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    let viewport = if let Some(vp) = arguments.get("viewport").and_then(Value::as_object) {
        let width = vp
            .get("width")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
            .filter(|&w| (1..=16_384).contains(&w))
            .ok_or_else(|| {
                McpError::InvalidParams("viewport.width must be an integer in 1..=16384".into())
            })?;
        let height = vp
            .get("height")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
            .filter(|&h| (1..=16_384).contains(&h))
            .ok_or_else(|| {
                McpError::InvalidParams("viewport.height must be an integer in 1..=16384".into())
            })?;
        let device_scale_factor = vp
            .get("device_scale_factor")
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        if !device_scale_factor.is_finite()
            || device_scale_factor <= 0.0
            || device_scale_factor > 10.0
        {
            return Err(McpError::InvalidParams(
                "viewport.device_scale_factor must be in (0, 10]".into(),
            ));
        }
        let mobile = vp.get("mobile").and_then(Value::as_bool).unwrap_or(false);
        Some(ViewportOptions {
            width,
            height,
            device_scale_factor,
            mobile,
        })
    } else {
        None
    };
    let geolocation = if let Some(g) = arguments.get("geolocation").and_then(Value::as_object) {
        let latitude = g
            .get("latitude")
            .and_then(Value::as_f64)
            .filter(|&lat| lat.is_finite() && (-90.0..=90.0).contains(&lat))
            .ok_or_else(|| {
                McpError::InvalidParams("geolocation.latitude must be in -90..=90".into())
            })?;
        let longitude = g
            .get("longitude")
            .and_then(Value::as_f64)
            .filter(|&lon| lon.is_finite() && (-180.0..=180.0).contains(&lon))
            .ok_or_else(|| {
                McpError::InvalidParams("geolocation.longitude must be in -180..=180".into())
            })?;
        let accuracy = g.get("accuracy").and_then(Value::as_f64).unwrap_or(10.0);
        if !accuracy.is_finite() || accuracy < 0.0 {
            return Err(McpError::InvalidParams(
                "geolocation.accuracy must be a non-negative finite number".into(),
            ));
        }
        Some(GeolocationOptions {
            latitude,
            longitude,
            accuracy,
        })
    } else {
        None
    };
    let opts = EmulationOptions {
        viewport,
        timezone: arg_opt_string(arguments, "timezone"),
        locale: arg_opt_string(arguments, "locale"),
        geolocation,
    };
    let permissions = if let Some(perms) = arguments.get("permissions").and_then(Value::as_object) {
        let origin = perms
            .get("origin")
            .and_then(Value::as_str)
            .map(str::to_string);
        let raw_grants = perms
            .get("grants")
            .and_then(Value::as_array)
            .ok_or_else(|| McpError::InvalidParams("permissions.grants must be an array".into()))?;
        let mut grants = Vec::with_capacity(raw_grants.len());
        for entry in raw_grants {
            let perm = entry.as_str().ok_or_else(|| {
                McpError::InvalidParams("each permissions.grants entry must be a string".into())
            })?;
            grants.push(perm.to_string());
        }
        Some((origin, grants))
    } else {
        None
    };
    if arg_bool(arguments, "reset") {
        if let Err(e) = session.reset_emulation().await {
            return Ok(ToolResult::error(e.to_string()));
        }
    }
    if let Err(e) = session.emulate(opts).await {
        return Ok(ToolResult::error(e.to_string()));
    }
    if let Some((origin, grants)) = permissions {
        if let Err(e) = session.grant_permissions(origin.as_deref(), &grants).await {
            return Ok(ToolResult::error(e.to_string()));
        }
    }
    Ok(ToolResult::text("emulation updated".to_string()))
}

/// MCP URL allowlist (H4 fix): agent boundary must not read local files.
/// Only `http(s)://`, `data:`, and `about:blank` are navigable via MCP.
/// CLI commands keep full `file://` support (local-user trust boundary).
///
/// Expects an already-normalized URL: `resolve_mcp_url` adds the scheme for a
/// bare host first, so the shorthand `example.com` is judged as the
/// `https://example.com` it becomes rather than rejected for having no scheme.
pub(super) fn validate_mcp_url(url: &str) -> Result<(), McpError> {
    let t = url.trim();
    if t.is_empty() {
        return Err(McpError::InvalidParams("url must not be empty".to_string()));
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("data:")
        || lower == "about:blank"
    {
        // Reject file/chrome/view-source smuggling via whitespace/control.
        if t.chars().any(|c| c.is_control()) {
            return Err(McpError::InvalidParams(
                "url contains control characters".to_string(),
            ));
        }
        return Ok(());
    }
    Err(McpError::InvalidParams(format!(
        "url scheme not allowed via MCP (allowed: http, https, data, about:blank): {t}"
    )))
}

/// Resolve a caller's URL, then apply the MCP allowlist to the result.
///
/// Order matters: normalizing first means `example.com` is checked as
/// `https://example.com`, while `file:///etc/passwd` keeps its scheme and is
/// still rejected by the allowlist.
pub(super) fn resolve_mcp_url(raw: &str) -> Result<String, McpError> {
    let url = normalize_url(raw).map_err(|e| McpError::InvalidParams(e.to_string()))?;
    validate_mcp_url(&url)?;
    Ok(url)
}

pub(super) async fn call_open(
    arguments: &Value,
    handle: &SessionHandle,
    default_config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let raw = arg_string(arguments, "url")?;
    let url = resolve_mcp_url(&raw)?;
    let policy = arg_resource_policy(arguments, default_config)?;
    let mut guard = ensure_session(handle, default_config, policy).await?;
    let session = require_session(&mut guard)?;
    let mut cfg = default_config.clone();
    cfg.resource_policy = session.resource_policy();
    let existing_tabs = match session.recover_closed_active_tab(&cfg).await {
        Ok(tabs) => tabs,
        Err(error) => return Ok(ToolResult::error(error.to_string())),
    };
    let opener = session.target_id().to_string();
    let existing: std::collections::HashSet<String> =
        existing_tabs.into_iter().map(|t| t.target_id).collect();
    match session.open(&url).await {
        Ok(tree) => match focus_new_popup(session, &opener, &existing, &cfg).await {
            Ok(Some(popup_tree)) => Ok(ToolResult::text(popup_tree)),
            Ok(None) => Ok(ToolResult::text(tree)),
            Err(e) => Ok(ToolResult::error(e.to_string())),
        },
        Err(e) => Ok(ToolResult::error(e.to_string())),
    }
}

pub(super) async fn focus_new_popup(
    session: &mut BrowserSession,
    opener: &str,
    existing: &std::collections::HashSet<String>,
    config: &SessionConfig,
) -> Result<Option<String>, flashbrowse_core::CoreError> {
    if let Some(tab) = session.take_popup_for(opener, existing) {
        if let Ok(tree) = session.select_tab(&tab.target_id, config).await {
            return Ok(Some(tree));
        }
    }
    let tabs = session.list_tabs().await?;
    let popup = tabs.iter().rev().find(|t| {
        t.kind == "page"
            && t.opener_id.as_deref() == Some(opener)
            && !existing.contains(&t.target_id)
    });
    match popup {
        Some(tab) => session.select_tab(&tab.target_id, config).await.map(Some),
        None => Ok(None),
    }
}

pub(super) async fn call_tabs(
    arguments: &Value,
    handle: &SessionHandle,
    default_config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let action = arg_string(arguments, "action")?;
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    let mut config = default_config.clone();
    config.resource_policy = session.resource_policy();
    session
        .recover_closed_active_tab(&config)
        .await
        .map_err(|error| McpError::Internal(error.to_string()))?;
    match action.as_str() {
        "list" => {
            let active = session.target_id().to_string();
            let tabs = session
                .list_tabs()
                .await
                .map_err(|e| McpError::Internal(e.to_string()))?;
            let rows: Vec<Value> = tabs
                .into_iter()
                .map(|t| {
                    serde_json::json!({
                        "target_id": t.target_id,
                        "title": t.title,
                        "url": t.url,
                        "active": t.target_id == active
                    })
                })
                .collect();
            Ok(ToolResult::text(
                serde_json::to_string(&rows).unwrap_or_default(),
            ))
        }
        "select" => {
            let id = arg_string(arguments, "target_id")?;
            session
                .select_tab(&id, &config)
                .await
                .map(ToolResult::text)
                .map_err(|e| McpError::InvalidParams(e.to_string()))
        }
        "close" => {
            let id = arg_string(arguments, "target_id")?;
            session
                .close_tab(&id, &config)
                .await
                .map(|tree| {
                    tree.map(ToolResult::text)
                        .unwrap_or_else(|| ToolResult::text("Tab closed".to_string()))
                })
                .map_err(|e| McpError::InvalidParams(e.to_string()))
        }
        _ => Err(McpError::InvalidParams(
            "action must be list, select, or close".into(),
        )),
    }
}
