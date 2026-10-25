//! Minimal Prometheus text exposition for `GET /metrics` (#4).
//!
//! Series:
//! - `llm_requests_total{model,outcome}` counter
//! - `llm_prompt_tokens_total{model}` / `llm_completion_tokens_total{model}` counters
//! - `llm_estimated_cost_usd_total{model}` counter
//! - `llm_request_latency_ms` and `llm_ttft_ms` histograms

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::sync::Mutex;

use crate::store::outcome_str;
use crate::types::LLMMetrics;

const BUCKETS_MS: &[f64] = &[
    10.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0, 60000.0,
];

const MAX_MODEL_LABELS: usize = 100;
const MAX_MODEL_LABEL_CHARS: usize = 128;

#[derive(Default)]
struct Histogram {
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Histogram {
    fn observe(&mut self, v: f64) {
        if self.counts.is_empty() {
            self.counts = vec![0; BUCKETS_MS.len()];
        }
        for (i, b) in BUCKETS_MS.iter().enumerate() {
            if v <= *b {
                self.counts[i] += 1;
            }
        }
        self.sum += v;
        self.count += 1;
    }

    fn render(&self, name: &str, help: &str, out: &mut String) {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} histogram");
        for (i, b) in BUCKETS_MS.iter().enumerate() {
            let c = self.counts.get(i).copied().unwrap_or(0);
            let _ = writeln!(out, "{name}_bucket{{le=\"{b}\"}} {c}");
        }
        let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {}", self.count);
        let _ = writeln!(out, "{name}_sum {}", self.sum);
        let _ = writeln!(out, "{name}_count {}", self.count);
    }
}

#[derive(Default)]
struct Inner {
    model_labels: BTreeSet<String>,
    requests: BTreeMap<(String, String), u64>,
    prompt_tokens: BTreeMap<String, u64>,
    completion_tokens: BTreeMap<String, u64>,
    cost: BTreeMap<String, f64>,
    latency: Histogram,
    ttft: Histogram,
}

pub struct PromMetrics {
    inner: Mutex<Inner>,
    model_limit: usize,
}

impl Default for PromMetrics {
    fn default() -> Self {
        Self::with_model_limit(MAX_MODEL_LABELS)
    }
}

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

