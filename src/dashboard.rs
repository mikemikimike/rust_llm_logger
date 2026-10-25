//! `GET /` recent-calls dashboard (#6). Read-only; never touches the proxy path.

use crate::store::CallRow;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "—".into())
}

pub fn render(rows: &[CallRow], model_filter: Option<&str>) -> String {
    render_with_prompts(rows, model_filter, true)
}

pub fn render_with_prompts(
    rows: &[CallRow],
    model_filter: Option<&str>,
    show_prompts: bool,
) -> String {
    let total = rows.len();
    let errors = rows.iter().filter(|r| r.outcome != "ok").count();
    let err_rate = if total == 0 {
        0.0
    } else {
        errors as f64 * 100.0 / total as f64
    };
    let mut body = String::new();
    if rows.is_empty() {
        body.push_str(r#"<p class="empty">No calls recorded yet. Send a request through <code>/proxy/{port}/…</code>.</p>"#);
    } else {
        body.push_str("<table><thead><tr><th>Time (UTC)</th><th>Model</th><th>Status</th><th>Outcome</th><th>Prompt tok</th><th>Completion tok</th><th>Latency ms</th><th>TTFT ms</th><th>Cost USD</th><th>Trace</th><th>Prompt</th></tr></thead><tbody>");
        for r in rows {
            let prompt: String = if show_prompts {
                r.prompt.chars().take(120).collect()
            } else {
                "[hidden]".into()
            };
            body.push_str(&format!(
                "<tr class=\"{cls}\"><td>{ts}</td><td><a href=\"/?model={m_url}\">{m}</a></td><td>{st}</td><td>{oc}</td><td>{pt}</td><td>{ct}</td><td>{lat}</td><td>{tt}</td><td>{cost}</td><td><code>{tr}</code></td><td>{pr}</td></tr>",
                cls = if r.outcome == "ok" { "ok" } else { "err" },
                ts = esc(&r.ts),
                m_url = esc(&r.model.replace(' ', "%20")),
                m = esc(&r.model),
                st = r.status,
                oc = esc(&r.outcome),
                pt = opt(r.prompt_tokens),
                ct = opt(r.completion_tokens),
                lat = r.latency_ms,
                tt = opt(r.ttft_ms),
                cost = r.estimated_cost_usd.map(|c| format!("{c:.6}")).unwrap_or_else(|| "—".into()),
                tr = esc(r.trace_id.as_deref().unwrap_or("—")),
                pr = esc(&prompt),
            ));
        }
        body.push_str("</tbody></table>");
    }
    let filter = model_filter
        .map(|m| format!(" · model <b>{}</b> (<a href=\"/\">clear</a>)", esc(m)))
        .unwrap_or_default();
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>rust_llm_logger</title>
<meta http-equiv="refresh" content="10">
<style>body{{font:14px system-ui,sans-serif;margin:1.5rem}}table{{border-collapse:collapse;width:100%}}
th,td{{border-bottom:1px solid #ddd;padding:.3rem .5rem;text-align:left;vertical-align:top}}
tr.err td{{background:#fff1f0}}code{{font-size:12px}}.empty{{color:#666}}</style></head>
<body><h1>Recent LLM calls</h1><p>{total} shown · error rate {err_rate:.1}%{filter} · <a href="/metrics">/metrics</a></p>{body}</body></html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_state() {
        let html = render(&[], None);
        assert!(html.contains("No calls recorded yet"));
    }

    #[test]
    fn escapes_prompt() {
        let row = CallRow {
            ts: "t".into(),
            model: "m".into(),
            prompt: "<script>x</script>".into(),
            prompt_tokens: Some(1),
            completion_tokens: None,
            latency_ms: 1,
            ttft_ms: None,
            status: 200,
            outcome: "ok".into(),
            estimated_cost_usd: None,
            trace_id: None,
        };
        let html = render(&[row], Some("m"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>x"));
    }
}
