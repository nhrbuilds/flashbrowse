use super::url_matches;

#[cfg(unix)]
#[tokio::test]
async fn fetch_event_lag_disables_interception_for_only_its_target() {
    use super::{RouteInterceptor, RouteRule};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn read_request(reader: &mut BufReader<tokio::io::DuplexStream>) -> serde_json::Value {
        let mut frame = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.read_until(0, &mut frame),
        )
        .await
        .expect("CDP request did not arrive")
        .expect("CDP pipe read failed");
        assert_eq!(frame.pop(), Some(0));
        serde_json::from_slice(&frame).expect("valid CDP request")
    }

    async fn respond(writer: &mut tokio::io::DuplexStream, request: &serde_json::Value) {
        let response = serde_json::json!({
            "id": request["id"],
            "sessionId": request["sessionId"],
            "result": {}
        });
        writer
            .write_all(response.to_string().as_bytes())
            .await
            .expect("write CDP response");
        writer.write_all(b"\0").await.expect("terminate CDP frame");
    }

    let (client_writer, peer_reader) = tokio::io::duplex(64 * 1024);
    let (mut peer_writer, client_reader) = tokio::io::duplex(64 * 1024);
    let client = Arc::new(
        flashbrowse_cdp::CdpClient::connect_pipe(client_reader, client_writer)
            .await
            .expect("connect test CDP pipe"),
    );
    let mut peer_reader = BufReader::new(peer_reader);
    let (_stop_tx, stop_rx) = tokio::sync::watch::channel(());
    let routes = RouteInterceptor::install(&client, "session-a", stop_rx);

    let routes_for_add = routes.clone();
    let client_for_add = client.clone();
    let add_rule = tokio::spawn(async move {
        routes_for_add
            .add_rule(
                &client_for_add,
                "session-a",
                RouteRule {
                    url_pattern: "never-match".to_string(),
                    status: 200,
                    content_type: "text/plain".to_string(),
                    body: "unused".to_string(),
                    abort: false,
                },
            )
            .await
    });
    let enable = read_request(&mut peer_reader).await;
    assert_eq!(enable["method"], "Fetch.enable");
    assert_eq!(enable["sessionId"], "session-a");
    respond(&mut peer_writer, &enable).await;
    add_rule
        .await
        .expect("add route task")
        .expect("enable route rule");

    let emit_request = |id: usize| {
        client.events().emit_with_session(
            "Fetch.requestPaused",
            Some("session-a".to_string()),
            serde_json::json!({
                "requestId": format!("request-{id}"),
                "request": {"url": "https://example.test/resource"}
            }),
        );
    };

    emit_request(0);
    let first_continue = read_request(&mut peer_reader).await;
    assert_eq!(first_continue["method"], "Fetch.continueRequest");
    assert_eq!(first_continue["sessionId"], "session-a");

    // Keep the worker waiting for its first continuation response while
    // overflowing the bounded broadcast queue.
    for id in 1..=257 {
        emit_request(id);
    }
    respond(&mut peer_writer, &first_continue).await;

    let disable = read_request(&mut peer_reader).await;
    assert_eq!(disable["method"], "Fetch.disable");
    assert_eq!(disable["sessionId"], "session-a");
    respond(&mut peer_writer, &disable).await;

    assert!(routes.list_rules().is_empty());
    assert!(routes.state.lock().expect("route state lock").failed);
    let reenable = routes
        .add_rule(
            &client,
            "session-a",
            RouteRule {
                url_pattern: "never-match".to_string(),
                status: 200,
                content_type: "text/plain".to_string(),
                body: "unused".to_string(),
                abort: false,
            },
        )
        .await;
    assert!(
        matches!(
        &reenable,
            Err(flashbrowse_cdp::CdpError::ProtocolError {
                method,
                code: -1,
                message,
            }) if method == "RouteInterceptor.addRule"
                && message == "request interception was disabled after losing Fetch events; recreate the browser session before adding routes"
        ),
        "interception must stay disabled after event loss, got {reenable:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn event_worker_panic_disconnects_session() {
    use std::sync::Arc;
    let (writer, _peer_reader) = tokio::io::duplex(128);
    let (_peer_writer, reader) = tokio::io::duplex(128);
    let client = Arc::new(
        flashbrowse_cdp::CdpClient::connect_pipe(reader, writer)
            .await
            .unwrap(),
    );
    super::spawn_supervised(&client, async { panic!("test event worker panic") });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while client.is_connected() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn url_wildcard_and_substring_matching() {
    assert!(!url_matches("", "http://127.0.0.1:8080/api/user"));
    assert!(url_matches(
        "/api/user",
        "http://127.0.0.1:8080/api/user?id=1"
    ));
    assert!(url_matches("*/api/*", "http://127.0.0.1:8080/api/user"));
    assert!(!url_matches(
        "*/api/*.json",
        "http://127.0.0.1:8080/api/user.xml"
    ));
    assert!(url_matches(
        "*/api/*.json",
        "http://127.0.0.1:8080/api/user.json"
    ));
    assert!(url_matches("*a*b", "http://127.0.0.1/a_first_b_second_ab"));
}
