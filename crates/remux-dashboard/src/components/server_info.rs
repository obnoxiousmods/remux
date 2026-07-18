use crate::{
    components::{Card, LoadingText},
    state::AppState,
};
use dioxus::prelude::*;
use remux_sdks::remux::{GetItemCounts, ItemCounts, PublicSystemInfo, RestartServer};

/// Parse an RFC 3339 timestamp (the format chrono::to_rfc3339() produces)
/// into seconds since the Unix epoch.
fn parse_rfc3339(s: &str) -> Option<u64> {
    // Format: YYYY-MM-DDTHH:MM:SS[.fraction][Z|+HH:MM|-HH:MM]
    if s.len() < 19 || s.as_bytes()[10] != b'T' {
        return None;
    }
    let year: i64 = s[0..4]
        .parse()
        .ok()?;
    let month: u32 = s[5..7]
        .parse()
        .ok()?;
    let day: u32 = s[8..10]
        .parse()
        .ok()?;
    let hour: u32 = s[11..13]
        .parse()
        .ok()?;
    let min: u32 = s[14..16]
        .parse()
        .ok()?;
    let sec: u32 = s[17..19]
        .parse()
        .ok()?;

    // Convert to Unix timestamp using the Gregorian calendar algorithm.
    let (y, m) = if month <= 2 {
        (year - 1, month as i64 + 12)
    } else {
        (year, month as i64)
    };
    // Days since 1970-01-01
    let days =
        y * 365 + y / 4 - y / 100 + y / 400 + (153 * m + 3) / 5 + day as i64 - 719469;
    Some((days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64) as u64)
}

fn format_uptime(started_at: &str) -> String {
    let now = (js_sys::Date::now() / 1000.0) as u64;
    let started = parse_rfc3339(started_at).unwrap_or(0);
    if started == 0 || started > now {
        return "N/A".to_string();
    }
    let diff_secs = now - started;
    let days = diff_secs / 86400;
    let hours = (diff_secs % 86400) / 3600;
    let mins = (diff_secs % 3600) / 60;
    format!("{days}d {hours}h {mins}m")
}

#[component]
pub fn KvRow(label: &'static str, value: String) -> Element {
    rsx! {
        div { class: "kv-row",
            span { class: "kv-label", "{label}" }
            span { class: "kv-value", "{value}" }
        }
    }
}

/// Group digits with thousands separators (`104727` → `104,727`) so large
/// library counts read at a glance. Pure, so it is unit-tested.
pub fn fmt_count(n: impl Into<i64>) -> String {
    let n = n.into();
    let digits = n
        .unsigned_abs()
        .to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3 + 1);
    for (i, ch) in digits
        .chars()
        .enumerate()
    {
        // Insert a separator before every group of three from the right.
        if i != 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// A single dashboard statistic: a large tabular number over a label.
#[component]
pub fn StatTile(num: String, label: &'static str) -> Element {
    rsx! {
        div { class: "stat-tile",
            span { class: "stat-num", "{num}" }
            span { class: "stat-key", "{label}" }
        }
    }
}

#[component]
pub fn ServerInfoCard(app_state: AppState) -> Element {
    let mut server_info: Signal<Option<PublicSystemInfo>> = use_signal(|| None);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| Option::<String>::None);
    let mut restarting = use_signal(|| false);
    let app_state_for_effect = app_state.clone();
    let app_state_for_restart = app_state.clone();

    use_effect(move || {
        let client = app_state_for_effect
            .client
            .clone();
        spawn(async move {
            match client
                .execute(PublicSystemInfo::default())
                .await
            {
                Ok(info) => {
                    server_info.set(Some(info));
                    error.set(None);
                }
                Err(e) => error.set(Some(format!("Failed to fetch server info: {e}"))),
            }
            loading.set(false);
        });
    });

    let on_restart = move |_| {
        let client = app_state_for_restart
            .client
            .clone();
        restarting.set(true);
        spawn(async move {
            let _ = client
                .execute(RestartServer)
                .await;
            // Give the server a moment to restart, then refresh the page
            gloo_timers::future::sleep(std::time::Duration::from_secs(10)).await;
            if let Some(window) = web_sys::window() {
                window
                    .location()
                    .reload()
                    .ok();
            }
        });
    };

    rsx! {
        Card { title: "Server",
            if *loading.read() {
                LoadingText {}
            } else if let Some(err) = error.read().as_ref() {
                span { class: "loading-text", style: "color:var(--error)", "{err}" }
            } else if let Some(info) = server_info.read().as_ref() {
                KvRow { label: "Name", value: info.server_name.clone() }
                KvRow { label: "Version", value: info.remux_version.clone() }
                div { class: "kv-row",
                    span { class: "kv-label", "Uptime" }
                    span { class: "kv-value",
                        {
                            info.remux_started_at
                                .as_deref()
                                .map(format_uptime)
                                .unwrap_or_else(|| "N/A".to_string())
                        }
                        button {
                            class: "btn btn-ghost",
                            style: "height:30px;font-size:.68rem;padding:0 10px;margin-left:0.5rem",
                            disabled: *restarting.read(),
                            onclick: on_restart,
                            if *restarting.read() {
                                "Restarting..."
                            } else {
                                "Restart"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
pub fn MediaStatsCard(app_state: AppState) -> Element {
    let mut counts: Signal<Option<ItemCounts>> = use_signal(|| None);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| Option::<String>::None);

    use_effect(move || {
        let client = app_state
            .client
            .clone();
        spawn(async move {
            match client
                .execute(GetItemCounts)
                .await
            {
                Ok(c) => {
                    counts.set(Some(c));
                    error.set(None);
                }
                Err(e) => error.set(Some(format!("Failed to fetch media counts: {e}"))),
            }
            loading.set(false);
        });
    });

    rsx! {
        Card { title: "Library",
            if *loading.read() {
                LoadingText {}
            } else if let Some(err) = error.read().as_ref() {
                span { class: "loading-text", style: "color:var(--error)", "{err}" }
            } else if let Some(c) = counts.read().as_ref() {
                div { class: "stat-grid",
                    StatTile { num: fmt_count(c.movie_count), label: "Movies" }
                    StatTile { num: fmt_count(c.series_count), label: "Series" }
                    StatTile { num: fmt_count(c.episode_count), label: "Episodes" }
                    StatTile { num: fmt_count(c.album_count), label: "Albums" }
                    StatTile { num: fmt_count(c.song_count), label: "Tracks" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_count;

    #[test]
    fn fmt_count_groups_thousands() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(42), "42");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1,000");
        assert_eq!(fmt_count(5310), "5,310");
        assert_eq!(fmt_count(715062), "715,062");
        assert_eq!(fmt_count(1234567), "1,234,567");
        assert_eq!(fmt_count(-1234), "-1,234");
    }
}
