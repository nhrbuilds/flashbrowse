//! MCP tool definitions and execution.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use flashbrowse_core::actions::ScrollDirection;
use flashbrowse_core::{
    normalize_url, BrowserSession, EmulationOptions, GeolocationOptions, PdfOptions,
    ResourcePolicy, RouteRule, ScreenshotOptions, SessionConfig, ViewportOptions, WaitCondition,
    SCREENSHOT_PREPARE_MS_DEFAULT, SCREENSHOT_PREPARE_MS_MAX,
};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::protocol::{McpError, ToolResult};

/// Multi-context session manager sharing one Chromium process across isolated contexts.
pub struct SessionManager {
    active: String,
    sessions: HashMap<String, BrowserSession>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            active: "default".to_string(),
            sessions: HashMap::new(),
        }
    }

    pub fn active_name(&self) -> &str {
        &self.active
    }

    pub fn as_ref(&self) -> Option<&BrowserSession> {
        self.sessions.get(&self.active)
    }

    pub fn as_mut(&mut self) -> Option<&mut BrowserSession> {
        self.sessions.get_mut(&self.active)
    }

    pub fn is_none(&self) -> bool {
        self.as_ref().is_none()
    }

    pub fn session_mut_for(
        &mut self,
        context: Option<&str>,
    ) -> Result<&mut BrowserSession, McpError> {
        let name = context.unwrap_or(&self.active).to_string();
        self.sessions.get_mut(&name).ok_or_else(|| {
            if name == "default" {
                McpError::Internal("no browser session: call browser_open first".to_string())
            } else {
                McpError::InvalidParams(format!("unknown browser context '{name}'"))
            }
        })
    }

    pub async fn shutdown_all(&mut self) {
        if let Err(error) = flashbrowse_core::async_runtime::run(self.shutdown_all_inner()).await {
            tracing::warn!(%error, "browser session cleanup could not start");
        }
    }

    async fn shutdown_all_inner(&mut self) {
        // Remove every session from shared state before awaiting teardown. If
        // this future is cancelled or a session teardown fails, the manager is
        // already clean and dropping remaining sessions still triggers their
        // browser process guards.
        let mut sessions = std::mem::take(&mut self.sessions);
        self.active = "default".to_string();
        let default_session = sessions.remove("default");
        for (name, session) in sessions {
            shutdown_session(name, session).await;
        }
        if let Some(session) = default_session {
            shutdown_session("default".to_string(), session).await;
        }
    }
}

