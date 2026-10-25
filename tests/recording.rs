use axum::{body::Body, http::Request, routing::any, Router};
use http_body_util::BodyExt;
use rust_llm_logger::{
    app,
    pricing::Pricing,
    prom::PromMetrics,
    store::Store,
    telemetry::Telemetry,
    types::{CallOutcome, LLMMetrics},
};
use std::{collections::HashSet, sync::Arc, time::Duration};

async fn spawn(router: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    port
}

async fn echo(req: Request<Body>) -> Body {
    req.into_body()
}

#[tokio::test]
async fn passthrough_requests_are_forwarded_without_recording() {
    let upstream = spawn(Router::new().route("/*path", any(echo))).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("calls.db")).unwrap());
    let telemetry = Arc::new(Telemetry::new(Pricing::empty(), Some(Arc::clone(&store))));
    let proxy = spawn(app::build_app(
        Arc::new(HashSet::from([upstream])),
        Arc::clone(&telemetry),
    ))
    .await;
    let client = app::create_http_client();
    for (method, path) in [
        ("GET", "/api/tags"),
        ("GET", "/api/version"),
        ("GET", "/v1/models"),
        ("GET", "/health"),
        ("GET", "/api/generate"),
        ("PUT", "/v1/chat/completions"),
        ("POST", "/api/pull"),
        ("POST", "/nested/v1/chat/completions"),
    ] {
        let body = format!("unparsed body for {method} {path}");
        let response = client
            .request(
                Request::builder()
                    .method(method)
                    .uri(format!(
                        "http://127.0.0.1:{proxy}/proxy/{upstream}{path}?q=1"
                    ))
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{method} {path}");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            body
        );
    }
    // A subsequent inference call marks completion of the recording path.
    let response = client
        .request(
            Request::post(format!(
                "http://127.0.0.1:{proxy}/proxy/{upstream}/api/generate"
            ))
            .body(Body::from(r#"{"model":"control","prompt":"hello"}"#))
            .unwrap(),
        )
        .await
        .unwrap();
    response.into_body().collect().await.unwrap();
    for _ in 0..100 {
        if !store.recent(1, Some("control")).unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        store.count().unwrap(),
        1,
        "only the inference call is stored"
    );
    let metrics = telemetry.prom.render();
    assert!(
        metrics.contains("llm_request_latency_ms_count 1"),
        "{metrics}"
    );
    assert!(!metrics.contains("model=\"unknown\""), "{metrics}");
    assert_eq!(telemetry.recent(100, None).len(), 1);
}

#[tokio::test]
async fn every_inference_endpoint_preserves_usage_cost_and_original_model() {
    let upstream = spawn(Router::new().route(
        "/*path",
        any(|| async {
            axum::Json(serde_json::json!({"usage":{"prompt_tokens":11,"completion_tokens":22}}))
        }),
    ))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("calls.db")).unwrap());
    let mut pricing = Pricing::empty();
    pricing
        .merge_json(r#"{"acme":{"prompt_per_1k":1.0,"completion_per_1k":2.0}}"#)
        .unwrap();
    let mut telemetry = Telemetry::new(pricing, Some(Arc::clone(&store)));
    telemetry.prom = PromMetrics::with_model_limit(0);
    let telemetry = Arc::new(telemetry);
    let proxy = spawn(app::build_app(
        Arc::new(HashSet::from([upstream])),
        Arc::clone(&telemetry),
    ))
    .await;
    let client = app::create_http_client();
    for path in [
        "/api/generate",
        "/api/chat/",
        "/v1/chat/completions",
        "/v1/completions",
        "/v1/messages",
        "/v1/embeddings",
    ] {
        let response = client
            .request(
                Request::post(format!(
                    "http://127.0.0.1:{proxy}/proxy/{upstream}{path}?q=1"
                ))
                .body(Body::from(r#"{"model":"ACME","prompt":"hello"}"#))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["usage"]["prompt_tokens"], 11);
    }
    for _ in 0..100 {
        if store.count().unwrap() == 6 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(store.count().unwrap(), 6);
    for row in store.recent(6, None).unwrap() {
        assert_eq!(row.model, "ACME");
        assert_eq!(row.prompt_tokens, Some(11));
        assert_eq!(row.completion_tokens, Some(22));
        assert_eq!(row.estimated_cost_usd, Some(0.055));
    }
    assert!(telemetry
        .prom
        .render()
        .contains(r#"llm_requests_total{model="other",outcome="ok"} 6"#));
}

#[tokio::test]
async fn passthrough_bodies_are_not_subject_to_inference_collection_limit() {
    let upstream = spawn(Router::new().route("/*path", any(echo))).await;
    let telemetry = Arc::new(Telemetry::new(Pricing::empty(), None));
    let proxy = spawn(app::build_app(
        Arc::new(HashSet::from([upstream])),
        Arc::clone(&telemetry),
    ))
    .await;
    let client = app::create_http_client();
    let bytes = vec![b'x'; rust_llm_logger::middleware::MAX_REQUEST_BODY_BYTES + 1];
    for (path, status) in [("/api/pull", 200), ("/api/generate", 413)] {
        let response = client
            .request(
                Request::post(format!("http://127.0.0.1:{proxy}/proxy/{upstream}{path}"))
                    .body(Body::from(bytes.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        if status == 200 {
            assert_eq!(body.as_ref(), bytes.as_slice());
        }
    }
    assert!(telemetry.recent(1, None).is_empty());
    assert!(telemetry
        .prom
        .render()
        .contains("llm_request_latency_ms_count 0"));
}

#[tokio::test]
async fn passthrough_upload_is_streamed_before_body_completion() {
    let upstream = spawn(Router::new().route(
        "/*path",
        any(|req: Request<Body>| async {
            let mut body = req.into_body();
            let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
            Body::from(frame)
        }),
    ))
    .await;
    let telemetry = Arc::new(Telemetry::new(Pricing::empty(), None));
    let proxy = spawn(app::build_app(
        Arc::new(HashSet::from([upstream])),
        Arc::clone(&telemetry),
    ))
    .await;
    let client = app::create_http_client();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(1);
    tx.send(Ok(bytes::Bytes::from_static(b"first chunk")))
        .await
        .unwrap();
    let request = Request::post(format!(
        "http://127.0.0.1:{proxy}/proxy/{upstream}/api/pull"
    ))
    .body(Body::from_stream(
        tokio_stream::wrappers::ReceiverStream::new(rx),
    ))
    .unwrap();
    // Keeping tx open prevents EOF. Buffered middleware cannot reach upstream.
    let response = tokio::time::timeout(Duration::from_secs(2), client.request(request))
        .await
        .expect("pass-through upload must reach upstream before EOF")
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "first chunk"
    );
    drop(tx);
    assert!(telemetry.recent(1, None).is_empty());
    assert!(telemetry
        .prom
        .render()
        .contains("llm_request_latency_ms_count 0"));
}

fn metric(model: String) -> LLMMetrics {
    LLMMetrics {
        model,
        prompt: "hello".into(),
        prompt_tokens: Some(1),
        completion_tokens: Some(2),
        latency_ms: 1,
        ttft_ms: Some(1),
        status: 200,
        outcome: CallOutcome::Ok,
        timestamp: String::new(),
        estimated_cost_usd: Some(0.5),
        trace_id: None,
    }
}

#[test]
fn arbitrary_models_cannot_grow_prometheus_series_without_bound() {
    let prom = PromMetrics::new();
    for i in 0..10_000 {
        prom.observe(&metric(format!("random-model-{i}")));
    }
    let rendered = prom.render();
    for name in [
        "llm_requests_total",
        "llm_prompt_tokens_total",
        "llm_completion_tokens_total",
        "llm_estimated_cost_usd_total",
    ] {
        let prefix = format!("{name}{{");
        let lines: Vec<_> = rendered
            .lines()
            .filter(|l| l.starts_with(&prefix))
            .collect();
        assert!(
            lines.len() <= 101,
            "{name} has {} model series",
            lines.len()
        );
        let total: f64 = lines
            .iter()
            .map(|l| l.rsplit_once(' ').unwrap().1.parse::<f64>().unwrap())
            .sum();
        let expected = match name {
            "llm_completion_tokens_total" => 20_000.0,
            "llm_estimated_cost_usd_total" => 5_000.0,
            _ => 10_000.0,
        };
        assert_eq!(total, expected, "{name} must retain overflow totals");
    }
    assert!(rendered.contains("llm_request_latency_ms_count 10000"));
}
