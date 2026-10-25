//! Secret redaction before anything is persisted (#9, #24).
//!
//! Headers containing credentials are dropped. Stored text is scanned with a compiled
//! set of common credential patterns; deployments may add regular expressions at startup.

use hyper::header::HeaderMap;
use regex::{Regex, RegexSet};
use std::sync::OnceLock;

pub const REDACTED: &str = "[REDACTED]";

const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "api-key",
    "x-api-key",
    "openai-api-key",
    "anthropic-api-key",
    "x-goog-api-key",
];

pub fn is_sensitive_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    SENSITIVE_HEADERS.contains(&n.as_str()) || n.contains("token") || n.contains("secret")
}

/// Header names/values safe to persist; sensitive headers are omitted entirely.
pub fn safe_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| !is_sensitive_header(k.as_str()))
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                redact_text(&String::from_utf8_lossy(v.as_bytes())),
            )
        })
        .collect();
    out.sort();
    out
}

// Each prefix identifies a candidate rule without requiring the secret's suffix.
// Keeping prefix and suffix together makes the bounded dispatch scan conservative.
const DEFAULT_PATTERNS: &[(&str, &str)] = &[
    (r"(?i)\bbearer", r#"\s+[^\s"']+"#),
    (r"\b(?:sk|rk)_(?:live|test)_", r"[A-Za-z0-9]{8,}"),
    (r"\bsk-", r"[A-Za-z0-9_-]{13,}"),
    (r"\bAIza", r"[A-Za-z0-9_-]{16,}"),
    (r"\bhf_", r"[A-Za-z0-9_-]{13,}"),
    (r"\bxox[abposr]-", r"[A-Za-z0-9-]{5,}"),
    (r"\bxapp-", r"[A-Za-z0-9-]{8,}"),
    (r"\b(?:AKIA|ASIA)", r"[0-9A-Z]{16}\b"),
    (r"\bgh[pousr]_", r"[A-Za-z0-9]{20,}\b"),
    (r"\bgithub_pat_", r"[A-Za-z0-9_]{20,}\b"),
    (r"\beyJ", r"[\w-]+\.[\w-]+\.[\w-]+"),
    (
        r"(?i)\b(?:password|passwd|secret|api[_-]?key|token)\b",
        r#"["']?\s*[:=]\s*(?:"[^"]*"|'[^']*'|[^\s,;}"']+)"#,
    ),
];

/// Compiled redaction rules reused for every prompt.
#[derive(Clone)]
pub struct Redactor {
    set: RegexSet,
    patterns: Vec<Regex>,
    pem_begin: Regex,
    pem_end: Regex,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new(&[]).expect("built-in redaction patterns are valid")
    }
}

