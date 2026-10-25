//! Real proxy -> SQLite/ring/dashboard/logs, with unchanged upstream prompts.
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    routing::post,
    Router,
};
use http_body_util::BodyExt;
use rust_llm_logger::{app, pricing::Pricing, store::Store, telemetry::Telemetry};
use std::{
    collections::HashSet,
    io::{self, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn serve(router: Router) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (
        port,
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() }),
    )
}

async fn backend(State(received): State<Arc<Mutex<Vec<String>>>>, request: Request) -> String {
    let bytes = to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let text = value
        .get("prompt")
        .and_then(|p| p.as_str())
        .unwrap_or_else(|| value["messages"][0]["content"].as_str().unwrap());
    received.lock().unwrap().push(text.to_string());
    r#"{"done":true,"prompt_eval_count":11,"eval_count":22}"#.into()
}

#[tokio::test]
async fn credentials_never_reach_storage_dashboard_or_logs() {
    let logs = LogCapture::default();
    let writer = logs.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    )
    .unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let (upstream, upstream_task) = serve(
        Router::new()
            .route("/api/generate", post(backend))
            .route("/v1/chat/completions", post(backend))
            .with_state(Arc::clone(&received)),
    )
    .await;
    let samples = [
        "sk_live_51Hxabcdefgh",
        "rk_test_51Hxabcdefgh",
        "AKIAIOSFODNN7EXAMPLE",
        "ghp_123456789012345678901234567890",
        "github_pat_123456789012345678901234567890",
        "xoxp-1234567890",
        "xoxa-1234567890",
        "xapp-1234567890",
        "eyJhbGciOiJIUzI1NiJ9.payload.signature",
        "-----BEGIN PRIVATE KEY-----\nPEMcontents12345\n-----END PRIVATE KEY-----",
        "password=super-secret",
        "api_key: another-secret",
    ];
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("calls.db");
    let store = Arc::new(Store::open(&database).unwrap());
    let client = app::create_http_client();
    for persistent in [true, false] {
        let telemetry = Arc::new(Telemetry::new(
            Pricing::empty(),
            persistent.then(|| Arc::clone(&store)),
        ));
        let (proxy, proxy_task) = serve(app::build_app(
            Arc::new(HashSet::from([upstream])),
            Arc::clone(&telemetry),
        ))
        .await;
        let mut count = 0;
        for path in ["/api/generate", "/v1/chat/completions"] {
            for secret in samples {
                let prompt = format!("{secret} task-list skeleton");
                let body = if path == "/api/generate" {
                    serde_json::json!({"model":"m", "prompt":prompt})
                } else {
                    serde_json::json!({"model":"m", "messages":[{"role":"user", "content":prompt}]})
                };
                let response = client
                    .request(
                        axum::http::Request::post(format!(
                            "http://127.0.0.1:{proxy}/proxy/{upstream}{path}"
                        ))
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), 200);
                let _ = response.into_body().collect().await.unwrap();
                count += 1;
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if telemetry.recent(100, None).len() == count {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                let row = telemetry.recent(1, None).remove(0);
                assert!(!row.prompt.contains(secret), "leaked {secret}");
                assert!(row.prompt.contains("[REDACTED]"), "{}", row.prompt);
                assert!(row.prompt.contains("task-list skeleton"));
                let html = client
                    .request(
                        axum::http::Request::get(format!("http://127.0.0.1:{proxy}/"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes();
                let html = String::from_utf8(html.to_vec()).unwrap();
                assert!(!html.contains(secret));
                assert!(html.contains("[REDACTED]"));
                assert_eq!(
                    received.lock().unwrap().last().unwrap(),
                    &prompt,
                    "upstream must receive original prompt"
                );
            }
        }
        proxy_task.abort();
        let _ = proxy_task.await;
    }
    upstream_task.abort();
    let _ = upstream_task.await;
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("Metrics:"));
    for secret in samples {
        assert!(!logs.contains(secret), "secret leaked to logs: {secret}");
    }
    drop(store);
    let reopened = Store::open(&database).unwrap();
    assert_eq!(reopened.count().unwrap(), 24);
    for row in reopened.recent(100, None).unwrap() {
        assert!(row.prompt.contains("[REDACTED]"));
    }
}
