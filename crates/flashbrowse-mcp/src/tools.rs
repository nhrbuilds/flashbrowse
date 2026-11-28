//! MCP tool definitions and execution.

mod actions;
mod files;
mod navigation;
mod session;

#[cfg(test)]
use files::{is_windows_reserved_stem, resolve_upload_files, storage_state_path};
#[cfg(test)]
use navigation::resolve_mcp_url;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use crate::protocol::{McpError, ToolResult};
use flashbrowse_core::actions::ScrollDirection;
use flashbrowse_core::{
    normalize_url, BrowserSession, EmulationOptions, GeolocationOptions, PdfOptions,
    ResourcePolicy, RouteRule, ScreenshotOptions, SessionConfig, ViewportOptions, WaitCondition,
    SCREENSHOT_PREPARE_MS_DEFAULT, SCREENSHOT_PREPARE_MS_MAX,
};
use serde_json::Value;
use tokio::sync::Mutex;

/// Multi-context session manager sharing one Chromium process across isolated contexts.
pub struct SessionManager {
    active: String,
    sessions: HashMap<String, BrowserSession>,
}

/// Shared session manager across all `tools/call` invocations.
pub type SessionHandle = Arc<Mutex<SessionManager>>;

/// Parameter schema for tools that need no arguments except an explicit
/// `{}` object is still valid per MCP's `inputSchema` requirement.
fn empty_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

