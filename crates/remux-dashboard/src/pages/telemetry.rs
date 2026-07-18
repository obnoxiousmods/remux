use crate::{
    components::{Card, EmptyState, LoadingText},
    state::AppState,
};
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use remux_sdks::remux::{
    DeleteTelemetryView, GetTelemetryExplore, GetTelemetryViews, SaveTelemetryView,
    TelemetryBreakdownRow, TelemetryExploreResponse, TelemetryFilterOptions,
    TelemetryRequestEvent, TelemetrySavedView, TelemetrySeriesPoint, TelemetryStats,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

const SERIES_COLORS: [&str; 10] = [
    "#8b5cf6", "#06b6d4", "#22c55e", "#f59e0b", "#ef4444", "#ec4899", "#3b82f6",
    "#84cc16", "#f97316", "#14b8a6",
];
const TABLE_COLUMNS: [(&str, &str); 8] = [
    ("count", "Requests"),
    ("errors", "Errors"),
    ("errorRate", "Error %"),
    ("mean", "Mean"),
    ("p50", "p50"),
    ("p95", "p95"),
    ("p99", "p99"),
    ("max", "Max"),
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct QueryConfig {
    hours: i64,
    bucket_minutes: i64,
    group_by: String,
    route: String,
    device: String,
    client: String,
    user: String,
    content: String,
    method: String,
    status_class: String,
    sample_reason: String,
    sort_by: String,
    sort_dir: String,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            hours: 24,
            bucket_minutes: 30,
            group_by: "route".into(),
            route: String::new(),
            device: String::new(),
            client: String::new(),
            user: String::new(),
            content: String::new(),
            method: String::new(),
            status_class: String::new(),
            sample_reason: String::new(),
            sort_by: "p95".into(),
            sort_dir: "desc".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ViewConfig {
    hours: i64,
    bucket_minutes: i64,
    group_by: String,
    metric: String,
    route: String,
    device: String,
    client: String,
    user: String,
    content: String,
    method: String,
    status_class: String,
    sample_reason: String,
    sort_by: String,
    sort_dir: String,
    chart_type: String,
    series_limit: usize,
    show_points: bool,
    compare_previous: bool,
    auto_refresh_secs: u32,
    columns: Vec<String>,
}

impl Default for ViewConfig {
    fn default() -> Self {
        let query = QueryConfig::default();
        Self {
            hours: query.hours,
            bucket_minutes: query.bucket_minutes,
            group_by: query.group_by,
            metric: "p95".into(),
            route: query.route,
            device: query.device,
            client: query.client,
            user: query.user,
            content: query.content,
            method: query.method,
            status_class: query.status_class,
            sample_reason: query.sample_reason,
            sort_by: query.sort_by,
            sort_dir: query.sort_dir,
            chart_type: "line".into(),
            series_limit: 6,
            show_points: false,
            compare_previous: false,
            auto_refresh_secs: 0,
            columns: TABLE_COLUMNS
                .iter()
                .map(|(key, _)| (*key).to_string())
                .collect(),
        }
    }
}

impl ViewConfig {
    fn query(&self) -> QueryConfig {
        QueryConfig {
            hours: self.hours,
            bucket_minutes: self.bucket_minutes,
            group_by: self
                .group_by
                .clone(),
            route: self
                .route
                .clone(),
            device: self
                .device
                .clone(),
            client: self
                .client
                .clone(),
            user: self
                .user
                .clone(),
            content: self
                .content
                .clone(),
            method: self
                .method
                .clone(),
            status_class: self
                .status_class
                .clone(),
            sample_reason: self
                .sample_reason
                .clone(),
            sort_by: self
                .sort_by
                .clone(),
            sort_dir: self
                .sort_dir
                .clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct ChartSeries {
    label: String,
    values: Vec<Option<f64>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ChartModel {
    buckets: Vec<String>,
    series: Vec<ChartSeries>,
    max: f64,
}

fn stats_metric(stats: &TelemetryStats, key: &str) -> f64 {
    match key {
        "count" => stats.count as f64,
        "errors" => stats.error_count as f64,
        "errorRate" => stats.error_rate,
        "mean" => stats.mean_latency_ms,
        "p50" => stats.p50_latency_ms,
        "p99" => stats.p99_latency_ms,
        "max" => stats.max_latency_ms,
        _ => stats.p95_latency_ms,
    }
}

fn metric_label(key: &str) -> &'static str {
    match key {
        "count" => "Request count",
        "errors" => "Error count",
        "errorRate" => "Error rate",
        "mean" => "Mean latency",
        "p50" => "p50 latency",
        "p99" => "p99 latency",
        "max" => "Maximum latency",
        _ => "p95 latency",
    }
}

fn metric_value(value: f64, key: &str) -> String {
    if key == "errorRate" {
        format!("{value:.1}%")
    } else if key == "count" || key == "errors" {
        format_count(value.round() as i64)
    } else if value >= 1_000.0 {
        format!("{:.2} s", value / 1_000.0)
    } else {
        format!("{value:.1} ms")
    }
}

fn format_count(value: i64) -> String {
    let negative = value < 0;
    let digits = value
        .unsigned_abs()
        .to_string();
    let mut output = String::new();
    for (index, ch) in digits
        .chars()
        .enumerate()
    {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            output.push(',');
        }
        output.push(ch);
    }
    if negative {
        format!("-{output}")
    } else {
        output
    }
}

fn short_bucket(value: &str, hours: i64) -> String {
    if hours <= 48 {
        value
            .get(11..16)
            .unwrap_or(value)
            .to_string()
    } else {
        value
            .get(5..10)
            .unwrap_or(value)
            .to_string()
    }
}

fn delta_percent(current: f64, previous: f64) -> Option<f64> {
    if previous.abs() < f64::EPSILON {
        None
    } else {
        Some((current - previous) * 100.0 / previous)
    }
}

fn delta_text(current: f64, previous: Option<f64>) -> Option<String> {
    previous.and_then(|previous| {
        delta_percent(current, previous).map(|delta| {
            let arrow = if delta > 0.05 {
                "↑"
            } else if delta < -0.05 {
                "↓"
            } else {
                "→"
            };
            format!("{arrow} {:.1}% vs previous", delta.abs())
        })
    })
}

fn build_chart_model(
    points: &[TelemetrySeriesPoint],
    breakdown: &[TelemetryBreakdownRow],
    metric_key: &str,
    series_limit: usize,
) -> ChartModel {
    let buckets: Vec<String> = points
        .iter()
        .map(|point| {
            point
                .bucket_start
                .clone()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut ranked = breakdown.to_vec();
    ranked.sort_by(|a, b| {
        stats_metric(&b.stats, metric_key)
            .total_cmp(&stats_metric(&a.stats, metric_key))
    });
    let labels: Vec<String> = ranked
        .into_iter()
        .take(series_limit.clamp(1, 10))
        .map(|row| row.label)
        .collect();
    let values: HashMap<(String, String), f64> = points
        .iter()
        .map(|point| {
            (
                (
                    point
                        .label
                        .clone(),
                    point
                        .bucket_start
                        .clone(),
                ),
                stats_metric(&point.stats, metric_key),
            )
        })
        .collect();
    let series: Vec<_> = labels
        .into_iter()
        .map(|label| ChartSeries {
            values: buckets
                .iter()
                .map(|bucket| {
                    values
                        .get(&(label.clone(), bucket.clone()))
                        .copied()
                })
                .collect(),
            label,
        })
        .collect();
    let max = series
        .iter()
        .flat_map(|series| {
            series
                .values
                .iter()
                .flatten()
        })
        .copied()
        .fold(0.0_f64, f64::max)
        .max(1.0);
    ChartModel {
        buckets,
        series,
        max,
    }
}

fn chart_point(index: usize, length: usize, value: f64, max: f64) -> (f64, f64) {
    const LEFT: f64 = 64.0;
    const TOP: f64 = 18.0;
    const WIDTH: f64 = 916.0;
    const HEIGHT: f64 = 278.0;
    let denominator = length
        .saturating_sub(1)
        .max(1) as f64;
    (
        LEFT + index as f64 / denominator * WIDTH,
        TOP + HEIGHT - value / max.max(1.0) * HEIGHT,
    )
}

fn line_segments(values: &[Option<f64>], max: f64) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = Vec::new();
    for (index, value) in values
        .iter()
        .enumerate()
    {
        if let Some(value) = value {
            let (x, y) = chart_point(index, values.len(), *value, max);
            current.push(format!("{x:.1},{y:.1}"));
        } else if !current.is_empty() {
            result.push(current.join(" "));
            current.clear();
        }
    }
    if !current.is_empty() {
        result.push(current.join(" "));
    }
    result
}

fn area_segments(values: &[Option<f64>], max: f64) -> Vec<String> {
    line_segments(values, max)
        .into_iter()
        .filter_map(|line| {
            let first = line
                .split_whitespace()
                .next()?
                .split_once(',')?
                .0;
            let last = line
                .split_whitespace()
                .last()?
                .split_once(',')?
                .0;
            Some(format!("{line} {last},296 {first},296"))
        })
        .collect()
}

fn x_label_indices(length: usize) -> Vec<usize> {
    if length == 0 {
        return Vec::new();
    }
    [0, length / 4, length / 2, length * 3 / 4, length - 1]
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn active_filter_count(query: &QueryConfig) -> usize {
    [
        &query.route,
        &query.device,
        &query.client,
        &query.user,
        &query.content,
        &query.method,
        &query.status_class,
        &query.sample_reason,
    ]
    .into_iter()
    .filter(|value| !value.is_empty())
    .count()
}

fn merge_options(target: &mut TelemetryFilterOptions, source: &TelemetryFilterOptions) {
    fn merge(target: &mut Vec<String>, source: &[String]) {
        target.extend(
            source
                .iter()
                .cloned(),
        );
        target.retain(|value| {
            !value
                .trim()
                .is_empty()
        });
        target.sort();
        target.dedup();
    }
    merge(&mut target.routes, &source.routes);
    merge(&mut target.devices, &source.devices);
    merge(&mut target.clients, &source.clients);
    merge(&mut target.users, &source.users);
    merge(&mut target.contents, &source.contents);
    merge(&mut target.methods, &source.methods);
    merge(&mut target.sample_reasons, &source.sample_reasons);
}

fn request(
    query: &QueryConfig,
    offset_hours: i64,
    group_by: &str,
) -> GetTelemetryExplore {
    GetTelemetryExplore {
        hours: query.hours,
        offset_hours,
        bucket_minutes: query.bucket_minutes,
        group_by: group_by.to_string(),
        route: query
            .route
            .clone(),
        device: query
            .device
            .clone(),
        client: query
            .client
            .clone(),
        user: query
            .user
            .clone(),
        content: query
            .content
            .clone(),
        method: query
            .method
            .clone(),
        status_class: query
            .status_class
            .clone(),
        sample_reason: query
            .sample_reason
            .clone(),
        sort_by: query
            .sort_by
            .clone(),
        sort_dir: query
            .sort_dir
            .clone(),
        limit: 200,
    }
}

fn column_enabled(columns: &[String], key: &str) -> bool {
    columns
        .iter()
        .any(|column| column == key)
}

fn csv_cell(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn breakdown_csv(rows: &[TelemetryBreakdownRow]) -> String {
    let mut output =
        "Name,Requests,Errors,Error %,Mean ms,p50 ms,p95 ms,p99 ms,Max ms\n"
            .to_string();
    for row in rows {
        output.push_str(&format!(
            "{},{},{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4}\n",
            csv_cell(&row.label),
            row.stats
                .count,
            row.stats
                .error_count,
            row.stats
                .error_rate,
            row.stats
                .mean_latency_ms,
            row.stats
                .p50_latency_ms,
            row.stats
                .p95_latency_ms,
            row.stats
                .p99_latency_ms,
            row.stats
                .max_latency_ms,
        ));
    }
    output
}

fn recent_csv(rows: &[TelemetryRequestEvent]) -> String {
    let mut output = "Time,Method,Endpoint,Status,Latency ms,Device,Client,Version,User,Content,Capture reason,Error\n".to_string();
    for row in rows {
        output.push_str(&format!(
            "{},{},{},{},{:.4},{},{},{},{},{},{},{}\n",
            csv_cell(&row.created_at),
            csv_cell(&row.method),
            csv_cell(&row.route),
            row.status,
            row.latency_ms,
            csv_cell(&row.device),
            csv_cell(&row.client),
            csv_cell(&row.client_version),
            csv_cell(&row.user),
            csv_cell(&row.content),
            csv_cell(&row.sample_reason),
            csv_cell(&row.error_category),
        ));
    }
    output
}

fn csv_url(value: &str) -> String {
    format!("data:text/csv;charset=utf-8,{}", urlencoding::encode(value))
}

fn filtered_breakdown(
    rows: &[TelemetryBreakdownRow],
    search: &str,
) -> Vec<TelemetryBreakdownRow> {
    let needle = search
        .trim()
        .to_lowercase();
    rows.iter()
        .filter(|row| {
            needle.is_empty()
                || row
                    .label
                    .to_lowercase()
                    .contains(&needle)
        })
        .cloned()
        .collect()
}

fn sorted_recent(
    rows: &[TelemetryRequestEvent],
    search: &str,
    sort: &str,
    ascending: bool,
) -> Vec<TelemetryRequestEvent> {
    let needle = search
        .trim()
        .to_lowercase();
    let mut rows: Vec<_> = rows
        .iter()
        .filter(|row| {
            needle.is_empty()
                || [
                    &row.created_at,
                    &row.method,
                    &row.route,
                    &row.device,
                    &row.client,
                    &row.user,
                    &row.content,
                    &row.sample_reason,
                    &row.error_category,
                ]
                .into_iter()
                .any(|value| {
                    value
                        .to_lowercase()
                        .contains(&needle)
                })
        })
        .cloned()
        .collect();
    rows.sort_by(|a, b| {
        let ordering = match sort {
            "endpoint" => (&a.route, &a.method).cmp(&(&b.route, &b.method)),
            "status" => a
                .status
                .cmp(&b.status),
            "latency" => a
                .latency_ms
                .total_cmp(&b.latency_ms),
            "device" => a
                .device
                .to_lowercase()
                .cmp(
                    &b.device
                        .to_lowercase(),
                ),
            "client" => a
                .client
                .to_lowercase()
                .cmp(
                    &b.client
                        .to_lowercase(),
                ),
            "user" => a
                .user
                .to_lowercase()
                .cmp(
                    &b.user
                        .to_lowercase(),
                ),
            "content" => a
                .content
                .to_lowercase()
                .cmp(
                    &b.content
                        .to_lowercase(),
                ),
            "reason" => a
                .sample_reason
                .cmp(&b.sample_reason),
            _ => a
                .created_at
                .cmp(&b.created_at),
        };
        if ascending {
            ordering
        } else {
            ordering.reverse()
        }
    });
    rows
}

fn drilldown(query: &QueryConfig, label: &str) -> Option<QueryConfig> {
    let mut next = query.clone();
    match query
        .group_by
        .as_str()
    {
        "route" => next.route = label.into(),
        "device" => next.device = label.into(),
        "client" => next.client = label.into(),
        "user" => next.user = label.into(),
        "content" => next.content = label.into(),
        "method" => next.method = label.into(),
        "status" => next.status_class = label.into(),
        _ => return None,
    }
    Some(next)
}

#[component]
fn StatCard(
    label: String,
    value: String,
    #[props(default)] delta: Option<String>,
    #[props(default)] danger: bool,
) -> Element {
    rsx! {
        div { class: if danger { "telemetry-stat danger" } else { "telemetry-stat" },
            span { "{label}" }
            strong { "{value}" }
            if let Some(delta)=delta { small { "{delta}" } }
        }
    }
}

#[component]
fn TimeSeriesChart(
    points: Vec<TelemetrySeriesPoint>,
    breakdown: Vec<TelemetryBreakdownRow>,
    metric_key: String,
    chart_type: String,
    series_limit: usize,
    show_points: bool,
    hours: i64,
) -> Element {
    let model = build_chart_model(&points, &breakdown, &metric_key, series_limit);
    let mut hidden = use_signal(BTreeSet::<String>::new);
    let mut hovered = use_signal(|| None::<usize>);
    let visible: Vec<_> = model
        .series
        .iter()
        .enumerate()
        .filter(|(_, series)| {
            !hidden
                .read()
                .contains(&series.label)
        })
        .collect();
    let slot = 916.0
        / model
            .buckets
            .len()
            .max(1) as f64;
    let denominator = model
        .buckets
        .len()
        .saturating_sub(1)
        .max(1) as f64;
    let hover_index = *hovered.read();
    rsx! {
        div { class:"telemetry-chart-shell",
            if model.buckets.is_empty() || model.series.is_empty() {
                div { class:"telemetry-empty-chart", "No time-series data matches this view." }
            } else {
                div { class:"telemetry-chart-canvas",
                    svg { view_box:"0 0 1000 340", role:"img",
                        title { "{metric_label(&metric_key)} over time" }
                        for tick in 0..=4 {
                            {let y=18.0+278.0-(tick as f64/4.0*278.0);let value=model.max*tick as f64/4.0;rsx!{
                                line { x1:"64",y1:"{y}",x2:"980",y2:"{y}",class:"telemetry-grid-line" }
                                text { x:"56",y:"{y}",text_anchor:"end",dominant_baseline:"middle",class:"telemetry-axis-label","{metric_value(value,&metric_key)}" }
                            }}
                        }
                        for index in x_label_indices(model.buckets.len()) {
                            {let x=64.0+index as f64/denominator*916.0;rsx!{
                                text { x:"{x}",y:"325",text_anchor:if index==0{"start"}else if index+1==model.buckets.len(){"end"}else{"middle"},class:"telemetry-axis-label","{short_bucket(&model.buckets[index],hours)}" }
                            }}
                        }
                        if chart_type=="bar" {
                            for (visible_index,(series_index,series)) in visible.iter().enumerate() {
                                for (index,value) in series.values.iter().enumerate() {
                                    if let Some(value)=value {
                                        {let width=(slot*0.78/visible.len().max(1) as f64).max(0.8);let x=64.0+index as f64*slot+slot*0.11+visible_index as f64*width;let height=*value/model.max*278.0;let y=296.0-height;rsx!{
                                            rect { key:"{series.label}-{index}",x:"{x}",y:"{y}",width:"{width}",height:"{height}",rx:"1.5",fill:"{SERIES_COLORS[*series_index%SERIES_COLORS.len()]}",class:"telemetry-bar",
                                                title { "{series.label}: {metric_value(*value,&metric_key)}" }
                                            }
                                        }}
                                    }
                                }
                            }
                        } else {
                            for (series_index,series) in &visible {
                                if chart_type=="area" {
                                    for segment in area_segments(&series.values,model.max) {
                                        polygon { points:"{segment}",fill:"{SERIES_COLORS[*series_index%SERIES_COLORS.len()]}",class:"telemetry-area" }
                                    }
                                }
                                for segment in line_segments(&series.values,model.max) {
                                    polyline { points:"{segment}",fill:"none",stroke:"{SERIES_COLORS[*series_index%SERIES_COLORS.len()]}",stroke_width:"2.5",vector_effect:"non-scaling-stroke",class:"telemetry-line" }
                                }
                                if show_points && model.buckets.len()<=96 {
                                    for (index,value) in series.values.iter().enumerate() {
                                        if let Some(value)=value {
                                            {let (x,y)=chart_point(index,series.values.len(),*value,model.max);rsx!{
                                                circle { key:"point-{series.label}-{index}",cx:"{x}",cy:"{y}",r:"3",fill:"{SERIES_COLORS[*series_index%SERIES_COLORS.len()]}",class:"telemetry-point" }
                                            }}
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(index)=hover_index {
                            {let x=64.0+index as f64/denominator*916.0;rsx!{line{x1:"{x}",y1:"18",x2:"{x}",y2:"296",class:"telemetry-crosshair"}}}
                        }
                        for index in 0..model.buckets.len() {
                            {let x=64.0+index as f64*slot;rsx!{
                                rect { key:"hover-{index}",x:"{x}",y:"18",width:"{slot.max(2.0)}",height:"278",fill:"transparent",onmouseenter:move |_|hovered.set(Some(index)),onclick:move |_|hovered.set(Some(index)) }
                            }}
                        }
                    }
                    if let Some(index)=hover_index {
                        {let tooltip_left=((index as f64+0.5)/model.buckets.len() as f64*100.0).clamp(8.0,92.0);rsx!{
                            div { class:"telemetry-tooltip",style:"left:{tooltip_left}%",
                                strong { "{model.buckets[index]}" }
                                for (series_index,series) in &visible {
                                    div { i { style:"background:{SERIES_COLORS[*series_index%SERIES_COLORS.len()]}" } span { "{series.label}" } b { if let Some(value)=series.values[index]{"{metric_value(value,&metric_key)}"}else{"No data"} } }
                                }
                                button { onclick:move |_|hovered.set(None),"Close" }
                            }
                        }}
                    }
                }
                div { class:"telemetry-legend",
                    for (index,series) in model.series.iter().enumerate() {
                        {let label=series.label.clone();let is_hidden=hidden.read().contains(&label);rsx!{
                            button { key:"{label}",class:if is_hidden{"muted"}else{""},onclick:move |_|hidden.with_mut(|values|{if !values.remove(&label){values.insert(label.clone());}}),
                                i { style:"background:{SERIES_COLORS[index%SERIES_COLORS.len()]}" } "{series.label}"
                            }
                        }}
                    }
                }
            }
        }
    }
}

#[component]
fn BreakdownBars(
    rows: Vec<TelemetryBreakdownRow>,
    metric_key: String,
    on_drilldown: EventHandler<String>,
) -> Element {
    let mut ranked = rows;
    ranked.sort_by(|a, b| {
        stats_metric(&b.stats, &metric_key)
            .total_cmp(&stats_metric(&a.stats, &metric_key))
    });
    ranked.truncate(12);
    let max = ranked
        .iter()
        .map(|row| stats_metric(&row.stats, &metric_key))
        .fold(0.0_f64, f64::max)
        .max(1.0);
    rsx! {
        div { class:"telemetry-rank-chart",
            if ranked.is_empty() { EmptyState { message:"No contributors match this view." } }
            for (index,row) in ranked.iter().enumerate() {
                {let value=stats_metric(&row.stats,&metric_key);let width=value/max*100.0;let label=row.label.clone();rsx!{
                    button { key:"{row.label}",class:"telemetry-rank-row",onclick:move |_|on_drilldown.call(label.clone()),
                        span { class:"telemetry-rank-index","{index+1}" }
                        span { class:"telemetry-rank-name",title:"{row.label}","{row.label}" }
                        span { class:"telemetry-rank-track",i { style:"width:{width}%;background:{SERIES_COLORS[index%SERIES_COLORS.len()]}" } }
                        strong { "{metric_value(value,&metric_key)}" }
                    }
                }}
            }
        }
    }
}

#[component]
fn StatusDonut(rows: Vec<TelemetryBreakdownRow>) -> Element {
    let total: i64 = rows
        .iter()
        .map(|row| {
            row.stats
                .count
        })
        .sum();
    let mut offset = 0.0;
    let colors: HashMap<&str, &str> = HashMap::from([
        ("2xx", "#22c55e"),
        ("3xx", "#06b6d4"),
        ("4xx", "#f59e0b"),
        ("5xx", "#ef4444"),
    ]);
    rsx! {
        div { class:"telemetry-status-chart",
            if total==0 { EmptyState { message:"No status data matches this view." } }
            else {
                div { class:"telemetry-donut-wrap",
                    svg { view_box:"0 0 120 120",role:"img",
                        title { "HTTP status distribution" }
                        circle { cx:"60",cy:"60",r:"46",fill:"none",stroke:"var(--surface-2)",stroke_width:"16" }
                        for row in &rows {
                            {let fraction=row.stats.count as f64/total as f64*100.0;let dash_offset=-offset;offset+=fraction;let color=colors.get(row.label.as_str()).copied().unwrap_or("#8b5cf6");rsx!{
                                circle { key:"{row.label}",cx:"60",cy:"60",r:"46",path_length:"100",fill:"none",stroke:"{color}",stroke_width:"16",stroke_dasharray:"{fraction} {100.0-fraction}",stroke_dashoffset:"{dash_offset}",transform:"rotate(-90 60 60)",class:"telemetry-donut-segment" }
                            }}
                        }
                    }
                    div { strong { "{format_count(total)}" } span { "responses" } }
                }
                div { class:"telemetry-status-legend",
                    for row in &rows {
                        {let color=colors.get(row.label.as_str()).copied().unwrap_or("#8b5cf6");rsx!{
                            div { i { style:"background:{color}" } span { "{row.label}" } strong { "{format_count(row.stats.count)}" } small { "{row.stats.count as f64*100.0/total as f64:.1}%" } }
                        }}
                    }
                }
            }
        }
    }
}

#[component]
pub fn TelemetryPage(app_state: AppState) -> Element {
    let mut draft = use_signal(QueryConfig::default);
    let mut applied = use_signal(QueryConfig::default);
    let mut data = use_signal(|| None::<TelemetryExploreResponse>);
    let mut previous = use_signal(|| None::<TelemetryExploreResponse>);
    let mut status_data = use_signal(|| None::<TelemetryExploreResponse>);
    let mut options = use_signal(TelemetryFilterOptions::default);
    let mut loading = use_signal(|| true);
    let mut refreshing = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut refresh = use_signal(|| 0_u64);
    let mut metric_key = use_signal(|| "p95".to_string());
    let mut chart_type = use_signal(|| "line".to_string());
    let mut series_limit = use_signal(|| 6_usize);
    let mut show_points = use_signal(|| false);
    let mut compare_previous = use_signal(|| false);
    let mut auto_refresh_secs = use_signal(|| 0_u32);
    let mut table_search = use_signal(String::new);
    let mut recent_search = use_signal(String::new);
    let mut recent_sort = use_signal(|| "time".to_string());
    let mut recent_ascending = use_signal(|| false);
    let mut selected_event = use_signal(|| None::<TelemetryRequestEvent>);
    let mut columns = use_signal(|| {
        TABLE_COLUMNS
            .iter()
            .map(|(key, _)| (*key).to_string())
            .collect::<Vec<_>>()
    });
    let mut views = use_signal(Vec::<TelemetrySavedView>::new);
    let mut view_name = use_signal(String::new);
    let mut view_message = use_signal(String::new);

    let telemetry_client = app_state
        .client
        .clone();
    use_effect(move || {
        let query = applied();
        let generation = *refresh.read();
        let compare = *compare_previous.read();
        let api = telemetry_client.clone();
        if data
            .peek()
            .is_none()
        {
            loading.set(true);
        } else {
            refreshing.set(true);
        }
        spawn(async move {
            let primary_request = request(&query, 0, &query.group_by);
            let status_request = request(&query, 0, "status");
            let primary_api = api.clone();
            let status_api = api.clone();
            let (primary_result, status_result) = futures::join!(
                primary_api.execute(primary_request),
                status_api.execute(status_request)
            );
            let comparison_result = if compare {
                api.execute(request(&query, query.hours, &query.group_by))
                    .await
                    .ok()
            } else {
                None
            };
            match primary_result {
                Ok(value) => {
                    options.with_mut(|target| merge_options(target, &value.filters));
                    data.set(Some(value));
                    previous.set(comparison_result);
                    status_data.set(status_result.ok());
                    error.set(None);
                }
                Err(err) => {
                    error.set(Some(format!("Telemetry could not be loaded: {err}")));
                }
            }
            let _ = generation;
            loading.set(false);
            refreshing.set(false);
        });
    });

    let views_client = app_state
        .client
        .clone();
    use_effect(move || {
        let api = views_client.clone();
        spawn(async move {
            if let Ok(value) = api
                .execute(GetTelemetryViews)
                .await
            {
                views.set(value);
            }
        });
    });

    use_effect(move || {
        let seconds = *auto_refresh_secs.read();
        if seconds == 0 {
            return;
        }
        spawn(async move {
            loop {
                TimeoutFuture::new(seconds * 1_000).await;
                if *auto_refresh_secs.peek() != seconds || seconds == 0 {
                    break;
                }
                refresh += 1;
            }
        });
    });

    let applied_snapshot = applied();
    let draft_snapshot = draft();
    let options_snapshot = options();
    let result = data();
    let comparison = previous();
    let breakdown_rows = result
        .as_ref()
        .map(|value| filtered_breakdown(&value.breakdown, &table_search()))
        .unwrap_or_default();
    let recent_rows = result
        .as_ref()
        .map(|value| {
            sorted_recent(
                &value.recent,
                &recent_search(),
                &recent_sort(),
                *recent_ascending.read(),
            )
        })
        .unwrap_or_default();
    let comparison_map: HashMap<String, TelemetryStats> = comparison
        .as_ref()
        .map(|value| {
            value
                .breakdown
                .iter()
                .map(|row| {
                    (
                        row.label
                            .clone(),
                        row.stats
                            .clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let filter_count = active_filter_count(&applied_snapshot);
    let breakdown_arrow = if applied_snapshot.sort_dir == "asc" {
        "↑"
    } else {
        "↓"
    };
    let recent_arrow = if *recent_ascending.read() {
        "↑"
    } else {
        "↓"
    };

    let mut apply_draft = move || {
        let next = draft();
        if next == applied() {
            refresh += 1;
        } else {
            applied.set(next);
        }
    };

    rsx! {
        div { class:"telemetry-page telemetry-workspace",
            div { class:"telemetry-toolbar telemetry-workspace-header",
                div {
                    div { class:"telemetry-eyebrow",span { class:"telemetry-live-dot" } "Server-side observability" }
                    h1 { "Telemetry" }
                    p { "Explore every endpoint by device, client, user and content. Build, compare, save and export any view." }
                }
                div { class:"telemetry-header-actions",
                    label { class:"telemetry-auto-refresh",span { "Auto refresh" }
                        select { value:"{auto_refresh_secs}",onchange:move|event|if let Ok(value)=event.value().parse(){auto_refresh_secs.set(value);},
                            option { value:"0","Off" } option { value:"10","10 sec" } option { value:"30","30 sec" } option { value:"60","1 min" } option { value:"300","5 min" }
                        }
                    }
                    button { class:"btn btn-primary telemetry-refresh",disabled:*refreshing.read(),onclick:move |_|refresh+=1,
                        if *refreshing.read(){span{class:"telemetry-spinner"}} "Refresh"
                    }
                }
            }

            div { class:"telemetry-timebar",
                div { class:"telemetry-time-presets",
                    for (value,label,bucket_value) in [(1,"1H",1),(6,"6H",5),(24,"24H",30),(168,"7D",240),(336,"14D",240),(720,"30D",1440),(2160,"90D",1440),(4320,"180D",1440)] {
                        button { class:if applied_snapshot.hours==value{"active"}else{""},onclick:move |_|{let mut next=applied();next.hours=value;next.bucket_minutes=bucket_value;draft.set(next.clone());applied.set(next);},"{label}" }
                    }
                }
                div { class:"telemetry-resolution-badge",
                    if let Some(value)=result.as_ref(){span{class:if value.resolution=="raw"{"raw"}else{"rollup"},if value.resolution=="raw"{"Exact raw"}else{"Hourly rollup"}} " · {value.bucket_minutes}m buckets · {format_count(value.summary.count)} requests"}
                }
            }

            details { class:"telemetry-query-panel",open:true,
                summary { span { "Query builder" } if filter_count>0 { b { "{filter_count} active filters" } } i { "▾" } }
                div { class:"telemetry-query-body",
                    div { class:"telemetry-query-grid",
                        label { "Group by" select { value:"{draft_snapshot.group_by}",onchange:move|e|draft.with_mut(|value|value.group_by=e.value()),
                            option{value:"route","Endpoint"} option{value:"device","Device"} option{value:"client","Client"} option{value:"deviceClient","Device + client"}
                            option{value:"routeClient","Endpoint + client"} option{value:"routeDevice","Endpoint + device"} option{value:"user","User"} option{value:"content","Content"}
                            option{value:"method","Method"} option{value:"status","HTTP status"} option{value:"none","All requests"}
                        } }
                        label { "Bucket size" select { value:"{draft_snapshot.bucket_minutes}",onchange:move|e|if let Ok(value)=e.value().parse(){draft.with_mut(|query|query.bucket_minutes=value);},
                            option{value:"1","1 minute"} option{value:"5","5 minutes"} option{value:"15","15 minutes"} option{value:"30","30 minutes"} option{value:"60","1 hour"} option{value:"240","4 hours"} option{value:"720","12 hours"} option{value:"1440","1 day"}
                        } }
                        label { "Endpoint" input { class:"form-input",list:"telemetry-routes",placeholder:"All endpoints",value:"{draft_snapshot.route}",oninput:move|e|draft.with_mut(|query|query.route=e.value()) } }
                        label { "Device" input { class:"form-input",list:"telemetry-devices",placeholder:"All devices",value:"{draft_snapshot.device}",oninput:move|e|draft.with_mut(|query|query.device=e.value()) } }
                        label { "Client" input { class:"form-input",list:"telemetry-clients",placeholder:"All clients",value:"{draft_snapshot.client}",oninput:move|e|draft.with_mut(|query|query.client=e.value()) } }
                        label { "User" input { class:"form-input",list:"telemetry-users",placeholder:"All users",value:"{draft_snapshot.user}",oninput:move|e|draft.with_mut(|query|query.user=e.value()) } }
                        label { "Content" input { class:"form-input",list:"telemetry-contents",placeholder:"All content",value:"{draft_snapshot.content}",oninput:move|e|draft.with_mut(|query|query.content=e.value()) } }
                        label { "Method" select { value:"{draft_snapshot.method}",onchange:move|e|draft.with_mut(|query|query.method=e.value()),option{value:"","All methods"} for value in &options_snapshot.methods{option{value:"{value}","{value}"}} } }
                        label { "HTTP status" select { value:"{draft_snapshot.status_class}",onchange:move|e|draft.with_mut(|query|query.status_class=e.value()),option{value:"","All statuses"}option{value:"2xx","2xx success"}option{value:"3xx","3xx redirect"}option{value:"4xx","4xx client error"}option{value:"5xx","5xx server error"}option{value:"errors","All errors"} } }
                        label { "Capture reason" select { value:"{draft_snapshot.sample_reason}",onchange:move|e|draft.with_mut(|query|query.sample_reason=e.value()),option{value:"","All captures"} for value in &options_snapshot.sample_reasons{option{value:"{value}","{value}"}} } }
                    }
                    datalist { id:"telemetry-routes",for value in &options_snapshot.routes{option{value:"{value}"}} }
                    datalist { id:"telemetry-devices",for value in &options_snapshot.devices{option{value:"{value}"}} }
                    datalist { id:"telemetry-clients",for value in &options_snapshot.clients{option{value:"{value}"}} }
                    datalist { id:"telemetry-users",for value in &options_snapshot.users{option{value:"{value}"}} }
                    datalist { id:"telemetry-contents",for value in &options_snapshot.contents{option{value:"{value}"}} }
                    div { class:"telemetry-query-actions",
                        button { class:"btn btn-primary",onclick:move |_|apply_draft(),"Apply query" }
                        button { class:"btn btn-ghost",onclick:move |_|{let mut next=QueryConfig::default();next.hours=applied().hours;next.bucket_minutes=applied().bucket_minutes;draft.set(next.clone());applied.set(next);},"Clear filters" }
                        span { if draft_snapshot!=applied_snapshot { "Unapplied changes" } else { "Query is current" } }
                    }
                }
            }

            if filter_count>0 {
                div { class:"telemetry-filter-chips",
                    span { "Active" }
                    if !applied_snapshot.route.is_empty(){button{onclick:move |_|{let mut next=applied();next.route.clear();draft.set(next.clone());applied.set(next);},"Endpoint: {applied_snapshot.route} ×"}}
                    if !applied_snapshot.device.is_empty(){button{onclick:move |_|{let mut next=applied();next.device.clear();draft.set(next.clone());applied.set(next);},"Device: {applied_snapshot.device} ×"}}
                    if !applied_snapshot.client.is_empty(){button{onclick:move |_|{let mut next=applied();next.client.clear();draft.set(next.clone());applied.set(next);},"Client: {applied_snapshot.client} ×"}}
                    if !applied_snapshot.user.is_empty(){button{onclick:move |_|{let mut next=applied();next.user.clear();draft.set(next.clone());applied.set(next);},"User: {applied_snapshot.user} ×"}}
                    if !applied_snapshot.content.is_empty(){button{onclick:move |_|{let mut next=applied();next.content.clear();draft.set(next.clone());applied.set(next);},"Content: {applied_snapshot.content} ×"}}
                    if !applied_snapshot.method.is_empty(){button{onclick:move |_|{let mut next=applied();next.method.clear();draft.set(next.clone());applied.set(next);},"Method: {applied_snapshot.method} ×"}}
                    if !applied_snapshot.status_class.is_empty(){button{onclick:move |_|{let mut next=applied();next.status_class.clear();draft.set(next.clone());applied.set(next);},"Status: {applied_snapshot.status_class} ×"}}
                    if !applied_snapshot.sample_reason.is_empty(){button{onclick:move |_|{let mut next=applied();next.sample_reason.clear();draft.set(next.clone());applied.set(next);},"Reason: {applied_snapshot.sample_reason} ×"}}
                }
            }

            div { class:"telemetry-view-strip",
                div { class:"telemetry-saved-views",
                    span { "Shared views" }
                    if views.read().is_empty(){small{"None saved yet"}}
                    for saved in views.read().clone(){
                        {let apply_view=saved.clone();let remove=saved.clone();rsx!{
                            span { class:"telemetry-view-chip",
                                button { title:"Apply shared view",onclick:move |_|{if let Ok(config)=serde_json::from_str::<ViewConfig>(&apply_view.config_json){let query=config.query();draft.set(query.clone());applied.set(query);metric_key.set(config.metric);chart_type.set(config.chart_type);series_limit.set(config.series_limit.clamp(1,10));show_points.set(config.show_points);compare_previous.set(config.compare_previous);auto_refresh_secs.set(config.auto_refresh_secs);if !config.columns.is_empty(){columns.set(config.columns);}}},"{saved.name}" }
                                button { class:"telemetry-view-delete",title:"Delete shared view",onclick:{let api=app_state.client.clone();move |_|{let api=api.clone();let id=remove.id.clone();spawn(async move{match api.execute(DeleteTelemetryView{id}).await{Ok(_)=>{if let Ok(value)=api.execute(GetTelemetryViews).await{views.set(value);}},Err(err)=>view_message.set(format!("Delete failed: {err}"))}});}},"×" }
                            }
                        }}
                    }
                }
                div { class:"telemetry-view-actions",
                    input { class:"form-input",placeholder:"Name this view",value:"{view_name}",oninput:move|e|view_name.set(e.value()) }
                    button { class:"btn btn-ghost",disabled:view_name.read().trim().is_empty(),onclick:{let api=app_state.client.clone();move |_|{let api=api.clone();let query=applied();let config=ViewConfig{hours:query.hours,bucket_minutes:query.bucket_minutes,group_by:query.group_by,metric:metric_key(),route:query.route,device:query.device,client:query.client,user:query.user,content:query.content,method:query.method,status_class:query.status_class,sample_reason:query.sample_reason,sort_by:query.sort_by,sort_dir:query.sort_dir,chart_type:chart_type(),series_limit:*series_limit.read(),show_points:*show_points.read(),compare_previous:*compare_previous.read(),auto_refresh_secs:*auto_refresh_secs.read(),columns:columns()};let name=view_name().trim().to_string();spawn(async move{match api.execute(SaveTelemetryView{name,config:serde_json::to_value(config).unwrap_or_default()}).await{Ok(_)=>{view_name.set(String::new());view_message.set("Shared view saved".into());if let Ok(value)=api.execute(GetTelemetryViews).await{views.set(value);}},Err(err)=>view_message.set(format!("Save failed: {err}"))}});}},"Save current view" }
                }
            }
            if !view_message.read().is_empty(){div{class:"telemetry-message","{view_message}"}}

            if *loading.read() && result.is_none(){LoadingText{}}
            else if let Some(message)=error.read().as_ref(){div{class:"telemetry-error",strong{"Could not load telemetry"}span{"{message}"}button{class:"btn btn-ghost",onclick:move |_|refresh+=1,"Try again"}}}
            else if let Some(result)=result.as_ref(){
                div { class:"telemetry-stat-grid telemetry-stat-grid-complete",
                    StatCard { label:"Requests",value:format_count(result.summary.count),delta:delta_text(result.summary.count as f64,comparison.as_ref().map(|value|value.summary.count as f64)) }
                    StatCard { label:"Errors",value:format!("{} ({:.1}%)",format_count(result.summary.error_count),result.summary.error_rate),delta:delta_text(result.summary.error_rate,comparison.as_ref().map(|value|value.summary.error_rate)),danger:result.summary.error_rate>=5.0 }
                    StatCard { label:"Mean",value:metric_value(result.summary.mean_latency_ms,"mean"),delta:delta_text(result.summary.mean_latency_ms,comparison.as_ref().map(|value|value.summary.mean_latency_ms)) }
                    StatCard { label:"p50",value:metric_value(result.summary.p50_latency_ms,"p50"),delta:delta_text(result.summary.p50_latency_ms,comparison.as_ref().map(|value|value.summary.p50_latency_ms)) }
                    StatCard { label:"p95",value:metric_value(result.summary.p95_latency_ms,"p95"),delta:delta_text(result.summary.p95_latency_ms,comparison.as_ref().map(|value|value.summary.p95_latency_ms)) }
                    StatCard { label:"p99",value:metric_value(result.summary.p99_latency_ms,"p99"),delta:delta_text(result.summary.p99_latency_ms,comparison.as_ref().map(|value|value.summary.p99_latency_ms)) }
                    StatCard { label:"Maximum",value:metric_value(result.summary.max_latency_ms,"max"),delta:delta_text(result.summary.max_latency_ms,comparison.as_ref().map(|value|value.summary.max_latency_ms)) }
                }

                Card { title:format!("{} over time",metric_label(&metric_key())),action:rsx!{
                    div { class:"telemetry-chart-controls",
                        select { aria_label:"Metric",value:"{metric_key}",onchange:move|e|metric_key.set(e.value()),
                            option{value:"p95","p95 latency"} option{value:"p99","p99 latency"} option{value:"p50","p50 latency"} option{value:"mean","Mean latency"} option{value:"max","Maximum latency"} option{value:"count","Request count"} option{value:"errors","Error count"} option{value:"errorRate","Error rate"}
                        }
                        div { class:"telemetry-segmented",for (value,label) in [("line","Line"),("area","Area"),("bar","Bars")]{button{class:if chart_type()==value{"active"}else{""},onclick:move |_|chart_type.set(value.into()),"{label}"}} }
                        select { aria_label:"Series count",value:"{series_limit}",onchange:move|e|if let Ok(value)=e.value().parse(){series_limit.set(value);},option{value:"1","Top 1"}option{value:"3","Top 3"}option{value:"6","Top 6"}option{value:"10","Top 10"} }
                        label { class:"telemetry-check",input{r#type:"checkbox",checked:*show_points.read(),onchange:move|e|show_points.set(e.checked())}"Points" }
                        label { class:"telemetry-check",input{r#type:"checkbox",checked:*compare_previous.read(),onchange:move|e|compare_previous.set(e.checked())}"Compare previous" }
                    }
                },
                    TimeSeriesChart { points:result.series.clone(),breakdown:result.breakdown.clone(),metric_key:metric_key(),chart_type:chart_type(),series_limit:*series_limit.read(),show_points:*show_points.read(),hours:result.hours }
                }

                div { class:"telemetry-secondary-grid",
                    Card { title:format!("Top {} by {}",applied_snapshot.group_by,metric_label(&metric_key())),
                        BreakdownBars { rows:result.breakdown.clone(),metric_key:metric_key(),on_drilldown:move|label:String|{if let Some(next)=drilldown(&applied(),&label){draft.set(next.clone());applied.set(next);}} }
                    }
                    Card { title:"HTTP status distribution",
                        StatusDonut { rows:status_data.read().as_ref().map(|value|value.breakdown.clone()).unwrap_or_default() }
                    }
                }

                Card {
                    title: format!("Breakdown by {}",applied_snapshot.group_by),
                    tight: true,
                    action: rsx! {
                        div { class:"telemetry-table-actions",
                            input { class:"form-input",placeholder:"Search breakdown",value:"{table_search}",oninput:move|e|table_search.set(e.value()) }
                            details { class:"telemetry-columns",
                                summary { "Columns ({columns.read().len()})" }
                                div {
                                    for (key,label) in TABLE_COLUMNS {
                                        label {
                                            input { r#type:"checkbox",checked:column_enabled(&columns(),key),onchange:move|e|columns.with_mut(|values|{if e.checked(){if !values.iter().any(|value|value==key){values.push(key.into());}}else{values.retain(|value|value!=key);}}) }
                                            "{label}"
                                        }
                                    }
                                }
                            }
                            a { class:"btn btn-ghost",href:"{csv_url(&breakdown_csv(&breakdown_rows))}",download:"remux-telemetry-breakdown.csv","Export CSV" }
                        }
                    },
                    if breakdown_rows.is_empty() {
                        EmptyState { message:"No matching breakdown rows." }
                    } else {
                        div { class:"data-table-container telemetry-table",
                            table {
                                thead { tr {
                                    th { button { onclick:move |_|{let mut next=applied();if next.sort_by=="label"{next.sort_dir=if next.sort_dir=="asc"{"desc".into()}else{"asc".into()};}else{next.sort_by="label".into();next.sort_dir="asc".into();}draft.set(next.clone());applied.set(next);},
                                        "Name" if applied_snapshot.sort_by=="label" { span { " {breakdown_arrow}" } }
                                    } }
                                    for (key,label) in TABLE_COLUMNS {
                                        if column_enabled(&columns(),key) {
                                            th { button { onclick:move |_|{let mut next=applied();if next.sort_by==key{next.sort_dir=if next.sort_dir=="desc"{"asc".into()}else{"desc".into()};}else{next.sort_by=key.into();next.sort_dir="desc".into();}draft.set(next.clone());applied.set(next);},
                                                "{label}" if applied_snapshot.sort_by==key { span { " {breakdown_arrow}" } }
                                            } }
                                        }
                                    }
                                    if *compare_previous.read() { th { "Change" } }
                                } }
                                tbody {
                                    for row in &breakdown_rows {
                                        tr {
                                            td { class:"telemetry-label",
                                                button { title:"Filter to {row.label}",onclick:{let label=row.label.clone();move |_|{if let Some(next)=drilldown(&applied(),&label){draft.set(next.clone());applied.set(next);}}},"{row.label}" }
                                            }
                                            if column_enabled(&columns(),"count") { td { "{format_count(row.stats.count)}" } }
                                            if column_enabled(&columns(),"errors") { td { class:if row.stats.error_count>0{"telemetry-status-error"}else{""},"{format_count(row.stats.error_count)}" } }
                                            if column_enabled(&columns(),"errorRate") { td { "{row.stats.error_rate:.1}%" } }
                                            if column_enabled(&columns(),"mean") { td { {metric_value(row.stats.mean_latency_ms,"mean")} } }
                                            if column_enabled(&columns(),"p50") { td { {metric_value(row.stats.p50_latency_ms,"p50")} } }
                                            if column_enabled(&columns(),"p95") { td { {metric_value(row.stats.p95_latency_ms,"p95")} } }
                                            if column_enabled(&columns(),"p99") { td { {metric_value(row.stats.p99_latency_ms,"p99")} } }
                                            if column_enabled(&columns(),"max") { td { {metric_value(row.stats.max_latency_ms,"max")} } }
                                            if *compare_previous.read() {
                                                td { class:"telemetry-delta",
                                                    if let Some(old)=comparison_map.get(&row.label) {
                                                        {delta_text(stats_metric(&row.stats,&metric_key()),Some(stats_metric(old,&metric_key()))).unwrap_or_else(||"New".into())}
                                                    } else { "New" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                Card {
                    title:"Recent requests",
                    tight:true,
                    action:rsx! {
                        div { class:"telemetry-table-actions",
                            input { class:"form-input",placeholder:"Search recent requests",value:"{recent_search}",oninput:move|e|recent_search.set(e.value()) }
                            a { class:"btn btn-ghost",href:"{csv_url(&recent_csv(&recent_rows))}",download:"remux-telemetry-requests.csv","Export CSV" }
                        }
                    },
                    if recent_rows.is_empty() {
                        EmptyState { message:if result.resolution=="raw"{"No recent requests match this view."}else{"Recent raw requests are unavailable for this long-range rollup."} }
                    } else {
                        div { class:"data-table-container telemetry-table telemetry-events",
                            table {
                                thead { tr {
                                    for (key,label) in [("time","Time"),("endpoint","Endpoint"),("status","Status"),("latency","Latency"),("device","Device"),("client","Client"),("user","User"),("content","Content"),("reason","Capture")] {
                                        th { button { onclick:move |_|{if recent_sort()==key{let next=!*recent_ascending.peek();recent_ascending.set(next);}else{recent_sort.set(key.into());recent_ascending.set(true);}},
                                            "{label}" if recent_sort()==key { span { " {recent_arrow}" } }
                                        } }
                                    }
                                } }
                                tbody {
                                    for event in &recent_rows {
                                        tr { class:if selected_event.read().as_ref().map(|value|value.id)==Some(event.id){"selected"}else{""},onclick:{let value=event.clone();move |_|selected_event.set(Some(value.clone()))},
                                            td { "{event.created_at}" }
                                            td { class:"telemetry-label","{event.method} {event.route}" }
                                            td { class:if event.status>=500{"telemetry-status-error"}else if event.status>=400{"telemetry-status-warn"}else{""},"{event.status}" }
                                            td { {metric_value(event.latency_ms,"mean")} }
                                            td { "{event.device}" }
                                            td { "{event.client} {event.client_version}" }
                                            td { "{event.user}" }
                                            td { title:"{event.content}","{event.content}" }
                                            td { "{event.sample_reason}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Some(event)=selected_event.read().as_ref() {
                        div { class:"telemetry-inspector",
                            div { h3 { "Request details" } button { title:"Close",onclick:move |_|selected_event.set(None),"×" } }
                            dl {
                                div { dt { "Timestamp" } dd { "{event.created_at}" } }
                                div { dt { "Request" } dd { code { "{event.method} {event.route}" } } }
                                div { dt { "Status" } dd { "{event.status}" } }
                                div { dt { "Latency" } dd { "{event.latency_ms:.3} ms" } }
                                div { dt { "Device" } dd { "{event.device}" } }
                                div { dt { "Client" } dd { "{event.client} {event.client_version}" } }
                                div { dt { "User" } dd { "{event.user}" } }
                                div { dt { "Content" } dd { "{event.content}" } }
                                div { dt { "Capture reason" } dd { "{event.sample_reason}" } }
                                div { dt { "Error category" } dd { if event.error_category.is_empty(){"None"}else{"{event.error_category}"} } }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(count: i64, p95: f64) -> TelemetryStats {
        TelemetryStats {
            count,
            p95_latency_ms: p95,
            ..Default::default()
        }
    }

    #[test]
    fn chart_model_aligns_missing_time_buckets() {
        let points = vec![
            TelemetrySeriesPoint {
                bucket_start: "2026-01-01T00:00:00Z".into(),
                label: "A".into(),
                stats: stats(1, 10.0),
            },
            TelemetrySeriesPoint {
                bucket_start: "2026-01-01T01:00:00Z".into(),
                label: "B".into(),
                stats: stats(1, 20.0),
            },
            TelemetrySeriesPoint {
                bucket_start: "2026-01-01T02:00:00Z".into(),
                label: "A".into(),
                stats: stats(1, 30.0),
            },
        ];
        let breakdown = vec![
            TelemetryBreakdownRow {
                label: "A".into(),
                stats: stats(2, 30.0),
            },
            TelemetryBreakdownRow {
                label: "B".into(),
                stats: stats(1, 20.0),
            },
        ];
        let model = build_chart_model(&points, &breakdown, "p95", 2);
        assert_eq!(
            model
                .buckets
                .len(),
            3
        );
        assert_eq!(model.series[0].values, vec![Some(10.0), None, Some(30.0)]);
        assert_eq!(model.series[1].values, vec![None, Some(20.0), None]);
    }

    #[test]
    fn chart_series_are_ranked_by_selected_metric() {
        let breakdown = vec![
            TelemetryBreakdownRow {
                label: "slow".into(),
                stats: stats(1, 900.0),
            },
            TelemetryBreakdownRow {
                label: "busy".into(),
                stats: stats(100, 10.0),
            },
        ];
        let model = build_chart_model(&[], &breakdown, "count", 1);
        assert_eq!(model.series[0].label, "busy");
    }

    #[test]
    fn line_segments_do_not_bridge_missing_measurements() {
        let segments = line_segments(&[Some(10.0), None, Some(30.0), Some(40.0)], 40.0);
        assert_eq!(segments.len(), 2);
        assert_eq!(
            segments[0]
                .split_whitespace()
                .count(),
            1
        );
        assert_eq!(
            segments[1]
                .split_whitespace()
                .count(),
            2
        );
    }

    #[test]
    fn filters_and_recent_sorting_are_deterministic() {
        let mut query = QueryConfig::default();
        query.route = "/items/{id}".into();
        query.client = "Jellyflix".into();
        assert_eq!(active_filter_count(&query), 2);
        let rows = vec![
            TelemetryRequestEvent {
                id: 1,
                latency_ms: 50.0,
                route: "/a".into(),
                ..Default::default()
            },
            TelemetryRequestEvent {
                id: 2,
                latency_ms: 10.0,
                route: "/b".into(),
                ..Default::default()
            },
        ];
        assert_eq!(sorted_recent(&rows, "", "latency", false)[0].id, 1);
        assert_eq!(sorted_recent(&rows, "/b", "time", false).len(), 1);
    }

    #[test]
    fn csv_escapes_operator_visible_names() {
        assert_eq!(csv_cell("a, \"quoted\""), "\"a, \"\"quoted\"\"\"");
        assert!(breakdown_csv(&[TelemetryBreakdownRow {
            label: "A".into(),
            stats: stats(2, 3.0)
        }])
        .contains("Name,Requests"));
    }

    #[test]
    fn saved_view_defaults_keep_old_configs_compatible() {
        let old = serde_json::json!({
            "hours":24,"bucketMinutes":30,"groupBy":"route","metric":"p95",
            "route":"","device":"","client":"","user":"","content":"",
            "method":"","statusClass":"","sampleReason":"","sortBy":"p95","sortDir":"desc"
        });
        let value: ViewConfig = serde_json::from_value(old).unwrap();
        assert_eq!(value.chart_type, "line");
        assert_eq!(value.series_limit, 6);
        assert_eq!(
            value
                .columns
                .len(),
            TABLE_COLUMNS.len()
        );
    }
}
