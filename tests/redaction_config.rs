use axum::{body::Body, http::Request, routing::post, Router};
use http_body_util::BodyExt;
use rust_llm_logger::{app, store::Store};
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};

struct ProxyProcess(Child);
impl Drop for ProxyProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn cli_and_environment_patterns_reach_the_proxy() {
    let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = backend.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        axum::serve(
            backend,
            Router::new().route(
                "/api/generate",
                post(|| async { r#"{"done":true,"prompt_eval_count":1,"eval_count":1}"# }),
            ),
        )
        .await
        .unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let client = app::create_http_client();
    for env_pattern in [false, true] {
        let db = dir.path().join(format!("{env_pattern}.db"));
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        drop(socket);
        let mut command = Command::new(env!("CARGO_BIN_EXE_rust_llm_logger"));
        command
            .args([
                "--bind",
                &address.to_string(),
                "--allowed-backend-ports",
                &upstream.to_string(),
                "--metrics-db",
            ])
            .arg(&db)
            .env("RUST_LOG", "warn")
            .env_remove("LLM_LOGGER_REDACT_PATTERN")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if env_pattern {
            command.env(
                "LLM_LOGGER_REDACT_PATTERN",
                "internal-[A-Z0-9]{8,12}|密钥内容.*",
            );
        } else {
            command.args([
                "--redact-pattern",
                "internal-[A-Z0-9]{8,12}",
                "--redact-pattern",
                "密钥内容.*",
            ]);
        }
        let mut child = ProxyProcess(command.spawn().unwrap());
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if tokio::net::TcpStream::connect(address).await.is_ok() {
                    break;
                }
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "proxy exited during startup"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        for prompt in ["internal-AB12CD34", "密钥内容12345"] {
            let response = client
                .request(
                    Request::post(format!("http://{address}/proxy/{upstream}/api/generate"))
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({"model":"m", "prompt":prompt}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let _ = response.into_body().collect().await.unwrap();
        }
        let store = Store::open(&db).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.count().unwrap() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        for row in store.recent(2, None).unwrap() {
            assert_eq!(row.prompt, "[REDACTED]");
        }
        drop(child);
    }
    task.abort();
    let _ = task.await;
}

#[test]
fn invalid_patterns_fail_at_startup() {
    for use_env in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rust_llm_logger"));
        command.env_remove("LLM_LOGGER_REDACT_PATTERN");
        if use_env {
            command.env("LLM_LOGGER_REDACT_PATTERN", "[");
        } else {
            command.args(["--redact-pattern", "["]);
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("config error:"));
    }
}
