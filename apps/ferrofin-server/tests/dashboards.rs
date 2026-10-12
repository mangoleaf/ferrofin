//! The Grafana dashboards in `contrib/metrics/` against the metrics Ferrofin
//! actually exposes, and the Helm chart's copies of them.
//!
//! - Every dashboard parses, has a unique uid, unique panel ids and a
//!   `datasource` variable.
//! - Every metric name in every PromQL expression (panel targets and
//!   `label_values(...)` variables) is one Ferrofin exposes: the union of each
//!   instrument-owning module's `METRIC_NAMES` list (each module's own tests
//!   assert its list is exactly what it exports), histograms with their
//!   `_bucket`/`_sum`/`_count` series. A dashboard can no longer silently chart
//!   a metric that does not exist.
//! - Every label a matcher or a `by (...)` clause names is one Ferrofin (or the
//!   Prometheus target: `job`, `instance`) puts on a series.
//! - Every metric selector is scoped to the `$job` variable, so a dashboard
//!   pointed at a Prometheus that also scrapes a Jellyfin (or two Ferrofins)
//!   never mixes them.
//! - `charts/ferrofin/dashboards/` holds byte-equal copies of exactly those
//!   dashboards (Helm reads only files inside the chart). Fix a drift with
//!   `cp contrib/metrics/grafana-*.json charts/ferrofin/dashboards/`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The repository root (this crate lives in `apps/ferrofin-server`).
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The `grafana-*.json` files in `dir`, by file name.
fn dashboards_in(dir: &Path) -> BTreeMap<String, String> {
    std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let json = Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
            (name.starts_with("grafana-") && json).then(|| {
                let text = std::fs::read_to_string(entry.path()).expect("read dashboard");
                (name, text)
            })
        })
        .collect()
}

/// Every metric name Ferrofin exposes, plus `up`, the series Prometheus adds
/// for every scrape target.
fn exposed() -> BTreeSet<String> {
    let mut names = BTreeSet::from(["up".to_owned()]);
    for list in [
        ferrofin_metrics::METRIC_NAMES,
        ferrofin_server::metrics_wiring::GAUGE_NAMES,
        ferrofin_core::scan_metrics::METRIC_NAMES,
        ferrofin_providers::metrics::METRIC_NAMES,
    ] {
        for name in list {
            names.insert((*name).to_owned());
            for suffix in ["_bucket", "_sum", "_count"] {
                names.insert(format!("{name}{suffix}"));
            }
        }
    }
    names
}

/// Every label name Ferrofin's series carry, plus the target labels
/// Prometheus adds and the histogram bucket label.
const LABELS: &[&str] = &[
    // Prometheus target labels.
    "job",
    "instance",
    // Histograms.
    "le",
    // http_* (prometheus-net parity).
    "code",
    "method",
    "controller",
    "action",
    "page",
    "endpoint",
    // Sampler-fed gauges.
    "pool",
    "type",
    // Library scans, probes, providers.
    "trigger",
    "result",
    "outcome",
    "pass",
    "provider",
];

/// The PromQL keywords and grouping modifiers that look like identifiers.
const KEYWORDS: &[&str] = &[
    "by",
    "without",
    "on",
    "ignoring",
    "group_left",
    "group_right",
    "bool",
    "and",
    "or",
    "unless",
    "offset",
    "atan2",
    "inf",
    "nan",
];

/// The aggregation operators — never metric names, and followed by
/// `by (...)` as often as by `(`.
const AGGREGATIONS: &[&str] = &[
    "sum",
    "min",
    "max",
    "avg",
    "group",
    "stddev",
    "stdvar",
    "count",
    "count_values",
    "bottomk",
    "topk",
    "quantile",
    "limitk",
    "limit_ratio",
];

/// What one PromQL expression references.
#[derive(Debug, Default)]
struct Refs {
    metrics: BTreeSet<String>,
    labels: BTreeSet<String>,
    /// Metrics selected without a `job` matcher.
    unscoped: BTreeSet<String>,
}

