use axum::{
    body::Body,
    extract::{Path, Request, State},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::header::HeaderName;
use hyper::StatusCode;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use crate::app::AppState;
use crate::parsers::{detect_backend, parser_for, BackendStreamParser, BackendType};
use crate::telemetry::Telemetry;
use crate::types::{CallOutcome, LLMMetrics, RequestData, TokenUsage};

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

fn strip_hop_by_hop(headers: &mut hyper::HeaderMap) {
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
    // Also drop any headers named in Connection
    if let Some(conn) = headers.get(hyper::header::CONNECTION).cloned() {
        if let Ok(s) = conn.to_str() {
            for part in s.split(',') {
                let h = part.trim();
                if !h.is_empty() {
                    if let Ok(name) = HeaderName::from_bytes(h.as_bytes()) {
                        headers.remove(name);
                    }
                }
            }
        }
    }
    headers.remove(hyper::header::CONNECTION);
}

/// Main proxy handler that routes to different backends
pub async fn proxy_handler(
    State(state): State<AppState>,
    Path((backend_port, path)): Path<(u16, String)>,
    req: Request,
) -> Response {
    let start_time = tokio::time::Instant::now();
    let request_data = req.extensions().get::<RequestData>().cloned();

    let upstream_uri = format!(
        "http://127.0.0.1:{}/{}",
        backend_port,
        path.trim_start_matches('/')
    );
    let upstream_uri = if let Some(query) = req.uri().query() {
        format!("{}?{}", upstream_uri, query)
    } else {
        upstream_uri
    };

    tracing::debug!("Proxying request to: {}", upstream_uri);

    let uri = match upstream_uri.parse::<hyper::Uri>() {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("Failed to parse upstream URI: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Invalid upstream URI").into_response();
        }
    };

    let (mut parts, body) = req.into_parts();
    parts.uri = uri;
    parts.headers.remove("host");
    strip_hop_by_hop(&mut parts.headers);
    let trace_id = crate::trace::ensure_traceparent(&mut parts.headers);
    let safe_headers = crate::redact::safe_headers(&parts.headers);

    let upstream_request = hyper::Request::from_parts(parts, body);

    let upstream_response = match state.client.request(upstream_request).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!("Failed to proxy request: {}", e);
            return (StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e)).into_response();
        }
    };

    let (mut parts, body) = upstream_response.into_parts();
    let status = parts.status.as_u16();
    strip_hop_by_hop(&mut parts.headers);

    let content_type = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let backend_type = detect_backend(content_type, &format!("/{}", path.trim_start_matches('/')));
    tracing::debug!(
        "Detected backend type: {:?}, content-type: {}",
        backend_type,
        content_type
    );

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let request_data_clone = request_data.clone();
    let telemetry = Arc::clone(&state.telemetry);
    tokio::spawn(async move {
        handle_stream_tee(
            body,
            tx,
            backend_type,
            request_data_clone,
            start_time,
            status,
            CallContext {
                telemetry,
                trace_id,
                safe_headers,
            },
        )
        .await;
    });

    let stream = ReceiverStream::new(rx);
    let body = StreamBody::new(stream.map(|result| result.map(hyper::body::Frame::data)));

    Response::from_parts(parts, Body::new(body))
}

/// Per-call context handed to the tee task for recording.
struct CallContext {
    telemetry: Arc<Telemetry>,
    trace_id: String,
    safe_headers: Vec<(String, String)>,
}

