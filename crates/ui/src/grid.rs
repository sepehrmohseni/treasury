//! Results: one tab per result set, a grid that keeps only the rows near the view, the
//! server's messages, export, and editing rows in place.
//!
//! Rows are fetched from the app in blocks as the view scrolls; the app reads them from the
//! server only as far as asked, so a million-row result costs what is on screen.
//!
//! When the app finds the table and key behind a result, its plain columns can be changed.
//! Edits collect here, marked in the grid, and are saved together once the user has read the
//! statements they become.

use crate::ipc;
use crate::state::{Dialog, ui};
use leptos::prelude::*;
use std::collections::HashMap;
use treasury_core::exec::ExecutionStatus;
use treasury_core::format::Cell;
use treasury_ipc::{CellEdit, EditInfo, ExportFormat, ExportView, RowEdits, WindowView};
use wasm_bindgen::JsCast;

const ROW: f64 = 24.0;
const BLOCK: u64 = 200;

pub fn thousands(n: u64) -> String {
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

/// The pending value of a cell, if it was edited: `Some(None)` is NULL.
fn pending_value(edits: &RowEdits, row: u64, column: usize) -> Option<Option<String>> {
    edits
        .cells
        .iter()
        .rev()
        .find(|c| c.row == row && c.column == column)
        .map(|c| c.value.clone())
}

/// How many cells and rows the pending edits change.
fn change_count(edits: &RowEdits) -> usize {
    let mut cells: Vec<(u64, usize)> = edits
        .cells
        .iter()
        .filter(|c| !edits.deletes.contains(&c.row))
        .map(|c| (c.row, c.column))
        .collect();
    cells.sort_unstable();
    cells.dedup();
    let mut rows = edits.deletes.clone();
    rows.sort_unstable();
    rows.dedup();
    cells.len() + rows.len()
}

/// Focuses and selects the cell editor once it is in the page.
fn focus_cell_editor() {
    request_animation_frame(|| {
        let input = document()
            .query_selector(".cell-input")
            .ok()
            .flatten()
            .and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok());
        if let Some(input) = input {
            let _ = input.focus();
            input.select();
        }
    });
}

