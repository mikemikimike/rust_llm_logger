# Rust LLM Logger

A high-performance, non-buffering reverse proxy for LLM servers built with Rust, Axum, and the Tower ecosystem. This proxy intercepts and logs LLM requests and responses with near-zero latency overhead (≤ 2-3ms) by streaming data through an in-memory channel while parsing metrics concurrently.

## Features

- **Zero-Copy Stream Interception**: Uses a stream-tee architecture to forward response chunks to clients while simultaneously parsing them for metrics
- **Multi-Backend Support**:
  - Ollama (NDJSON format)
  - OpenAI-compatible APIs (SSE/Server-Sent Events) - vLLM, llama.cpp, etc.
  - Anthropic Messages API (SSE `message_start` / `message_delta` usage)
  - Non-streaming `application/json` responses from any of the above
- **Dynamic Routing**: Route to different backend ports on the fly
- **Non-Blocking**: Client receives streaming responses without waiting for parsing or logging
- **Comprehensive Metrics**: Captures model name, prompt, token counts (input/output), and end-to-end latency

## Architecture

### Phase 1: High-Performance Proxy Foundation

The proxy is built on:
- **axum**: Web framework with Tower ecosystem integration
- **hyper**: High-performance HTTP client/server
- **tokio**: Async runtime
- **tower-http**: Composable middleware

### Phase 2: Non-Buffering Stream Interception

The core innovation is the stream-tee architecture implemented in `src/proxy.rs:handle_stream_tee`:

1. Incoming request is processed by middleware to extract model/prompt
2. Request is forwarded to upstream LLM server
3. Response body stream is split into two channels:
   - **Client channel**: Immediate forwarding via `mpsc::channel`
   - **Parser channel**: Concurrent parsing in separate tokio task
4. Metrics are aggregated and logged when stream completes

### Parsers

#### Ollama Parser (`src/parsers/ollama.rs`)
- Parses NDJSON (Newline Delimited JSON)
- Extracts `prompt_eval_count` and `eval_count` from final object with `"done": true`

#### OpenAI Parser (`src/parsers/openai.rs`)
- Parses SSE (Server-Sent Events) format
- Looks for final `usage` object containing `prompt_tokens` and `completion_tokens`
- Ignores intermediate delta chunks
- OpenAI only sends streamed usage when the request has `stream_options.include_usage=true`; run with `--inject-stream-usage` to add it automatically

#### Anthropic Parser (`src/parsers/anthropic.rs`)
- Used for `text/event-stream` responses on `/v1/messages`
- `input_tokens` from `message_start`, cumulative `output_tokens` from `message_delta`

#### JSON Parser (`src/parsers/json.rs`)
- Used for non-streaming `application/json` (OpenAI `stream:false`, Anthropic, Ollama `stream:false`)
- Tries OpenAI `usage.*_tokens`, Anthropic `usage.input/output_tokens`, then Ollama `done` + `*eval_count`

SSE framing (`src/parsers/sse.rs`) splits on raw bytes, so multi-byte UTF-8 split across chunks is safe. It accepts `\n\n`, `\r\n\r\n` and `\r\r` delimiters and `data:` with or without a space.

## Quick Start

### Build

```bash
cargo build --release
```

### Run

```bash
cargo run --release
```

The proxy will start on `http://127.0.0.1:3000` by default.

### Configuration (#5)

| Flag / env | Default | Meaning |
|---|---|---|
| `--bind` / `LLM_LOGGER_BIND` | `127.0.0.1:3000` | Listen address (use `0.0.0.0:3001` for Tailscale) |
| `--metrics-db` / `METRICS_DB` | empty | SQLite file; every finished call is written to the `calls` table (empty = in-memory only) |
| `--pricing-file` / `LLM_LOGGER_PRICING` | empty | JSON price table merged over the built-in defaults |
| `--allowed-backend-ports` / `ALLOWED_BACKEND_PORTS` | `11434,8080,8000,5000` | Ports permitted under `/proxy/{port}/` |
| `--inject-stream-usage` / `LLM_LOGGER_INJECT_STREAM_USAGE` | `false` | Add `stream_options.include_usage` to streamed OpenAI-compatible requests |
| `--log-filter` / `LLM_LOGGER_LOG` | `rust_llm_logger=info,...` | Used when `RUST_LOG` unset |

```bash
LLM_LOGGER_BIND=0.0.0.0:3001 cargo run
cargo run -- --help
```


### Usage

Route requests through the proxy using the pattern:

```
http://127.0.0.1:3000/proxy/<backend_port>/<endpoint>
```

#### Example 1: Ollama

If you have Ollama running on `localhost:11434`:

```bash
curl http://127.0.0.1:3000/proxy/11434/api/generate \
  -H "Content-Type: application/json" \
  -d '{
    "model": "llama2",
    "prompt": "Why is the sky blue?",
    "stream": true
  }'
```

#### Example 2: vLLM (OpenAI-compatible)

If you have vLLM running on `localhost:8080`:

```bash
curl http://127.0.0.1:3000/proxy/8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "meta-llama/Llama-2-7b-chat-hf",
    "messages": [
      {"role": "user", "content": "Explain quantum computing"}
    ],
    "stream": true
  }'
```

## Observability

### Persistence (#3) and retention (#10)

With `METRICS_DB=/var/lib/llm-logger/calls.db` each completed call becomes one row in
`calls` (model, tokens, latency, TTFT, outcome, cost, trace id, redacted prompt and
headers). Inserts run on a blocking thread so the streaming path never waits on disk.