impl PromMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Maximum distinct normalized model labels, excluding the `other` bucket.
    /// A zero limit aggregates every call under `other`.
    pub fn with_model_limit(model_limit: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            model_limit,
        }
    }

    pub fn observe(&self, m: &LLMMetrics) {
        let mut label: String = m.model.trim().chars().take(MAX_MODEL_LABEL_CHARS).collect();
        label.make_ascii_lowercase();
        if label.is_empty() {
            label.push_str("unknown");
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if label != "other" && !g.model_labels.contains(&label) {
            if g.model_labels.len() < self.model_limit {
                g.model_labels.insert(label.clone());
            } else {
                label = "other".to_string();
            }
        }
        *g.requests
            .entry((label.clone(), outcome_str(&m.outcome).to_string()))
            .or_default() += 1;
        if let Some(p) = m.prompt_tokens {
            *g.prompt_tokens.entry(label.clone()).or_default() += p as u64;
        }
        if let Some(c) = m.completion_tokens {
            *g.completion_tokens.entry(label.clone()).or_default() += c as u64;
        }
        if let Some(c) = m.estimated_cost_usd {
            *g.cost.entry(label).or_default() += c;
        }
        g.latency.observe(m.latency_ms as f64);
        if let Some(t) = m.ttft_ms {
            g.ttft.observe(t as f64);
        }
    }

    pub fn render(&self) -> String {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = String::new();
        let _ = writeln!(
            out,
            "# HELP llm_requests_total Proxied LLM calls by model and outcome."
        );
        let _ = writeln!(out, "# TYPE llm_requests_total counter");
        for ((model, outcome), v) in &g.requests {
            let _ = writeln!(
                out,
                "llm_requests_total{{model=\"{}\",outcome=\"{}\"}} {v}",
                esc(model),
                esc(outcome)
            );
        }
        for (name, help, map) in [
            (
                "llm_prompt_tokens_total",
                "Prompt tokens by model.",
                &g.prompt_tokens,
            ),
            (
                "llm_completion_tokens_total",
                "Completion tokens by model.",
                &g.completion_tokens,
            ),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            for (model, v) in map {
                let _ = writeln!(out, "{name}{{model=\"{}\"}} {v}", esc(model));
            }
        }
        let _ = writeln!(
            out,
            "# HELP llm_estimated_cost_usd_total Estimated spend by model."
        );
        let _ = writeln!(out, "# TYPE llm_estimated_cost_usd_total counter");
        for (model, v) in &g.cost {
            let _ = writeln!(
                out,
                "llm_estimated_cost_usd_total{{model=\"{}\"}} {v}",
                esc(model)
            );
        }
        g.latency.render(
            "llm_request_latency_ms",
            "End-to-end proxied call latency (ms).",
            &mut out,
        );
        g.ttft
            .render("llm_ttft_ms", "Time to first upstream byte (ms).", &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CallOutcome;
    use std::sync::Arc;

    fn metric(model: &str, outcome: CallOutcome) -> LLMMetrics {
        LLMMetrics {
            model: model.into(),
            prompt: String::new(),
            prompt_tokens: Some(1),
            completion_tokens: Some(2),
            estimated_cost_usd: Some(0.5),
            latency_ms: 3,
            ttft_ms: Some(1),
            status: 200,
            outcome,
            timestamp: String::new(),
            trace_id: None,
        }
    }

    #[test]
    fn normalizes_labels_and_keeps_existing_models_after_capacity() {
        let prom = PromMetrics::with_model_limit(2);
        for name in ["  Foo  ", "foo", "BAR", "OTHER", "overflow", "FOO"] {
            prom.observe(&metric(name, CallOutcome::Ok));
        }
        let inner = prom.inner.lock().unwrap();
        assert_eq!(inner.model_labels.len(), 2);
        assert_eq!(inner.requests.get(&("foo".into(), "ok".into())), Some(&3));
        assert_eq!(inner.requests.get(&("bar".into(), "ok".into())), Some(&1));
        assert_eq!(inner.requests.get(&("other".into(), "ok".into())), Some(&2));
        assert_eq!(inner.prompt_tokens.values().sum::<u64>(), 6);
        assert_eq!(inner.completion_tokens.values().sum::<u64>(), 12);
        assert_eq!(inner.cost.values().sum::<f64>(), 3.0);
    }

    #[test]
    fn limits_unicode_label_length_and_escapes_prometheus_text() {
        let prom = PromMetrics::new();
        let name = format!("  {}tail  ", "é".repeat(MAX_MODEL_LABEL_CHARS));
        prom.observe(&metric(&name, CallOutcome::Ok));
        prom.observe(&metric(" \t ", CallOutcome::Ok));
        prom.observe(&metric("quote\"slash\\line\nend", CallOutcome::Ok));
        let inner = prom.inner.lock().unwrap();
        assert!(inner
            .model_labels
            .contains(&"é".repeat(MAX_MODEL_LABEL_CHARS)));
        assert!(inner.model_labels.contains("unknown"));
        assert!(inner
            .model_labels
            .iter()
            .all(|s| s.chars().count() <= MAX_MODEL_LABEL_CHARS));
        drop(inner);
        assert!(prom.render().contains(r#"model="quote\"slash\\line\nend""#));
    }

    #[test]
    fn concurrent_observations_bound_all_retained_maps_and_preserve_outcomes() {
        let prom = Arc::new(PromMetrics::with_model_limit(3));
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let prom = Arc::clone(&prom);
                scope.spawn(move || {
                    for i in 0..1250 {
                        let outcome = match worker % 4 {
                            0 => CallOutcome::Ok,
                            1 => CallOutcome::UpstreamError,
                            2 => CallOutcome::ClientAborted,
                            _ => CallOutcome::UpstreamStreamError,
                        };
                        prom.observe(&metric(&format!("model-{worker}-{i}"), outcome));
                    }
                });
            }
        });
        let inner = prom.inner.lock().unwrap();
        assert_eq!(inner.model_labels.len(), 3);
        assert!(inner.requests.len() <= 4 * 4);
        assert_eq!(inner.requests.values().sum::<u64>(), 10_000);
        for outcome in [
            "ok",
            "upstream_error",
            "client_aborted",
            "upstream_stream_error",
        ] {
            let count: u64 = inner
                .requests
                .iter()
                .filter(|((_, o), _)| o == outcome)
                .map(|(_, n)| n)
                .sum();
            assert_eq!(count, 2500, "{outcome}");
        }
        assert!(inner.prompt_tokens.len() <= 4);
        assert!(inner.completion_tokens.len() <= 4);
        assert!(inner.cost.len() <= 4);
        assert_eq!(inner.prompt_tokens.values().sum::<u64>(), 10_000);
        assert_eq!(inner.completion_tokens.values().sum::<u64>(), 20_000);
        assert_eq!(inner.cost.values().sum::<f64>(), 5000.0);
        assert_eq!(inner.latency.count, 10_000);
        assert_eq!(inner.ttft.count, 10_000);
    }

    #[test]
    fn zero_limit_keeps_only_overflow_without_retaining_names() {
        let prom = PromMetrics::with_model_limit(0);
        for i in 0..1000 {
            prom.observe(&metric(&format!("model-{i}"), CallOutcome::Ok));
        }
        let inner = prom.inner.lock().unwrap();
        assert!(inner.model_labels.is_empty());
        assert_eq!(inner.requests.len(), 1);
        assert_eq!(
            inner.requests.get(&("other".into(), "ok".into())),
            Some(&1000)
        );
    }
}
