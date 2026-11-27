//! `Runtime` domain types (evaluate, callFunctionOn).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::CdpError;
use crate::transport::CdpClient;

/// `Runtime.enable` - required for `Runtime.consoleAPICalled` and `Runtime.exceptionThrown`.
pub async fn enable(client: &CdpClient, session_id: &str) -> Result<(), CdpError> {
    let _: Value = client
        .send(Some(session_id), "Runtime.enable", serde_json::json!({}))
        .await?;
    Ok(())
}

/// Params for `Runtime.evaluate`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateParams {
    pub expression: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub await_promise: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_by_value: Option<bool>,
}

/// Params for `Runtime.callFunctionOn`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallFunctionOnParams {
    pub function_declaration: String,
    pub object_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_by_value: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub await_promise: Option<bool>,
}

/// Runtime value wrapper.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeValue {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Response of `Runtime.evaluate`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateResponse {
    pub result: RuntimeValue,
    #[serde(default)]
    pub exception_details: Option<Value>,
}

/// Response of `Runtime.callFunctionOn`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallFunctionOnResponse {
    pub result: RuntimeValue,
    #[serde(default)]
    pub exception_details: Option<Value>,
}

/// `Runtime.evaluate` with `returnByValue: true` (markdown extraction, URL/title).
pub async fn evaluate(
    client: &CdpClient,
    session_id: &str,
    expression: &str,
) -> Result<EvaluateResponse, CdpError> {
    client
        .send_typed(
            Some(session_id),
            "Runtime.evaluate",
            &EvaluateParams {
                expression: expression.to_string(),
                await_promise: Some(false),
                return_by_value: Some(true),
            },
        )
        .await
}

/// `Runtime.callFunctionOn` - run a function against a resolved JS object.
pub fn call_function_on<'a>(
    client: &'a CdpClient,
    session_id: &'a str,
    object_id: &'a str,
    function_declaration: &'a str,
    args: Vec<Value>,
) -> impl std::future::Future<Output = Result<CallFunctionOnResponse, CdpError>> + 'a {
    let args = if let Some(error) = args
        .iter()
        .find_map(|value| crate::transport::validate_json_depth(value).err())
    {
        for value in args {
            crate::transport::drop_json_iteratively(value);
        }
        Err(error)
    } else {
        Ok(args)
    };
    async move {
        client
            .send_typed(
                Some(session_id),
                "Runtime.callFunctionOn",
                &CallFunctionOnParams {
                    function_declaration: function_declaration.to_string(),
                    object_id: object_id.to_string(),
                    arguments: Some(args?),
                    return_by_value: Some(true),
                    await_promise: Some(false),
                },
            )
            .await
    }
}

#[cfg(all(test, unix))]
mod tests {
    #[tokio::test]
    async fn deeply_nested_function_arguments_are_rejected_and_dropped_safely() {
        let (writer, _peer_reader) = tokio::io::duplex(128);
        let (_peer_writer, reader) = tokio::io::duplex(128);
        let client = crate::CdpClient::connect_pipe(reader, writer)
            .await
            .unwrap();
        let value = (0..10_000).fold(serde_json::Value::Null, |value, _| {
            serde_json::Value::Array(vec![value])
        });
        let result =
            super::call_function_on(&client, "session", "object", "() => null", vec![value]).await;
        assert!(result.is_err());
        let value = (0..10_000).fold(serde_json::Value::Null, |value, _| {
            serde_json::Value::Array(vec![value])
        });
        drop(super::call_function_on(
            &client,
            "session",
            "object",
            "() => null",
            vec![value],
        ));
    }
}