/// Tool definitions exposed via `tools/list`.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        serde_json::json!({
            "name": "browser_open",
            "description": "Navigates to a URL, applies stealth and network filtering, and returns the token-compressed, numbered semantic action tree. Use browser_screenshot on this same session for visuals (no second browser tool).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Target URL. The scheme may be omitted: https is assumed, and local hosts (localhost, 127.0.0.1, LAN addresses) use http. Allowed schemes: http, https, data, about:blank"},
                    "text_only": {"type": "boolean", "description": "Legacy: selects text-only resource policy", "default": false},
                    "resource_policy": {"type": "string", "enum": ["agent-default", "text-only", "full-render"], "description": "Network load policy (tree size unchanged); full-render loads images/fonts for faithful screenshots"}
                },
                "required": ["url"]
            }
        }),
        serde_json::json!({
            "name": "browser_act",
            "description": "Executes an interaction (click, double_click, right_click, drag_and_drop, fill, hover, press_key, scroll, select) against a numbered action element ID.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action_id": {"type": "integer", "minimum": 0, "maximum": 4294967295u64, "description": "Element ID number from the action tree (optional for press_key and viewport scroll)"},
                    "target_action_id": {"type": "integer", "minimum": 0, "maximum": 4294967295u64, "description": "Destination element ID for drag_and_drop"},
                    "action": {"type": "string", "enum": ["click", "double_click", "right_click", "drag_and_drop", "fill", "hover", "press_key", "scroll", "select"], "description": "Interaction to perform"},
                    "value": {"type": "string", "description": "Text payload for 'fill', key or chord (e.g. Control+K) for 'press_key', direction for 'scroll', or option label for 'select'"},
                    "amount": {"type": "number", "description": "Pixel distance for 'scroll' (default 300)"},
                    "dialog_action": {"type": "string", "enum": ["accept", "dismiss"], "description": "Optional disposition for a native JavaScript dialog opened by this action"},
                    "prompt_text": {"type": "string", "description": "Text supplied only when dialog_action is accept and a prompt opens"}
                },
                "required": ["action"]
            }
        }),
        serde_json::json!({
            "name": "browser_extract_markdown",
            "description": "Extracts readable article and document text formatted as clean Markdown, stripping all boilerplate.",
            "inputSchema": empty_schema()
        }),
        serde_json::json!({
            "name": "browser_evaluate",
            "description": "Evaluate a JavaScript expression in the live page and return the JSON-serialized result. Used for task success checks and diagnostics.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "expression": {"type": "string", "description": "JS expression evaluated in the page (returnByValue)"}
                },
                "required": ["expression"]
            }
        }),
        serde_json::json!({
            "name": "browser_screenshot",
            "description": "Captures a viewport, full-page, or element-clipped PNG of the current session (after browser_open/browser_act). Waits for images/network to settle. Use this tool instead of a separate browser MCP.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "full_page": {"type": "boolean", "default": false},
                    "action_id": {"type": "integer", "minimum": 0, "maximum": 4294967295u64, "description": "Optional action_id to clip the PNG to that element's bounding box"},
                    "prepare_lazy_content": {"type": "boolean", "description": "Scroll the page to surface lazy-loaded images", "default": true},
                    "timeout_ms": {"type": "integer", "minimum": 0, "description": "Max ms to wait before best-effort capture (500-60000)", "default": 8000}
                }
            }
        }),
        serde_json::json!({
            "name": "browser_tabs",
            "description": "Lists, selects, or closes pages in the current browser session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "select", "close"]},
                    "target_id": {"type": "string", "description": "Page target ID returned by list"}
                },
                "required": ["action"]
            }
        }),
        serde_json::json!({
            "name": "browser_storage_state",
            "description": "Imports or exports browser cookies and per-origin localStorage to a named JSON file confined to the configured storage-state root.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["import", "export"]},
                    "name": {"type": "string", "description": "Simple file name without a path or extension"}
                },
                "required": ["action", "name"]
            }
        }),
        serde_json::json!({
            "name": "browser_upload",
            "description": "Uploads one or more files to a file input. Paths are resolved inside the configured MCP file root.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action_id": {"type": "integer", "minimum": 0, "maximum": 4294967295u64, "description": "Action ID of an input[type=file]"},
                    "files": {"type": "array", "items": {"type": "string"}, "description": "File names or relative paths under the configured MCP file root"}
                },
                "required": ["action_id", "files"]
            }
        }),
        serde_json::json!({
            "name": "browser_downloads",
            "description": "Lists download progress or waits for one download to complete or fail. Downloads are saved under the configured MCP file root's downloads directory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "wait"], "default": "list"},
                    "guid": {"type": "string", "description": "Download identifier returned by the list action"},
                    "timeout_ms": {"type": "integer", "minimum": 0, "default": 10000, "description": "Maximum wait, capped at 20000 ms"}
                }
            }
        }),
        serde_json::json!({
            "name": "browser_fill_form",
            "description": "Fills multiple form fields sequentially from the current action tree and returns the refreshed final tree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "fields": {
                        "type": "array",
                        "description": "Ordered list of {action_id, value} entries to fill",
                        "items": {
                            "type": "object",
                            "properties": {
                                "action_id": {"type": "integer", "minimum": 0, "maximum": 4294967295u64, "description": "Element ID from the current action tree"},
                                "value": {"type": "string", "description": "Text value to enter"}
                            },
                            "required": ["action_id", "value"]
                        }
                    }
                },
                "required": ["fields"]
            }
        }),
        serde_json::json!({
            "name": "browser_navigate",
            "description": "Navigates session history back or forward, or reloads the current page, returning the refreshed semantic tree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["back", "forward", "reload"]},
                    "ignore_cache": {"type": "boolean", "default": false, "description": "Hard reload bypassing cache (only for action=reload)"}
                },
                "required": ["action"]
            }
        }),
        serde_json::json!({
            "name": "browser_wait_for",
            "description": "Waits for text, a CSS selector (including open shadow roots), a URL substring, or network idle, then returns the refreshed semantic tree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Wait until this visible text appears"},
                    "selector": {"type": "string", "description": "Wait until this CSS selector matches"},
                    "url": {"type": "string", "description": "Wait until location.href contains this substring"},
                    "network_idle_ms": {"type": "integer", "minimum": 0, "description": "Wait until network has been idle for this many ms"},
                    "timeout_ms": {"type": "integer", "minimum": 0, "default": 10000, "description": "Max wait duration in ms (100-30000)"}
                }
            }
        }),
        serde_json::json!({
            "name": "browser_console",
            "description": "Lists or clears captured browser console messages and uncaught JavaScript exceptions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "clear"], "default": "list"},
                    "level": {"type": "string", "description": "Optional filter: log, warning, error, info, debug, exception"}
                }
            }
        }),
        serde_json::json!({
            "name": "browser_network",
            "description": "Lists captured network requests, reads a response body by request_id, clears logs, or sets extra HTTP headers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "body", "clear", "set_headers"], "default": "list"},
                    "failed_only": {"type": "boolean", "default": false, "description": "Only return failed or >=400 responses (for action=list)"},
                    "request_id": {"type": "string", "description": "Request ID from list (for action=body)"},
                    "headers": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Key-value map of extra HTTP headers (for action=set_headers)"}
                }
            }
        }),
        serde_json::json!({
            "name": "browser_route",
            "description": "Adds, lists, or clears lazy Fetch request interception rules to mock responses or abort matching URLs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "list", "clear"], "default": "list"},
                    "url_pattern": {"type": "string", "description": "Substring or wildcard (*) URL pattern (required for action=add)"},
                    "status": {"type": "integer", "default": 200, "description": "HTTP status code for mocked response"},
                    "content_type": {"type": "string", "default": "application/json", "description": "Content-Type header for mocked response"},
                    "body": {"type": "string", "default": "", "description": "Response body text"},
                    "abort": {"type": "boolean", "default": false, "description": "If true, abort matching requests instead of fulfilling"}
                }
            }
        }),
        serde_json::json!({
            "name": "browser_contexts",
            "description": "Lists, creates, selects, or closes isolated incognito-like browser contexts sharing one Chrome process.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "create", "select", "close"], "default": "list"},
                    "name": {"type": "string", "description": "Context name (required for create, select, close; cannot close 'default')"}
                }
            }
        }),
        serde_json::json!({
            "name": "browser_emulate",
            "description": "Configures viewport/mobile emulation, timezone, locale, geolocation, and browser permissions on the active session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "viewport": {
                        "type": "object",
                        "properties": {
                            "width": {"type": "integer"},
                            "height": {"type": "integer"},
                            "device_scale_factor": {"type": "number", "default": 1.0},
                            "mobile": {"type": "boolean", "default": false}
                        },
                        "required": ["width", "height"]
                    },
                    "timezone": {"type": "string", "description": "IANA timezone ID (e.g. Asia/Tokyo)"},
                    "locale": {"type": "string", "description": "BCP-47 locale (e.g. ja-JP)"},
                    "geolocation": {
                        "type": "object",
                        "properties": {
                            "latitude": {"type": "number"},
                            "longitude": {"type": "number"},
                            "accuracy": {"type": "number", "default": 10.0}
                        },
                        "required": ["latitude", "longitude"]
                    },
                    "permissions": {
                        "type": "object",
                        "properties": {
                            "origin": {"type": "string"},
                            "grants": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["grants"]
                    },
                    "reset": {"type": "boolean", "default": false, "description": "Clear device metrics, geolocation, and permission overrides"}
                }
            }
        }),
        serde_json::json!({
            "name": "browser_pdf",
            "description": "Exports the current page to a PDF file confined under the configured MCP file root's pdfs directory and returns {path, bytes}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Simple file name without path or extension"},
                    "landscape": {"type": "boolean", "default": false},
                    "print_background": {"type": "boolean", "default": true},
                    "scale": {"type": "number", "default": 1.0}
                },
                "required": ["name"]
            }
        }),
    ]
}

