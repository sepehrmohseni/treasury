//! What ran on the open connection, newest first and searchable. The app keeps the history
//! encrypted on this computer; production connections keep none unless their settings say so.

use crate::grid::thousands;
use crate::ipc;
use crate::state::{Dialog, ui};
use leptos::prelude::*;
use std::time::Duration;
use treasury_ipc::{HistoryView, RunOutcome};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Year, month and day of `days` since 1970-01-01 (Howard Hinnant's civil-from-days).
fn civil(days: i64) -> (i64, usize, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, usize::try_from(month).unwrap_or(1), day)
}

/// When a statement ran, as the list shows it: the time alone for today, otherwise the date too.
/// The offsets are minutes east of UTC at each moment, so a change of daylight saving time in
/// between is taken into account.
fn when(at_ms: i64, at_offset: i64, now_ms: i64, now_offset: i64) -> String {
    let at = at_ms.div_euclid(60_000) + at_offset;
    let now = now_ms.div_euclid(60_000) + now_offset;
    let minute = at.rem_euclid(1_440);
    let time = format!("{:02}:{:02}", minute / 60, minute % 60);
    let (day, today) = (at.div_euclid(1_440), now.div_euclid(1_440));
    if day == today {
        return time;
    }
    let (year, month, d) = civil(day);
    let month = MONTHS.get(month - 1).copied().unwrap_or("");
    if year == civil(today).0 {
        format!("{month} {d}, {time}")
    } else {
        format!("{month} {d} {year}, {time}")
    }
}

/// How long a statement ran.
fn duration(ms: i64) -> String {
    match ms {
        ..1_000 => format!("{} ms", ms.max(0)),
        1_000..60_000 => format!("{:.1} s", ms as f64 / 1_000.0),
        _ => format!("{} min {} s", ms / 60_000, ms % 60_000 / 1_000),
    }
}

/// The facts after the statement's first line: when, how long, how many rows, how it ended.
fn summary(e: &HistoryView, now_ms: i64, offset: impl Fn(i64) -> i64) -> String {
    let at = when(e.executed_at, offset(e.executed_at), now_ms, offset(now_ms));
    if e.outcome == RunOutcome::Unfinished {
        return format!("{at} · not finished");
    }
    let mut parts = vec![at, duration(e.duration_ms)];
    match e.outcome {
        RunOutcome::Ok => {
            if let Some(n) = e.rows.and_then(|n| u64::try_from(n).ok()) {
                parts.push(if n == 1 {
                    "1 row".to_owned()
                } else {
                    format!("{} rows", thousands(n))
                });
            }
        }
        RunOutcome::Failed => parts.push("failed".to_owned()),
        RunOutcome::Cancelled => parts.push("cancelled".to_owned()),
        RunOutcome::Unfinished => {}
    }
    parts.join(" · ")
}

/// Minutes east of UTC at `ms`, in the computer's time zone.
fn local_offset(ms: i64) -> i64 {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    -(date.get_timezone_offset() as i64)
}