Old rows are pruned with the `gc` subcommand. By default deleted rows are first folded
into `daily_rollup` (per day/model counts, tokens, cost) so long-term totals survive:

```bash
METRICS_DB=calls.db rust_llm_logger gc --days 30          # roll up, then delete
METRICS_DB=calls.db rust_llm_logger gc --days 7 --no-rollup
# cron: 15 3 * * * METRICS_DB=/var/lib/llm-logger/calls.db /usr/local/bin/rust_llm_logger gc --days 30
```

### Prometheus (#4)

`GET /metrics` serves the text exposition format:

- `llm_requests_total{model,outcome}` (`ok`, `upstream_error`, `client_abort`)
- `llm_prompt_tokens_total{model}`, `llm_completion_tokens_total{model}`
- `llm_estimated_cost_usd_total{model}`
- `llm_request_latency_ms` and `llm_ttft_ms` histograms

Only `POST` requests to `/api/generate`, `/api/chat`, `/v1/chat/completions`,
`/v1/completions`, `/v1/messages`, and `/v1/embeddings` are recorded as calls.
Other requests (including health checks and model discovery) pass through without
collecting their bodies or recording telemetry.

Prometheus model labels are trimmed, ASCII-lowercased, and limited to 128 characters.
The first 100 distinct labels retain individual series; additional models share
`model="other"`. Existing admitted labels continue accumulating their own totals.
The library constructor `PromMetrics::with_model_limit` can select another limit
(zero aggregates all models). Original model names remain in stored calls and
the dashboard, and cost estimation uses the original name. Unknown-model pricing
warnings retain at most 100 keys, each limited to 128 characters.

```yaml
scrape_configs:
  - job_name: llm-logger
    static_configs:
      - targets: ["127.0.0.1:3000"]
```

### Dashboard (#6)

`GET /` renders the most recent calls (`?model=NAME` filters, `?limit=N`, default 100, max 1000).
It reads from SQLite when configured, otherwise from an in-memory ring of the last 200
calls, and shows an empty state before the first request.

### Cost estimation (#7)

Each call gets `estimated_cost_usd = prompt/1000 * prompt_per_1k + completion/1000 * completion_per_1k`.
Models are matched by longest prefix; unknown models get `null` and a one-time warning.
Override or extend the built-in table with `--pricing-file`:

```json
{ "gpt-4o-mini": { "prompt_per_1k": 0.00015, "completion_per_1k": 0.0006 },
  "llama3":      { "prompt_per_1k": 0.0,     "completion_per_1k": 0.0 } }
```

### Redaction (#9)

Credential headers (`authorization`, `proxy-authorization`, `cookie`, `set-cookie`,
`x-api-key`, `api-key`, `openai-api-key`, `anthropic-api-key`, and anything containing `token`/`secret`) are never logged or
stored. Prompts are scrubbed of bearer tokens and common key shapes (`sk-…`, `AIza…`,
`hf_…`, `xoxb-…`) and capped at 4000 characters before persistence. Requests are still
forwarded to the backend unchanged.

### Trace propagation (#11)

An incoming W3C `traceparent` is forwarded upstream as-is; when absent, one is generated.
The 32-hex trace id is stored with the call and included in the log line, so proxy rows
can be joined with application traces.

## Metrics Output

Metrics are logged to stdout in JSON format:

```json
{
  "model": "llama2",
  "prompt": "Why is the sky blue?",
  "prompt_tokens": 8,
  "completion_tokens": 150,
  "latency_ms": 1243,
  "estimated_cost_usd": 0.0003,
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
  "timestamp": "2025-11-09T12:34:56.789Z"
}
```

## Configuration

### Logging Level

Control logging verbosity via the `RUST_LOG` environment variable:

```bash
# Debug mode (verbose)
RUST_LOG=rust_llm_logger=debug cargo run

# Info mode (default)
RUST_LOG=rust_llm_logger=info cargo run

# Quiet mode
RUST_LOG=rust_llm_logger=warn cargo run
```

### Server Port

Use `--bind` / `LLM_LOGGER_BIND` instead of editing source.

## Project Structure

```
src/
├── main.rs              # CLI, config, `gc` subcommand
├── app.rs               # Router (/, /metrics, /proxy) and port allow-list
├── proxy.rs             # Core proxy handler and stream-tee logic
├── telemetry.rs         # Fan-out of finished calls to store / Prometheus / ring
├── store.rs             # SQLite persistence, rollups, GC
├── prom.rs              # Prometheus counters and histograms
├── dashboard.rs         # HTML recent-calls view
├── pricing.rs           # Cost table and estimation
├── redact.rs            # Header / prompt secret scrubbing
├── trace.rs             # W3C traceparent handling
├── middleware.rs        # Request body extraction middleware
├── types.rs             # Data structures and serialization types
└── parsers/
    ├── mod.rs           # Parser trait and backend detection
    ├── ollama.rs        # NDJSON parser for Ollama
    ├── openai.rs        # SSE parser for OpenAI-compatible APIs
    └── passthrough.rs   # Null parser for unknown formats
```

## Performance Characteristics

- **Added Latency**: ≤ 2-3ms overhead from stream-tee channel
- **Memory**: Minimal buffering - only stores incomplete JSON objects
- **Concurrency**: Fully async, handles thousands of concurrent connections
- **Streaming**: Client receives first byte immediately, no waiting for parsing

## Future Enhancements

- [ ] Additional persistence backends (PostgreSQL, ClickHouse)
- [ ] Authentication/API key management
- [ ] Request/response filtering and transformation
- [ ] Rate limiting per model/user

## License

MIT

## Contributing

Contributions welcome! Please open an issue or PR.
