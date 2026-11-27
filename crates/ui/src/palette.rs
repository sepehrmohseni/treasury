//! The command palette (Ctrl+K): connections, saved queries, the tables, views and collections
//! of the open connection, and the app's actions, found by typing part of their name.

use crate::state::{Dialog, EditorCommand, SideView, ui};
use leptos::prelude::*;
use treasury_core::config::ConnectionId;
use treasury_ipc::{CatalogEntry, SavedQueryView};

/// The most matches listed.
const SHOWN: usize = 40;

/// How well `query` matches `text`, or `None` when it does not: every character of the query
/// must appear in the text in order. Matches at the start of words and runs of consecutive
/// characters count most; gaps and long texts count against.
pub fn score(query: &str, text: &str) -> Option<i64> {
    let wanted: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if wanted.is_empty() {
        return Some(0);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut score = 0i64;
    let mut next = 0;
    let mut last: Option<usize> = None;
    for (i, c) in chars.iter().enumerate() {
        let Some(&w) = wanted.get(next) else {
            break;
        };
        if !c.to_lowercase().eq(std::iter::once(w)) {
            continue;
        }
        let previous = i.checked_sub(1).and_then(|p| chars.get(p));
        let word_start =
            previous.is_none_or(|p| !p.is_alphanumeric() || (p.is_lowercase() && c.is_uppercase()));
        score += 10;
        if word_start {
            score += 15;
        }
        match last {
            Some(l) if l + 1 == i => score += 20,
            Some(l) => score -= i64::try_from(i - l - 1).unwrap_or(i64::MAX).min(10),
            None => score -= i64::try_from(i).unwrap_or(i64::MAX).min(10),
        }
        last = Some(i);
        next += 1;
    }
    (next == wanted.len()).then(|| score - i64::try_from(chars.len() / 4).unwrap_or(0))
}

/// Something the palette can go to or do.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Connection(ConnectionId),
    Query(SavedQueryView),
    Object(CatalogEntry),
    Action(Action),
}

/// The app's actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    NewTab,
    RunAll,
    Format,
    SaveQuery,
    Settings,
    AddConnection,
    Import,
    ShowConnections,
    ShowQueries,
    ShowHistory,
    Theme(&'static str),
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Action::NewTab => "New tab",
            Action::RunAll => "Run all statements",
            Action::Format => "Format the statement",
            Action::SaveQuery => "Save the tab as a query",
            Action::Settings => "Settings",
            Action::AddConnection => "Add a connection",
            Action::Import => "Import connections",
            Action::ShowConnections => "Show connections",
            Action::ShowQueries => "Show saved queries",
            Action::ShowHistory => "Show history",
            Action::Theme("dark") => "Theme: dark",
            Action::Theme("light") => "Theme: light",
            Action::Theme(_) => "Theme: as the system",
        }
    }

    /// Whether it needs an open connection.
    fn needs_connection(self) -> bool {
        matches!(
            self,
            Action::NewTab | Action::RunAll | Action::Format | Action::SaveQuery
        )
    }
}

const ACTIONS: &[Action] = &[
    Action::NewTab,
    Action::RunAll,
    Action::Format,
    Action::SaveQuery,
    Action::Settings,
    Action::AddConnection,
    Action::Import,
    Action::ShowConnections,
    Action::ShowQueries,
    Action::ShowHistory,
    Action::Theme("dark"),
    Action::Theme("light"),
    Action::Theme("system"),
];

/// One line of the palette.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub label: String,
    pub detail: String,
    pub target: Target,
}