fn validate_tool_arguments(name: &str, arguments: &Value) -> Result<(), McpError> {
    static DEFINITIONS: OnceLock<Vec<Value>> = OnceLock::new();
    let definition = DEFINITIONS
        .get_or_init(tool_definitions)
        .iter()
        .find(|definition| definition.get("name").and_then(Value::as_str) == Some(name));
    if let Some(schema) = definition.and_then(|definition| definition.get("inputSchema")) {
        validate_value(schema, arguments, "arguments")?;
    }
    Ok(())
}

fn validate_value(schema: &Value, value: &Value, path: &str) -> Result<(), McpError> {
    let kind = schema.get("type").and_then(Value::as_str).unwrap_or("");
    let valid = match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        _ => true,
    };
    if !valid {
        return Err(McpError::InvalidParams(format!("'{path}' must be {kind}")));
    }
    if let Some(number) = value.as_f64() {
        if schema
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|min| number < min)
            || schema
                .get("maximum")
                .and_then(Value::as_f64)
                .is_some_and(|max| number > max)
        {
            return Err(McpError::InvalidParams(format!("'{path}' is out of range")));
        }
    }
    if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
        let policy_alias = path == "arguments.resource_policy"
            && value
                .as_str()
                .is_some_and(|policy| policy.parse::<ResourcePolicy>().is_ok());
        if !choices.contains(value) && !policy_alias {
            return Err(McpError::InvalidParams(format!(
                "'{path}' has an unsupported value"
            )));
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    return Err(McpError::InvalidParams(format!(
                        "missing required '{path}.{key}'"
                    )));
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (key, value) in object {
                if let Some(property) = properties.get(key) {
                    let required = schema
                        .get("required")
                        .and_then(Value::as_array)
                        .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(key)));
                    if value.is_null() && !required {
                        continue;
                    }
                    validate_value(property, value, &format!("{path}.{key}"))?;
                } else if let Some(property) =
                    schema.get("additionalProperties").filter(|v| v.is_object())
                {
                    validate_value(property, value, &format!("{path}.{key}"))?;
                }
            }
        } else if let Some(property) = schema.get("additionalProperties").filter(|v| v.is_object())
        {
            for (key, value) in object {
                validate_value(property, value, &format!("{path}.{key}"))?;
            }
        }
    }
    if let (Some(items), Some(values)) = (schema.get("items"), value.as_array()) {
        for (index, value) in values.iter().enumerate() {
            validate_value(items, value, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

/// Parse `text_only` from args (default `false`).
fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Parse required string arg, or `InvalidParams`.
fn arg_string(args: &Value, key: &str) -> Result<String, McpError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| McpError::InvalidParams(format!("missing required '{key}'")))
}

/// Parse optional string arg.
fn arg_opt_string(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// Parse required integer arg (`action_id`), rejecting overflow.
fn arg_u32(args: &Value, key: &str) -> Result<u32, McpError> {
    let raw = args
        .get(key)
        .ok_or_else(|| McpError::InvalidParams(format!("missing required '{key}'")))?;
    let n = raw
        .as_u64()
        .ok_or_else(|| McpError::InvalidParams(format!("'{key}' must be an integer")))?;
    u32::try_from(n).map_err(|_| McpError::InvalidParams(format!("'{key}' out of range (u32 max)")))
}

/// Resolve the resource policy for a `browser_open` call.
///
/// An unparseable `resource_policy` is an error, not a silent fallback: the
/// agent asked for a specific network policy and would otherwise get a
/// different one with no indication.
fn arg_resource_policy(
    arguments: &Value,
    default_config: &SessionConfig,
) -> Result<ResourcePolicy, McpError> {
    if let Some(s) = arguments.get("resource_policy").and_then(|v| v.as_str()) {
        return s.parse::<ResourcePolicy>().map_err(McpError::InvalidParams);
    }
    if arguments
        .get("text_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Ok(ResourcePolicy::TextOnly);
    }
    Ok(default_config.resource_policy)
}

async fn ensure_session<'a>(
    handle: &'a SessionHandle,
    default_config: &SessionConfig,
    want_policy: ResourcePolicy,
) -> Result<tokio::sync::MutexGuard<'a, SessionManager>, McpError> {
    let mut guard = handle.lock().await;
    if guard
        .as_ref()
        .is_some_and(|session| !session.client().is_connected())
    {
        guard.shutdown_all().await;
    }
    if let Some(session) = guard.as_mut() {
        session
            .set_resource_policy(want_policy)
            .await
            .map_err(|e| McpError::Internal(e.to_string()))?;
    } else if guard.active == "default" {
        let mut cfg = default_config.clone();
        cfg.resource_policy = want_policy;
        let fresh = BrowserSession::launch(cfg)
            .await
            .map_err(|e| McpError::Internal(e.to_string()))?;
        guard.sessions.insert("default".to_string(), fresh);
    } else {
        return Err(McpError::Internal(format!(
            "active browser context '{}' is not initialized",
            guard.active
        )));
    }
    Ok(guard)
}

