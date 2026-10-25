use rust_llm_logger::{app, middleware, pricing::Pricing, store::Store, telemetry::Telemetry};

use clap::{Parser, Subcommand};
use rust_llm_logger::auth::AccessConfig;
use std::collections::HashSet;
use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// LLM logging reverse proxy.
#[derive(Parser)]
#[command(name = "rust_llm_logger", about = "LLM request logging proxy")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Bind address (env: LLM_LOGGER_BIND)
    #[arg(long, env = "LLM_LOGGER_BIND", default_value = "127.0.0.1:3000")]
    bind: String,

    /// Bearer token required for dashboard/metrics (env: LLM_LOGGER_TOKEN)
    #[arg(long, env = "LLM_LOGGER_TOKEN", hide_env_values = true)]
    auth_token: Option<String>,

    /// Require X-LLM-Logger-Token on proxy requests, preserving upstream Authorization
    #[arg(long, env = "LLM_LOGGER_AUTH_PROXY", default_value_t = false)]
    auth_proxy: bool,

    /// Explicitly allow an unauthenticated non-loopback listener
    #[arg(long, default_value_t = false)]
    insecure_allow_public: bool,

    /// Show prompt text in the dashboard (stored data is unaffected)
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    dashboard_show_prompts: bool,

    /// SQLite path for persisted metrics (env: METRICS_DB); empty disables
    #[arg(long, env = "METRICS_DB", default_value = "")]
    metrics_db: String,

    /// JSON file of per-model prices merged over the built-in table (env: LLM_LOGGER_PRICING)
    #[arg(long, env = "LLM_LOGGER_PRICING", default_value = "")]
    pricing_file: String,

    /// Comma-separated backend ports allowed for /proxy/{port}/...
    /// (env: ALLOWED_BACKEND_PORTS). Default: common local LLM ports.
    #[arg(
        long,
        env = "ALLOWED_BACKEND_PORTS",
        default_value = "11434,8080,8000,5000"
    )]
    allowed_backend_ports: String,

    /// Add `stream_options.include_usage=true` to streamed OpenAI-compatible requests
    /// that omit it, so token usage is reported (env: LLM_LOGGER_INJECT_STREAM_USAGE)
    #[arg(long, env = "LLM_LOGGER_INJECT_STREAM_USAGE", default_value_t = false)]
    inject_stream_usage: bool,

    /// Log filter when RUST_LOG is unset
    #[arg(
        long,
        env = "LLM_LOGGER_LOG",
        default_value = "rust_llm_logger=info,tower_http=info"
    )]
    log_filter: String,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Delete persisted calls older than N days (rolled up into daily_rollup by default)
    Gc {
        /// Age threshold in days
        #[arg(long, default_value_t = 30)]
        days: u32,
        /// Delete without writing daily aggregates
        #[arg(long)]
        no_rollup: bool,
    },
}

fn parse_allow_list(raw: &str) -> Result<HashSet<u16>, String> {
    let mut set = HashSet::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let port: u16 = part
            .parse()
            .map_err(|_| format!("invalid port in allow-list: {part}"))?;
        if port == 0 {
            return Err("port 0 is not allowed".into());
        }
        set.insert(port);
    }
    if set.is_empty() {
        return Err("ALLOWED_BACKEND_PORTS allow-list is empty".into());
    }
    Ok(set)
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("config error: {msg}");
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .or_else(|_| tracing_subscriber::EnvFilter::try_new(&args.log_filter))
                .unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    if let Some(Command::Gc { days, no_rollup }) = args.command {
        if args.metrics_db.is_empty() {
            fail("gc needs --metrics-db / METRICS_DB");
        }
        let store = Store::open(&args.metrics_db).unwrap_or_else(|e| fail(e));
        match store.gc_days(days, !no_rollup) {
            Ok(r) => {
                println!(
                    "gc: deleted {} call(s) older than {days} day(s); {} rollup group(s) updated",
                    r.deleted, r.rolled_up_groups
                );
                return;
            }
            Err(e) => fail(e),
        }
    }

    let unauthenticated = args.auth_token.is_none();
    let access = AccessConfig::new(
        args.auth_token,
        args.auth_proxy,
        args.dashboard_show_prompts,
    )
    .unwrap_or_else(|e| fail(e));
    let addresses: Vec<_> = tokio::net::lookup_host(&args.bind)
        .await
        .unwrap_or_else(|e| fail(e))
        .collect();
    access
        .validate_bind(&addresses, args.insecure_allow_public)
        .unwrap_or_else(|e| fail(e));
    if unauthenticated
        && args.insecure_allow_public
        && addresses.iter().any(|a| !a.ip().is_loopback())
    {
        tracing::warn!("--insecure-allow-public permits network access without requiring a token");
    }
    let allow = Arc::new(parse_allow_list(&args.allowed_backend_ports).unwrap_or_else(|e| fail(e)));
    middleware::set_inject_stream_usage(args.inject_stream_usage);

    let pricing = Pricing::load(Some(&args.pricing_file)).unwrap_or_else(|e| fail(e));
    let store = if args.metrics_db.is_empty() {
        None
    } else {
        tracing::info!(path = %args.metrics_db, "persisting calls to SQLite");
        Some(Arc::new(
            Store::open(&args.metrics_db).unwrap_or_else(|e| fail(e)),
        ))
    };
    let telemetry = Arc::new(Telemetry::new(pricing, store));

    let app = app::build_app_with_access(Arc::clone(&allow), telemetry, access);

    let listener = tokio::net::TcpListener::bind(addresses.as_slice())
        .await
        .unwrap_or_else(|e| panic!("Failed to bind {}: {e}", args.bind));

    tracing::info!(
        "LLM Logging Proxy listening on {} (allowed ports: {:?}); dashboard at /, Prometheus at /metrics",
        listener.local_addr().unwrap(),
        allow
    );

    axum::serve(listener, app).await.expect("Server failed");
}

#[cfg(test)]
mod tests {
    use super::parse_allow_list;

    #[test]
    fn parses_ports() {
        let s = parse_allow_list("11434, 8080").unwrap();
        assert!(s.contains(&11434) && s.contains(&8080));
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_allow_list("").is_err());
        assert!(parse_allow_list(",,").is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_allow_list("abc").is_err());
    }
}
