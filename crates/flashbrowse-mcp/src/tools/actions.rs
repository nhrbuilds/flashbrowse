use super::navigation::focus_new_popup;
use super::*;

pub(super) async fn call_fill_form(
    arguments: &Value,
    handle: &SessionHandle,
) -> Result<ToolResult, McpError> {
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

pub(super) async fn call_act(
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

pub(super) async fn call_evaluate(
    arguments: &Value,
    handle: &SessionHandle,
) -> Result<ToolResult, McpError> {
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

pub(super) async fn call_markdown(handle: &SessionHandle) -> Result<ToolResult, McpError> {
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

pub(super) async fn call_screenshot(
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
