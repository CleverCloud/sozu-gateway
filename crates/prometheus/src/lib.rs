//! Render Sōzu `AggregatedMetrics` as Prometheus text exposition format (v0.0.4).
//!
//! Pure and I/O-free: it only borrows the protobuf types from `sozu-command-lib`,
//! mirroring the `translator` crate's purity. The controller calls [`render`]
//! with the `AggregatedMetrics` it pulls over the command socket (a
//! `QueryMetrics` request) and serves the result at `/metrics`. Sōzu has no
//! native `/metrics` endpoint, but its metric model is Prometheus-shaped.
//!
//! Mapping (every name prefixed `sozu_`; bytes outside `[A-Za-z0-9_]` → `_`):
//! - `Gauge`       → gauge
//! - `Count`       → counter (kept faithful to Sōzu's name — no forced `_total`)
//! - `Histogram`   → histogram. Sōzu's bucket counts are already **cumulative**
//!   (count of observations ≤ `le`; the data plane stores them that way — see
//!   `print_histograms` in sozu-command-lib, which subtracts the previous bucket
//!   to recover per-bucket counts), so they map straight onto Prometheus
//!   `_bucket{le}`, with an added `+Inf` bucket equal to the total count.
//! - `Percentiles` → summary (one series per quantile, plus `_sum`/`_count`).
//!   ⚠ Sōzu cannot statistically merge percentiles across workers (it takes the
//!   element-wise max); the companion `*_histogram` is the accurate source.
//! - `Time` / `TimeSerie` → skipped (never emitted by Sōzu).
//!
//! Label sets: proxy metrics have none; main-process metrics carry
//! `process="main"` (so they never collide with an identically-named proxy
//! series); cluster metrics carry `cluster_id`; backend metrics carry
//! `cluster_id` + `backend_id`.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;

use sozu_command_lib::proto::command::{
    filtered_metrics::Inner, AggregatedMetrics, FilteredHistogram, FilteredMetrics, Percentiles,
};

/// One Prometheus metric family: its type plus every series (one per label set,
/// across proxy / main / cluster / backend) sharing the same base name.
struct Family {
    mtype: &'static str,
    /// `(sort_key, rendered_lines)` per series — sorted on output for
    /// deterministic exposition (golden-snapshot friendly).
    series: Vec<(String, String)>,
}

/// Render `metrics` as a Prometheus text-format exposition document.
pub fn render(metrics: &AggregatedMetrics) -> String {
    let mut families: BTreeMap<String, Family> = BTreeMap::new();

    // Proxy metrics, already merged across workers — no labels.
    for (name, fm) in &metrics.proxying {
        add_metric(&mut families, name, &[], fm);
    }
    // Main (master) process metrics — tagged so they never alias a proxy series.
    for (name, fm) in &metrics.main {
        add_metric(&mut families, name, &[("process", "main")], fm);
    }
    // Per-cluster and per-backend metrics.
    for (cluster_id, cm) in &metrics.clusters {
        for (name, fm) in &cm.cluster {
            add_metric(&mut families, name, &[("cluster_id", cluster_id)], fm);
        }
        for backend in &cm.backends {
            let labels = [
                ("cluster_id", cluster_id.as_str()),
                ("backend_id", backend.backend_id.as_str()),
            ];
            for (name, fm) in &backend.metrics {
                add_metric(&mut families, name, &labels, fm);
            }
        }
    }

    let mut out = String::new();
    for (base, mut family) in families {
        let _ = writeln!(out, "# HELP {base} Sozu data-plane metric.");
        let _ = writeln!(out, "# TYPE {base} {}", family.mtype);
        family.series.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, lines) in family.series {
            out.push_str(&lines);
        }
    }
    out
}

