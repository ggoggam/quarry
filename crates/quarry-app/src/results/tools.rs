//! The bars over a result grid: the filter bar (and the sort it is under),
//! and the find bar. Each result tab has its own, with the text typed in it.
//!
//! The filter applies on Enter, and clearing its text clears the filter. The
//! find bar searches as you type, a moment after the last key; Enter and
//! ⇧Enter step through the matches.

use std::time::Duration;

use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::table::TableEvent;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _,
};
use quarry_core::view::filter;
use quarry_core::view::find::FindQuery;

use super::commands::{FILTER_CONTEXT, FIND_CONTEXT};
use super::grid::{GridState, focused_cell, refresh_range};
use super::{ResultTabId, ResultTable, Results, arrange, search};
use crate::format::{format_count, format_elapsed};

/// How long find waits after a key before it searches.
const FIND_DELAY: Duration = Duration::from_millis(150);

/// A result tab's filter and find inputs.
pub struct GridInputs {
    pub filter: Entity<InputState>,
    pub find: Entity<InputState>,
    pub find_open: bool,
    pub case_sensitive: bool,
    pub whole_cell: bool,
    _subscriptions: Vec<Subscription>,
}

impl GridInputs {
    fn query(&self, cx: &gpui_kit::App) -> FindQuery {
        FindQuery {
            text: self.find.read(cx).value().to_string(),
            case_sensitive: self.case_sensitive,
            whole_cell: self.whole_cell,
        }
    }
}

