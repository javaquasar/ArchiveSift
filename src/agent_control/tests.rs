use super::*;
use std::io::{BufReader, Cursor};

#[test]
fn active_requests_are_bounded_and_revocation_cancels_registered_queries() {
    let root = tempfile::tempdir().unwrap();
    let queue = Arc::new(UrlQueueService::with_registry_and_queue_path(
        crate::scraper::ScraperRegistry::with_public_scrapers(),
        root.path().join("queue.sqlite3"),
    ));
    let (service, secret) = AgentService::new(
        AppConfig::default(),
        queue,
        UiLog::default(),
        Arc::new(|| {}),
    )
    .unwrap();
    service.activate(false);
    let mut tokens = Vec::new();
    for id in 0..MAX_CONNECTIONS {
        let token = QueryCancellation::default();
        service.active.lock().unwrap().insert(
            id as u64,
            (token.clone(), Instant::now() - Duration::from_secs(1)),
        );
        tokens.push(token);
    }
    let request = Request {
        protocol_version: PROTOCOL_VERSION,
        instance_id: service.instance_id.clone(),
        token: secret.to_string(),
        tool: "archivesift_status".into(),
        arguments: json!({}),
    };
    assert_eq!(service.handle(&request).error, Some(ErrorCode::Busy));
    service.expire_queries();
    assert!(tokens.iter().all(QueryCancellation::is_cancelled));
    let token = QueryCancellation::default();
    service.active.lock().unwrap().insert(
        100,
        (token.clone(), Instant::now() + Duration::from_secs(10)),
    );
    service.disable();
    assert!(token.is_cancelled());
}

#[test]
fn framing_and_output_budgets_fail_without_returning_unbounded_content() {
    let bytes = vec![b'x'; wire::MAX_REQUEST_BYTES * 4];
    let mut reader = BufReader::new(Cursor::new(bytes));
    assert!(matches!(
        transport::read_frame(&mut reader, wire::MAX_REQUEST_BYTES, true),
        Err(ErrorCode::TooLarge)
    ));
    let response = Response::success(
        "example",
        json!({"data":"x".repeat(wire::MAX_RESPONSE_BYTES*2)}),
    );
    let encoded = wire::encode(&response);
    assert!(encoded.len() < wire::MAX_RESPONSE_BYTES);
    let response: Response = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(response.error, Some(ErrorCode::TooLarge));
    assert!(matches!(
        transport::read_frame(&mut BufReader::new(Cursor::new(b"{}")), 100, true),
        Err(ErrorCode::InvalidArguments)
    ));
}

#[test]
fn public_url_projection_strips_credentials_but_preserves_resource_identifiers() {
    assert_eq!(safe_url("https://user:test-password@example.test/topic?t=42&api_key=test-key&session=test-session&X-Amz-Credential=test-credential#fragment"),"https://example.test/topic?t=42");
    assert_eq!(safe_url("file:///private/file"), "");
    assert!(clip(&"К".repeat(100), 33).len() <= 33);
}