#[component]
pub fn HistoryPanel() -> impl IntoView {
    let state = ui();
    let typed = RwSignal::new(String::new());
    let search = RwSignal::new(String::new());
    let entries = RwSignal::new(None::<Result<Vec<HistoryView>, String>>);
    let off_in_settings = RwSignal::new(false);
    // Each keystroke and each request is counted, so only the latest one counts.
    let keystrokes = StoredValue::new(0u64);
    let requests = StoredValue::new(0u64);

    leptos::task::spawn_local(async move {
        let v = ipc::call::<_, Option<serde_json::Value>>(
            "setting",
            &serde_json::json!({ "key": "history" }),
        )
        .await;
        off_in_settings.set(matches!(v, Ok(Some(serde_json::Value::Bool(false)))));
    });

    Effect::new(move |_| {
        let (Some(id), q) = (state.active.get(), search.get()) else {
            return;
        };
        state.history_rev.track();
        requests.update_value(|n| *n += 1);
        let mine = requests.get_value();
        leptos::task::spawn_local(async move {
            let reply = ipc::call::<_, Vec<HistoryView>>(
                "history",
                &serde_json::json!({ "id": id, "search": q }),
            )
            .await;
            if requests.get_value() == mine {
                entries.set(Some(reply.map_err(|e| e.to_string())));
            }
        });
    });

    let on_input = move |ev| {
        let value = event_target_value(&ev);
        typed.set(value.clone());
        keystrokes.update_value(|n| *n += 1);
        let mine = keystrokes.get_value();
        set_timeout(
            move || {
                if keystrokes.get_value() == mine {
                    search.set(value);
                }
            },
            Duration::from_millis(200),
        );
    };

    let keeps_none = move || state.active_config().is_some_and(|c| !c.records_history());
    let empty = move || {
        entries.with(|e| {
            e.as_ref()
                .is_none_or(|r| r.as_ref().is_ok_and(Vec::is_empty))
        })
    };

    view! {
        <div class="list history">
            {move || state.active.get().is_none().then(|| view! {
                <p class="empty">"Open a connection to see what ran on it."</p>
            })}
            <Show when=move || state.active.get().is_some()>
                <div class="history-search">
                    <input id="history-search" type="search" placeholder="Search history" aria-label="Search history"
                        autocomplete="off" spellcheck="false" prop:value=move || typed.get() on:input=on_input />
                </div>
                {move || if off_in_settings.get() {
                    Some(view! { <p class="empty">"History is off in Settings, so statements are not recorded."</p> })
                } else if keeps_none() {
                    Some(view! { <p class="empty">"This connection keeps no history. Turn it on in the connection's settings."</p> })
                } else {
                    None
                }}
                {move || match entries.get() {
                    None => view! { <p class="empty">"Reading the history…"</p> }.into_any(),
                    Some(Err(e)) => view! { <p class="empty error">{e}</p> }.into_any(),
                    Some(Ok(list)) if list.is_empty() => view! {
                        <p class="empty">{if search.get().trim().is_empty() {
                            "No statements have run on this connection yet."
                        } else {
                            "Nothing in the history matches."
                        }}</p>
                    }.into_any(),
                    Some(Ok(list)) => {
                        let now = js_sys::Date::now() as i64;
                        list.into_iter().map(|e| {
                            let first = e.text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_owned();
                            let meta = summary(&e, now, local_offset);
                            let failed = matches!(e.outcome, RunOutcome::Failed | RunOutcome::Cancelled);
                            let text = e.text.clone();
                            view! {
                                <button class="entry" title=e.text.clone() on:click=move |_| crate::tabs::open_text(text.clone())>
                                    <span class="entry-text">{first}</span>
                                    <span class="entry-meta" class:failed=failed>{meta}</span>
                                </button>
                            }
                        }).collect_view().into_any()
                    }
                }}
                <div class="history-foot">
                    <button class="btn small clear-history" disabled=empty
                        on:click=move |_| if let Some(id) = state.active.get_untracked() { state.dialog.set(Dialog::ClearHistory(id)) }>
                        "Clear history"
                    </button>
                </div>
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;
    // 2026-11-26 14:05 UTC.
    const AT: i64 = 1_795_701_900_000;

    #[test]
    fn today_shows_the_time_other_days_the_date() {
        assert_eq!(when(AT, 0, AT + 60_000, 0), "14:05");
        assert_eq!(when(AT, 0, AT + DAY, 0), "Nov 26, 14:05");
        assert_eq!(when(AT, 0, AT + 400 * DAY, 0), "Nov 26 2026, 14:05");
        // Six hours west of UTC, 14:05 UTC is 08:05 the same day.
        assert_eq!(when(AT, -360, AT + 60_000, -360), "08:05");
        // Nine hours east, it is already 23:05.
        assert_eq!(when(AT, 540, AT + 60_000, 540), "23:05");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(12), "12 ms");
        assert_eq!(duration(1_450), "1.4 s");
        assert_eq!(duration(125_000), "2 min 5 s");
    }

    #[test]
    fn the_summary_says_rows_or_how_it_ended() {
        let mut e = HistoryView {
            id: 1,
            text: "select 1".into(),
            executed_at: AT,
            duration_ms: 3,
            rows: Some(1_200),
            outcome: RunOutcome::Ok,
        };
        assert_eq!(summary(&e, AT, |_| 0), "14:05 · 3 ms · 1,200 rows");
        e.rows = Some(1);
        assert_eq!(summary(&e, AT, |_| 0), "14:05 · 3 ms · 1 row");
        e.outcome = RunOutcome::Failed;
        assert_eq!(summary(&e, AT, |_| 0), "14:05 · 3 ms · failed");
        e.outcome = RunOutcome::Unfinished;
        assert_eq!(summary(&e, AT, |_| 0), "14:05 · not finished");
    }
}