/// Collects the metric names and label names `expr` references — a small
/// tokenizer, not a parser: string literals, range selectors and Grafana
/// variables are skipped, the label names of `{...}` matchers and of
/// `by`/`without`/`on`/`ignoring`/`group_*` lists are collected, and every
/// other identifier not followed by `(` (a function or aggregation) is a metric.
fn references(expr: &str) -> Refs {
    let chars: Vec<char> = expr.chars().collect();
    let mut refs = Refs::default();
    let mut i = 0;
    let ident_start = |c: char| c.is_ascii_alphabetic() || c == '_' || c == ':';
    let ident_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == ':';
    let mut last_keyword: Option<String> = None;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' | '\'' | '`' => i = skip_quoted(&chars, i),
            '[' => {
                while i < chars.len() && chars[i] != ']' {
                    i += 1;
                }
                i += 1;
            }
            '{' => {
                // Label matchers: `name op "value"`, comma-separated.
                i += 1;
                while i < chars.len() && chars[i] != '}' {
                    if chars[i] == '"' {
                        i = skip_quoted(&chars, i);
                    } else if ident_start(chars[i]) {
                        let start = i;
                        while i < chars.len() && ident_char(chars[i]) {
                            i += 1;
                        }
                        refs.labels.insert(chars[start..i].iter().collect());
                    } else {
                        i += 1;
                    }
                }
                i += 1;
            }
            '$' => {
                // A Grafana variable (`$__interval`, `${datasource}`).
                i += 1;
                while i < chars.len()
                    && (ident_char(chars[i]) || chars[i] == '{' || chars[i] == '}')
                {
                    i += 1;
                }
            }
            '(' if last_keyword.is_some() => {
                // The label list of a grouping modifier.
                i += 1;
                while i < chars.len() && chars[i] != ')' {
                    if ident_start(chars[i]) {
                        let start = i;
                        while i < chars.len() && ident_char(chars[i]) {
                            i += 1;
                        }
                        refs.labels.insert(chars[start..i].iter().collect());
                    } else {
                        i += 1;
                    }
                }
                i += 1;
                last_keyword = None;
            }
            c if c.is_ascii_digit() || c == '.' => {
                // A number, possibly `1e9` or a duration.
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                    i += 1;
                }
            }
            c if ident_start(c) => {
                let start = i;
                while i < chars.len() && ident_char(chars[i]) {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let next = chars[i..].iter().find(|c| !c.is_whitespace()).copied();
                let lower = word.to_ascii_lowercase();
                if KEYWORDS.contains(&lower.as_str()) {
                    last_keyword = matches!(
                        lower.as_str(),
                        "by" | "without" | "on" | "ignoring" | "group_left" | "group_right"
                    )
                    .then_some(lower);
                } else if next != Some('(') && !AGGREGATIONS.contains(&lower.as_str()) {
                    if !has_job_matcher(&chars[i..]) {
                        refs.unscoped.insert(word.clone());
                    }
                    refs.metrics.insert(word);
                }
            }
            _ => i += 1,
        }
    }
    refs
}

/// The index just past the string literal that opens at `start` (its quote
/// character), backslash escapes included.
fn skip_quoted(chars: &[char], start: usize) -> usize {
    let quote = chars[start];
    let mut i = start + 1;
    while i < chars.len() && chars[i] != quote {
        if chars[i] == '\\' {
            i += 1;
        }
        i += 1;
    }
    i + 1
}

/// Whether the selector that `rest` (the text right after a metric name)
/// opens carries exactly the `job=~"$job"` matcher (whitespace aside).
fn has_job_matcher(rest: &[char]) -> bool {
    let rest: String = rest.iter().collect();
    rest.trim_start().strip_prefix('{').is_some_and(|body| {
        body.split('}')
            .next()
            .unwrap_or_default()
            .split(',')
            .any(|matcher| {
                matcher
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>()
                    == r#"job=~"$job""#
            })
    })
}

/// Every PromQL expression of a dashboard — panel targets (rows' nested panels
/// included) and the selectors of `label_values(...)` variables, each with
/// whether it must be scoped to `$job` (all but the `job` variable's own
/// query) — and the label names those variables read.
fn expressions(dashboard: &serde_json::Value) -> (Vec<(String, bool)>, Vec<String>) {
    fn panels(list: &serde_json::Value, out: &mut Vec<(String, bool)>) {
        for panel in list.as_array().into_iter().flatten() {
            for target in panel["targets"].as_array().into_iter().flatten() {
                if let Some(expr) = target["expr"].as_str() {
                    out.push((expr.to_owned(), true));
                }
            }
            panels(&panel["panels"], out);
        }
    }
    let mut out = Vec::new();
    let mut labels = Vec::new();
    panels(&dashboard["panels"], &mut out);
    for variable in dashboard["templating"]["list"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let query = variable["query"]["query"]
            .as_str()
            .or_else(|| variable["query"].as_str());
        if variable["type"] == "query"
            && let Some(query) = query
            && let Some(args) = query
                .strip_prefix("label_values(")
                .and_then(|q| q.strip_suffix(')'))
        {
            // `label_values(selector, label)`: the selector is PromQL, the
            // label a label name.
            let (selector, label) = args.rsplit_once(',').unwrap_or(("", args));
            out.push((selector.trim().to_owned(), variable["name"] != "job"));
            labels.push(label.trim().to_owned());
        }
    }
    (out, labels)
}