async fn shutdown_session(name: String, session: BrowserSession) {
    match flashbrowse_core::async_runtime::run(tokio::time::timeout(
        std::time::Duration::from_secs(5),
        session.shutdown(),
    ))
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            tracing::warn!(context = %name, "browser session cleanup timed out; session dropped")
        }
        Err(error) => {
            tracing::error!(context = %name, %error, "browser session cleanup panicked and was contained");
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
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
        "browser_open" => call_open(arguments, handle, default_config).await,
        "browser_act" => call_act(arguments, handle, default_config).await,
        "browser_extract_markdown" => call_markdown(handle).await,
        "browser_evaluate" => call_evaluate(arguments, handle).await,
        "browser_screenshot" => call_screenshot(arguments, handle).await,
        "browser_tabs" => call_tabs(arguments, handle, default_config).await,
        "browser_storage_state" => call_storage_state(arguments, handle, default_config).await,
        "browser_upload" => call_upload(arguments, handle, default_config).await,
        "browser_downloads" => call_downloads(arguments, handle).await,
        "browser_fill_form" => call_fill_form(arguments, handle).await,
        "browser_navigate" => call_navigate(arguments, handle).await,
        "browser_wait_for" => call_wait_for(arguments, handle).await,
        "browser_console" => call_console(arguments, handle).await,
        "browser_network" => call_network(arguments, handle).await,
        "browser_route" => call_route(arguments, handle).await,
        "browser_contexts" => call_contexts(arguments, handle, default_config).await,
        "browser_emulate" => call_emulate(arguments, handle).await,
        "browser_pdf" => call_pdf(arguments, handle, default_config).await,
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

async fn call_navigate(arguments: &Value, handle: &SessionHandle) -> Result<ToolResult, McpError> {
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

async fn call_wait_for(arguments: &Value, handle: &SessionHandle) -> Result<ToolResult, McpError> {
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

async fn call_console(arguments: &Value, handle: &SessionHandle) -> Result<ToolResult, McpError> {
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

async fn call_network(arguments: &Value, handle: &SessionHandle) -> Result<ToolResult, McpError> {
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

async fn call_route(arguments: &Value, handle: &SessionHandle) -> Result<ToolResult, McpError> {
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

async fn call_contexts(
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

async fn call_emulate(arguments: &Value, handle: &SessionHandle) -> Result<ToolResult, McpError> {
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

fn is_windows_reserved_stem(name: &str) -> bool {
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

async fn call_pdf(
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

/// MCP URL allowlist (H4 fix): agent boundary must not read local files.
/// Only `http(s)://`, `data:`, and `about:blank` are navigable via MCP.
/// CLI commands keep full `file://` support (local-user trust boundary).
///
/// Expects an already-normalized URL: `resolve_mcp_url` adds the scheme for a
/// bare host first, so the shorthand `example.com` is judged as the
/// `https://example.com` it becomes rather than rejected for having no scheme.
fn validate_mcp_url(url: &str) -> Result<(), McpError> {
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
fn resolve_mcp_url(raw: &str) -> Result<String, McpError> {
    let url = normalize_url(raw).map_err(|e| McpError::InvalidParams(e.to_string()))?;
    validate_mcp_url(&url)?;
    Ok(url)
}

async fn call_open(
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

async fn focus_new_popup(
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

async fn call_tabs(
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

fn storage_state_path(config: &SessionConfig, name: &str) -> Result<std::path::PathBuf, McpError> {
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

fn mcp_file_root(config: &SessionConfig) -> Result<std::path::PathBuf, McpError> {
    let root = config
        .mcp_file_root
        .clone()
        .unwrap_or_else(|| flashbrowse_core::paths::cache_dir().join("files"));
    std::fs::create_dir_all(&root).map_err(|error| McpError::Internal(error.to_string()))?;
    std::fs::canonicalize(root).map_err(|error| McpError::Internal(error.to_string()))
}

fn resolve_upload_files(
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

async fn call_storage_state(
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

async fn call_upload(
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

async fn call_downloads(arguments: &Value, handle: &SessionHandle) -> Result<ToolResult, McpError> {
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

async fn call_fill_form(arguments: &Value, handle: &SessionHandle) -> Result<ToolResult, McpError> {
    let raw_fields = arguments
        .get("fields")
        .and_then(|v| v.as_array())
        .ok_or_else(|| McpError::InvalidParams("missing required 'fields' array".into()))?;
    if raw_fields.is_empty() {
        return Err(McpError::InvalidParams("'fields' must not be empty".into()));
    }
    let mut fields = Vec::with_capacity(raw_fields.len());
    for item in raw_fields {
        let action_id = arg_u32(item, "action_id")?;
        let value = arg_string(item, "value")?;
        fields.push((action_id, value));
    }
    let mut guard = handle.lock().await;
    if guard.is_none() {
        return Ok(ToolResult::error(
            "no browser session: call browser_open first".to_string(),
        ));
    }
    let Some(session) = guard.as_mut() else {
        return Ok(ToolResult::error(
            "no browser session: call browser_open first".to_string(),
        ));
    };
    match session.fill_form(&fields).await {
        Ok(outcome) => match outcome.failed {
            None => Ok(ToolResult::text(outcome.tree)),
            Some((failed_id, reason)) => Ok(ToolResult::error(format!(
                "completed fields {:?}; stopped at field {failed_id}: {reason}\n\n{}",
                outcome.completed, outcome.tree
            ))),
        },
        Err(e) => Ok(ToolResult::error(e.to_string())),
    }
}

async fn call_act(
    arguments: &Value,
    handle: &SessionHandle,
    default_config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let action = arg_string(arguments, "action")?;
    let action_id_opt = if arguments.get("action_id").is_some() {
        Some(arg_u32(arguments, "action_id")?)
    } else {
        None
    };
    let require_action_id = || {
        action_id_opt
            .ok_or_else(|| McpError::InvalidParams("missing required 'action_id'".to_string()))
    };
    if matches!(
        action.as_str(),
        "click" | "double_click" | "right_click" | "drag_and_drop" | "fill" | "hover" | "select"
    ) {
        require_action_id()?;
    }
    if action == "drag_and_drop" {
        arg_u32(arguments, "target_action_id")?;
    }
    let value = arg_opt_string(arguments, "value").unwrap_or_default();

    let mut guard = handle.lock().await;
    // Reuse existing session; if none, this is an error (must open first).
    if guard.is_none() {
        // Try launching with defaults so a fresh agent can act after open-less flow?
        // Spec ordering is open→act; surface a clear error instead of silent launch.
        return Ok(ToolResult::error(
            "no browser session: call browser_open first".to_string(),
        ));
    }
    // Touch default_config so the launch path stays explicit for future use.
    let _ = default_config;
    let Some(session) = guard.as_mut() else {
        return Ok(ToolResult::error(
            "no browser session: call browser_open first".to_string(),
        ));
    };
    let opener = session.target_id().to_string();
    let existing: std::collections::HashSet<String> = session
        .list_tabs()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.target_id)
        .collect();

    if action == "fill" && arguments.get("value").is_none() {
        return Ok(ToolResult::error(
            "fill requires 'value' (text payload, may be empty to clear)".to_string(),
        ));
    }
    if action == "press_key" && value.is_empty() {
        return Ok(ToolResult::error(
            "press_key requires 'value' (key name, e.g. Enter, ArrowDown, a)".to_string(),
        ));
    }
    if action == "select" && arguments.get("value").is_none() {
        return Ok(ToolResult::error(
            "select requires 'value' (option text or value)".to_string(),
        ));
    }
    if action == "scroll" {
        if !value.is_empty() && ScrollDirection::parse(&value).is_none() {
            return Ok(ToolResult::error(format!(
                "invalid scroll direction '{value}' (expected up|down|left|right)"
            )));
        }
        let amount = arguments
            .get("amount")
            .and_then(Value::as_f64)
            .unwrap_or(300.0);
        if !amount.is_finite() || amount < 0.0 {
            return Ok(ToolResult::error(
                "scroll 'amount' must be a non-negative finite number".to_string(),
            ));
        }
    }
    let prompt_text = arg_opt_string(arguments, "prompt_text");
    if let Some(dialog_action) = arg_opt_string(arguments, "dialog_action") {
        match dialog_action.as_str() {
            "accept" => session.set_dialog_override(true, prompt_text),
            "dismiss" if prompt_text.is_none() => session.set_dialog_override(false, None),
            "dismiss" => {
                return Ok(ToolResult::error(
                    "prompt_text requires dialog_action=accept".into(),
                ))
            }
            _ => {
                return Ok(ToolResult::error(
                    "dialog_action must be accept or dismiss".into(),
                ))
            }
        }
    } else if prompt_text.is_some() {
        return Ok(ToolResult::error(
            "prompt_text requires dialog_action=accept".into(),
        ));
    }

    let result = match action.as_str() {
        "click" => session.click(require_action_id()?).await,
        "double_click" => session.double_click(require_action_id()?).await,
        "right_click" => session.right_click(require_action_id()?).await,
        "drag_and_drop" => {
            let src = require_action_id()?;
            let dst = arg_u32(arguments, "target_action_id")?;
            session.drag_and_drop(src, dst).await
        }
        "fill" => session.fill(require_action_id()?, &value).await,
        "hover" => session.hover(require_action_id()?).await,
        "press_key" => session.press_key(&value).await,
        "select" => session.select(require_action_id()?, &value).await,
        "scroll" => {
            let direction = ScrollDirection::parse(&value).unwrap_or(ScrollDirection::Down);
            let amount = arguments
                .get("amount")
                .and_then(|v| v.as_f64())
                .unwrap_or(300.0);
            session.scroll_at(action_id_opt, direction, amount).await
        }
        other => {
            return Ok(ToolResult::error(format!(
                "unknown action '{other}' (expected click|double_click|right_click|drag_and_drop|fill|hover|press_key|scroll|select)"
            )))
        }
    };
    session.clear_dialog_override();
    let dialog = session.take_dialog_info();
    match result {
        Ok(tree) => {
            let tree = if let Some(dialog) = dialog {
                format!(
                    "{tree}\n\n[handled JavaScript {} dialog: {} ({})]",
                    dialog.dialog_type,
                    if dialog.accepted {
                        "accepted"
                    } else {
                        "dismissed"
                    },
                    dialog.message
                )
            } else {
                tree
            };
            let mut config = default_config.clone();
            config.resource_policy = session.resource_policy();
            match focus_new_popup(session, &opener, &existing, &config).await {
                Ok(Some(popup_tree)) => Ok(ToolResult::text(popup_tree)),
                Ok(None) => Ok(ToolResult::text(tree)),
                Err(e) => Ok(ToolResult::error(e.to_string())),
            }
        }
        Err(e) => Ok(ToolResult::error(e.to_string())),
    }
}

async fn call_evaluate(arguments: &Value, handle: &SessionHandle) -> Result<ToolResult, McpError> {
    let expression = arg_string(arguments, "expression")?;
    if expression.is_empty() {
        return Err(McpError::InvalidParams(
            "expression must not be empty".to_string(),
        ));
    }
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    match session.evaluate(&expression).await {
        Ok(value) => Ok(ToolResult::text(value.to_string())),
        Err(e) => Ok(ToolResult::error(e.to_string())),
    }
}

async fn call_markdown(handle: &SessionHandle) -> Result<ToolResult, McpError> {
    let guard = handle.lock().await;
    match guard.as_ref() {
        None => Ok(ToolResult::error(
            "no browser session: call browser_open first".to_string(),
        )),
        Some(s) => match s.extract_markdown().await {
            Ok(md) => Ok(ToolResult::text(md)),
            Err(e) => Ok(ToolResult::error(e.to_string())),
        },
    }
}

fn arg_u64(args: &Value, key: &str, default: u64) -> u64 {
    args.get(key)
        .and_then(|v| v.as_u64())
        .unwrap_or(default)
        .clamp(500, SCREENSHOT_PREPARE_MS_MAX)
}

async fn call_screenshot(
    arguments: &Value,
    handle: &SessionHandle,
) -> Result<ToolResult, McpError> {
    let action_id = if arguments.get("action_id").is_some() {
        Some(arg_u32(arguments, "action_id")?)
    } else {
        None
    };
    let opts = ScreenshotOptions {
        full_page: arg_bool(arguments, "full_page"),
        prepare_lazy_content: arguments
            .get("prepare_lazy_content")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        timeout_ms: arg_u64(arguments, "timeout_ms", SCREENSHOT_PREPARE_MS_DEFAULT),
        action_id,
    };
    let guard = handle.lock().await;
    match guard.as_ref() {
        None => Ok(ToolResult::error(
            "no browser session: call browser_open first".to_string(),
        )),
        Some(s) => match s.screenshot(opts).await {
            Ok(data) => Ok(ToolResult::image(data)),
            Err(e) => Ok(ToolResult::error(e.to_string())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_tool_arguments_are_rejected_before_execution() {
        for (name, value) in [
            (
                "browser_open",
                serde_json::json!({"url":"https://example.com", "text_only":"yes"}),
            ),
            (
                "browser_fill_form",
                serde_json::json!({"fields":[{"action_id":1,"value":7}]}),
            ),
            (
                "browser_emulate",
                serde_json::json!({"reset":true,"viewport":{"width":1}}),
            ),
            (
                "browser_network",
                serde_json::json!({"action":"set_headers","headers":{"X-Test":false}}),
            ),
            (
                "browser_screenshot",
                serde_json::json!({"timeout_ms": "slow"}),
            ),
            ("browser_screenshot", serde_json::json!({"timeout_ms": -1})),
            (
                "browser_act",
                serde_json::json!({"action": "click", "action_id": 4294967296u64}),
            ),
        ] {
            assert!(
                matches!(
                    validate_tool_arguments(name, &value),
                    Err(McpError::InvalidParams(_))
                ),
                "{name}: {value}"
            );
        }
        assert!(validate_tool_arguments(
            "browser_open",
            &serde_json::json!({"url":"https://example.com"})
        )
        .is_ok());
    }

    #[test]
    fn each_declared_tool_property_rejects_a_wrong_type() {
        for definition in tool_definitions() {
            let name = definition["name"].as_str().unwrap();
            let Some(properties) = definition["inputSchema"]["properties"].as_object() else {
                continue;
            };
            for (key, schema) in properties {
                let kind = schema["type"].as_str().unwrap();
                let bad_value = if kind == "string" {
                    serde_json::json!(false)
                } else {
                    serde_json::json!("wrong type")
                };
                assert!(
                    validate_value(schema, &bad_value, key).is_err(),
                    "{name}.{key} accepted {bad_value}"
                );
            }
        }
    }

    #[tokio::test]
    async fn invalid_arguments_do_not_create_browser_session() {
        let handle: SessionHandle = Arc::new(Mutex::new(SessionManager::new()));
        let result = call_tool(
            "browser_open",
            &serde_json::json!({"url": "https://example.com", "text_only": "yes"}),
            &handle,
            &SessionConfig::default(),
        )
        .await;
        assert!(matches!(result, Err(McpError::InvalidParams(_))));
        assert!(handle.lock().await.is_none());
    }

    #[test]
    fn bare_host_is_resolved_then_allowed() {
        assert_eq!(
            resolve_mcp_url("example.com").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            resolve_mcp_url("localhost:8099").unwrap(),
            "http://localhost:8099"
        );
    }

    #[test]
    fn file_urls_are_still_rejected() {
        // The allowlist runs on the normalized value, so an explicit file
        // scheme survives normalization and is refused here.
        assert!(resolve_mcp_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn paths_and_schemes_without_a_host_are_rejected() {
        // A bare path never becomes a URL, so it cannot slip through as
        // `https://` + path.
        assert!(resolve_mcp_url("/etc/passwd").is_err());
        assert!(resolve_mcp_url("../secret").is_err());
        assert!(resolve_mcp_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn data_and_about_blank_still_work() {
        assert_eq!(
            resolve_mcp_url("data:text/html,<b>x</b>").unwrap(),
            "data:text/html,<b>x</b>"
        );
        assert_eq!(resolve_mcp_url("about:blank").unwrap(), "about:blank");
    }

    #[test]
    fn allowlist_still_rejects_other_schemes() {
        // A scheme the allowlist does not know, with a host, is refused.
        assert!(resolve_mcp_url("ftp://example.com").is_err());
    }

    #[test]
    fn storage_state_names_cannot_escape_root() {
        let config = SessionConfig {
            storage_state_root: Some(std::env::temp_dir().join("flashbrowse-storage-test")),
            ..Default::default()
        };
        assert!(storage_state_path(&config, "../outside").is_err());
        assert!(storage_state_path(&config, "nested/name").is_err());
        assert!(storage_state_path(&config, "CON").is_err());
        assert!(storage_state_path(&config, "nul").is_err());
        assert!(storage_state_path(&config, "com1").is_err());
        assert!(storage_state_path(&config, "valid-state").is_ok());
    }

    #[test]
    fn windows_reserved_stems_are_rejected_case_insensitively() {
        for reserved in [
            "CON", "con", "PRN", "aux", "NUL", "com1", "COM9", "LPT1", "lpt9", "CON.pdf",
        ] {
            assert!(
                is_windows_reserved_stem(reserved),
                "{reserved} must be rejected"
            );
        }
        for allowed in ["report", "conifer", "com10", "lpt0", "nul_file"] {
            assert!(
                !is_windows_reserved_stem(allowed),
                "{allowed} must be allowed"
            );
        }
    }

    #[test]
    fn upload_paths_are_confined_to_the_configured_root() {
        let root =
            std::env::temp_dir().join(format!("flashbrowse-upload-root-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("inside.txt"), "x").unwrap();
        let config = SessionConfig {
            mcp_file_root: Some(root.clone()),
            ..Default::default()
        };
        assert!(resolve_upload_files(&config, &[Value::String("inside.txt".into())]).is_ok());
        assert!(resolve_upload_files(&config, &[Value::String("../outside.txt".into())]).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