fn require_session(guard: &mut SessionManager) -> Result<&mut BrowserSession, McpError> {
    guard.session_mut_for(None)
}

/// Execute one `tools/call`. Returns a `ToolResult` (success or `isError`).
///
/// Every call is timed and appended to the metrics trail so operators can
/// see latency, token cost, and error rate per tool.
pub async fn call_tool(
    name: &str,
    arguments: &Value,
    handle: &SessionHandle,
    default_config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    flashbrowse_core::async_runtime::run(call_tool_inner(name, arguments, handle, default_config))
        .await
        .map_err(|error| McpError::Internal(error.to_string()))?
}

async fn call_tool_inner(
    name: &str,
    arguments: &Value,
    handle: &SessionHandle,
    default_config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    validate_tool_arguments(name, arguments)?;
    let started = std::time::Instant::now();
    let result = match name {
        "browser_open" => navigation::call_open(arguments, handle, default_config).await,
        "browser_act" => actions::call_act(arguments, handle, default_config).await,
        "browser_extract_markdown" => actions::call_markdown(handle).await,
        "browser_evaluate" => actions::call_evaluate(arguments, handle).await,
        "browser_screenshot" => actions::call_screenshot(arguments, handle).await,
        "browser_tabs" => navigation::call_tabs(arguments, handle, default_config).await,
        "browser_storage_state" => {
            files::call_storage_state(arguments, handle, default_config).await
        }
        "browser_upload" => files::call_upload(arguments, handle, default_config).await,
        "browser_downloads" => files::call_downloads(arguments, handle).await,
        "browser_fill_form" => actions::call_fill_form(arguments, handle).await,
        "browser_navigate" => navigation::call_navigate(arguments, handle).await,
        "browser_wait_for" => navigation::call_wait_for(arguments, handle).await,
        "browser_console" => navigation::call_console(arguments, handle).await,
        "browser_network" => navigation::call_network(arguments, handle).await,
        "browser_route" => navigation::call_route(arguments, handle).await,
        "browser_contexts" => navigation::call_contexts(arguments, handle, default_config).await,
        "browser_emulate" => navigation::call_emulate(arguments, handle).await,
        "browser_pdf" => files::call_pdf(arguments, handle, default_config).await,
        other => Err(McpError::InvalidParams(format!("unknown tool '{other}'"))),
    };
    // `open` is the only tool that owns a settle; read it back from the
    // session (the handler has released the lock by now).
    let settle = if name == "browser_open" {
        let guard = handle.lock().await;
        guard.as_ref().and_then(|s| s.last_settle().cloned())
    } else {
        None
    };
    record_tool_metric(name, arguments, &result, started, settle);
    result
}

