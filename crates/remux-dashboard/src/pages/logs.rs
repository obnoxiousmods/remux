use crate::{
    components::{Card, EmptyState, LoadingText},
    state::{fmt_time, get_origin, AppState},
};
use dioxus::prelude::*;
use remux_sdks::remux::{GetLogFiles, GetLogTail, LogFile, LogTailResponse};

fn humanize_size(bytes: i64) -> String {
    if bytes < 0 {
        return "—".into();
    }
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn log_url(origin: &str, token: &str, name: &str) -> String {
    format!(
        "{}/system/logs/log?name={}&api_key={}",
        origin.trim_end_matches('/'),
        urlencoding::encode(name),
        urlencoding::encode(token)
    )
}

#[component]
pub fn LogsPage(app_state: AppState) -> Element {
    let mut files = use_signal(Vec::<LogFile>::new);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);
    let mut selected = use_signal(String::new);
    let mut tail = use_signal(|| None::<LogTailResponse>);
    let mut tail_loading = use_signal(|| false);
    let mut search = use_signal(String::new);
    let mut level = use_signal(|| "all".to_string());
    let mut line_count = use_signal(|| 500_usize);
    let mut refresh = use_signal(|| 0_u64);
    let token = app_state
        .server
        .access_token
        .clone();
    let origin = {
        let value = get_origin();
        if value.is_empty() {
            app_state
                .server
                .manual_address
                .clone()
        } else {
            value
        }
    };

    let list_client = app_state
        .client
        .clone();
    use_effect(move || {
        let api = list_client.clone();
        loading.set(true);
        spawn(async move {
            match api
                .execute(GetLogFiles)
                .await
            {
                Ok(list) => {
                    if selected
                        .read()
                        .is_empty()
                    {
                        if let Some(name) = list
                            .first()
                            .and_then(|file| {
                                file.name
                                    .clone()
                            })
                        {
                            selected.set(name);
                        }
                    }
                    files.set(list);
                    error.set(None);
                }
                Err(err) => error.set(Some(format!("Failed to load logs: {err}"))),
            }
            loading.set(false);
        });
    });

    let tail_client = app_state
        .client
        .clone();
    use_effect(move || {
        let _ = *refresh.read();
        let name = selected();
        if name.is_empty() {
            return;
        }
        let api = tail_client.clone();
        let request = GetLogTail {
            name,
            lines: *line_count.read(),
            search: search(),
            level: level(),
        };
        tail_loading.set(true);
        spawn(async move {
            match api
                .execute(request)
                .await
            {
                Ok(value) => {
                    tail.set(Some(value));
                    error.set(None);
                }
                Err(err) => error.set(Some(format!("Failed to read log tail: {err}"))),
            }
            tail_loading.set(false);
        });
    });

    rsx! {
        div { class: "logs-explorer",
            div { class: "telemetry-toolbar",
                div { h1 { "Logs Explorer" } p { "Fast tailing and filtering without downloading the entire server log." } }
                button { class: "btn btn-ghost", onclick: move |_| refresh += 1, "Refresh" }
            }
            if let Some(message) = error.read().as_ref() { div { class: "telemetry-error", "{message}" } }
            div { class: "logs-layout",
                Card { title: "Log files", tight: true,
                    if *loading.read() { LoadingText {} }
                    else if files.read().is_empty() { EmptyState { message: "No log files yet." } }
                    else { div { class: "logs-file-list",
                        for file in files.read().clone() {
                            { let name=file.name.clone().unwrap_or_default(); let active=selected()==name;
                              let size=humanize_size(file.size.unwrap_or(0));
                              let modified=file.date_modified.map(|date|fmt_time(date.format("%Y-%m-%d %H:%M"))).unwrap_or_else(||"—".into());
                              let pick=name.clone(); rsx! {
                                button { key: "{name}", class: if active { "logs-file active" } else { "logs-file" },
                                    onclick: move |_| selected.set(pick.clone()), strong { "{name}" } span { "{size} · {modified}" } }
                              }
                            }
                        }
                    } }
                }
                Card { title: if selected.read().is_empty() { "Log viewer".into() } else { selected() },
                    div { class: "logs-controls",
                        input { class: "form-input", placeholder: "Filter text", value: "{search}", oninput: move |event| search.set(event.value()) }
                        select { value: "{level}", onchange: move |event| level.set(event.value()),
                            option { value: "all", "All levels" } option { value: "error", "Errors" }
                            option { value: "warn", "Warnings" } option { value: "info", "Info" } option { value: "debug", "Debug" }
                        }
                        select { value: "{line_count}", onchange: move |event| if let Ok(value)=event.value().parse(){line_count.set(value);},
                            option { value: "100", "100 lines" } option { value: "500", "500 lines" }
                            option { value: "1000", "1,000 lines" } option { value: "5000", "5,000 lines" }
                        }
                        if !selected.read().is_empty() { a { class: "btn btn-ghost", href: "{log_url(&origin,&token,&selected())}", download: "{selected}", "Download full file" } }
                    }
                    if *tail_loading.read() { LoadingText {} }
                    else if let Some(result)=tail.read().as_ref() {
                        div { class: "logs-tail-meta", "Showing {result.lines.len()} matching lines · scanned {humanize_size(result.scanned_bytes as i64)} of {humanize_size(result.file_size as i64)}" if result.truncated { " · tail window" } }
                        pre { class: "logs-tail",
                            for (index,line) in result.lines.iter().enumerate() {
                                span { key: "{index}", class: if line.to_lowercase().contains("error") { "log-error" } else if line.to_lowercase().contains("warn") { "log-warn" } else { "" }, "{line}\n" }
                            }
                        }
                    } else { EmptyState { message: "Select a log file." } }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sizes() {
        assert_eq!(humanize_size(0), "0 B");
        assert_eq!(humanize_size(1536), "1.5 KB");
        assert_eq!(humanize_size(-1), "—");
    }
    #[test]
    fn urls() {
        let url = log_url("https://x/", "tok en", "a b.log");
        assert!(url.contains("name=a%20b.log"));
        assert!(url.contains("api_key=tok%20en"));
    }
}