impl Redactor {
    pub fn new(custom_patterns: &[String]) -> Result<Self, regex::Error> {
        let mut sources: Vec<String> = DEFAULT_PATTERNS
            .iter()
            .map(|(prefix, suffix)| format!("{prefix}{suffix}"))
            .collect();
        sources.extend(custom_patterns.iter().cloned());
        // Omit word boundaries in the prefilter so literal prefix searching stays
        // fast on large inputs. The full expressions retain boundary semantics.
        let set = RegexSet::new(
            DEFAULT_PATTERNS
                .iter()
                .map(|(prefix, _)| prefix.replace(r"\b", "")),
        )?;
        let patterns = sources
            .iter()
            .map(|pattern| Regex::new(pattern))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            set,
            patterns,
            pem_begin: Regex::new(r"-----BEGIN(?: [A-Z0-9]+)* PRIVATE KEY-----")?,
            pem_end: Regex::new(r"-----END(?: [A-Z0-9]+)* PRIVATE KEY-----")?,
        })
    }

    /// Mask secrets embedded in free text without per-character temporary allocations.
    pub fn redact(&self, input: &str) -> String {
        self.redact_prefix_bytes(input, input.len())
    }

    /// Keep bounded output while matching against the complete input. Arbitrary
    /// custom rules, JWTs and PEM blocks may extend far beyond the storage boundary;
    /// a fixed lookahead could otherwise expose the start of a secret.
    pub fn redact_prefix(&self, input: &str, max_chars: usize) -> String {
        let end = input
            .char_indices()
            .nth(max_chars)
            .map_or(input.len(), |(i, _)| i);
        let mut output = self.redact_prefix_bytes(input, end);
        let redacted_end = output.char_indices().nth(max_chars).map(|(i, _)| i);
        if let Some(i) = redacted_end {
            output.truncate(i);
        }
        if end < input.len() || redacted_end.is_some() {
            output.push('…');
        }
        output
    }

    fn redact_prefix_bytes(&self, input: &str, end: usize) -> String {
        // Default prefixes are shorter than 64 characters. Include that overlap
        // so a prefix starting just before the output boundary is still detected.
        let probe_end = input[end..]
            .char_indices()
            .nth(64)
            .map_or(input.len(), |(i, _)| end + i);
        let candidates = self.set.matches(&input[..probe_end]);
        let mut ranges = Vec::new();
        // Search delimiters directly rather than scanning a multi-megabyte PEM
        // body with a wildcard expression. Unterminated keys stay masked to EOF.
        let mut pem_cursor = 0;
        while pem_cursor < end {
            let Some(begin) = self.pem_begin.find_at(input, pem_cursor) else {
                break;
            };
            if begin.start() >= end {
                break;
            }
            let secret_end = self
                .pem_end
                .find_at(input, begin.end())
                .map_or(input.len(), |m| m.end());
            ranges.push((begin.start(), secret_end));
            pem_cursor = secret_end;
        }
        for (index, pattern) in self.patterns.iter().enumerate() {
            if index < DEFAULT_PATTERNS.len() && !candidates.matched(index) {
                continue;
            }
            ranges.extend(
                pattern
                    .find_iter(input)
                    .take_while(|m| m.start() < end)
                    .map(|m| (m.start(), m.end())),
            );
        }
        if ranges.is_empty() {
            return input[..end].to_owned();
        }
        ranges.sort_unstable_by_key(|(start, _)| *start);
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            if let Some((_, previous_end)) = merged.last_mut() {
                if start <= *previous_end {
                    *previous_end = (*previous_end).max(end);
                    continue;
                }
            }
            merged.push((start, end));
        }
        let mut out = String::with_capacity(end);
        let mut cursor = 0;
        for (start, end) in merged {
            out.push_str(&input[cursor..start]);
            let secret = &input[start..end];
            if secret
                .get(..7)
                .is_some_and(|s| s.eq_ignore_ascii_case("bearer "))
            {
                out.push_str("Bearer ");
            }
            out.push_str(REDACTED);
            cursor = end;
        }
        if cursor < end {
            out.push_str(&input[cursor..end]);
        }
        out
    }
}

pub(crate) fn default_redactor() -> &'static Redactor {
    static REDACTOR: OnceLock<Redactor> = OnceLock::new();
    REDACTOR.get_or_init(Redactor::default)
}

