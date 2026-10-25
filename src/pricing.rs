//! Per-model token cost estimation (#7).
//!
//! Prices are USD per 1K tokens. Built-in defaults cover common hosted models;
//! override or extend them with a JSON file (`--pricing-file` / `LLM_LOGGER_PRICING`):
//!
//! ```json
//! { "gpt-4o": { "prompt_per_1k": 0.0025, "completion_per_1k": 0.01 },
//!   "llama3": { "prompt_per_1k": 0.0, "completion_per_1k": 0.0 } }
//! ```
//!
//! Model names match exactly first, then by the longest configured prefix
//! (so `gpt-4o-mini-2024-07-18` uses `gpt-4o-mini`, not `gpt-4o`).

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::types::TokenUsage;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
pub struct ModelPrice {
    pub prompt_per_1k: f64,
    pub completion_per_1k: f64,
}

const DEFAULTS: &[(&str, f64, f64)] = &[
    ("gpt-4o-mini", 0.000_15, 0.000_6),
    ("gpt-4o", 0.002_5, 0.01),
    ("gpt-4.1-mini", 0.000_4, 0.001_6),
    ("gpt-4.1", 0.002, 0.008),
    ("gpt-3.5-turbo", 0.000_5, 0.001_5),
    ("claude-3-5-haiku", 0.000_8, 0.004),
    ("claude-3-5-sonnet", 0.003, 0.015),
    ("claude-sonnet-4", 0.003, 0.015),
    ("claude-opus-4", 0.015, 0.075),
];

const MAX_UNKNOWN_MODEL_WARNINGS: usize = 100;

pub struct Pricing {
    prices: HashMap<String, ModelPrice>,
    warned: Mutex<HashSet<String>>,
}

impl Default for Pricing {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl Pricing {
    pub fn empty() -> Self {
        Self {
            prices: HashMap::new(),
            warned: Mutex::new(HashSet::new()),
        }
    }

    pub fn with_defaults() -> Self {
        let mut p = Self::empty();
        for (m, a, b) in DEFAULTS {
            p.prices.insert(
                (*m).to_string(),
                ModelPrice {
                    prompt_per_1k: *a,
                    completion_per_1k: *b,
                },
            );
        }
        p
    }

    /// Merge overrides from a JSON object of `model -> ModelPrice`.
    pub fn merge_json(&mut self, json: &str) -> Result<usize, serde_json::Error> {
        let map: HashMap<String, ModelPrice> = serde_json::from_str(json)?;
        let n = map.len();
        self.prices.extend(map);
        Ok(n)
    }

    /// Defaults plus an optional override file.
    pub fn load(path: Option<&str>) -> Result<Self, String> {
        let mut p = Self::with_defaults();
        if let Some(path) = path.filter(|s| !s.is_empty()) {
            let raw = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            p.merge_json(&raw).map_err(|e| format!("{path}: {e}"))?;
        }
        Ok(p)
    }

    pub fn price_for(&self, model: &str) -> Option<ModelPrice> {
        let m = model.to_ascii_lowercase();
        if let Some(p) = self.prices.get(&m) {
            return Some(*p);
        }
        self.prices
            .iter()
            .filter(|(k, _)| m.starts_with(k.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, p)| *p)
    }

    /// Estimated USD cost, or `None` for unknown models / missing token counts.
    /// Warn once for each of the first 100 unknown models. Retained warning keys
    /// are length-limited so arbitrary client model names cannot grow memory.
    pub fn estimate(&self, model: &str, usage: &TokenUsage) -> Option<f64> {
        let Some(price) = self.price_for(model) else {
            let mut warned = self.warned.lock().unwrap_or_else(|e| e.into_inner());
            if warned.len() < MAX_UNKNOWN_MODEL_WARNINGS
                && warned.insert(model.chars().take(128).collect())
            {
                tracing::warn!(
                    model,
                    "no pricing for model; estimated_cost_usd will be null"
                );
            }
            return None;
        };
        if usage.prompt_tokens.is_none() && usage.completion_tokens.is_none() {
            return None;
        }
        let p = usage.prompt_tokens.unwrap_or(0) as f64 / 1000.0 * price.prompt_per_1k;
        let c = usage.completion_tokens.unwrap_or(0) as f64 / 1000.0 * price.completion_per_1k;
        Some(((p + c) * 1e8).round() / 1e8)
    }

    #[cfg(test)]
    fn warned_count(&self) -> usize {
        self.warned.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "acme-large": {"prompt_per_1k": 0.01, "completion_per_1k": 0.03},
        "acme": {"prompt_per_1k": 0.001, "completion_per_1k": 0.002}
    }"#;

    #[test]
    fn fixture_prices_and_longest_prefix() {
        let mut p = Pricing::empty();
        p.merge_json(FIXTURE).unwrap();
        let usage = TokenUsage::new(Some(2000), Some(1000));
        assert_eq!(p.estimate("acme-large-v2", &usage), Some(0.05));
        assert_eq!(p.estimate("acme-small", &usage), Some(0.004));
        assert_eq!(p.estimate("ACME", &usage), Some(0.004));
    }

    #[test]
    fn unknown_model_is_null_and_warns_once() {
        let p = Pricing::with_defaults();
        let usage = TokenUsage::new(Some(10), Some(10));
        assert_eq!(p.estimate("mystery-model", &usage), None);
        assert_eq!(p.estimate("mystery-model", &usage), None);
        assert_eq!(p.warned_count(), 1);
    }

    #[test]
    fn arbitrary_unknown_models_cannot_grow_warning_keys_without_bound() {
        let p = Pricing::empty();
        for i in 0..10_000 {
            assert_eq!(
                p.estimate(&format!("{i}-{}", "é".repeat(500)), &TokenUsage::default()),
                None
            );
        }
        let warned = p.warned.lock().unwrap();
        assert_eq!(warned.len(), MAX_UNKNOWN_MODEL_WARNINGS);
        assert!(warned.iter().all(|s| s.chars().count() <= 128));
    }

    #[test]
    fn defaults_prefer_specific_model() {
        let p = Pricing::with_defaults();
        let mini = p.price_for("gpt-4o-mini-2024-07-18").unwrap();
        assert_eq!(mini.prompt_per_1k, 0.000_15);
    }

    #[test]
    fn missing_tokens_is_null() {
        let p = Pricing::with_defaults();
        assert_eq!(p.estimate("gpt-4o", &TokenUsage::default()), None);
    }
}
