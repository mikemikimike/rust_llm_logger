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