#[test]
fn the_tokenizer_finds_metrics_and_labels() {
    let refs = references(
        r#"histogram_quantile(0.95, sum by (le, trigger) (increase(ferrofin_library_scan_duration_seconds_bucket{job=~"$job", trigger=~"a|b"}[$__interval]))) / on() group_left() (sum(rate(x_total[5m])) > 0) or vector(0) * 1e9"#,
    );
    assert_eq!(
        refs.metrics,
        BTreeSet::from([
            "ferrofin_library_scan_duration_seconds_bucket".to_owned(),
            "x_total".to_owned()
        ])
    );
    assert_eq!(
        refs.labels,
        BTreeSet::from(["le".to_owned(), "trigger".to_owned(), "job".to_owned()])
    );
    assert_eq!(refs.unscoped, BTreeSet::from(["x_total".to_owned()]));
    // Only the dashboard variable counts as scoping: a fixed job, a job
    // regex or a lookalike label does not.
    for selector in [
        r#"m{job="ferrofin"}"#,
        r#"m{job=~".*"}"#,
        r#"m{jobs=~"$job"}"#,
        r#"m{job!~"$job"}"#,
    ] {
        assert_eq!(
            references(selector).unscoped,
            BTreeSet::from(["m".to_owned()]),
            "{selector}"
        );
    }
    assert!(
        references(r#"m{instance="a", job =~ "$job"}"#)
            .unscoped
            .is_empty()
    );
}

#[test]
fn every_dashboard_charts_only_metrics_ferrofin_exposes() {
    let dashboards = dashboards_in(&repo().join("contrib/metrics"));
    assert!(
        dashboards.contains_key("grafana-library-scans.json"),
        "the library-scans dashboard is missing: {:?}",
        dashboards.keys()
    );
    let exposed = exposed();
    let mut uids = BTreeSet::new();
    let mut problems = Vec::new();
    for (file, text) in &dashboards {
        let dashboard: serde_json::Value =
            serde_json::from_str(text).unwrap_or_else(|e| panic!("{file} is not JSON: {e}"));
        let uid = dashboard["uid"].as_str().expect("uid").to_owned();
        assert!(uids.insert(uid.clone()), "{file}: duplicate uid {uid}");
        assert!(
            dashboard["templating"]["list"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|v| v["name"] == "datasource" && v["type"] == "datasource"),
            "{file}: no `datasource` variable"
        );
        // Panel ids are unique across the dashboard, the panels nested in
        // collapsed rows included.
        let mut ids = BTreeSet::new();
        let mut stack: Vec<&serde_json::Value> = dashboard["panels"]
            .as_array()
            .expect("panels")
            .iter()
            .collect();
        while let Some(panel) = stack.pop() {
            let id = panel["id"].as_u64().expect("panel id");
            assert!(ids.insert(id), "{file}: duplicate panel id {id}");
            stack.extend(panel["panels"].as_array().into_iter().flatten());
        }
        let (exprs, variable_labels) = expressions(&dashboard);
        assert!(!exprs.is_empty(), "{file}: no PromQL found");
        for label in variable_labels {
            if !LABELS.contains(&label.as_str()) {
                problems.push(format!("{file}: a variable reads unknown label `{label}`"));
            }
        }
        assert!(
            dashboard["templating"]["list"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|v| v["name"] == "job" && v.get("allValue").is_none()),
            "{file}: no `job` variable, or one whose All is a wildcard"
        );
        for (expr, scoped) in exprs {
            let refs = references(&expr);
            for metric in &refs.metrics {
                if !exposed.contains(metric) {
                    problems.push(format!("{file}: unknown metric `{metric}` in `{expr}`"));
                }
            }
            for metric in refs.unscoped.iter().filter(|_| scoped) {
                problems.push(format!(
                    "{file}: `{metric}` is selected without `job=~\"$job\"` in `{expr}`"
                ));
            }
            for label in &refs.labels {
                if !LABELS.contains(&label.as_str()) {
                    problems.push(format!("{file}: unknown label `{label}` in `{expr}`"));
                }
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_chart_ships_byte_equal_copies_of_the_dashboards() {
    let source = dashboards_in(&repo().join("contrib/metrics"));
    let chart = dashboards_in(&repo().join("charts/ferrofin/dashboards"));
    assert_eq!(
        source.keys().collect::<Vec<_>>(),
        chart.keys().collect::<Vec<_>>(),
        "charts/ferrofin/dashboards/ must hold exactly contrib/metrics/grafana-*.json \
         (cp contrib/metrics/grafana-*.json charts/ferrofin/dashboards/)"
    );
    for (file, text) in &source {
        assert!(
            chart[file] == *text,
            "charts/ferrofin/dashboards/{file} drifted from contrib/metrics/{file}; \
             run: cp contrib/metrics/grafana-*.json charts/ferrofin/dashboards/"
        );
    }
}