/// Handles the stream-tee: forwards chunks to client and parser simultaneously
async fn handle_stream_tee(
    mut upstream_body: hyper::body::Incoming,
    client_tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    backend_type: BackendType,
    request_data: Option<RequestData>,
    start_time: tokio::time::Instant,
    status: u16,
    ctx: CallContext,
) {
    let mut parser: Box<dyn BackendStreamParser> = parser_for(backend_type);

    let mut client_aborted = false;
    let mut upstream_stream_error = false;
    let mut ttft_ms: Option<u64> = None;

    loop {
        match upstream_body.frame().await {
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    if ttft_ms.is_none() && !data.is_empty() {
                        ttft_ms = Some(start_time.elapsed().as_millis() as u64);
                    }
                    parser.feed_chunk(&data).await;

                    if client_tx.send(Ok(data)).await.is_err() {
                        tracing::debug!("Client disconnected");
                        client_aborted = true;
                        break;
                    }
                }
            }
            Some(Err(e)) => {
                tracing::error!("Error reading upstream body: {}", e);
                upstream_stream_error = true;
                let _ = client_tx
                    .send(Err(std::io::Error::other(e.to_string())))
                    .await;
                break;
            }
            None => break,
        }
    }

    // Close the client stream before recording so the response completes promptly.
    drop(client_tx);
    let token_usage: TokenUsage = parser.finalize().await;
    let latency = start_time.elapsed();

    let outcome = if client_aborted {
        CallOutcome::ClientAborted
    } else if upstream_stream_error {
        CallOutcome::UpstreamStreamError
    } else if status >= 400 {
        CallOutcome::UpstreamError
    } else {
        CallOutcome::Ok
    };

    if let Some(req_data) = request_data {
        let model = req_data.model;
        let raw_prompt = req_data.prompt;
        let telemetry_for_redaction = Arc::clone(&ctx.telemetry);
        let prompt = tokio::task::spawn_blocking(move || {
            telemetry_for_redaction.sanitize_prompt_with_redactor(&raw_prompt)
        })
        .await
        .unwrap_or_else(|error| {
            tracing::error!("prompt redaction task failed: {error}");
            crate::redact::REDACTED.to_string()
        });
        let estimated_cost_usd = ctx.telemetry.pricing.estimate(&model, &token_usage);
        let metrics = LLMMetrics {
            model,
            prompt,
            prompt_tokens: token_usage.prompt_tokens,
            completion_tokens: token_usage.completion_tokens,
            latency_ms: latency.as_millis() as u64,
            ttft_ms,
            status,
            outcome: outcome.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            estimated_cost_usd,
            trace_id: Some(ctx.trace_id),
        };

        let label = match metrics.outcome {
            CallOutcome::Ok => "LLM Request Complete",
            CallOutcome::UpstreamError => "LLM Request Upstream Error",
            CallOutcome::ClientAborted => "LLM Request Client Aborted",
            CallOutcome::UpstreamStreamError => "LLM Request Upstream Stream Error",
        };

        tracing::info!(
            "{}: model={}, status={}, outcome={:?}, prompt_tokens={:?}, completion_tokens={:?}, latency_ms={}, ttft_ms={:?}, cost_usd={:?}, trace_id={:?}",
            label,
            metrics.model,
            metrics.status,
            metrics.outcome,
            metrics.prompt_tokens,
            metrics.completion_tokens,
            metrics.latency_ms,
            metrics.ttft_ms,
            metrics.estimated_cost_usd,
            metrics.trace_id
        );

        if let Ok(json) = serde_json::to_string_pretty(&metrics) {
            tracing::info!("Metrics: {}", json);
        }

        ctx.telemetry.record(&metrics, ctx.safe_headers).await;
    }
}

/// Pure helper used by unit tests to classify outcomes.
pub fn classify_outcome(
    status: u16,
    client_aborted: bool,
    upstream_stream_error: bool,
) -> CallOutcome {
    if client_aborted {
        CallOutcome::ClientAborted
    } else if upstream_stream_error {
        CallOutcome::UpstreamStreamError
    } else if status >= 400 {
        CallOutcome::UpstreamError
    } else {
        CallOutcome::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_500_is_upstream_error() {
        assert_eq!(
            classify_outcome(500, false, false),
            CallOutcome::UpstreamError
        );
    }

    #[test]
    fn client_abort_wins() {
        assert_eq!(
            classify_outcome(200, true, false),
            CallOutcome::ClientAborted
        );
    }

    #[test]
    fn ok_on_2xx() {
        assert_eq!(classify_outcome(200, false, false), CallOutcome::Ok);
    }
}
