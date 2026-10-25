use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    routing::any,
    Router,
};
use http_body_util::BodyExt;
use rust_llm_logger::{
    app, auth::AccessConfig, pricing::Pricing, store::Store, telemetry::Telemetry,
};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(router: Router) -> (u16, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (port, Server(task))
}

async fn backend(
    State(received): State<Arc<Mutex<Vec<HeaderMap>>>>,
    req: Request,
) -> axum::Json<serde_json::Value> {
    received.lock().unwrap().push(req.headers().clone());
    axum::Json(serde_json::json!({"done":true,"prompt_eval_count":11,"eval_count":22}))
}

async fn send(
    client: &app::HttpClient,
    port: u16,
    path: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> (StatusCode, HeaderMap, String) {
    let mut builder = Request::builder().uri(format!("http://127.0.0.1:{port}{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client.request(builder.body(body).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn protects_telemetry_and_proxy_without_forwarding_logger_credentials() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let (upstream, _backend) = serve(
        Router::new()
            .route("/api/generate", any(backend))
            .with_state(Arc::clone(&received)),
    )
    .await;
    let client = app::create_http_client();
    let dir = tempfile::tempdir().unwrap();
    for persistent in [false, true] {
        for show_prompts in [false, true] {
            let store = Arc::new(
                Store::open(dir.path().join(format!("{persistent}-{show_prompts}.db"))).unwrap(),
            );
            let telemetry = Arc::new(Telemetry::new(
                Pricing::empty(),
                persistent.then(|| Arc::clone(&store)),
            ));
            let access =
                AccessConfig::new(Some("logger-secret".into()), true, show_prompts).unwrap();
            let (proxy, _proxy) = serve(app::build_app_with_access(
                Arc::new(HashSet::from([upstream])),
                Arc::clone(&telemetry),
                access,
            ))
            .await;
            for path in [
                "/",
                "/metrics",
                "/?limit=1000",
                "/?limit=invalid",
                "/?token=logger-secret",
            ] {
                for headers in [
                    vec![],
                    vec![("authorization", "Bearer wrong")],
                    vec![("authorization", "Basic logger-secret")],
                    vec![
                        ("authorization", "Bearer logger-secret"),
                        ("authorization", "Bearer wrong"),
                    ],
                ] {
                    let (status, response_headers, body) =
                        send(&client, proxy, path, &headers, Body::empty()).await;
                    assert_eq!(status, 401, "{path}");
                    assert_eq!(
                        response_headers["www-authenticate"],
                        "Bearer realm=\"llm-logger\""
                    );
                    assert_eq!(response_headers["cache-control"], "no-store");
                    assert!(!body.contains("logger-secret"));
                }
            }
            for path in ["/", "/metrics"] {
                for scheme in ["Bearer", "bearer"] {
                    let auth = format!("{scheme} logger-secret");
                    let (status, headers, _) = send(
                        &client,
                        proxy,
                        path,
                        &[("authorization", &auth)],
                        Body::empty(),
                    )
                    .await;
                    assert_eq!(status, 200);
                    assert_eq!(headers["cache-control"], "no-store");
                }
            }
            let path = format!("/proxy/{upstream}/api/generate");
            let before = received.lock().unwrap().len();
            for headers in [
                vec![("authorization", "Bearer upstream-secret")],
                vec![("authorization", "Bearer logger-secret")],
                vec![("x-llm-logger-token", "wrong")],
                vec![
                    ("x-llm-logger-token", "logger-secret"),
                    ("x-llm-logger-token", "wrong"),
                ],
            ] {
                // An unending upload proves auth runs before collecting the body.
                let pending = futures::stream::pending::<
                    Result<hyper::body::Frame<bytes::Bytes>, std::io::Error>,
                >();
                let (status, _, _) = send(
                    &client,
                    proxy,
                    &path,
                    &headers,
                    Body::new(http_body_util::StreamBody::new(pending)),
                )
                .await;
                assert_eq!(status, 401);
            }
            assert_eq!(received.lock().unwrap().len(), before);
            assert!(telemetry.recent(10, None).is_empty());
            let forbidden_port = if upstream == 65535 { 65534 } else { 65535 };
            let (status, _, _) = send(
                &client,
                proxy,
                &format!("/proxy/{forbidden_port}/api/generate"),
                &[("x-llm-logger-token", "logger-secret")],
                Body::empty(),
            )
            .await;
            assert_eq!(
                status, 403,
                "authentication must retain the backend allow-list"
            );
            let (status, _, _) = send(
                &client,
                proxy,
                &path,
                &[
                    ("authorization", "Bearer upstream-secret"),
                    ("x-llm-logger-token", "logger-secret"),
                    ("content-type", "application/json"),
                ],
                Body::from(r#"{"model":"private-model","prompt":"owner-only prompt"}"#),
            )
            .await;
            assert_eq!(status, 200);
            tokio::time::timeout(Duration::from_secs(5), async {
                while telemetry.recent(10, None).is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let captured = received.lock().unwrap().last().unwrap().clone();
            assert_eq!(captured["authorization"], "Bearer upstream-secret");
            assert!(!captured.contains_key("x-llm-logger-token"));
            assert_eq!(telemetry.recent(1, None)[0].prompt, "owner-only prompt");
            if persistent {
                let headers = store.latest_headers().unwrap().unwrap();
                assert!(!headers.contains("logger-secret") && !headers.contains("upstream-secret"));
            }
            let (status, _, html) = send(
                &client,
                proxy,
                "/",
                &[("authorization", "Bearer logger-secret")],
                Body::empty(),
            )
            .await;
            assert_eq!(status, 200);
            assert_eq!(html.contains("owner-only prompt"), show_prompts);
            assert!(html.contains("private-model") && html.contains("11") && html.contains("22"));
            assert!(!html.contains("logger-secret"));
        }
    }
}

#[tokio::test]
async fn proxy_auth_is_optional_and_logger_header_is_always_removed() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let (upstream, _backend) = serve(
        Router::new()
            .route("/api/generate", any(backend))
            .with_state(Arc::clone(&received)),
    )
    .await;
    let client = app::create_http_client();
    for token in [None, Some("logger-secret".into())] {
        let telemetry = Arc::new(Telemetry::new(Pricing::empty(), None));
        let access = AccessConfig::new(token, false, true).unwrap();
        let (proxy, _proxy) = serve(app::build_app_with_access(
            Arc::new(HashSet::from([upstream])),
            telemetry,
            access,
        ))
        .await;
        let (status, _, _) = send(
            &client,
            proxy,
            &format!("/proxy/{upstream}/api/generate"),
            &[
                ("authorization", "Bearer upstream-secret"),
                ("x-llm-logger-token", "discard-this"),
            ],
            Body::empty(),
        )
        .await;
        assert_eq!(status, 200);
        let headers = received.lock().unwrap().last().unwrap().clone();
        assert_eq!(headers["authorization"], "Bearer upstream-secret");
        assert!(!headers.contains_key("x-llm-logger-token"));
    }
}
