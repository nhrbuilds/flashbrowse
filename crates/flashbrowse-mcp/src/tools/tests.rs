use super::*;
use std::sync::Arc;
use tokio::sync::Mutex;

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
    let root = std::env::temp_dir().join(format!("flashbrowse-upload-root-{}", std::process::id()));
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