/// Mask secrets embedded in free text using built-in rules.
pub fn redact_text(input: &str) -> String {
    default_redactor().redact(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    #[test]
    fn strips_sensitive_headers() {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer sk-abc"));
        h.insert("x-api-key", HeaderValue::from_static("k"));
        h.insert("cookie", HeaderValue::from_static("session=1"));
        h.insert("x-auth-token", HeaderValue::from_static("t"));
        h.insert("content-type", HeaderValue::from_static("application/json"));
        assert_eq!(
            safe_headers(&h),
            vec![("content-type".to_string(), "application/json".to_string())]
        );
    }

    #[test]
    fn masks_keys_in_text() {
        let s = "use key sk-proj-ABCDEFGHIJKLMNOPQRST and Bearer eyJhbGciOi.x.y please";
        let r = redact_text(s);
        assert!(!r.contains("sk-proj"), "{r}");
        assert!(!r.contains("eyJhbGciOi"), "{r}");
        assert_eq!(r, "use key [REDACTED] and Bearer [REDACTED] please");
        assert_eq!(redact_text("task-list is fine"), "task-list is fine");
        assert_eq!(redact_text("short sk-1"), "short sk-1");
    }

    #[test]
    fn masks_common_secret_formats_without_false_positives() {
        let samples = [
            "sk_live_51Hxabcdefgh",
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_123456789012345678901234567890",
            "github_pat_123456789012345678901234567890",
            "xoxp-1234567890",
            "xoxa-1234567890",
            "xapp-1234567890",
            "eyJhbGciOiJIUzI1NiJ9.payload.signature",
            "-----BEGIN PRIVATE KEY-----\nsecret\n-----END PRIVATE KEY-----",
            "password=super-secret",
            "api_key: super-secret",
            "rk_test_51Hxabcdefgh",
            "ASIAIOSFODNN7EXAMPLE",
            "gho_123456789012345678901234567890",
            "ghu_123456789012345678901234567890",
            "ghs_123456789012345678901234567890",
            "ghr_123456789012345678901234567890",
            "xoxb-12345",
            "xoxs-1234567890",
            "xoxr-1234567890",
            "hf_1234567890123456",
            "AIza12345678901234567890",
            "-----BEGIN RSA PRIVATE KEY-----\nsecret\n-----END RSA PRIVATE KEY-----",
            "-----BEGIN EC PRIVATE KEY-----\nsecret\n-----END EC PRIVATE KEY-----",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n-----END OPENSSH PRIVATE KEY-----",
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nsecret\n-----END ENCRYPTED PRIVATE KEY-----",
            "-----BEGIN PRIVATE KEY-----\nunclosed secret",
            "passwd=super-secret",
            "secret=super-secret",
            "token: super-secret",
            r#""password": "secret with spaces""#,
        ];
        for sample in samples {
            let redacted = redact_text(sample);
            assert!(!redacted.contains(sample), "{sample}");
            assert!(redacted.contains(REDACTED), "{sample}");
        }
        assert_eq!(redact_text("task-list skeleton"), "task-list skeleton");
    }

    #[test]
    fn applies_custom_patterns() {
        let custom = vec![r"internal-[A-Z0-9]{8}".to_string()];
        let redactor = Redactor::new(&custom).unwrap();
        assert_eq!(redactor.redact("internal-AB12CD34"), REDACTED);
        assert_eq!(redactor.redact("task-list skeleton"), "task-list skeleton");
        assert!(Redactor::new(&["[".into()]).is_err());
    }

    #[test]
    fn custom_unicode_and_overlapping_rules_are_safe() {
        let redactor =
            Redactor::new(&["密钥内容.*".into(), "ABCDEFGHIJKLMNOPQRST".into()]).unwrap();
        assert_eq!(redactor.redact("密钥内容12345"), REDACTED);
        assert_eq!(redactor.redact("sk-proj-ABCDEFGHIJKLMNOPQRST"), REDACTED);
        assert_eq!(
            redactor.redact("密钥内容sk-proj-ABCDEFGHIJKLMNOPQRST"),
            REDACTED
        );
    }

    #[test]
    fn prefix_matching_uses_full_context_for_long_secrets() {
        let redactor = Redactor::new(&[r"private-start[\s\S]*private-end".into()]).unwrap();
        for secret in [
            format!(
                "-----BEGIN PRIVATE KEY-----{}-----END PRIVATE KEY-----",
                "A".repeat(8000)
            ),
            format!("eyJ{}.payload.signature", "A".repeat(8000)),
            format!("private-start{}private-end", "A ".repeat(8000)),
        ] {
            assert_eq!(redactor.redact_prefix(&secret, 4000), "[REDACTED]…");
        }
        assert_eq!(redactor.redact_prefix("你好世界", 2), "你好…");
        assert_eq!(redactor.redact_prefix("你好", 2), "你好");
        assert_eq!(redactor.redact_prefix("", 2), "");
    }

    #[test]
    fn pem_blocks_are_independent_and_nested_markers_stay_masked() {
        let block = "-----BEGIN PRIVATE KEY-----body-----END PRIVATE KEY-----";
        assert_eq!(
            redact_text(&format!("{block} safe {block}")),
            "[REDACTED] safe [REDACTED]"
        );
        assert_eq!(
            redact_text(&format!("-----BEGIN PRIVATE KEY-----{block} safe")),
            "[REDACTED] safe"
        );
        assert_eq!(redact_text("Bearer\u{2003}secret"), "[REDACTED]");
        assert_eq!(redact_text("api_Key=secret"), "[REDACTED]");
    }
}
