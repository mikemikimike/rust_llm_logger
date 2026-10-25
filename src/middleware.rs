use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use http_body_util::{BodyExt, Limited};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::types::{content_to_text, GenericRequest, RequestData};

/// Default max request body size (10 MiB). Oversized bodies return 413.
pub const MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024;

static INJECT_STREAM_USAGE: AtomicBool = AtomicBool::new(false);

/// Enable `stream_options.include_usage` injection (`--inject-stream-usage`, #12).
pub fn set_inject_stream_usage(on: bool) {
    INJECT_STREAM_USAGE.store(on, Ordering::Relaxed);
}

/// Returns whether a proxied request invokes an inference endpoint whose
/// request and response should be included in LLM telemetry.
fn is_inference_request(method: &str, path: &str) -> bool {
    if method != "POST" {
        return false;
    }
    let path = path.trim_end_matches('/');
    let Some((port, endpoint)) = path.strip_prefix("/proxy/").and_then(|p| p.split_once('/'))
    else {
        return false;
    };
    port.parse::<u16>().is_ok()
        && matches!(
            endpoint,
            "api/generate"
                | "api/chat"
                | "v1/chat/completions"
                | "v1/completions"
                | "v1/messages"
                | "v1/embeddings"
        )
}

/// For streamed OpenAI-compatible chat/completions requests that don't set
/// `stream_options`, add `{"include_usage": true}` so the server emits a final usage
/// chunk. Returns the rewritten body, or `None` when nothing should change.
pub fn inject_stream_usage(path: &str, body: &[u8]) -> Option<Vec<u8>> {
    let p = path.trim_end_matches('/');
    if !(p.ends_with("/chat/completions") || p.ends_with("/v1/completions")) {
        return None;
    }
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object_mut()?;
    if obj.get("stream").and_then(|s| s.as_bool()) != Some(true) {
        return None;
    }
    match obj.get_mut("stream_options") {
        Some(serde_json::Value::Object(opts)) => {
            if opts.contains_key("include_usage") {
                return None;
            }
            opts.insert("include_usage".into(), serde_json::Value::Bool(true));
        }
        Some(_) => return None,
        None => {
            obj.insert(
                "stream_options".into(),
                serde_json::json!({ "include_usage": true }),
            );
        }
    }
    serde_json::to_vec(&v).ok()
}

/// Extracts model and prompt from the request body, then reconstructs the body
pub async fn extract_request_data(mut req: Request, next: Next) -> Response {
    if !is_inference_request(req.method().as_str(), req.uri().path()) {
        return next.run(req).await;
    }
    let body = std::mem::replace(req.body_mut(), Body::empty());
    let limited = Limited::new(body, MAX_REQUEST_BODY_BYTES);

    let collected = match limited.collect().await {
        Ok(c) => c,
        Err(e) => {
            let msg = e.to_string();
            // http_body_util::LengthLimitError surfaces when the cap is hit
            if msg.contains("length limit") || msg.contains("LengthLimit") {
                tracing::warn!("Request body exceeded {} bytes", MAX_REQUEST_BODY_BYTES);
                return Response::builder()
                    .status(413)
                    .body(Body::from("Request body too large"))
                    .unwrap();
            }
            tracing::error!("Failed to read request body: {}", e);
            return Response::builder()
                .status(400)
                .body(Body::from("Failed to read request body"))
                .unwrap();
        }
    };

    let body_bytes = collected.to_bytes();

    let (model, prompt) = parse_model_and_prompt(&body_bytes);

    req.extensions_mut().insert(RequestData { model, prompt });

    if INJECT_STREAM_USAGE.load(Ordering::Relaxed) {
        if let Some(rewritten) = inject_stream_usage(req.uri().path(), &body_bytes) {
            let len = rewritten.len();
            req.headers_mut().insert(
                axum::http::header::CONTENT_LENGTH,
                axum::http::HeaderValue::from(len),
            );
            *req.body_mut() = Body::from(rewritten);
            return next.run(req).await;
        }
    }

    *req.body_mut() = Body::from(body_bytes);
    next.run(req).await
}

/// Parse model independently of messages so array/null content cannot wipe it.
pub fn parse_model_and_prompt(body_bytes: &[u8]) -> (String, String) {
    // Prefer a top-level model even when messages fail to deserialize.
    let model = serde_json::from_slice::<serde_json::Value>(body_bytes)
        .ok()
        .and_then(|v| {
            v.get("model")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    if let Ok(parsed) = serde_json::from_slice::<GenericRequest>(body_bytes) {
        let prompt = extract_prompt(&parsed);
        let model = parsed.model.unwrap_or(model);
        return (model, prompt);
    }

    tracing::warn!("Failed to parse full request body; model may still be known");
    (model, "unparseable".to_string())
}

fn extract_prompt(request: &GenericRequest) -> String {
    if let Some(prompt) = &request.prompt {
        prompt.clone()
    } else if let Some(messages) = &request.messages {
        messages
            .iter()
            .map(|m| format!("{}: {}", m.role, content_to_text(&m.content)))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        "no prompt found".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn array_and_null_content_keeps_model_and_text() {
        let body = serde_json::to_vec(&json!({
            "model": "llama3.2",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "image_url", "image_url": {"url": "x"}}
                ]},
                {"role": "assistant", "content": null}
            ]
        }))
        .unwrap();
        let (model, prompt) = parse_model_and_prompt(&body);
        assert_eq!(model, "llama3.2");
        assert!(prompt.contains("hello"), "{prompt}");
        assert!(prompt.contains("assistant:"), "{prompt}");
    }

    #[test]
    fn injects_include_usage_for_streamed_chat() {
        let body = br#"{"model":"gpt","stream":true,"messages":[]}"#;
        let out = inject_stream_usage("/proxy/8080/v1/chat/completions", body).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream_options"]["include_usage"], json!(true));
    }

    #[test]
    fn leaves_non_streamed_or_explicit_requests_alone() {
        let path = "/proxy/8080/v1/chat/completions";
        assert!(inject_stream_usage(path, br#"{"stream":false}"#).is_none());
        assert!(inject_stream_usage(path, br#"{"messages":[]}"#).is_none());
        assert!(inject_stream_usage(
            path,
            br#"{"stream":true,"stream_options":{"include_usage":false}}"#
        )
        .is_none());
        assert!(inject_stream_usage("/proxy/11434/api/chat", br#"{"stream":true}"#).is_none());
    }

    #[test]
    fn model_survives_when_only_top_level_parses() {
        // Intentionally weird messages that still leave model extractable
        let body = br#"{"model":"mistral","messages":"not-an-array"}"#;
        let (model, prompt) = parse_model_and_prompt(body);
        assert_eq!(model, "mistral");
        assert_eq!(prompt, "unparseable");
    }

    #[test]
    fn only_supported_post_endpoints_are_inference_requests() {
        assert!(is_inference_request("POST", "/proxy/11434/api/generate"));
        assert!(is_inference_request(
            "POST",
            "/proxy/8080/v1/chat/completions/"
        ));
        assert!(!is_inference_request("GET", "/proxy/11434/api/tags"));
        assert!(!is_inference_request("POST", "/proxy/11434/api/pull"));
        assert!(!is_inference_request(
            "PUT",
            "/proxy/8080/v1/chat/completions"
        ));
    }
}
