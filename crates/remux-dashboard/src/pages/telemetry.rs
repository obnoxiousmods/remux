use crate::{
    components::{Card, EmptyState, LoadingText},
    state::AppState,
};
use dioxus::prelude::*;
use remux_sdks::remux::{
    DeleteTelemetryView, GetTelemetryExplore, GetTelemetryViews, SaveTelemetryView,
    TelemetryBreakdownRow, TelemetryExploreResponse, TelemetrySavedView,
    TelemetrySeriesPoint,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
}

fn metric(point: &TelemetrySeriesPoint, key: &str) -> f64 {
    match key {
        "count" => {
            point
                .stats
                .count as f64
        }
        "errors" => {
            point
                .stats
                .error_count as f64
        }
        "errorRate" => {
            point
                .stats
                .error_rate
        }
        "mean" => {
            point
                .stats
                .mean_latency_ms
        }
        "p50" => {
            point
                .stats
                .p50_latency_ms
        }
        "max" => {
            point
                .stats
                .max_latency_ms
        }
        _ => {
            point
                .stats
                .p95_latency_ms
        }
    }
}

fn points(values: &[f64], max: f64) -> String {
    let width = 920.0;
    let height = 230.0;
    let denominator = (values
        .len()
        .saturating_sub(1)
        .max(1)) as f64;
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let x = index as f64 / denominator * width;
            let y = height - (value / max.max(1.0) * height);
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[component]
fn TelemetryChart(
    series: Vec<TelemetrySeriesPoint>,
    breakdown: Vec<TelemetryBreakdownRow>,
    metric_key: String,
) -> Element {
    let labels: Vec<String> = breakdown
        .iter()
        .take(6)
        .map(|row| {
            row.label
                .clone()
        })
        .collect();
    let mut grouped: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for point in &series {
        if labels.contains(&point.label) {
            grouped
                .entry(
                    point
                        .label
                        .clone(),
                )
                .or_default()
                .push(metric(point, &metric_key));
        }
    }
    let max = grouped
        .values()
        .flatten()
        .copied()
        .fold(0.0_f64, f64::max);
    let colors = [
        "#7c3aed", "#06b6d4", "#22c55e", "#f59e0b", "#ef4444", "#ec4899",
    ];
    rsx! {
        div { class:"telemetry-chart",
            if grouped.is_empty() { div { class:"telemetry-empty-chart", "No data matches these filters." } }
            else {
                svg { view_box:"0 0 920 250", preserve_aspect_ratio:"none", role:"img",
                    for y in [0, 58, 116, 174, 232] { line { x1:"0",y1:"{y}",x2:"920",y2:"{y}",class:"telemetry-grid-line" } }
                    for (index,(label,values)) in grouped.iter().enumerate() {
                        polyline { key:"{label}", points:"{points(values,max)}", fill:"none", stroke:"{colors[index%colors.len()]}", stroke_width:"3", vector_effect:"non-scaling-stroke" }
                    }
                }
                div { class:"telemetry-legend", for (index,label) in grouped.keys().enumerate() {
                    span { key:"{label}", i { style:"background:{colors[index%colors.len()]}" } "{label}" }
                } }
            }
        }
    }
}

#[component]
pub fn TelemetryPage(app_state: AppState) -> Element {
    let mut data = use_signal(|| None::<TelemetryExploreResponse>);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);
    let mut hours = use_signal(|| 24_i64);
    let mut bucket = use_signal(|| 30_i64);
    let mut group = use_signal(|| "route".to_string());
    let mut metric_key = use_signal(|| "p95".to_string());
    let mut route = use_signal(String::new);
    let mut device = use_signal(String::new);
    let mut client = use_signal(String::new);
    let mut user = use_signal(String::new);
    let mut content = use_signal(String::new);
    let mut method = use_signal(String::new);
    let mut status = use_signal(String::new);
    let mut reason = use_signal(String::new);
    let mut sort_by = use_signal(|| "p95".to_string());
    let mut sort_dir = use_signal(|| "desc".to_string());
    let mut refresh = use_signal(|| 0_u64);
    let mut views = use_signal(Vec::<TelemetrySavedView>::new);
    let mut view_name = use_signal(String::new);
    let mut view_message = use_signal(String::new);

    let client_for_effect = app_state
        .client
        .clone();
    use_effect(move || {
        let _ = *refresh.read();
        let request = GetTelemetryExplore {
            hours: *hours.read(),
            bucket_minutes: *bucket.read(),
            group_by: group(),
            route: route(),
            device: device(),
            client: client(),
            user: user(),
            content: content(),
            method: method(),
            status_class: status(),
            sample_reason: reason(),
            sort_by: sort_by(),
            sort_dir: sort_dir(),
        };
        let api = client_for_effect.clone();
        loading.set(true);
        spawn(async move {
            match api
                .execute(request)
                .await
            {
                Ok(value) => {
                    data.set(Some(value));
                    error.set(None)
                }
                Err(e) => error.set(Some(format!("Failed to load telemetry: {e}"))),
            }
            loading.set(false);
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

    let current_config = move || ViewConfig {
        hours: *hours.read(),
        bucket_minutes: *bucket.read(),
        group_by: group(),
        metric: metric_key(),
        route: route(),
        device: device(),
        client: client(),
        user: user(),
        content: content(),
        method: method(),
        status_class: status(),
        sample_reason: reason(),
        sort_by: sort_by(),
        sort_dir: sort_dir(),
    };
    rsx! {
        div { class:"telemetry-page",
            div { class:"telemetry-toolbar",
                div { h1 { "Telemetry Explorer" } p { "Every endpoint, attributable to devices and clients, with one filter state across charts and tables." } }
                div { class:"telemetry-view-actions",
                    input { class:"form-input", placeholder:"Saved view name", value:"{view_name}", oninput:move|e|view_name.set(e.value()) }
                    button { class:"btn btn-primary", disabled:view_name.read().trim().is_empty(), onclick:{let api=app_state.client.clone();move|_|{let api=api.clone();let name=view_name().trim().to_string();let config=serde_json::to_value(current_config()).unwrap_or_default();spawn(async move{match api.execute(SaveTelemetryView{name,config}).await{Ok(_)=>{view_message.set("Saved".into());if let Ok(v)=api.execute(GetTelemetryViews).await{views.set(v);}},Err(e)=>view_message.set(format!("Save failed: {e}"))}});}}, "Save view" }
                }
            }
            if !view_message.read().is_empty(){ div { class:"telemetry-message", "{view_message}" } }
            if !views.read().is_empty(){ div { class:"telemetry-saved-views", for saved in views.read().clone(){
                {let apply=saved.clone();let remove=saved.clone();rsx!{ span { class:"telemetry-view-chip",
                    button { onclick:move|_|{if let Ok(cfg)=serde_json::from_str::<ViewConfig>(&apply.config_json){hours.set(cfg.hours);bucket.set(cfg.bucket_minutes);group.set(cfg.group_by);metric_key.set(cfg.metric);route.set(cfg.route);device.set(cfg.device);client.set(cfg.client);user.set(cfg.user);content.set(cfg.content);method.set(cfg.method);status.set(cfg.status_class);reason.set(cfg.sample_reason);sort_by.set(cfg.sort_by);sort_dir.set(cfg.sort_dir);refresh+=1;}}, "{saved.name}" }
                    button { class:"telemetry-view-delete", title:"Delete shared view", onclick:{let api=app_state.client.clone();move|_|{let api=api.clone();let id=remove.id.clone();spawn(async move{let _=api.execute(DeleteTelemetryView{id}).await;if let Ok(v)=api.execute(GetTelemetryViews).await{views.set(v);}});}}, "×" }
                }}}
            } } }
            Card { title:"Filters",
                div { class:"telemetry-filter-grid",
                    label { "Time range" select { value:"{hours}",onchange:move|e|if let Ok(v)=e.value().parse(){hours.set(v);}, option{value:"1","1 hour"} option{value:"6","6 hours"} option{value:"24","24 hours"} option{value:"168","7 days"} option{value:"336","14 days"} option{value:"720","30 days"} option{value:"2160","90 days"} option{value:"4320","180 days"} } }
                    label { "Bucket" select { value:"{bucket}",onchange:move|e|if let Ok(v)=e.value().parse(){bucket.set(v);}, option{value:"1","1 minute"} option{value:"5","5 minutes"} option{value:"30","30 minutes"} option{value:"60","1 hour"} option{value:"240","4 hours"} option{value:"1440","1 day"} } }
                    label { "Group graph/table by" select { value:"{group}",onchange:move|e|group.set(e.value()),option{value:"route","Endpoint"}option{value:"device","Device"}option{value:"client","Client"}option{value:"user","User"}option{value:"content","Content"}option{value:"method","Method"}option{value:"status","Status"}option{value:"none","All requests"} } }
                    label { "Graph metric" select { value:"{metric_key}",onchange:move|e|metric_key.set(e.value()),option{value:"p95","p95 latency"}option{value:"p50","p50 latency"}option{value:"mean","Mean latency"}option{value:"max","Max latency"}option{value:"count","Request count"}option{value:"errors","Error count"}option{value:"errorRate","Error rate"} } }
                    label { "Endpoint" select { value:"{route}",onchange:move|e|route.set(e.value()),option{value:"","All endpoints"} if let Some(d)=data.read().as_ref(){for v in &d.filters.routes{option{value:"{v}","{v}"}}} } }
                    label { "Device" select { value:"{device}",onchange:move|e|device.set(e.value()),option{value:"","All devices"} if let Some(d)=data.read().as_ref(){for v in &d.filters.devices{option{value:"{v}","{v}"}}} } }
                    label { "Client" select { value:"{client}",onchange:move|e|client.set(e.value()),option{value:"","All clients"} if let Some(d)=data.read().as_ref(){for v in &d.filters.clients{option{value:"{v}","{v}"}}} } }
                    label { "User" select { value:"{user}",onchange:move|e|user.set(e.value()),option{value:"","All users"} if let Some(d)=data.read().as_ref(){for v in &d.filters.users{option{value:"{v}","{v}"}}} } }
                    label { "Method" select { value:"{method}",onchange:move|e|method.set(e.value()),option{value:"","All methods"} if let Some(d)=data.read().as_ref(){for v in &d.filters.methods{option{value:"{v}","{v}"}}} } }
                    label { "Status" select { value:"{status}",onchange:move|e|status.set(e.value()),option{value:"","All statuses"}option{value:"2xx","2xx"}option{value:"3xx","3xx"}option{value:"4xx","4xx"}option{value:"5xx","5xx"}option{value:"errors","All errors"} } }
                    label { "Capture reason" select { value:"{reason}",onchange:move|e|reason.set(e.value()),option{value:"","All requests"} if let Some(d)=data.read().as_ref(){for v in &d.filters.sample_reasons{option{value:"{v}","{v}"}}} } }
                    div { class:"telemetry-filter-actions", button { class:"btn btn-ghost",onclick:move|_|{route.set(String::new());device.set(String::new());client.set(String::new());user.set(String::new());content.set(String::new());method.set(String::new());status.set(String::new());reason.set(String::new());refresh+=1;},"Clear filters" } button{class:"btn btn-primary",onclick:move|_|refresh+=1,"Refresh"} }
                }
            }
            if *loading.read(){LoadingText{}} else if let Some(message)=error.read().as_ref(){div{class:"telemetry-error","{message}"}} else if let Some(result)=data.read().as_ref(){
                div { class:"telemetry-message", if result.resolution=="raw" { "Exact raw request telemetry" } else { "Server-side hourly rollups; long-range percentiles use latency bands" } " · {result.captured_rows} stored rows" if result.truncated { " · result capped" } }
                div { class:"telemetry-stat-grid",
                    for (label,value) in [("Requests",format!("{}",result.summary.count)),("Errors",format!("{} ({:.1}%)",result.summary.error_count,result.summary.error_rate)),("Mean",format!("{:.1} ms",result.summary.mean_latency_ms)),("p50",format!("{:.1} ms",result.summary.p50_latency_ms)),("p95",format!("{:.1} ms",result.summary.p95_latency_ms)),("p99",format!("{:.1} ms",result.summary.p99_latency_ms)),("Max",format!("{:.1} ms",result.summary.max_latency_ms))] { div { class:"telemetry-stat",span{"{label}"}strong{"{value}"} } }
                }
                Card { title:format!("{} over time",metric_key()), TelemetryChart { series:result.series.clone(),breakdown:result.breakdown.clone(),metric_key:metric_key() } }
                Card { title:format!("By {}",group()), tight:true,
                    if result.breakdown.is_empty(){EmptyState{message:"No matching telemetry."}} else { div { class:"data-table-container telemetry-table", table { thead { tr { th{"Name"} for (key,label) in [("count","Requests"),("errors","Errors"),("errorRate","Error %"),("mean","Mean"),("p50","p50"),("p95","p95"),("p99","p99"),("max","Max")] { th { button { onclick:move|_|{if sort_by()==key{sort_dir.set(if sort_dir()=="desc"{"asc".into()}else{"desc".into()});}else{sort_by.set(key.into());sort_dir.set("desc".into());}refresh+=1;},"{label}" } } } } } tbody { for row in &result.breakdown { tr { td{class:"telemetry-label","{row.label}"}td{"{row.stats.count}"}td{"{row.stats.error_count}"}td{"{row.stats.error_rate:.1}%"}td{"{row.stats.mean_latency_ms:.1}"}td{"{row.stats.p50_latency_ms:.1}"}td{"{row.stats.p95_latency_ms:.1}"}td{"{row.stats.p99_latency_ms:.1}"}td{"{row.stats.max_latency_ms:.1}"} } } } } } }
                }
                Card { title:"Recent requests", tight:true, div { class:"data-table-container telemetry-table telemetry-events", table { thead { tr { th{"Time"}th{"Endpoint"}th{"Status"}th{"Latency"}th{"Device"}th{"Client"}th{"User"}th{"Content"} } } tbody { for event in &result.recent { tr { td{"{event.created_at}"}td{class:"telemetry-label","{event.method} {event.route}"}td{class:if event.status>=400{"telemetry-status-error"}else{""},"{event.status}"}td{"{event.latency_ms:.1} ms"}td{"{event.device}"}td{"{event.client} {event.client_version}"}td{"{event.user}"}td{"{event.content}"} } } } } } }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chart_points_are_bounded() {
        let p = points(&[0.0, 50.0, 100.0], 100.0);
        assert!(p.contains("0.0,230.0"));
        assert!(p.contains("920.0,0.0"));
    }
}
