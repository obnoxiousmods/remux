use dioxus::prelude::*;
use std::rc::Rc;

/// One option in a [`Select`].
#[derive(Clone, PartialEq)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

impl SelectOption {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

/// Inline style for the fixed-position menu, given the trigger's viewport rect
/// (`left`, `top`, `bottom`, `width` in px) and the viewport height (0 when
/// unknown). Anchors the menu below the trigger, flipping upward when there is
/// more room above it, and clamps the menu's max-height to the available space
/// so it never runs past the viewport edge.
fn menu_position_style(
    left: f64,
    top: f64,
    bottom: f64,
    width: f64,
    viewport_h: f64,
) -> String {
    const GAP: f64 = 6.0;
    const EDGE: f64 = 8.0;
    const MAX_H: f64 = 280.0;
    let below = (viewport_h - bottom - GAP - EDGE).max(0.0);
    let above = (top - GAP - EDGE).max(0.0);
    if viewport_h > 0.0 && above > below {
        let bottom_edge = viewport_h - top + GAP;
        format!(
            "position:fixed;top:auto;right:auto;left:{left:.0}px;width:{width:.0}px;bottom:{bottom_edge:.0}px;max-height:{:.0}px",
            above.min(MAX_H)
        )
    } else {
        let max_h = if viewport_h > 0.0 {
            below.min(MAX_H)
        } else {
            MAX_H
        };
        let top_edge = bottom + GAP;
        format!(
            "position:fixed;bottom:auto;right:auto;left:{left:.0}px;width:{width:.0}px;top:{top_edge:.0}px;max-height:{max_h:.0}px"
        )
    }
}

/// A modern dropdown that replaces the native `<select>`.
///
/// The native option list can't be themed with CSS, so this renders a styled
/// trigger plus a themed popover menu (selected option accented, click-outside
/// to close, Escape to close). Emits the chosen option's `value` via `on_change`.
///
/// The menu is measured from the trigger on open and rendered with
/// `position: fixed` in viewport coordinates: plain absolute positioning gets
/// clipped by `overflow: hidden` cards and the scrolling `.modal-body`, which
/// hid the menu whenever the control sat near the bottom of its container.
#[component]
pub fn Select(
    /// Currently-selected value.
    value: String,
    /// Options in display order.
    options: Vec<SelectOption>,
    /// Fired with the newly-selected value.
    on_change: EventHandler<String>,
    /// Shown when `value` matches no option.
    #[props(default)]
    placeholder: Option<String>,
    /// Extra class(es) for the wrapper (e.g. sizing).
    #[props(default)]
    class: Option<String>,
    /// When true the control is greyed out and can't be opened.
    #[props(default)]
    disabled: bool,
) -> Element {
    let mut open = use_signal(|| false);
    // Menu geometry measured from the trigger each time the menu opens. `None`
    // (or a failed measurement) falls back to the stylesheet's absolute position.
    let mut menu_style: Signal<Option<String>> = use_signal(|| None);
    let mut trigger_el: Signal<Option<Rc<MountedData>>> = use_signal(|| None);

    let current_label = options
        .iter()
        .find(|o| o.value == value)
        .map(|o| {
            o.label
                .clone()
        })
        .or_else(|| placeholder.clone())
        .unwrap_or_default();
    let has_value = options
        .iter()
        .any(|o| o.value == value);

    let wrapper_class = match &class {
        Some(c) => format!("cselect {c}"),
        None => "cselect".to_string(),
    };

    rsx! {
        div {
            class: "{wrapper_class}",
            // Escape closes the menu; handled on the wrapper so it works from
            // both the trigger and the option buttons. Stopped from bubbling so
            // it doesn't also dismiss an enclosing Modal.
            onkeydown: move |e| {
                if e.key() == Key::Escape && *open.read() {
                    e.stop_propagation();
                    open.set(false);
                }
            },
            button {
                r#type: "button",
                disabled,
                class: if *open.read() { "cselect-trigger cselect-trigger--open" } else { "cselect-trigger" },
                onmounted: move |e| trigger_el.set(Some(e.data())),
                onclick: move |_| {
                    if disabled { return; }
                    if *open.read() {
                        open.set(false);
                        return;
                    }
                    // Measure the trigger's viewport rect, then open the menu
                    // pinned to it (see the component doc comment).
                    let el = trigger_el.read().clone();
                    spawn(async move {
                        if let Some(el) = el {
                            if let Ok(rect) = el.get_client_rect().await {
                                let viewport_h = web_sys::window()
                                    .and_then(|w| w.inner_height().ok())
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                menu_style.set(Some(menu_position_style(
                                    rect.min_x(),
                                    rect.min_y(),
                                    rect.max_y(),
                                    rect.width(),
                                    viewport_h,
                                )));
                            }
                        }
                        open.set(true);
                    });
                },
                span {
                    class: if has_value { "cselect-value" } else { "cselect-value cselect-placeholder" },
                    "{current_label}"
                }
                svg {
                    class: "cselect-chevron",
                    width: "16",
                    height: "16",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    polyline { points: "6 9 12 15 18 9" }
                }
            }
            if *open.read() {
                div {
                    class: "cselect-backdrop",
                    // `prevent_default` is required, not cosmetic: this control is often
                    // wrapped in a `<label>`, and the backdrop is then a descendant of it.
                    // A click would run the label's activation behaviour, forwarding a
                    // synthetic click to the labeled control (the trigger button), which
                    // would immediately reopen the menu we just closed. Cancelling the
                    // event's default action suppresses that forwarding.
                    onclick: move |e| {
                        e.prevent_default();
                        open.set(false);
                    },
                }
                div {
                    class: "cselect-menu",
                    style: if let Some(s) = menu_style.read().as_ref() { "{s}" } else { "" },
                    for opt in options.iter().cloned() {
                        {
                            let selected = opt.value == value;
                            let v = opt.value.clone();
                            rsx! {
                                button {
                                    r#type: "button",
                                    key: "{opt.value}",
                                    class: if selected { "cselect-option cselect-option--selected" } else { "cselect-option" },
                                    onclick: move |_| {
                                        on_change.call(v.clone());
                                        open.set(false);
                                    },
                                    span { "{opt.label}" }
                                    if selected {
                                        svg {
                                            class: "cselect-check",
                                            width: "15",
                                            height: "15",
                                            view_box: "0 0 24 24",
                                            fill: "none",
                                            stroke: "currentColor",
                                            stroke_width: "2.5",
                                            stroke_linecap: "round",
                                            stroke_linejoin: "round",
                                            polyline { points: "20 6 9 17 4 12" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn matching_options(
    options: &[SelectOption],
    query: &str,
    limit: usize,
) -> Vec<SelectOption> {
    let query = query
        .trim()
        .to_lowercase();
    let mut matches: Vec<SelectOption> = options
        .iter()
        .filter(|option| {
            query.is_empty()
                || option
                    .label
                    .to_lowercase()
                    .contains(&query)
                || option
                    .value
                    .to_lowercase()
                    .contains(&query)
        })
        .cloned()
        .collect();
    matches.sort_by_key(|option| {
        let label = option
            .label
            .to_lowercase();
        let value = option
            .value
            .to_lowercase();
        if label == query || value == query {
            0
        } else if label.starts_with(&query) || value.starts_with(&query) {
            1
        } else {
            2
        }
    });
    matches.truncate(limit);
    matches
}

fn next_option_index(
    current: Option<usize>,
    len: usize,
    forward: bool,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (current, forward) {
        (None, true) => 0,
        (None, false) => len - 1,
        (Some(index), true) => (index + 1).min(len - 1),
        (Some(index), false) => index.saturating_sub(1),
    })
}

/// Searchable, keyboard-accessible combobox for server-provided option sets.
///
/// Typing updates the value immediately (so operators may still enter a value
/// not yet present in the sampled filter list), while the popover ranks exact
/// and prefix matches ahead of substring matches. Arrow keys move through the
/// visible results, Enter accepts the highlighted result, and Escape closes it.
#[component]
pub fn SearchSelect(
    value: String,
    options: Vec<SelectOption>,
    on_change: EventHandler<String>,
    #[props(default)] placeholder: Option<String>,
    #[props(default)] class: Option<String>,
) -> Element {
    let mut open = use_signal(|| false);
    let mut active_index = use_signal(|| None::<usize>);
    let mut menu_style: Signal<Option<String>> = use_signal(|| None);
    let mut control_el: Signal<Option<Rc<MountedData>>> = use_signal(|| None);
    let matches = matching_options(&options, &value, 100);
    let wrapper_class = class
        .map(|class| format!("cselect ccombobox {class}"))
        .unwrap_or_else(|| "cselect ccombobox".to_string());

    rsx! {
        div {
            class: "{wrapper_class}",
            div {
                class: if *open.read() { "ccombobox-control cselect-trigger--open" } else { "ccombobox-control" },
                onmounted: move |event| control_el.set(Some(event.data())),
                input {
                    class: "ccombobox-input",
                    role: "combobox",
                    aria_autocomplete: "list",
                    aria_expanded: if *open.read() { "true" } else { "false" },
                    autocomplete: "off",
                    placeholder: placeholder.unwrap_or_default(),
                    value: "{value}",
                    onfocus: move |_| {
                        let el = control_el.read().clone();
                        spawn(async move {
                            if let Some(el) = el {
                                if let Ok(rect) = el.get_client_rect().await {
                                    let viewport_h = web_sys::window()
                                        .and_then(|window| window.inner_height().ok())
                                        .and_then(|height| height.as_f64())
                                        .unwrap_or(0.0);
                                    menu_style.set(Some(menu_position_style(
                                        rect.min_x(),
                                        rect.min_y(),
                                        rect.max_y(),
                                        rect.width(),
                                        viewport_h,
                                    )));
                                }
                            }
                            active_index.set(None);
                            open.set(true);
                        });
                    },
                    oninput: move |event| {
                        active_index.set(None);
                        open.set(true);
                        on_change.call(event.value());
                    },
                    onkeydown: {
                        let keyboard_matches = matches.clone();
                        move |event: KeyboardEvent| match event.key() {
                            Key::ArrowDown => {
                                event.prevent_default();
                                let next = next_option_index(
                                    *active_index.read(),
                                    keyboard_matches.len(),
                                    true,
                                );
                                active_index.set(next);
                                open.set(true);
                            }
                            Key::ArrowUp => {
                                event.prevent_default();
                                let next = next_option_index(
                                    *active_index.read(),
                                    keyboard_matches.len(),
                                    false,
                                );
                                active_index.set(next);
                                open.set(true);
                            }
                            Key::Enter if *open.read() => {
                                event.prevent_default();
                                let index = (*active_index.read()).unwrap_or(0);
                                if let Some(option) = keyboard_matches.get(index) {
                                    on_change.call(option.value.clone());
                                    open.set(false);
                                }
                            }
                            Key::Escape => {
                                event.stop_propagation();
                                open.set(false);
                            }
                            _ => {}
                        }
                    }
                }
                if !value.is_empty() {
                    button {
                        r#type: "button",
                        class: "ccombobox-clear",
                        aria_label: "Clear selection",
                        onclick: move |_| {
                            active_index.set(None);
                            on_change.call(String::new());
                            open.set(true);
                        },
                        "×"
                    }
                }
                svg {
                    class: "cselect-chevron",
                    width: "16",
                    height: "16",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    polyline { points: "6 9 12 15 18 9" }
                }
            }
            if *open.read() {
                div {
                    class: "cselect-backdrop",
                    onmousedown: move |_| open.set(false),
                    // See the note on `Select`'s backdrop: without cancelling the click's
                    // default action, an enclosing `<label>` forwards it to the input,
                    // which refocuses it and reopens the menu via `onfocus`.
                    onclick: move |e| {
                        e.prevent_default();
                        open.set(false);
                    },
                }
                div {
                    class: "cselect-menu ccombobox-menu",
                    role: "listbox",
                    style: if let Some(style) = menu_style.read().as_ref() { "{style}" } else { "" },
                    if matches.is_empty() {
                        div { class: "ccombobox-empty", "No matching values" }
                    }
                    for (index, option) in matches.iter().cloned().enumerate() {
                        {
                            let option_value = option.value.clone();
                            let selected = option.value == value;
                            let active = Some(index) == *active_index.read();
                            rsx! {
                                button {
                                    r#type: "button",
                                    key: "{option.value}",
                                    role: "option",
                                    aria_selected: if selected { "true" } else { "false" },
                                    class: if selected { "cselect-option cselect-option--selected" } else if active { "cselect-option cselect-option--active" } else { "cselect-option" },
                                    onmouseenter: move |_| active_index.set(Some(index)),
                                    onmousedown: move |event| {
                                        event.prevent_default();
                                        on_change.call(option_value.clone());
                                        open.set(false);
                                    },
                                    span { "{option.label}" }
                                    if selected { span { class: "cselect-check", "✓" } }
                                }
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

    /// Mid-viewport trigger: menu opens below with the full max height.
    #[test]
    fn opens_below_with_full_height() {
        let style = menu_position_style(100.0, 260.0, 300.0, 220.0, 900.0);
        assert_eq!(
            style,
            "position:fixed;bottom:auto;right:auto;left:100px;width:220px;top:306px;max-height:280px"
        );
    }

    /// Trigger near the viewport bottom: menu flips up and is anchored to the
    /// trigger's top edge.
    #[test]
    fn flips_up_when_more_room_above() {
        let style = menu_position_style(100.0, 800.0, 840.0, 220.0, 900.0);
        assert_eq!(
            style,
            "position:fixed;top:auto;right:auto;left:100px;width:220px;bottom:106px;max-height:280px"
        );
    }

    /// Little room in either direction: clamp max-height to the larger side.
    #[test]
    fn clamps_max_height_to_available_space() {
        let style = menu_position_style(100.0, 600.0, 640.0, 220.0, 700.0);
        // below = 700-640-14 = 46, above = 600-14 = 586 → opens upward, clamped to 280.
        assert!(style.contains("bottom:106px"));
        assert!(style.contains("max-height:280px"));
        // And when below wins but is tight:
        let style = menu_position_style(100.0, 20.0, 640.0, 220.0, 700.0);
        assert!(style.contains("top:646px"));
        assert!(style.contains("max-height:46px"));
    }

    /// Unknown viewport height: fall back to opening below at full height.
    #[test]
    fn unknown_viewport_opens_below() {
        let style = menu_position_style(100.0, 260.0, 300.0, 220.0, 0.0);
        assert!(style.contains("top:306px"));
        assert!(style.contains("max-height:280px"));
    }

    #[test]
    fn searchable_options_rank_exact_prefix_then_substring() {
        let options = vec![
            SelectOption::new("Finamp Beta", "Finamp Beta"),
            SelectOption::new("Beta Player", "Beta Player"),
            SelectOption::new("Beta", "Beta"),
            SelectOption::new("Discrete", "Discrete"),
        ];
        let values: Vec<String> = matching_options(&options, "beta", 10)
            .into_iter()
            .map(|option| option.value)
            .collect();
        assert_eq!(values, vec!["Beta", "Beta Player", "Finamp Beta"]);
    }

    #[test]
    fn searchable_options_are_case_insensitive_and_limited() {
        let options = vec![
            SelectOption::new("iPhone 15", "Joey's iPhone 15"),
            SelectOption::new("iPad", "Living Room iPad"),
            SelectOption::new("TV", "Television"),
        ];
        let values = matching_options(&options, "IP", 1);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].value, "iPhone 15");
    }

    #[test]
    fn keyboard_navigation_enters_and_clamps_the_option_list() {
        assert_eq!(next_option_index(None, 3, true), Some(0));
        assert_eq!(next_option_index(Some(0), 3, true), Some(1));
        assert_eq!(next_option_index(Some(2), 3, true), Some(2));
        assert_eq!(next_option_index(None, 3, false), Some(2));
        assert_eq!(next_option_index(Some(0), 3, false), Some(0));
        assert_eq!(next_option_index(None, 0, true), None);
    }
}