/// The items that match `query`, best first. Without a query: connections, saved queries and
/// actions, as listed (tables only show up once something is typed, there can be many).
pub fn search(query: &str, items: &[Item]) -> Vec<Item> {
    if query.trim().is_empty() {
        return items
            .iter()
            .filter(|i| !matches!(i.target, Target::Object(_)))
            .take(SHOWN)
            .cloned()
            .collect();
    }
    let mut found: Vec<(i64, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(n, i)| {
            let on_label = score(query, &i.label);
            // The detail (a schema, a folder) may match too, for less.
            let on_both = score(query, &format!("{} {}", i.detail, i.label)).map(|s| s - 15);
            on_label.max(on_both).map(|s| (s, n))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    found
        .into_iter()
        .take(SHOWN)
        .filter_map(|(_, n)| items.get(n).cloned())
        .collect()
}

fn kind_label(kind: treasury_core::schema::ObjectKind) -> &'static str {
    use treasury_core::schema::ObjectKind;
    match kind {
        ObjectKind::View => "View",
        ObjectKind::MaterializedView => "Materialized view",
        ObjectKind::Collection => "Collection",
        _ => "Table",
    }
}

/// Everything the palette offers now.
fn items() -> Vec<Item> {
    let state = ui();
    let active = state.active.get();
    let live = active
        .as_ref()
        .is_some_and(|id| state.live.with(|m| m.contains_key(id)));
    let mut out: Vec<Item> = state.connections.with(|list| {
        list.iter()
            .map(|c| Item {
                label: c.config.name.clone(),
                detail: format!(
                    "Connection · {}",
                    crate::sidebar::env_label(c.config.environment)
                ),
                target: Target::Connection(c.config.id.clone()),
            })
            .collect()
    });
    if let Some(id) = &active {
        out.extend(state.queries.with(|list| {
            list.iter()
                .filter(|q| q.connection.is_none() || q.connection.as_ref() == Some(id))
                .map(|q| Item {
                    label: q.name.clone(),
                    detail: if q.folder.is_empty() {
                        "Saved query".to_owned()
                    } else {
                        format!("Saved query · {}", q.folder)
                    },
                    target: Target::Query(q.clone()),
                })
                .collect::<Vec<_>>()
        }));
    }
    out.extend(
        ACTIONS
            .iter()
            .filter(|a| live || !a.needs_connection())
            .map(|a| Item {
                label: a.label().to_owned(),
                detail: "Action".to_owned(),
                target: Target::Action(*a),
            }),
    );
    if let Some(id) = active.filter(|_| live) {
        crate::complete::fetch::catalog(&id);
        out.extend(state.catalogs.with(|m| {
            m.get(&id)
                .map(|c| {
                    c.entries
                        .iter()
                        .map(|e| Item {
                            label: e.name.clone(),
                            detail: match (&e.schema, &e.database) {
                                (Some(s), _) | (None, Some(s)) => {
                                    format!("{} · {s}", kind_label(e.kind))
                                }
                                (None, None) => kind_label(e.kind).to_owned(),
                            },
                            target: Target::Object(e.clone()),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        }));
    }
    out
}

/// Goes to or does `target`.
fn go(target: Target) {
    let state = ui();
    state.dialog.set(Dialog::None);
    match target {
        Target::Connection(id) => {
            state.side_view.set(SideView::Connections);
            crate::sidebar::open(id);
        }
        Target::Query(q) => crate::tabs::open_saved(q),
        Target::Object(e) => {
            let node = treasury_core::schema::ObjectNode {
                path: e.path.clone(),
                kind: e.kind,
                name: e.name.clone(),
                expandable: false,
                detail: None,
            };
            if let Some(statement) = state
                .active_config_untracked()
                .and_then(|c| crate::editor::browse_statement(&c, &node))
            {
                state.open_rows.set(Some((e.name, statement)));
            }
        }
        Target::Action(a) => match a {
            Action::NewTab => crate::tabs::new_tab(),
            Action::RunAll => state.command.set(Some(EditorCommand::RunAll)),
            Action::Format => state.command.set(Some(EditorCommand::Format)),
            Action::SaveQuery => crate::tabs::save_current(),
            Action::Settings => state.dialog.set(Dialog::Settings),
            Action::AddConnection => crate::sidebar::add_connection(),
            Action::Import => crate::sidebar::import_connections(),
            Action::ShowConnections => state.side_view.set(SideView::Connections),
            Action::ShowQueries => state.side_view.set(SideView::Queries),
            Action::ShowHistory => state.side_view.set(SideView::History),
            Action::Theme(t) => {
                crate::dialogs::apply_theme(t);
                crate::home::set_setting("theme", serde_json::Value::String(t.to_owned()));
            }
        },
    }
}

#[component]
pub fn Palette() -> impl IntoView {
    let state = ui();
    let query = RwSignal::new(String::new());
    let selected = RwSignal::new(0usize);
    let found = Memo::new(move |_| search(&query.get(), &items()));
    request_animation_frame(|| {
        if let Some(el) = document().get_element_by_id("palette-input") {
            use wasm_bindgen::JsCast;
            if let Ok(input) = el.dyn_into::<web_sys::HtmlInputElement>() {
                let _ = input.focus();
            }
        }
    });
    let keydown = move |ev: web_sys::KeyboardEvent| {
        let n = found.with_untracked(Vec::len);
        match ev.key().as_str() {
            "ArrowDown" if n > 0 => {
                ev.prevent_default();
                selected.update(|s| *s = (*s + 1) % n);
            }
            "ArrowUp" if n > 0 => {
                ev.prevent_default();
                selected.update(|s| *s = (*s + n - 1) % n);
            }
            "Enter" => {
                ev.prevent_default();
                if let Some(item) =
                    found.with_untracked(|f| f.get(selected.get_untracked()).cloned())
                {
                    go(item.target);
                }
            }
            "Escape" => {
                ev.prevent_default();
                state.dialog.set(Dialog::None);
            }
            _ => {}
        }
    };
    view! {
        <div class="backdrop palette-backdrop" on:mousedown=move |ev| {
            // A click outside the palette closes it.
            use wasm_bindgen::JsCast;
            let outside = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .is_some_and(|t| t.class_list().contains("palette-backdrop"));
            if outside {
                state.dialog.set(Dialog::None);
            }
        }>
            <div class="dialog palette" role="dialog" aria-modal="true" aria-label="Command palette">
                <input id="palette-input" autocomplete="off" spellcheck="false"
                    placeholder="Go to a connection, table, saved query or action"
                    prop:value=move || query.get()
                    on:input=move |ev| { query.set(event_target_value(&ev)); selected.set(0); }
                    on:keydown=keydown />
                {move || {
                    let list = found.get();
                    if list.is_empty() {
                        return view! { <p class="empty">"Nothing matches."</p> }.into_any();
                    }
                    view! {
                        <ul class="palette-list" role="listbox">
                            {list.into_iter().enumerate().map(|(i, item)| {
                                let target = item.target.clone();
                                view! {
                                    <li role="option" class:on=move || selected.get() == i
                                        aria-selected=move || (selected.get() == i).to_string()
                                        on:mousemove=move |_| selected.set(i)
                                        on:mousedown=move |ev| { ev.prevent_default(); go(target.clone()); }>
                                        <span>{item.label}</span>
                                        <span class="detail">{item.detail}</span>
                                    </li>
                                }
                            }).collect_view()}
                        </ul>
                    }.into_any()
                }}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closer_matches_score_higher() {
        let s = |q, t| score(q, t).unwrap();
        assert!(score("xyz", "orders").is_none());
        assert!(s("ord", "orders") > s("ord", "big_order_lines"));
        assert!(s("ol", "order_lines") > s("ol", "pool"));
        assert!(s("set", "Settings") > s("set", "Save the tab"));
        assert_eq!(score("", "anything"), Some(0));
        // Case and spaces in the query do not matter.
        assert!(score("Order L", "order_lines").is_some());
    }

    fn item(label: &str, target: Target) -> Item {
        Item {
            label: label.into(),
            detail: String::new(),
            target,
        }
    }

    #[test]
    fn best_matches_first_and_tables_only_when_asked_for() {
        let table = CatalogEntry {
            path: treasury_core::schema::ObjectPath::root(),
            kind: treasury_core::schema::ObjectKind::Table,
            name: "orders".into(),
            schema: Some("public".into()),
            database: None,
        };
        let items = vec![
            item("Settings", Target::Action(Action::Settings)),
            item("orders", Target::Object(table)),
            item("Show saved queries", Target::Action(Action::ShowQueries)),
        ];
        let labels = |q: &str| {
            search(q, &items)
                .into_iter()
                .map(|i| i.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(""), ["Settings", "Show saved queries"]);
        assert_eq!(labels("ord"), ["orders"]);
        assert_eq!(labels("se")[0], "Settings");
    }
}
