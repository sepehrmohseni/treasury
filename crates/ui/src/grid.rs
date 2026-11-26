//! Results: one tab per result set, a grid that keeps only the rows near the view, the
//! server's messages, and export.
//!
//! Rows are fetched from the app in blocks as the view scrolls; the app reads them from the
//! server only as far as asked, so a million-row result costs what is on screen.

use crate::ipc;
use crate::state::ui;
use leptos::prelude::*;
use std::collections::HashMap;
use treasury_core::exec::ExecutionStatus;
use treasury_core::format::Cell;
use treasury_ipc::{ExportFormat, ExportView, WindowView};

const ROW: f64 = 24.0;
const BLOCK: u64 = 200;

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[component]
pub fn Results(result: RwSignal<Option<u64>>) -> impl IntoView {
    let state = ui();
    let status = RwSignal::new(None::<ExecutionStatus>);
    let set = RwSignal::new(0usize);
    let blocks = RwSignal::new(HashMap::<u64, Vec<Vec<Cell>>>::new());
    let loading = RwSignal::new(Vec::<u64>::new());
    let scroll = RwSignal::new(0.0f64);
    let height = RwSignal::new(400.0f64);
    let viewport = NodeRef::<leptos::html::Div>::new();

    let refresh = move |rid: u64| {
        leptos::task::spawn_local(async move {
            if let Ok(s) =
                ipc::call::<_, ExecutionStatus>("status", &serde_json::json!({ "result": rid }))
                    .await
            {
                status.set(Some(s));
            }
        });
    };

    let fetch = move |rid: u64, block: u64| {
        if blocks.with_untracked(|b| b.contains_key(&block))
            || loading.with_untracked(|l| l.contains(&block))
        {
            return;
        }
        loading.update(|l| l.push(block));
        let s = set.get_untracked();
        leptos::task::spawn_local(async move {
            let w = ipc::call::<_, WindowView>(
                "window",
                &serde_json::json!({ "result": rid, "set": s, "start": block * BLOCK, "count": BLOCK }),
            )
            .await;
            loading.update(|l| l.retain(|b| *b != block));
            match w {
                Ok(w) => blocks.update(|b| {
                    b.insert(block, w.rows);
                }),
                Err(e) => state.notice.set(Some(e.to_string())),
            }
            refresh(rid);
        });
    };

    // A new result starts from the top.
    Effect::new(move |_| {
        let rid = result.get();
        blocks.set(HashMap::new());
        set.set(0);
        scroll.set(0.0);
        status.set(None);
        if let Some(rid) = rid {
            fetch(rid, 0);
        }
    });

    // Fetch the blocks the view needs.
    Effect::new(move |_| {
        let (Some(rid), Some(st)) = (result.get(), status.get()) else {
            return;
        };
        let Some(s) = st.sets.get(set.get()) else {
            return;
        };
        let first = (scroll.get() / ROW) as u64;
        let last = first + (height.get() / ROW) as u64 + 40;
        let known = s.rows;
        for block in (first / BLOCK)..=(last / BLOCK) {
            if block * BLOCK < known || (!s.complete && block * BLOCK <= known) {
                fetch(rid, block);
            }
        }
    });

    let on_scroll = move |_| {
        if let Some(el) = viewport.get_untracked() {
            scroll.set(f64::from(el.scroll_top()));
            height.set(f64::from(el.client_height()));
        }
    };

    let export = move |format: ExportFormat| {
        let Some(rid) = result.get_untracked() else {
            return;
        };
        let s = set.get_untracked();
        leptos::task::spawn_local(async move {
            let name = format!("result.{}", format.extension());
            let Some(path) = ipc::save_file("Export result", &name).await else {
                return;
            };
            state.notice.set(Some("Exporting…".into()));
            match ipc::call::<_, ExportView>(
                "export_result",
                &serde_json::json!({ "result": rid, "set": s, "path": path, "format": format }),
            )
            .await
            {
                Ok(r) => state.notice.set(Some(format!(
                    "Exported {} rows to {}",
                    thousands(r.rows),
                    r.path.display()
                ))),
                Err(e) => state.notice.set(Some(e.to_string())),
            }
        });
    };

    view! {
        <section class="results" aria-label="Results">
            <div class="result-tabs" role="tablist">
                {move || status.get().map(|st| st.sets.iter().enumerate().map(|(i, s)| {
                    let label = if s.columns.is_empty() {
                        match s.affected { Some(n) => format!("{} rows changed", thousands(n)), None => "Done".into() }
                    } else {
                        format!("Result {}", i + 1)
                    };
                    view! {
                        <button role="tab" class:on=move || set.get() == i
                            on:click=move |_| { set.set(i); blocks.set(HashMap::new()); }>{label}</button>
                    }
                }).collect_view())}
            </div>
            <div class="grid" node_ref=viewport on:scroll=on_scroll role="grid">
                {move || {
                    let st = status.get();
                    let error = st.as_ref().and_then(|s| s.error.clone());
                    let Some(s) = st.as_ref().and_then(|st| st.sets.get(set.get()).cloned()) else {
                        return match (result.get(), error) {
                            (_, Some(e)) => view! { <div class="message error">{e.to_string()}</div> }.into_any(),
                            (Some(_), None) => view! { <div class="message">"Running…"</div> }.into_any(),
                            (None, None) => view! { <div class="message">"Run a statement to see its result here."</div> }.into_any(),
                        };
                    };
                    if s.columns.is_empty() {
                        let text = match s.affected {
                            Some(1) => "1 row changed.".to_owned(),
                            Some(n) => format!("{} rows changed.", thousands(n)),
                            None => "Done.".to_owned(),
                        };
                        return view! { <div class="message">{text}</div> }.into_any();
                    }
                    let numeric: Vec<bool> = s.columns.iter().map(|c| c.kind.is_numeric()).collect();
                    let total = s.rows;
                    let first = ((scroll.get() / ROW) as u64).min(total);
                    let count = (height.get() / ROW) as u64 + 40;
                    let last = (first + count).min(total);
                    let rows = blocks.with(|b| {
                        (first..last).map(|i| {
                            let cells = b.get(&(i / BLOCK)).and_then(|blk| blk.get((i % BLOCK) as usize)).cloned();
                            (i, cells)
                        }).collect::<Vec<_>>()
                    });
                    view! {
                        <table>
                            <thead><tr>
                                <th class="rownum">"#"</th>
                                {s.columns.iter().map(|c| view! {
                                    <th title=c.type_name.clone()>{c.name.clone()}<span class="type">{c.type_name.clone()}</span></th>
                                }).collect_view()}
                            </tr></thead>
                            <tbody>
                                <tr class="spacer-row"><td style:height=format!("{}px", first as f64 * ROW)></td></tr>
                                {rows.into_iter().map(|(i, cells)| view! {
                                    <tr>
                                        <td class="rownum">{(i + 1).to_string()}</td>
                                        {match cells {
                                            Some(cells) => cells.into_iter().enumerate().map(|(c, cell)| {
                                                let num = numeric.get(c).copied().unwrap_or(false);
                                                if cell.null {
                                                    view! { <td class="null">"NULL"</td> }.into_any()
                                                } else {
                                                    let title = cell.text.clone();
                                                    view! { <td class:num=num title=title>{cell.text}</td> }.into_any()
                                                }
                                            }).collect_view().into_any(),
                                            None => view! { <td class="null">"…"</td> }.into_any(),
                                        }}
                                    </tr>
                                }).collect_view()}
                                <tr class="spacer-row"><td style:height=format!("{}px", (total - last) as f64 * ROW)></td></tr>
                            </tbody>
                        </table>
                    }.into_any()
                }}
                {move || status.get().filter(|s| !s.notices.is_empty()).map(|s| view! {
                    <div class="notices">{s.notices.iter().map(|n| view! { <div>{n.message.clone()}</div> }).collect_view()}</div>
                })}
            </div>
            <div class="result-foot">
                {move || status.get().and_then(|st| st.sets.get(set.get()).cloned()).filter(|s| !s.columns.is_empty()).map(|s| {
                    let shown = thousands(s.rows);
                    let text = if s.limited {
                        format!("First {shown} rows (limited)")
                    } else if s.complete {
                        format!("{shown} rows")
                    } else {
                        format!("{shown}+ rows, more as you scroll")
                    };
                    view! { <span>{text}</span> }
                })}
                <span class="spacer" style:flex="1"></span>
                {move || result.get().map(|_| view! {
                    <span>"Export"</span>
                    {ExportFormat::ALL.iter().map(|f| {
                        let f = *f;
                        view! { <button class="btn small" on:click=move |_| export(f)>{f.label()}</button> }
                    }).collect_view()}
                })}
            </div>
        </section>
    }
}