/// Payload size (chars) and actionable-element count of a tool result.
fn payload_of(result: &Result<ToolResult, McpError>) -> (usize, usize) {
    let Ok(tr) = result else {
        return (0, 0);
    };
    let mut chars = 0usize;
    let mut actions = 0usize;
    for item in &tr.content {
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            chars = chars.saturating_add(text.len());
            actions = actions.saturating_add(flashbrowse_core::observability::count_actions(text));
        }
    }
    (chars, actions)
}

/// Append one MCP tool call to the metrics trail.
fn record_tool_metric(
    name: &str,
    arguments: &Value,
    result: &Result<ToolResult, McpError>,
    started: std::time::Instant,
    settle: Option<flashbrowse_core::observability::SettleBreakdown>,
) {
    use flashbrowse_core::observability;

    let target = arguments
        .get("url")
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            arguments
                .get("action")
                .and_then(|a| a.as_str())
                .map(|a| format!("mcp:{a}"))
                .unwrap_or_else(|| format!("mcp:{name}"))
        });
    let (chars, actions) = payload_of(result);
    let (ok, error) = match result {
        Ok(tr) => (tr.is_error != Some(true), None),
        Err(e) => (false, Some(e.to_string())),
    };
    let r = observability::record_op_with_settle(
        &format!("mcp:{name}"),
        &target,
        started,
        Some((chars, actions)),
        ok,
        error,
        settle,
    );
    tracing::debug!(
        tool = name,
        ms = r.ms,
        tokens = r.tokens,
        actions = r.actions,
        ok = r.ok,
        "mcp tool call"
    );
}

#[cfg(test)]
#[path = "tools/tests.rs"]
mod tests;
