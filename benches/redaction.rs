//! Run `cargo bench --bench redaction` for the 10 MiB prompt regression.
use rust_llm_logger::{redact::Redactor, telemetry::MAX_STORED_PROMPT_CHARS};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

fn main() {
    let redactor = Redactor::default();
    let size = 10 * 1024 * 1024;
    for (name, prompt) in [
        ("plain", "x".repeat(size)),
        (
            "credential",
            format!("password=hidden {}", "x".repeat(size - 16)),
        ),
        (
            "PEM",
            format!(
                "-----BEGIN PRIVATE KEY-----{}-----END PRIVATE KEY-----",
                "A".repeat(size - 51)
            ),
        ),
    ] {
        // Compile the regex engines' lazy state before measuring steady state.
        black_box(redactor.redact_prefix(&prompt, MAX_STORED_PROMPT_CHARS));
        let mut best = Duration::MAX;
        for _ in 0..5 {
            let start = Instant::now();
            let output = redactor.redact_prefix(black_box(&prompt), MAX_STORED_PROMPT_CHARS);
            best = best.min(start.elapsed());
            assert!(output.chars().count() <= MAX_STORED_PROMPT_CHARS + 1);
            black_box(output);
        }
        println!(
            "{name}: {:.3} ms for a 10 MiB prompt",
            best.as_secs_f64() * 1000.0
        );
        if !cfg!(debug_assertions) {
            assert!(best < Duration::from_millis(50));
        }
        let start = Instant::now();
        black_box(redactor.redact(black_box(&prompt)));
        let elapsed = start.elapsed();
        println!(
            "{name}: {:.3} ms without output truncation",
            elapsed.as_secs_f64() * 1000.0
        );
        if !cfg!(debug_assertions) {
            assert!(elapsed < Duration::from_millis(50));
        }
    }
}
