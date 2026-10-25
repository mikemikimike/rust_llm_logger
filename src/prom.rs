//! Minimal Prometheus text exposition for `GET /metrics` (#4).
//!
//! Series:
//! - `llm_requests_total{model,outcome}` counter
//! - `llm_prompt_tokens_total{model}` / `llm_completion_tokens_total{model}` counters
//! - `llm_estimated_cost_usd_total{model}` counter
//! - `llm_request_latency_ms` and `llm_ttft_ms` histograms

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;

use crate::store::outcome_str;
use crate::types::LLMMetrics;

const BUCKETS_MS: &[f64] = &[
    10.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0, 60000.0,
];

const MAX_MODEL_LABELS: usize = 100;

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
    model_labels: BTreeMap<String, String>,
    requests: BTreeMap<(String, String), u64>,
    prompt_tokens: BTreeMap<String, u64>,
    completion_tokens: BTreeMap<String, u64>,
    cost: BTreeMap<String, f64>,
    latency: Histogram,
    ttft: Histogram,
}

#[derive(Default)]
pub struct PromMetrics {
    inner: Mutex<Inner>,
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

    pub fn observe(&self, m: &LLMMetrics) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let label = if let Some(label) = g.model_labels.get(&m.model) {
            label.clone()
        } else if g.model_labels.len() >= MAX_MODEL_LABELS {
            "other".to_string()
        } else {
            let label = m.model.clone();
            g.model_labels.insert(m.model.clone(), label.clone());
            label
        };
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