fn add_metric(
    families: &mut BTreeMap<String, Family>,
    raw_name: &str,
    labels: &[(&str, &str)],
    fm: &FilteredMetrics,
) {
    let base = sanitize(raw_name);
    match &fm.inner {
        Some(Inner::Gauge(v)) => push_scalar(families, base, "gauge", labels, &v.to_string()),
        Some(Inner::Count(v)) => push_scalar(families, base, "counter", labels, &v.to_string()),
        Some(Inner::Histogram(h)) => push_histogram(families, base, labels, h),
        Some(Inner::Percentiles(p)) => push_summary(families, base, labels, p),
        // Time / TimeSerie are never emitted by Sōzu; None carries nothing.
        Some(Inner::Time(_)) | Some(Inner::TimeSerie(_)) | None => {}
    }
}

/// `sozu_` followed by `raw` with every non-`[A-Za-z0-9_]` char turned into `_`.
fn sanitize(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len() + 5);
    s.push_str("sozu_");
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            s.push(c);
        } else {
            s.push('_');
        }
    }
    s
}

fn push_scalar(
    families: &mut BTreeMap<String, Family>,
    base: String,
    mtype: &'static str,
    labels: &[(&str, &str)],
    value: &str,
) {
    let lset = fmt_labels(labels, &[]);
    let line = format!("{base}{lset} {value}\n");
    insert(families, base, mtype, lset, line);
}

fn push_histogram(
    families: &mut BTreeMap<String, Family>,
    base: String,
    labels: &[(&str, &str)],
    h: &FilteredHistogram,
) {
    let mut lines = String::new();
    // Sōzu bucket counts are already cumulative, so they are valid Prometheus
    // `le` buckets verbatim.
    for b in &h.buckets {
        let le = b.le.to_string();
        let lset = fmt_labels(labels, &[("le", &le)]);
        let _ = writeln!(lines, "{base}_bucket{lset} {}", b.count);
    }
    // The mandatory +Inf bucket equals the total observation count.
    let inf = fmt_labels(labels, &[("le", "+Inf")]);
    let _ = writeln!(lines, "{base}_bucket{inf} {}", h.count);
    let lset = fmt_labels(labels, &[]);
    let _ = writeln!(lines, "{base}_sum{lset} {}", h.sum);
    let _ = writeln!(lines, "{base}_count{lset} {}", h.count);
    insert(families, base, "histogram", lset, lines);
}

fn push_summary(
    families: &mut BTreeMap<String, Family>,
    base: String,
    labels: &[(&str, &str)],
    p: &Percentiles,
) {
    let mut lines = String::new();
    for (quantile, value) in [
        ("0.5", p.p_50),
        ("0.9", p.p_90),
        ("0.99", p.p_99),
        ("0.999", p.p_99_9),
        ("0.9999", p.p_99_99),
        ("0.99999", p.p_99_999),
        ("1", p.p_100),
    ] {
        let lset = fmt_labels(labels, &[("quantile", quantile)]);
        let _ = writeln!(lines, "{base}{lset} {value}");
    }
    let lset = fmt_labels(labels, &[]);
    let _ = writeln!(lines, "{base}_sum{lset} {}", p.sum);
    let _ = writeln!(lines, "{base}_count{lset} {}", p.samples);
    insert(families, base, "summary", lset, lines);
}

fn insert(
    families: &mut BTreeMap<String, Family>,
    base: String,
    mtype: &'static str,
    sort_key: String,
    lines: String,
) {
    families
        .entry(base)
        .or_insert_with(|| Family {
            mtype,
            series: Vec::new(),
        })
        .series
        .push((sort_key, lines));
}

/// Format a label set as `{k1="v1",k2="v2"}` (empty string when no labels).
/// `extra` is appended after `base` — used for the synthetic `le` / `quantile`.
fn fmt_labels(base: &[(&str, &str)], extra: &[(&str, &str)]) -> String {
    if base.is_empty() && extra.is_empty() {
        return String::new();
    }
    let mut s = String::from("{");
    for (i, (k, v)) in base.iter().chain(extra.iter()).enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{k}=\"{}\"", escape(v));
    }
    s.push('}');
    s
}

/// Escape a label value per the Prometheus text format (`\`, `"`, newline).
fn escape(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            _ => s.push(c),
        }
    }
    s
}