impl Results {
    /// The inputs of result tab `id`, whose grid is `table`.
    pub(super) fn grid_inputs(
        &mut self,
        id: ResultTabId,
        table: &ResultTable,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> GridInputs {
        let filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder(
                "Filter: a word, or a condition such as amount > 100 AND status = 'paid'",
            )
        });
        let find = cx.new(|cx| InputState::new(window, cx).placeholder("Find in result"));
        let subscriptions = vec![
            cx.subscribe_in(&filter, window, move |this, input, event, window, cx| {
                match event {
                    InputEvent::PressEnter { .. } => {
                        let text = input.read(cx).value().to_string();
                        this.apply_filter(id, &text, window, cx);
                    }
                    // Clearing the text clears the filter.
                    InputEvent::Change if input.read(cx).value().trim().is_empty() => {
                        let applied = this.table_of(id).is_some_and(|t| {
                            t.read(cx).delegate().arrange.filter.is_some()
                                || t.read(cx).delegate().arrange.error.is_some()
                        });
                        if applied {
                            this.apply_filter(id, "", window, cx);
                        }
                    }
                    _ => {}
                }
            }),
            cx.subscribe_in(
                &find,
                window,
                move |this, _, event, window, cx| match event {
                    InputEvent::Change => this.find_changed(id, cx),
                    InputEvent::PressEnter { shift, .. } => {
                        this.search_now(id, cx);
                        this.step_in(id, !shift, window, cx);
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(
                table,
                window,
                move |this, table, event, window, cx| match event {
                    TableEvent::SelectCell(..)
                    | TableEvent::SelectRow(_)
                    | TableEvent::SelectColumn(_)
                    | TableEvent::ClearSelection => table.update(cx, |table, cx| {
                        let range = &mut table.delegate_mut().range;
                        if range.extending {
                            range.extending = false;
                        } else {
                            range.anchor = None;
                            range.all = false;
                        }
                        refresh_range(table);
                        cx.notify();
                    }),
                    TableEvent::DoubleClickedCell(..) => this.view_value(window, cx),
                    _ => {}
                },
            ),
        ];
        GridInputs {
            filter,
            find,
            find_open: false,
            case_sensitive: false,
            whole_cell: false,
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn table_of(&self, id: ResultTabId) -> Option<&ResultTable> {
        Some(&self.tabs.get(id)?.data.table)
    }

    /// The table of the result tab on screen.
    pub(super) fn active_table(&self) -> Option<ResultTable> {
        self.active_result().map(|r| r.table.clone())
    }

    fn update_active_table<R>(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut GridState, &mut Context<GridState>) -> R,
    ) -> Option<R> {
        let table = self.active_table()?;
        Some(table.update(cx, f))
    }

    // ── Selection ────────────────────────────────────────────────────────────

    pub fn select_all_cells(&mut self, cx: &mut Context<Self>) {
        self.update_active_table(cx, |table, cx| {
            if table.delegate().view_len() == 0 {
                return;
            }
            let range = &mut table.delegate_mut().range;
            range.all = true;
            range.anchor = None;
            if focused_cell(table).is_none() {
                table.delegate_mut().range.extending = true;
                table.set_selected_cell(0, 1, cx);
            }
            refresh_range(table);
            cx.notify();
        });
    }

    /// Grow the selection by a row or a column, as ⇧ and an arrow do.
    pub fn extend_selection(&mut self, rows: isize, cols: isize, cx: &mut Context<Self>) {
        self.update_active_table(cx, |table, cx| {
            let Some(gpui_kit::component::table::TableSelection::Cell(row, col)) =
                Some(table.selection())
            else {
                return;
            };
            let (max_row, max_col) = (
                table.delegate().view_len().saturating_sub(1),
                table.delegate().data_columns(),
            );
            let range = &mut table.delegate_mut().range;
            range.anchor.get_or_insert((row, col));
            range.extending = true;
            range.all = false;
            let row = row.saturating_add_signed(rows).min(max_row);
            let col = col.saturating_add_signed(cols).clamp(1, max_col.max(1));
            table.set_selected_cell(row, col, cx);
            refresh_range(table);
        });
    }

    // ── Sorting ──────────────────────────────────────────────────────────────

    /// Sort by the selected cell's column, as the only key or (`add`) after
    /// the others.
    pub fn sort_selected(&mut self, descending: bool, add: bool, cx: &mut Context<Self>) {
        self.update_active_table(cx, |table, cx| {
            let Some((_, col)) = focused_cell(table) else {
                return;
            };
            let keys =
                quarry_core::view::sort::set(&table.delegate().arrange.sort, col, descending, add);
            arrange::set_sort(table, keys, cx);
        });
    }

    pub fn clear_sort(&mut self, cx: &mut Context<Self>) {
        self.update_active_table(cx, |table, cx| arrange::set_sort(table, Vec::new(), cx));
    }

    // ── Filtering ────────────────────────────────────────────────────────────

    fn apply_filter(
        &mut self,
        id: ResultTabId,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(table) = self.table_of(id).cloned() else {
            return;
        };
        let applied = table.update(cx, |table, cx| arrange::set_filter(table, text, cx));
        if applied.is_ok() && !text.trim().is_empty() {
            // Back to the grid, to look at what is left.
            table.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// Keep only the rows whose selected column equals (`exclude`: differs
    /// from) the selected cell, on top of the filter there is.
    pub fn filter_by_selection(
        &mut self,
        exclude: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.active_result() else {
            return;
        };
        let id = active.id;
        let table = active.table.read(cx);
        let delegate = table.delegate();
        let (Some((row, col)), Some(result)) = (focused_cell(table), delegate.result()) else {
            return;
        };
        let Some(base) = delegate.base_row(row) else {
            return;
        };
        let condition = filter::value_condition(result, base, col, exclude);
        let existing = delegate.arrange.filter_text.clone();
        let text = filter::and(Some(&existing), &condition, result.schema());
        let Some(inputs) = self.tabs.get(id).map(|t| t.data.inputs.filter.clone()) else {
            return;
        };
        inputs.update(cx, |input, cx| input.set_value(text.clone(), window, cx));
        self.apply_filter(id, &text, window, cx);
    }

    pub fn edit_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active_id() else {
            return;
        };
        if let Some(tab) = self.tabs.get(id) {
            let filter = tab.data.inputs.filter.clone();
            filter.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        }
    }

    pub fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active_id() else {
            return;
        };
        if let Some(tab) = self.tabs.get(id) {
            let filter = tab.data.inputs.filter.clone();
            filter.update(cx, |input, cx| input.set_value(String::new(), window, cx));
        }
        self.apply_filter(id, "", window, cx);
    }

    // ── Find ─────────────────────────────────────────────────────────────────

    /// Show the find bar and put the caret in it; `use_selection` searches
    /// for the selected cell's value (⌘E).
    pub fn open_find(&mut self, use_selection: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.active_result() else {
            return;
        };
        let id = active.id;
        let selected = use_selection
            .then(|| {
                let table = active.table.read(cx);
                let (row, col) = focused_cell(table)?;
                let delegate = table.delegate();
                delegate.result()?.cell_value(delegate.base_row(row)?, col)
            })
            .flatten();
        let Some(tab) = self.tabs.get_mut(id) else {
            return;
        };
        tab.data.inputs.find_open = true;
        let find = tab.data.inputs.find.clone();
        find.update(cx, |input, cx| {
            if let Some(text) = &selected {
                input.set_value(text.clone(), window, cx);
            }
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.search_now(id, cx);
        cx.notify();
    }

    pub fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active_id() else {
            return;
        };
        let Some(tab) = self.tabs.get_mut(id) else {
            return;
        };
        tab.data.inputs.find_open = false;
        let table = tab.data.table.clone();
        table.update(cx, search::clear);
        table.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Next or previous match (⌘G, ⇧⌘G); opens the find bar if it is shut.
    pub fn find_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active_id() else {
            return;
        };
        let open = self.tabs.get(id).is_some_and(|t| t.data.inputs.find_open);
        if !open {
            self.open_find(false, window, cx);
            return;
        }
        self.search_now(id, cx);
        self.step_in(id, forward, window, cx);
    }

    fn step_in(&mut self, id: ResultTabId, forward: bool, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(table) = self.table_of(id).cloned() {
            table.update(cx, |table, cx| search::step(table, forward, cx));
        }
    }

    /// Search again a moment after the last change.
    fn find_changed(&mut self, id: ResultTabId, cx: &mut Context<Self>) {
        self.find_delay = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIND_DELAY).await;
            let _ = this.update(cx, |this, cx| this.search_now(id, cx));
        }));
    }

