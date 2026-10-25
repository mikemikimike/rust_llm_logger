//! Fan-out for completed calls: pricing (#7), redaction (#9), SQLite (#3),
//! Prometheus (#4) and the in-memory ring used by the dashboard (#6).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::pricing::Pricing;
use crate::prom::PromMetrics;
use crate::redact::{default_redactor, Redactor};
use crate::store::{CallRow, Store};
use crate::types::LLMMetrics;

/// Max prompt characters kept in stored rows / the dashboard.
pub const MAX_STORED_PROMPT_CHARS: usize = 4000;
const RING_CAPACITY: usize = 200;

pub struct Telemetry {
    pub pricing: Pricing,
    pub store: Option<Arc<Store>>,
    pub prom: PromMetrics,
    redactor: Redactor,
    recent: Mutex<VecDeque<CallRow>>,
}

impl Telemetry {
    pub fn new(pricing: Pricing, store: Option<Arc<Store>>) -> Self {
        Self::new_with_redactor(pricing, store, Redactor::default())
    }

    pub fn new_with_redactor(
        pricing: Pricing,
        store: Option<Arc<Store>>,
        redactor: Redactor,
    ) -> Self {
        Self {
            pricing,
            store,
            prom: PromMetrics::new(),
            redactor,
            recent: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
        }
    }

    /// Redact a prompt for storage/logging and cap its length.
    pub fn sanitize_prompt(prompt: &str) -> String {
        default_redactor().redact_prefix(prompt, MAX_STORED_PROMPT_CHARS)
    }

    pub fn sanitize_prompt_with_redactor(&self, prompt: &str) -> String {
        self.redactor.redact_prefix(prompt, MAX_STORED_PROMPT_CHARS)
    }

    /// Record a finished call. SQLite writes run on the blocking pool so they never
    /// stall the async runtime; the response stream has already been forwarded.
    pub async fn record(&self, metrics: &LLMMetrics, safe_headers: Vec<(String, String)>) {
        self.prom.observe(metrics);
        let row = CallRow::from_metrics(metrics);
        {
            let mut ring = self.recent.lock().unwrap_or_else(|e| e.into_inner());
            if ring.len() == RING_CAPACITY {
                ring.pop_back();
            }
            ring.push_front(row.clone());
        }
        if let Some(store) = &self.store {
            let store = Arc::clone(store);
            let headers_json = serde_json::to_string(&safe_headers).unwrap_or_else(|_| "[]".into());
            let res = tokio::task::spawn_blocking(move || store.insert(&row, &headers_json)).await;
            match res {
                Ok(Err(e)) => tracing::error!("failed to persist call: {e}"),
                Err(e) => tracing::error!("persist task failed: {e}"),
                Ok(Ok(())) => {}
            }
        }
    }

    /// Recent calls for the dashboard: SQLite when configured, else the ring buffer.
    pub fn recent(&self, limit: usize, model: Option<&str>) -> Vec<CallRow> {
        if let Some(store) = &self.store {
            match store.recent(limit, model) {
                Ok(rows) => return rows,
                Err(e) => tracing::warn!("dashboard query failed, using ring buffer: {e}"),
            }
        }
        let ring = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        ring.iter()
            .filter(|r| model.is_none_or(|m| r.model == m))
            .take(limit)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_before_scanning_without_leaking_boundary_secret() {
        let prompt = format!(
            "{} sk_live_51Hxabcdefgh",
            "x".repeat(MAX_STORED_PROMPT_CHARS - 5)
        );
        let redacted = Telemetry::sanitize_prompt(&prompt);
        assert!(!redacted.contains("sk_live_"));
        assert!(redacted.ends_with('…'));
        assert_eq!(redacted.chars().count(), MAX_STORED_PROMPT_CHARS + 1);
    }
}
