//! Golden + invariant tests for the Prometheus renderer.

use std::collections::BTreeMap;

use sozu_command_lib::proto::command::{
    filtered_metrics::Inner, AggregatedMetrics, BackendMetrics, Bucket, ClusterMetrics,
    FilteredHistogram, FilteredMetrics, Percentiles,
};

fn gauge(v: u64) -> FilteredMetrics {
    FilteredMetrics {
        inner: Some(Inner::Gauge(v)),
    }
}

fn count(v: i64) -> FilteredMetrics {
    FilteredMetrics {
        inner: Some(Inner::Count(v)),
    }
}

fn percentiles() -> FilteredMetrics {
    FilteredMetrics {
        inner: Some(Inner::Percentiles(Percentiles {
            samples: 100,
            p_50: 5,
            p_90: 9,
            p_99: 20,
            p_99_9: 30,
            p_99_99: 40,
            p_99_999: 50,
            p_100: 60,
            sum: 800,
        })),
    }
}

fn histogram() -> FilteredMetrics {
    // Cumulative bucket counts (Sōzu stores them this way): ≤0, ≤1, ≤3, ≤7 ...
    FilteredMetrics {
        inner: Some(Inner::Histogram(FilteredHistogram {
            sum: 1234,
            count: 50,
            buckets: vec![
                Bucket { le: 0, count: 10 },
                Bucket { le: 1, count: 25 },
                Bucket { le: 3, count: 40 },
                Bucket { le: 7, count: 48 },
            ],
        })),
    }
}

/// A metric kind Sōzu never emits — must be skipped, not rendered.
fn time(v: u64) -> FilteredMetrics {
    FilteredMetrics {
        inner: Some(Inner::Time(v)),
    }
}

fn sample_metrics() -> AggregatedMetrics {
    let proxying = BTreeMap::from([
        ("connections_active".to_string(), gauge(7)),
        ("requests".to_string(), count(1000)),
        ("ignored_time".to_string(), time(42)),
    ]);
    let main = BTreeMap::from([("config.reloads".to_string(), count(3))]);

    let cluster = ClusterMetrics {
        cluster: BTreeMap::from([
            ("requests".to_string(), count(500)),
            ("response_time".to_string(), percentiles()),
            ("response_time_histogram".to_string(), histogram()),
        ]),
        backends: vec![BackendMetrics {
            backend_id: "demo-backend-0".to_string(),
            metrics: BTreeMap::from([
                ("bytes_out".to_string(), count(2048)),
                ("connections".to_string(), gauge(2)),
            ]),
        }],
    };
    let clusters = BTreeMap::from([("demo/app".to_string(), cluster)]);

    AggregatedMetrics {
        main,
        workers: BTreeMap::new(),
        clusters,
        proxying,
    }
}

#[test]
fn golden_exposition() {
    insta::assert_snapshot!(sozu_gw_prometheus::render(&sample_metrics()));
}

#[test]
fn well_formed() {
    let out = sozu_gw_prometheus::render(&sample_metrics());

    // Every metric name is prefixed and label-keyed as expected.
    assert!(out.contains("# TYPE sozu_requests counter"));
    assert!(out.contains("sozu_requests 1000\n"));
    assert!(out.contains("sozu_requests{cluster_id=\"demo/app\"} 500\n"));
    assert!(out.contains("sozu_connections_active 7\n"));
    assert!(out.contains("sozu_config_reloads{process=\"main\"} 3\n"));
    assert!(out
        .contains("sozu_bytes_out{cluster_id=\"demo/app\",backend_id=\"demo-backend-0\"} 2048\n"));

    // Histogram: cumulative buckets verbatim + mandatory +Inf == _count.
    assert!(out.contains("# TYPE sozu_response_time_histogram histogram"));
    assert!(
        out.contains("sozu_response_time_histogram_bucket{cluster_id=\"demo/app\",le=\"7\"} 48\n")
    );
    assert!(out
        .contains("sozu_response_time_histogram_bucket{cluster_id=\"demo/app\",le=\"+Inf\"} 50\n"));
    assert!(out.contains("sozu_response_time_histogram_sum{cluster_id=\"demo/app\"} 1234\n"));
    assert!(out.contains("sozu_response_time_histogram_count{cluster_id=\"demo/app\"} 50\n"));

    // Percentiles -> summary with quantile labels + _sum/_count.
    assert!(out.contains("# TYPE sozu_response_time summary"));
    assert!(out.contains("sozu_response_time{cluster_id=\"demo/app\",quantile=\"0.99\"} 20\n"));
    assert!(out.contains("sozu_response_time_count{cluster_id=\"demo/app\"} 100\n"));

    // Time / TimeSerie are skipped entirely.
    assert!(!out.contains("ignored_time"));

    // No duplicated HELP/TYPE lines for any family.
    let type_lines: Vec<_> = out.lines().filter(|l| l.starts_with("# TYPE ")).collect();
    let mut deduped = type_lines.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(type_lines.len(), deduped.len(), "duplicate # TYPE lines");
}