    /// Search for what the find bar holds, unless that search is under way
    /// or done.
    fn search_now(&mut self, id: ResultTabId, cx: &mut Context<Self>) {
        self.find_delay = None;
        let Some(tab) = self.tabs.get(id) else {
            return;
        };
        let query = tab.data.inputs.query(cx);
        let table = tab.data.table.clone();
        table.update(cx, |table, cx| {
            if table.delegate().search.query != query {
                search::set_query(table, query, cx);
            }
        });
    }

    fn toggle_find_option(&mut self, id: ResultTabId, case: bool, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(id) {
            let inputs = &mut tab.data.inputs;
            if case {
                inputs.case_sensitive = !inputs.case_sensitive;
            } else {
                inputs.whole_cell = !inputs.whole_cell;
            }
        }
        self.search_now(id, cx);
        cx.notify();
    }

    // ── Drawing ──────────────────────────────────────────────────────────────

    /// The filter bar, and the find bar when it is open, over tab `id`.
    pub(super) fn render_tools(
        &self,
        id: ResultTabId,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let Some(tab) = self.tabs.get(id) else {
            return div().into_any_element();
        };
        let inputs = &tab.data.inputs;
        let table = tab.data.table.read(cx);
        let delegate = table.delegate();
        let arrange = &delegate.arrange;
        let theme = cx.theme();
        let (muted, danger, border) = (theme.muted_foreground, theme.danger, theme.border);

        let sort = (!arrange.sort.is_empty()).then(|| {
            let result = delegate.result();
            let keys: Vec<String> = arrange
                .sort
                .iter()
                .map(|k| {
                    let name = result.map_or("?", |r| r.column_name(k.column));
                    format!("{name} {}", if k.descending { "↓" } else { "↑" })
                })
                .collect();
            h_flex()
                .flex_shrink_0()
                .gap_1()
                .text_xs()
                .text_color(muted)
                .child(format!("Sorted by {}", keys.join(", ")))
                .child(
                    Button::new(("clear-sort", id.raw()))
                        .icon(Icon::new(IconName::Close))
                        .xsmall()
                        .ghost()
                        .tooltip("Clear the sort")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.clear_sort(cx))),
                )
        });
        let state: Option<(SharedString, gpui_kit::Hsla)> = match (&arrange.job, &arrange.error) {
            (Some(job), _) => Some((
                match job.progress {
                    Some(p) => format!("{}… {:.0}%", job.what, p * 100.0),
                    None => format!("{}…", job.what),
                }
                .into(),
                muted,
            )),
            (None, Some(error)) => Some((error.clone(), danger)),
            (None, None) if arrange.is_active() => {
                let shown = delegate.view_len();
                let total = delegate.row_count();
                let mut text = if arrange.filter.is_some() {
                    format!("{} of {} rows", format_count(shown), format_count(total))
                } else {
                    String::new()
                };
                if arrange.basis < total {
                    // Rows arrived since the view was worked out.
                    text = format!(
                        "view of the first {} rows; updates when the query ends",
                        format_count(arrange.basis)
                    );
                }
                if let Some(elapsed) = arrange.elapsed.filter(|_| !text.is_empty()) {
                    text.push_str(&format!(" · {}", format_elapsed(elapsed)));
                }
                (!text.is_empty()).then(|| (text.into(), muted))
            }
            _ => None,
        };
        let filter_bar = h_flex()
            .key_context(FILTER_CONTEXT)
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .border_b_1()
            .border_color(border)
            .child(Icon::new(IconName::Search).small().text_color(muted))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&inputs.filter).small().cleanable(true)),
            )
            .when_some(state, |this, (text, color)| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .max_w_96()
                        .truncate()
                        .text_xs()
                        .text_color(color)
                        .child(text),
                )
            })
            .children(sort);

        let find_bar = inputs.find_open.then(|| {
            let search = &delegate.search;
            let count = match (search.progress, search.total) {
                _ if !search.is_active() => String::new(),
                (Some(p), found) => format!("{} found, {:.0}%…", format_count(found), p * 100.0),
                (None, 0) => "No matches".into(),
                (None, total) => {
                    let more = if total > search.matches.len() {
                        "+"
                    } else {
                        ""
                    };
                    match search.current {
                        Some(ix) => {
                            format!("{} of {}{more}", format_count(ix + 1), format_count(total))
                        }
                        None => format!("{}{more} matches", format_count(total)),
                    }
                }
            };
            let toggle = |label: &'static str, tip: &'static str, on: bool, case: bool| {
                Button::new((label, id.raw()))
                    .label(label)
                    .xsmall()
                    .ghost()
                    .selected(on)
                    .tooltip(tip)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle_find_option(id, case, cx)
                    }))
            };
            h_flex()
                .key_context(FIND_CONTEXT)
                .w_full()
                .px_2()
                .py_1()
                .gap_1()
                .border_b_1()
                .border_color(border)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&inputs.find).small().cleanable(true)),
                )
                .child(toggle("Aa", "Match case", inputs.case_sensitive, true))
                .child(toggle(
                    "Cell",
                    "Match the whole cell",
                    inputs.whole_cell,
                    false,
                ))
                .child(
                    div()
                        .flex_shrink_0()
                        .min_w_24()
                        .px_1()
                        .text_xs()
                        .text_color(muted)
                        .child(count),
                )
                .child(
                    Button::new(("find-previous", id.raw()))
                        .icon(Icon::new(IconName::ChevronUp))
                        .xsmall()
                        .ghost()
                        .tooltip("Previous match (⇧⌘G)")
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.search_now(id, cx);
                            this.step_in(id, false, window, cx)
                        })),
                )
                .child(
                    Button::new(("find-next", id.raw()))
                        .icon(Icon::new(IconName::ChevronDown))
                        .xsmall()
                        .ghost()
                        .tooltip("Next match (⌘G)")
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.search_now(id, cx);
                            this.step_in(id, true, window, cx)
                        })),
                )
                .child(
                    Button::new(("find-close", id.raw()))
                        .icon(Icon::new(IconName::Close))
                        .xsmall()
                        .ghost()
                        .tooltip("Close (Esc)")
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.close_find(window, cx)
                        })),
                )
        });

        v_flex()
            .w_full()
            .child(filter_bar)
            .children(find_bar)
            .children(self.render_export_progress(id, cx))
            .into_any_element()
    }
}