#[component]
pub fn Results(
    result: RwSignal<Option<u64>>,
    /// Called after edits were saved, to read the rows again.
    #[prop(into)]
    on_saved: Callback<()>,
) -> impl IntoView {
    let state = ui();
    let status = RwSignal::new(None::<ExecutionStatus>);
    let set = RwSignal::new(0usize);
    let blocks = RwSignal::new(HashMap::<u64, Vec<Vec<Cell>>>::new());
    let loading = RwSignal::new(Vec::<u64>::new());
    let scroll = RwSignal::new(0.0f64);
    let height = RwSignal::new(400.0f64);
    let viewport = NodeRef::<leptos::html::Div>::new();
    // Editing.
    let info = RwSignal::new(None::<EditInfo>);
    let asked = RwSignal::new(None::<(u64, usize)>);
    let pending = RwSignal::new(RowEdits::default());
    let selected = RwSignal::new(None::<(u64, usize)>);
    let editing = RwSignal::new(None::<(u64, usize)>);
    let draft = RwSignal::new(String::new());

    let reset_edits = move || {
        info.set(None);
        asked.set(None);
        pending.set(RowEdits::default());
        selected.set(None);
        editing.set(None);
    };

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

    // A new result starts from the top, with nothing pending.
    Effect::new(move |_| {
        let rid = result.get();
        blocks.set(HashMap::new());
        set.set(0);
        scroll.set(0.0);
        status.set(None);
        reset_edits();
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

    // Once a result set's columns are known, ask whether its rows can be edited.
    Effect::new(move |_| {
        let (Some(rid), Some(st)) = (result.get(), status.get()) else {
            return;
        };
        let i = set.get();
        if asked.get_untracked() == Some((rid, i))
            || st.sets.get(i).is_none_or(|s| s.columns.is_empty())
        {
            return;
        }
        asked.set(Some((rid, i)));
        leptos::task::spawn_local(async move {
            let reply = ipc::call::<_, EditInfo>(
                "edit_info",
                &serde_json::json!({ "result": rid, "set": i }),
            )
            .await;
            // The answer is for a result that may have been replaced meanwhile.
            if asked.get_untracked() == Some((rid, i)) {
                info.set(Some(reply.unwrap_or_else(|e| EditInfo {
                    reason: Some(e.to_string()),
                    ..EditInfo::default()
                })));
            }
        });
    });

    // The review dialog saved the edits: read the rows again.
    Effect::new(move |_| {
        let saved = state.saved.get();
        if saved.is_some() && saved == result.get_untracked() {
            state.saved.set(None);
            pending.set(RowEdits::default());
            on_saved.run(());
        }
    });

    let on_scroll = move |_| {
        if let Some(el) = viewport.get_untracked() {
            scroll.set(f64::from(el.scroll_top()));
            height.set(f64::from(el.client_height()));
        }
    };

    let editable = move |column: usize| {
        info.with(|i| {
            i.as_ref()
                .is_some_and(|i| i.columns.get(column).copied().unwrap_or(false))
        })
    };

    let start_edit = move |row: u64, column: usize, text: String| {
        draft.set(text);
        selected.set(Some((row, column)));
        editing.set(Some((row, column)));
        focus_cell_editor();
    };

    // Ends editing `row`, `column`, keeping the draft unless it is what the cell already shows.
    let commit = move |row: u64, column: usize, shown: Option<String>| {
        if editing.get_untracked() != Some((row, column)) {
            return;
        }
        editing.set(None);
        let value = draft.get_untracked();
        if shown.as_deref() == Some(value.as_str()) {
            return;
        }
        pending.update(|p| {
            p.cells.push(CellEdit {
                row,
                column,
                value: Some(value),
            });
        });
    };

    let set_null = move |_| {
        if let Some((row, column)) = selected.get_untracked().filter(|(_, c)| editable(*c)) {
            pending.update(|p| {
                p.cells.push(CellEdit {
                    row,
                    column,
                    value: None,
                });
            });
        }
    };

    let toggle_delete = move |_| {
        if let Some((row, _)) = selected.get_untracked() {
            pending.update(|p| {
                if p.deletes.contains(&row) {
                    p.deletes.retain(|r| *r != row);
                } else {
                    p.deletes.push(row);
                }
            });
        }
    };

    let review = move |_| {
        let Some(rid) = result.get_untracked() else {
            return;
        };
        let s = set.get_untracked();
        let edits = pending.get_untracked();
        leptos::task::spawn_local(async move {
            match ipc::call::<_, Vec<String>>(
                "edit_review",
                &serde_json::json!({ "result": rid, "set": s, "edits": edits }),
            )
            .await
            {
                Ok(statements) => state.dialog.set(Dialog::ReviewEdits {
                    result: rid,
                    set: s,
                    statements,
                    edits,
                }),
                Err(e) => state.notice.set(Some(e.to_string())),
            }
        });
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
                        match s.affected { Some(1) => "1 row changed".into(), Some(n) => format!("{} rows changed", thousands(n)), None => "Done".into() }
                    } else {
                        format!("Result {}", i + 1)
                    };
                    view! {
                        <button role="tab" class:on=move || set.get() == i
                            on:click=move |_| { set.set(i); blocks.set(HashMap::new()); reset_edits(); }>{label}</button>
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
                    let can_edit: Vec<bool> = (0..s.columns.len()).map(editable).collect();
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
                    let edits = pending.get();
                    let sel = selected.get();
                    let open = editing.get();
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
                                {rows.into_iter().map(|(i, cells)| {
                                    let deleted = edits.deletes.contains(&i);
                                    view! {
                                        <tr class:deleted=deleted>
                                            <td class="rownum" on:click=move |_| selected.set(Some((i, 0)))>{(i + 1).to_string()}</td>
                                            {match cells {
                                                Some(cells) => cells.into_iter().enumerate().map(|(c, cell)| {
                                                    let num = numeric.get(c).copied().unwrap_or(false);
                                                    let changed = pending_value(&edits, i, c);
                                                    let (text, null) = match &changed {
                                                        Some(Some(t)) => (t.clone(), false),
                                                        Some(None) => (String::new(), true),
                                                        None => (cell.text.clone(), cell.null),
                                                    };
                                                    let shown = (!null).then(|| text.clone());
                                                    let is_sel = sel == Some((i, c));
                                                    let may_edit = can_edit.get(c).copied().unwrap_or(false) && !deleted;
                                                    if open == Some((i, c)) {
                                                        let (on_key, on_blur) = (shown.clone(), shown);
                                                        return view! {
                                                            <td class="editing">
                                                                <input class="cell-input" aria-label="New value" prop:value=draft.get_untracked()
                                                                    on:input=move |ev| draft.set(event_target_value(&ev))
                                                                    on:keydown=move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
                                                                        "Enter" => { ev.prevent_default(); commit(i, c, on_key.clone()); }
                                                                        "Escape" => editing.set(None),
                                                                        _ => {}
                                                                    }
                                                                    on:blur=move |_| commit(i, c, on_blur.clone()) />
                                                            </td>
                                                        }.into_any();
                                                    }
                                                    let start_text = text.clone();
                                                    let label = if null { "NULL".to_owned() } else { text.clone() };
                                                    view! {
                                                        <td class:num=num class:null=null class:changed=changed.is_some() class:sel=is_sel
                                                            title=if null { String::new() } else { text }
                                                            on:click=move |_| selected.set(Some((i, c)))
                                                            on:dblclick=move |_| if may_edit { start_edit(i, c, start_text.clone()) }>
                                                            {label}
                                                        </td>
                                                    }.into_any()
                                                }).collect_view().into_any(),
                                                None => view! { <td class="null">"…"</td> }.into_any(),
                                            }}
                                        </tr>
                                    }
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
                    } else if s.complete && s.rows == 1 {
                        "1 row".to_owned()
                    } else if s.complete {
                        format!("{shown} rows")
                    } else {
                        format!("{shown}+ rows, more as you scroll")
                    };
                    view! { <span>{text}</span> }
                })}
                {move || info.get().map(|i| match (i.table, i.reason) {
                    (Some(t), _) => view! {
                        <span class="detail edit-state">{format!("Rows of {t} can be edited: double-click a cell")}</span>
                    }.into_any(),
                    (None, Some(why)) => {
                        let title = why.clone();
                        view! { <span class="detail edit-state" title=title>{why}</span> }.into_any()
                    }
                    (None, None) => ().into_any(),
                })}
                <span class="spacer" style:flex="1"></span>
                {move || {
                    let row_selected = selected.get().is_some() && info.with(|i| i.as_ref().is_some_and(|i| i.table.is_some()));
                    row_selected.then(|| {
                        let deleting = selected.get().is_some_and(|(r, _)| pending.with(|p| p.deletes.contains(&r)));
                        view! {
                            <button class="btn small" on:click=set_null disabled=move || !selected.get().is_some_and(|(_, c)| editable(c))>"Set NULL"</button>
                            <button class="btn small delete-row" on:click=toggle_delete>{if deleting { "Keep row" } else { "Delete row" }}</button>
                        }
                    })
                }}
                {move || {
                    let n = pending.with(change_count);
                    (n > 0).then(|| view! {
                        <span class="pending">{if n == 1 { "1 change".to_owned() } else { format!("{n} changes") }}</span>
                        <button class="btn small" on:click=move |_| { pending.set(RowEdits::default()); editing.set(None); }>"Discard"</button>
                        <button class="btn small primary" on:click=review>"Review changes"</button>
                    })
                }}
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(row: u64, column: usize, value: Option<&str>) -> CellEdit {
        CellEdit {
            row,
            column,
            value: value.map(str::to_owned),
        }
    }

    #[test]
    fn the_latest_edit_of_a_cell_is_what_it_shows() {
        let e = RowEdits {
            cells: vec![
                cell(1, 0, Some("a")),
                cell(1, 0, None),
                cell(2, 1, Some("b")),
            ],
            deletes: vec![],
        };
        assert_eq!(pending_value(&e, 1, 0), Some(None));
        assert_eq!(pending_value(&e, 2, 1), Some(Some("b".into())));
        assert_eq!(pending_value(&e, 0, 0), None);
    }

    #[test]
    fn changes_count_cells_and_deleted_rows_once() {
        let e = RowEdits {
            cells: vec![
                cell(1, 0, Some("a")),
                cell(1, 0, Some("b")),
                cell(3, 0, Some("c")),
            ],
            deletes: vec![3, 4, 4],
        };
        // Row 1's cell, and rows 3 and 4 (row 3's cell goes with it).
        assert_eq!(change_count(&e), 3);
    }
}
