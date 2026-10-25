use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rust_llm_logger"));
    command
        .env_remove("LLM_LOGGER_TOKEN")
        .env_remove("LLM_LOGGER_AUTH_PROXY")
        .env("METRICS_DB", "")
        .env("RUST_LOG", "warn");
    command
}

fn expect_config_error(args: &[&str]) {
    let mut child = Process(
        command()
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "configuration was accepted: {args:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(2));
    let mut stderr = String::new();
    std::io::Read::read_to_string(child.0.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(stderr.contains("config error:"), "{stderr}");
}

#[test]
fn public_listener_without_token_exits_with_config_error() {
    expect_config_error(&["--bind", "0.0.0.0:3001"]);
}

#[test]
fn invalid_auth_configuration_is_rejected() {
    expect_config_error(&["--auth-proxy"]);
    expect_config_error(&["--auth-token", ""]);
    expect_config_error(&["--auth-token", "with space"]);
    expect_config_error(&["--bind", "[::]:3001"]);
}

#[tokio::test]
async fn binary_honors_token_environment_and_explicit_public_override() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use rust_llm_logger::app;
    for protected in [true, false] {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        let mut command = command();
        command
            .args([
                "--bind",
                &format!("0.0.0.0:{port}"),
                "--dashboard-show-prompts=false",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if protected {
            command.env("LLM_LOGGER_TOKEN", "environment-secret");
        } else {
            command.arg("--insecure-allow-public");
        }
        let mut child = Process(command.spawn().unwrap());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_ok()
                {
                    break;
                }
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "server exited before listening"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let client = app::create_http_client();
        for path in ["/", "/metrics"] {
            for credential in [false, true] {
                let mut builder = Request::get(format!("http://127.0.0.1:{port}{path}"));
                if credential {
                    builder = builder.header("authorization", "Bearer environment-secret");
                }
                let response = client
                    .request(builder.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    response.status().as_u16(),
                    if protected && !credential { 401 } else { 200 }
                );
                let _ = response.into_body().collect().await.unwrap();
            }
        }
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let mut output = String::new();
        std::io::Read::read_to_string(child.0.stdout.as_mut().unwrap(), &mut output).unwrap();
        std::io::Read::read_to_string(child.0.stderr.as_mut().unwrap(), &mut output).unwrap();
        assert!(!output.contains("environment-secret"));
        if !protected {
            assert!(output.contains("insecure-allow-public"));
        }
    }
    let output = command()
        .arg("--help")
        .env("LLM_LOGGER_TOKEN", "environment-secret")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("environment-secret"));
}
