//! Filterable admin telemetry explorer.

use crate::{AppState, db::auth};
use axum::{
    Json,
    extract::{Query, State},
    response::IntoResponse,
};
use axum_anyhow::ApiResult as Result;
use chrono::{DateTime, Utc};
use remux_macros::get;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, QueryBuilder, Sqlite};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExploreQuery {
    pub hours: Option<i64>,
    pub offset_hours: Option<i64>,
    pub bucket_minutes: Option<i64>,
    pub group_by: Option<String>,
    pub route: Option<String>,
    pub device: Option<String>,
    pub client: Option<String>,
    pub user: Option<String>,
    pub content: Option<String>,
    pub method: Option<String>,
    pub status_class: Option<String>,
    pub sample_reason: Option<String>,
    pub sort_by: Option<String>,
    pub sort_dir: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, FromRow)]
struct RequestRow {
    id: i64,
    created_at: String,
    method: String,
    route_template: String,
    status: i64,
    latency_ms: f64,
    sample_reason: String,
    device_name: Option<String>,
    client_name: Option<String>,
    client_version: Option<String>,
    user_name: Option<String>,
    item_name: Option<String>,
    error_category: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct RollupRow {
    bucket_start: String,
    route_template: String,
    method: String,
    device_name: String,
    client_name: String,
    user_name: String,
    item_name: String,
    status_class: String,
    sample_reason: String,
    request_count: i64,
    error_count: i64,
    total_latency_ms: f64,
    max_latency_ms: f64,
    latency_lt_100: i64,
    latency_lt_500: i64,
    latency_lt_1000: i64,
    latency_lt_2500: i64,
    latency_lt_5000: i64,
    latency_lt_10000: i64,
    latency_ge_10000: i64,
}

#[derive(Debug, Clone, FromRow)]
struct PlaybackRow {
    event: String,
    elapsed_ms: f64,
    status: i64,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub count: i64,
    pub error_count: i64,
    pub error_rate: f64,
    pub mean_latency_ms: f64,
    pub p50_latency_ms: f64,
    pub p95_latency_ms: f64,
    pub p99_latency_ms: f64,
    pub max_latency_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesPoint {
    pub bucket_start: String,
    pub label: String,
    #[serde(flatten)]
    pub stats: Stats,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakdownRow {
    pub label: String,
    #[serde(flatten)]
    pub stats: Stats,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackReadiness {
    /// Server-observed playback-path requests only. Browser-submitted player
    /// diagnostics are deliberately excluded from this dataset.
    pub summary: Stats,
    pub breakdown: Vec<BreakdownRow>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestEvent {
    pub id: i64,
    pub created_at: String,
    pub method: String,
    pub route: String,
    pub status: i64,
    pub latency_ms: f64,
    pub sample_reason: String,
    pub device: String,
    pub client: String,
    pub client_version: String,
    pub user: String,
    pub content: String,
    pub error_category: String,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FilterOptions {
    pub routes: Vec<String>,
    pub devices: Vec<String>,
    pub clients: Vec<String>,
    pub users: Vec<String>,
    pub contents: Vec<String>,
    pub methods: Vec<String>,
    pub sample_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExploreResponse {
    pub hours: i64,
    pub offset_hours: i64,
    pub bucket_minutes: i64,
    pub group_by: String,
    pub resolution: String,
    pub captured_rows: usize,
    pub truncated: bool,
    pub summary: Stats,
    pub series: Vec<SeriesPoint>,
    pub breakdown: Vec<BreakdownRow>,
    pub recent: Vec<RequestEvent>,
    pub filters: FilterOptions,
    pub playback_readiness: PlaybackReadiness,
}

#[derive(Default, Clone)]
struct RollupAcc {
    count: i64,
    errors: i64,
    total: f64,
    max: f64,
    bands: [i64; 7],
}

impl RollupAcc {
    fn add(&mut self, row: &RollupRow) {
        self.count += row.request_count;
        self.errors += row.error_count;
        self.total += row.total_latency_ms;
        self.max = self
            .max
            .max(row.max_latency_ms);
        for (target, value) in self
            .bands
            .iter_mut()
            .zip([
                row.latency_lt_100,
                row.latency_lt_500,
                row.latency_lt_1000,
                row.latency_lt_2500,
                row.latency_lt_5000,
                row.latency_lt_10000,
                row.latency_ge_10000,
            ])
        {
            *target += value;
        }
    }

    fn percentile(&self, fraction: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let target = (self.count as f64 * fraction).ceil() as i64;
        let bounds = [100.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0, self.max];
        let mut seen = 0;
        for (count, bound) in self
            .bands
            .iter()
            .zip(bounds)
        {
            seen += count;
            if seen >= target {
                return bound.max(0.0);
            }
        }
        self.max
    }

    fn finish(self) -> Stats {
        Stats {
            count: self.count,
            error_count: self.errors,
            error_rate: if self.count == 0 {
                0.0
            } else {
                self.errors as f64 * 100.0 / self.count as f64
            },
            mean_latency_ms: if self.count == 0 {
                0.0
            } else {
                self.total / self.count as f64
            },
            p50_latency_ms: self.percentile(0.50),
            p95_latency_ms: self.percentile(0.95),
            p99_latency_ms: self.percentile(0.99),
            max_latency_ms: self.max,
        }
    }
}

#[derive(Default, Clone)]
struct Acc {
    values: Vec<f64>,
    errors: i64,
    total: f64,
    max: f64,
}
impl Acc {
    fn add(&mut self, latency: f64, status: i64) {
        self.values
            .push(latency);
        self.total += latency;
        self.max = self
            .max
            .max(latency);
        if status >= 400 {
            self.errors += 1;
        }
    }
    fn finish(mut self) -> Stats {
        self.values
            .sort_by(|a, b| a.total_cmp(b));
        let count = self
            .values
            .len() as i64;
        let percentile = |p: f64| {
            if self
                .values
                .is_empty()
            {
                0.0
            } else {
                let i = ((self
                    .values
                    .len() as f64
                    * p)
                    .ceil() as usize)
                    .saturating_sub(1);
                self.values[i.min(
                    self.values
                        .len()
                        - 1,
                )]
            }
        };
        Stats {
            count,
            error_count: self.errors,
            error_rate: if count == 0 {
                0.0
            } else {
                self.errors as f64 * 100.0 / count as f64
            },
            mean_latency_ms: if count == 0 {
                0.0
            } else {
                self.total / count as f64
            },
            p50_latency_ms: percentile(0.50),
            p95_latency_ms: percentile(0.95),
            p99_latency_ms: percentile(0.99),
            max_latency_ms: self.max,
        }
    }
}

fn label(row: &RequestRow, dimension: &str) -> String {
    let known = |value: Option<&str>| {
        value
            .filter(|value| {
                !value
                    .trim()
                    .is_empty()
            })
            .unwrap_or("Unknown")
            .to_string()
    };
    match dimension {
        "deviceClient" => {
            return format!(
                "{} · {} {}",
                known(
                    row.device_name
                        .as_deref()
                ),
                known(
                    row.client_name
                        .as_deref()
                ),
                row.client_version
                    .as_deref()
                    .unwrap_or_default()
            )
            .trim()
            .to_string();
        }
        "routeClient" => {
            return format!(
                "{} · {}",
                row.route_template,
                known(
                    row.client_name
                        .as_deref()
                )
            );
        }
        "routeDevice" => {
            return format!(
                "{} · {}",
                row.route_template,
                known(
                    row.device_name
                        .as_deref()
                )
            );
        }
        _ => {}
    }
    let value = match dimension {
        "device" => row
            .device_name
            .as_deref(),
        "client" => row
            .client_name
            .as_deref(),
        "user" => row
            .user_name
            .as_deref(),
        "content" => row
            .item_name
            .as_deref(),
        "method" => Some(
            row.method
                .as_str(),
        ),
        "status" => return format!("{}xx", row.status / 100),
        "none" => Some("All requests"),
        _ => Some(
            row.route_template
                .as_str(),
        ),
    };
    value
        .filter(|v| {
            !v.trim()
                .is_empty()
        })
        .unwrap_or("Unknown")
        .to_string()
}

fn filter(
    builder: &mut QueryBuilder<'_, Sqlite>,
    column: &'static str,
    value: &Option<String>,
) {
    if let Some(value) = value
        .as_deref()
        .filter(|v| !v.is_empty())
    {
        builder
            .push(" AND ")
            .push(column)
            .push(" = ")
            .push_bind(value.to_string());
    }
}

fn playback_label(event: &str) -> String {
    match event {
        "server-playback-info-ready" | "server-playback-info-error" => {
            "Playback info".to_string()
        }
        "server-manifest-ready" | "server-manifest-error" => "HLS manifest".to_string(),
        "server-stream-ready" | "server-stream-error" => "Direct stream".to_string(),
        other => other
            .trim_start_matches("server-")
            .replace('-', " "),
    }
}

async fn playback_readiness(
    state: &AppState,
    query: &ExploreQuery,
    hours: i64,
    offset_hours: i64,
) -> Result<PlaybackReadiness> {
    let mut sql = QueryBuilder::<Sqlite>::new(
        "SELECT p.event, COALESCE(p.elapsed_ms, 0) AS elapsed_ms, \
         CASE WHEN p.event LIKE '%-error' OR p.error_category IS NOT NULL THEN 500 ELSE 200 END AS status \
         FROM telemetry_playback_events p \
         LEFT JOIN users u ON replace(lower(p.user_id), '-', '') = lower(hex(u.id)) \
         WHERE p.event IN ('server-playback-info-ready', 'server-playback-info-error', \
         'server-manifest-ready', 'server-manifest-error', 'server-stream-ready', 'server-stream-error') \
         AND p.details_json LIKE '%\"source\":\"server\"%' AND p.created_at >= ",
    );
    let range_end = Utc::now() - chrono::Duration::hours(offset_hours);
    let range_start = range_end - chrono::Duration::hours(hours);
    sql.push_bind(range_start.to_rfc3339())
        .push(" AND p.created_at < ")
        .push_bind(range_end.to_rfc3339());
    filter(&mut sql, "p.device_name", &query.device);
    filter(&mut sql, "p.client_name", &query.client);
    filter(&mut sql, "u.username", &query.user);
    filter(&mut sql, "p.item_name", &query.content);
    if let Some(route) = query
        .route
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        sql.push(" AND json_extract(p.details_json, '$.route') = ")
            .push_bind(route.to_string());
    }
    if let Some(status_class) = query
        .status_class
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        if status_class == "errors" {
            sql.push(" AND (p.event LIKE '%-error' OR p.error_category IS NOT NULL)");
        } else if let Some(prefix) = status_class.strip_suffix("xx") {
            if let Ok(hundred) = prefix.parse::<i64>() {
                sql.push(" AND CAST(json_extract(p.details_json, '$.status') AS INTEGER) >= ")
                    .push_bind(hundred * 100)
                    .push(" AND CAST(json_extract(p.details_json, '$.status') AS INTEGER) < ")
                    .push_bind((hundred + 1) * 100);
            }
        }
    }
    let rows = sql
        .build_query_as::<PlaybackRow>()
        .fetch_all(
            &state
                .ctx
                .db,
        )
        .await?;
    let mut summary = Acc::default();
    let mut breakdown: BTreeMap<String, Acc> = BTreeMap::new();
    for row in rows {
        summary.add(row.elapsed_ms, row.status);
        breakdown
            .entry(playback_label(&row.event))
            .or_default()
            .add(row.elapsed_ms, row.status);
    }
    Ok(PlaybackReadiness {
        summary: summary.finish(),
        breakdown: breakdown
            .into_iter()
            .map(|(label, acc)| BreakdownRow {
                label,
                stats: acc.finish(),
            })
            .collect(),
    })
}

async fn rows(
    state: &AppState,
    query: &ExploreQuery,
    hours: i64,
    offset_hours: i64,
) -> Result<Vec<RequestRow>> {
    let mut sql = QueryBuilder::<Sqlite>::new(
        "SELECT id, created_at, method, route_template, status, latency_ms, sample_reason, device_name, client_name, client_version, user_name, item_name, error_category FROM telemetry_request_events WHERE created_at >= ",
    );
    let range_end = Utc::now() - chrono::Duration::hours(offset_hours);
    let range_start = range_end - chrono::Duration::hours(hours);
    sql.push_bind(range_start.to_rfc3339())
        .push(" AND created_at < ")
        .push_bind(range_end.to_rfc3339());
    filter(&mut sql, "route_template", &query.route);
    filter(&mut sql, "device_name", &query.device);
    filter(&mut sql, "client_name", &query.client);
    filter(&mut sql, "user_name", &query.user);
    filter(&mut sql, "item_name", &query.content);
    filter(&mut sql, "method", &query.method);
    filter(&mut sql, "sample_reason", &query.sample_reason);
    match query
        .status_class
        .as_deref()
    {
        Some("2xx") => {
            sql.push(" AND status BETWEEN 200 AND 299");
        }
        Some("3xx") => {
            sql.push(" AND status BETWEEN 300 AND 399");
        }
        Some("4xx") => {
            sql.push(" AND status BETWEEN 400 AND 499");
        }
        Some("5xx") => {
            sql.push(" AND status BETWEEN 500 AND 599");
        }
        Some("errors") => {
            sql.push(" AND status >= 400");
        }
        _ => {}
    }
    sql.push(" ORDER BY created_at DESC LIMIT 500001");
    Ok(sql
        .build_query_as::<RequestRow>()
        .fetch_all(
            &state
                .ctx
                .db,
        )
        .await?)
}

async fn recent_rows(
    state: &AppState,
    query: &ExploreQuery,
    hours: i64,
) -> Result<Vec<RequestRow>> {
    let mut sql = QueryBuilder::<Sqlite>::new(
        "SELECT id, created_at, method, route_template, status, latency_ms, sample_reason, device_name, client_name, client_version, user_name, item_name, error_category FROM telemetry_request_events WHERE created_at >= ",
    );
    sql.push_bind(
        (Utc::now() - chrono::Duration::hours(hours.min(24 * 14))).to_rfc3339(),
    );
    filter(&mut sql, "route_template", &query.route);
    filter(&mut sql, "device_name", &query.device);
    filter(&mut sql, "client_name", &query.client);
    filter(&mut sql, "user_name", &query.user);
    filter(&mut sql, "item_name", &query.content);
    filter(&mut sql, "method", &query.method);
    filter(&mut sql, "sample_reason", &query.sample_reason);
    match query
        .status_class
        .as_deref()
    {
        Some("2xx") => sql.push(" AND status BETWEEN 200 AND 299"),
        Some("3xx") => sql.push(" AND status BETWEEN 300 AND 399"),
        Some("4xx") => sql.push(" AND status BETWEEN 400 AND 499"),
        Some("5xx") => sql.push(" AND status BETWEEN 500 AND 599"),
        Some("errors") => sql.push(" AND status >= 400"),
        _ => &mut sql,
    };
    sql.push(" ORDER BY created_at DESC LIMIT 100");
    Ok(sql
        .build_query_as::<RequestRow>()
        .fetch_all(
            &state
                .ctx
                .db,
        )
        .await?)
}

fn request_event(row: &RequestRow) -> RequestEvent {
    RequestEvent {
        id: row.id,
        created_at: row
            .created_at
            .clone(),
        method: row
            .method
            .clone(),
        route: row
            .route_template
            .clone(),
        status: row.status,
        latency_ms: row.latency_ms,
        sample_reason: row
            .sample_reason
            .clone(),
        device: row
            .device_name
            .clone()
            .unwrap_or_else(|| "Unknown".into()),
        client: row
            .client_name
            .clone()
            .unwrap_or_else(|| "Unknown".into()),
        client_version: row
            .client_version
            .clone()
            .unwrap_or_default(),
        user: row
            .user_name
            .clone()
            .unwrap_or_else(|| "Unknown".into()),
        content: row
            .item_name
            .clone()
            .unwrap_or_default(),
        error_category: row
            .error_category
            .clone()
            .unwrap_or_default(),
    }
}

fn rollup_label(row: &RollupRow, dimension: &str) -> String {
    let known = |value: &str| {
        if value
            .trim()
            .is_empty()
        {
            "Unknown".to_string()
        } else {
            value.to_string()
        }
    };
    match dimension {
        "deviceClient" => {
            return format!(
                "{} · {}",
                known(&row.device_name),
                known(&row.client_name)
            );
        }
        "routeClient" => {
            return format!("{} · {}", row.route_template, known(&row.client_name));
        }
        "routeDevice" => {
            return format!("{} · {}", row.route_template, known(&row.device_name));
        }
        _ => {}
    }
    let value = match dimension {
        "device" => row
            .device_name
            .as_str(),
        "client" => row
            .client_name
            .as_str(),
        "user" => row
            .user_name
            .as_str(),
        "content" => row
            .item_name
            .as_str(),
        "method" => row
            .method
            .as_str(),
        "status" => row
            .status_class
            .as_str(),
        "none" => "All requests",
        _ => row
            .route_template
            .as_str(),
    };
    if value
        .trim()
        .is_empty()
    {
        "Unknown".into()
    } else {
        value.into()
    }
}

async fn rollup_rows(
    state: &AppState,
    query: &ExploreQuery,
    hours: i64,
    offset_hours: i64,
) -> Result<Vec<RollupRow>> {
    let mut sql = QueryBuilder::<Sqlite>::new(
        "SELECT bucket_start, route_template, method, device_name, client_name, user_name, item_name, status_class, sample_reason, request_count, error_count, total_latency_ms, max_latency_ms, latency_lt_100, latency_lt_500, latency_lt_1000, latency_lt_2500, latency_lt_5000, latency_lt_10000, latency_ge_10000 FROM telemetry_hourly_rollups WHERE bucket_start >= ",
    );
    let range_end = Utc::now() - chrono::Duration::hours(offset_hours);
    let range_start = range_end - chrono::Duration::hours(hours);
    sql.push_bind(range_start.to_rfc3339())
        .push(" AND bucket_start < ")
        .push_bind(range_end.to_rfc3339());
    filter(&mut sql, "route_template", &query.route);
    filter(&mut sql, "device_name", &query.device);
    filter(&mut sql, "client_name", &query.client);
    filter(&mut sql, "user_name", &query.user);
    filter(&mut sql, "item_name", &query.content);
    filter(&mut sql, "method", &query.method);
    filter(&mut sql, "sample_reason", &query.sample_reason);
    if let Some(status) = query
        .status_class
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        if status == "errors" {
            sql.push(" AND status_class IN ('4xx','5xx')");
        } else {
            sql.push(" AND status_class = ")
                .push_bind(status.to_string());
        }
    }
    sql.push(" ORDER BY bucket_start ASC LIMIT 500001");
    Ok(sql
        .build_query_as::<RollupRow>()
        .fetch_all(
            &state
                .ctx
                .db,
        )
        .await?)
}

fn sort_breakdown(rows: &mut Vec<BreakdownRow>, query: &ExploreQuery) {
    let sort = query
        .sort_by
        .as_deref()
        .unwrap_or("p95");
    if sort == "label" {
        rows.sort_by(|a, b| {
            a.label
                .to_lowercase()
                .cmp(
                    &b.label
                        .to_lowercase(),
                )
        });
        if query
            .sort_dir
            .as_deref()
            != Some("asc")
        {
            rows.reverse();
        }
    } else {
        rows.sort_by(|a, b| {
            let value = |row: &BreakdownRow| match sort {
                "count" => {
                    row.stats
                        .count as f64
                }
                "errors" => {
                    row.stats
                        .error_count as f64
                }
                "errorRate" => {
                    row.stats
                        .error_rate
                }
                "mean" => {
                    row.stats
                        .mean_latency_ms
                }
                "p50" => {
                    row.stats
                        .p50_latency_ms
                }
                "max" => {
                    row.stats
                        .max_latency_ms
                }
                _ => {
                    row.stats
                        .p95_latency_ms
                }
            };
            value(b).total_cmp(&value(a))
        });
        if query
            .sort_dir
            .as_deref()
            == Some("asc")
        {
            rows.reverse();
        }
    }
    rows.truncate(
        query
            .limit
            .unwrap_or(100)
            .clamp(1, 200),
    );
}

async fn explore_rollups(
    state: &AppState,
    query: &ExploreQuery,
    hours: i64,
    offset_hours: i64,
    bucket_minutes: i64,
    group_by: String,
) -> Result<ExploreResponse> {
    let playback_readiness =
        playback_readiness(state, query, hours, offset_hours).await?;
    let mut rows = rollup_rows(state, query, hours, offset_hours).await?;
    let truncated = rows.len() > 500_000;
    rows.truncate(500_000);
    let mut summary = RollupAcc::default();
    let mut breakdown: HashMap<String, RollupAcc> = HashMap::new();
    let mut series: BTreeMap<(i64, String), RollupAcc> = BTreeMap::new();
    let mut filters = FilterOptions::default();
    for row in &rows {
        summary.add(row);
        let label = rollup_label(row, &group_by);
        breakdown
            .entry(label.clone())
            .or_default()
            .add(row);
        if let Ok(time) = DateTime::parse_from_rfc3339(&row.bucket_start) {
            let width = bucket_minutes.max(60) * 60;
            let bucket = time
                .timestamp()
                .div_euclid(width)
                * width;
            series
                .entry((bucket, label))
                .or_default()
                .add(row);
        }
        filters
            .routes
            .push(
                row.route_template
                    .clone(),
            );
        filters
            .devices
            .push(
                row.device_name
                    .clone(),
            );
        filters
            .clients
            .push(
                row.client_name
                    .clone(),
            );
        filters
            .users
            .push(
                row.user_name
                    .clone(),
            );
        filters
            .contents
            .push(
                row.item_name
                    .clone(),
            );
        filters
            .methods
            .push(
                row.method
                    .clone(),
            );
        filters
            .sample_reasons
            .push(
                row.sample_reason
                    .clone(),
            );
    }
    for values in [
        &mut filters.routes,
        &mut filters.devices,
        &mut filters.clients,
        &mut filters.users,
        &mut filters.contents,
        &mut filters.methods,
        &mut filters.sample_reasons,
    ] {
        values.retain(|value| !value.is_empty());
        values.sort();
        values.dedup();
    }
    let mut breakdown: Vec<_> = breakdown
        .into_iter()
        .map(|(label, acc)| BreakdownRow {
            label,
            stats: acc.finish(),
        })
        .collect();
    sort_breakdown(&mut breakdown, query);
    let series = series
        .into_iter()
        .filter_map(|((bucket, label), acc)| {
            DateTime::<Utc>::from_timestamp(bucket, 0).map(|time| SeriesPoint {
                bucket_start: time.to_rfc3339(),
                label,
                stats: acc.finish(),
            })
        })
        .collect();
    let recent = if offset_hours == 0 {
        recent_rows(state, query, hours)
            .await?
            .iter()
            .map(request_event)
            .collect()
    } else {
        Vec::new()
    };
    Ok(ExploreResponse {
        hours,
        offset_hours,
        bucket_minutes: bucket_minutes.max(60),
        group_by,
        resolution: "hourly-rollup".into(),
        captured_rows: rows.len(),
        truncated,
        summary: summary.finish(),
        series,
        breakdown,
        recent,
        filters,
        playback_readiness,
    })
}

#[get("/remux/telemetry/explore")]
pub async fn explore(
    State(state): State<AppState>,
    _admin: auth::AdminSession,
    Query(query): Query<ExploreQuery>,
) -> Result<impl IntoResponse> {
    let hours = query
        .hours
        .unwrap_or(24)
        .clamp(1, 24 * 180);
    let offset_hours = query
        .offset_hours
        .unwrap_or(0)
        .clamp(0, 24 * 180);
    let bucket_minutes = query
        .bucket_minutes
        .unwrap_or(if hours <= 6 {
            5
        } else if hours <= 48 {
            30
        } else {
            240
        })
        .clamp(1, 1440);
    let group_by = match query
        .group_by
        .as_deref()
    {
        Some(
            "device" | "client" | "user" | "content" | "method" | "status" | "none"
            | "deviceClient" | "routeClient" | "routeDevice",
        ) => query
            .group_by
            .clone()
            .unwrap(),
        _ => "route".to_string(),
    };
    if hours + offset_hours > 48 {
        return Ok(Json(
            explore_rollups(
                &state,
                &query,
                hours,
                offset_hours,
                bucket_minutes,
                group_by,
            )
            .await?,
        ));
    }
    let mut rows = rows(&state, &query, hours, offset_hours).await?;
    let playback_readiness =
        playback_readiness(&state, &query, hours, offset_hours).await?;
    let truncated = rows.len() > 500_000;
    rows.truncate(500_000);
    let mut summary = Acc::default();
    let mut breakdown: HashMap<String, Acc> = HashMap::new();
    let mut series: BTreeMap<(i64, String), Acc> = BTreeMap::new();
    let mut routes = BTreeSet::new();
    let mut devices = BTreeSet::new();
    let mut clients = BTreeSet::new();
    let mut users = BTreeSet::new();
    let mut contents = BTreeSet::new();
    let mut methods = BTreeSet::new();
    let mut reasons = BTreeSet::new();
    for row in &rows {
        summary.add(row.latency_ms, row.status);
        let group = label(row, &group_by);
        breakdown
            .entry(group.clone())
            .or_default()
            .add(row.latency_ms, row.status);
        if let Ok(time) = DateTime::parse_from_rfc3339(&row.created_at) {
            let width = bucket_minutes * 60;
            let bucket = time
                .timestamp()
                .div_euclid(width)
                * width;
            series
                .entry((bucket, group))
                .or_default()
                .add(row.latency_ms, row.status);
        }
        routes.insert(
            row.route_template
                .clone(),
        );
        methods.insert(
            row.method
                .clone(),
        );
        reasons.insert(
            row.sample_reason
                .clone(),
        );
        if let Some(v) = row
            .device_name
            .as_ref()
            .filter(|v| !v.is_empty())
        {
            devices.insert(v.clone());
        }
        if let Some(v) = row
            .client_name
            .as_ref()
            .filter(|v| !v.is_empty())
        {
            clients.insert(v.clone());
        }
        if let Some(v) = row
            .user_name
            .as_ref()
            .filter(|v| !v.is_empty())
        {
            users.insert(v.clone());
        }
        if let Some(v) = row
            .item_name
            .as_ref()
            .filter(|v| !v.is_empty())
        {
            contents.insert(v.clone());
        }
    }
    let mut breakdown: Vec<_> = breakdown
        .into_iter()
        .map(|(label, acc)| BreakdownRow {
            label,
            stats: acc.finish(),
        })
        .collect();
    sort_breakdown(&mut breakdown, &query);
    let series = series
        .into_iter()
        .filter_map(|((bucket, label), acc)| {
            DateTime::<Utc>::from_timestamp(bucket, 0).map(|time| SeriesPoint {
                bucket_start: time.to_rfc3339(),
                label,
                stats: acc.finish(),
            })
        })
        .collect();
    let recent = rows
        .iter()
        .take(100)
        .map(request_event)
        .collect();
    let filters = FilterOptions {
        routes: routes
            .into_iter()
            .collect(),
        devices: devices
            .into_iter()
            .collect(),
        clients: clients
            .into_iter()
            .collect(),
        users: users
            .into_iter()
            .collect(),
        contents: contents
            .into_iter()
            .collect(),
        methods: methods
            .into_iter()
            .collect(),
        sample_reasons: reasons
            .into_iter()
            .collect(),
    };
    Ok(Json(ExploreResponse {
        hours,
        offset_hours,
        bucket_minutes,
        group_by,
        resolution: "raw".into(),
        captured_rows: rows.len(),
        truncated,
        summary: summary.finish(),
        series,
        breakdown,
        recent,
        filters,
        playback_readiness,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percentiles_and_errors() {
        let mut a = Acc::default();
        for n in 1..=100 {
            a.add(n as f64, if n > 95 { 500 } else { 200 });
        }
        let s = a.finish();
        assert_eq!(s.p50_latency_ms, 50.0);
        assert_eq!(s.p95_latency_ms, 95.0);
        assert_eq!(s.p99_latency_ms, 99.0);
        assert_eq!(s.error_count, 5);
    }

    #[test]
    fn rollup_percentiles_use_server_latency_bands() {
        let acc = RollupAcc {
            count: 100,
            errors: 2,
            total: 20_000.0,
            max: 12_000.0,
            bands: [60, 30, 5, 2, 1, 1, 1],
        };
        let stats = acc.finish();
        assert_eq!(stats.p50_latency_ms, 100.0);
        assert_eq!(stats.p95_latency_ms, 1_000.0);
        assert_eq!(stats.p99_latency_ms, 10_000.0);
        assert_eq!(stats.error_rate, 2.0);
    }

    #[test]
    fn composite_dimensions_and_label_sort_are_stable() {
        let row = RequestRow {
            id: 1,
            created_at: "2026-01-01T00:00:00Z".into(),
            method: "GET".into(),
            route_template: "/items/{id}".into(),
            status: 200,
            latency_ms: 10.0,
            sample_reason: "sample".into(),
            device_name: Some("Living Room".into()),
            client_name: Some("Jellyflix".into()),
            client_version: Some("2.3.1".into()),
            user_name: Some("viewer".into()),
            item_name: Some("Episode".into()),
            error_category: None,
        };
        assert_eq!(label(&row, "deviceClient"), "Living Room · Jellyflix 2.3.1");
        assert_eq!(label(&row, "routeClient"), "/items/{id} · Jellyflix");

        let mut rows = vec![
            BreakdownRow {
                label: "Zulu".into(),
                stats: Stats::default(),
            },
            BreakdownRow {
                label: "alpha".into(),
                stats: Stats::default(),
            },
        ];
        sort_breakdown(
            &mut rows,
            &ExploreQuery {
                sort_by: Some("label".into()),
                sort_dir: Some("asc".into()),
                ..Default::default()
            },
        );
        assert_eq!(rows[0].label, "alpha");
    }
}
