//! The Query History dialog: the statements run so far, newest first, with
//! a search box. Enter or a double click puts the selected statement into
//! the editor at the caret; ⌘↵ or Run runs it again on the current
//! connection.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IndexPath, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};

use super::{Entry, History, Outcome, store};
use crate::format::{format_count, format_elapsed, now_ms, plural, when};
use crate::icons::{LucideIcon, icon};

/// Does something with a statement picked from the history.
pub type Handler = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// What the dialog does with a statement.
pub struct Handlers {
    /// Put it into the editor at the caret.
    pub insert: Handler,
    /// Run it on the current connection.
    pub run: Handler,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pick {
    Insert,
    Run,
}

pub struct HistoryDelegate {
    entries: Rc<Vec<Entry>>,
    /// Each entry's statement and connection in lower case, to search.
    haystacks: Rc<Vec<String>>,
    /// Indexes into `entries` of what the search and filter let through.
    matches: Vec<usize>,
    terms: Vec<String>,
    /// Only this connection's entries, when set.
    only: Option<String>,
    selected: Option<usize>,
    /// The click count of the item click being handled, to tell a click
    /// (select) from a double click (insert).
    click: Option<usize>,
    picked: Option<Pick>,
    now: i64,
}

impl HistoryDelegate {
    fn new(entries: Vec<Entry>) -> Self {
        let haystacks = entries.iter().map(Entry::haystack).collect();
        let mut this = Self {
            entries: Rc::new(entries),
            haystacks: Rc::new(haystacks),
            matches: Vec::new(),
            terms: Vec::new(),
            only: None,
            selected: None,
            click: None,
            picked: None,
            now: now_ms(),
        };
        this.filter();
        this
    }

    fn filter(&mut self) {
        let only = self.only.as_deref();
        self.matches = (0..self.entries.len())
            .filter(|&ix| only.is_none_or(|c| self.entries[ix].connection == c))
            .filter(|&ix| store::matches(&self.haystacks[ix], &self.terms))
            .collect();
        self.selected = (!self.matches.is_empty()).then_some(0);
    }

    fn selected_entry(&self) -> Option<&Entry> {
        let ix = *self.matches.get(self.selected?)?;
        self.entries.get(ix)
    }
}

impl ListDelegate for HistoryDelegate {
    type Item = ListItem;

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.terms = store::search_terms(query);
        self.filter();
        Task::ready(())
    }

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.matches.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let entry = self.entries.get(*self.matches.get(ix.row)?)?;
        let theme = cx.theme();
        let (status, color) = match entry.outcome {
            Outcome::Succeeded => (outcome_text(entry), theme.muted_foreground),
            Outcome::Failed => ("failed".to_owned(), theme.danger),
            Outcome::Cancelled => ("cancelled".to_owned(), theme.warning),
        };
        let meta = format!(
            "{} · {} · {}",
            when(entry.at, self.now),
            entry.connection,
            format_elapsed(std::time::Duration::from_millis(entry.elapsed_ms)),
        );
        let first_line = crate::results::preview(&entry.statement, 200);
        Some(
            ListItem::new(ix.row)
                .py_1()
                .child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            div()
                                .w_full()
                                .truncate()
                                .text_sm()
                                .font_family(theme.mono_font_family.clone())
                                .child(first_line),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .text_xs()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(theme.muted_foreground)
                                        .child(meta),
                                )
                                .child(div().flex_shrink_0().text_color(color).child(status)),
                        ),
                )
                .on_click(cx.listener(|this, event: &ClickEvent, _, _| {
                    this.delegate_mut().click = Some(event.click_count());
                })),
        )
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        let message = if self.entries.is_empty() {
            "Statements you run show up here."
        } else {
            "No statement matches."
        };
        crate::ui::empty_state(icon(LucideIcon::RotateCcwClock), message, None, cx)
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix.map(|ix| ix.row);
        cx.notify();
    }

    fn confirm(&mut self, secondary: bool, _: &mut Window, _: &mut Context<ListState<Self>>) {
        // A single click only selects; Enter and a double click pick.
        if self.click.take().is_some_and(|clicks| clicks < 2) {
            return;
        }
        self.picked = Some(if secondary { Pick::Run } else { Pick::Insert });
    }
}

fn outcome_text(entry: &Entry) -> String {
    match (entry.rows, entry.affected) {
        (Some(rows), _) => plural(rows as usize, "row"),
        (None, Some(affected)) => format!("{} affected", format_count(affected as usize)),
        (None, None) => "done".to_owned(),
    }
}

pub struct HistoryPanel {
    list: Entity<ListState<HistoryDelegate>>,
    /// The current connection's key and how the sidebar describes it.
    connection: (String, SharedString),
    handlers: Rc<Handlers>,
    _subscriptions: Vec<Subscription>,
}

impl HistoryPanel {
    fn new(
        connection: (String, SharedString),
        handlers: Rc<Handlers>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let entries: Vec<Entry> = History::global(cx).read(cx).entries().cloned().collect();
        let list = cx.new(|cx| {
            let mut list =
                ListState::new(HistoryDelegate::new(entries), window, cx).searchable(true);
            if list.delegate().selected.is_some() {
                list.set_selected_index(Some(IndexPath::new(0)), window, cx);
            }
            list
        });
        let subscriptions = vec![
            cx.observe(&list, |_, _, cx| cx.notify()),
            cx.subscribe_in(&list, window, |this, _, event: &ListEvent, window, cx| {
                if let ListEvent::Confirm(_) = event {
                    let picked = this
                        .list
                        .update(cx, |list, _| list.delegate_mut().picked.take());
                    if let Some(pick) = picked {
                        this.pick(pick, window, cx);
                    }
                }
            }),
        ];
        Self {
            list,
            connection,
            handlers,
            _subscriptions: subscriptions,
        }
    }

    fn selected(&self, cx: &App) -> Option<Entry> {
        self.list.read(cx).delegate().selected_entry().cloned()
    }

    fn pick(&mut self, pick: Pick, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected(cx).filter(|e| !e.cut) else {
            return;
        };
        window.close_dialog(cx);
        let handler = match pick {
            Pick::Insert => self.handlers.insert.clone(),
            Pick::Run => self.handlers.run.clone(),
        };
        handler(entry.statement, window, cx);
    }

    fn set_only_connection(&mut self, only: bool, window: &mut Window, cx: &mut Context<Self>) {
        let connection = self.connection.0.clone();
        self.list.update(cx, |list, cx| {
            let delegate = list.delegate_mut();
            delegate.only = only.then_some(connection);
            delegate.filter();
            let selected = delegate.selected.map(IndexPath::new);
            list.set_selected_index(selected, window, cx);
        });
    }
}

impl Render for HistoryPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let delegate = self.list.read(cx).delegate();
        let shown = delegate.matches.len();
        let total = delegate.entries.len();
        let only = delegate.only.is_some();
        let selected = delegate.selected_entry().cloned();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let count = if shown == total {
            plural(total, "statement")
        } else {
            format!("{} of {}", format_count(shown), plural(total, "statement"))
        };
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Checkbox::new("history-this-connection")
                            .label(format!("Only {}", self.connection.1))
                            .text_sm()
                            .checked(only)
                            .on_click(cx.listener(|this, checked: &bool, window, cx| {
                                this.set_only_connection(*checked, window, cx)
                            })),
                    )
                    .child(div().flex_1())
                    .child(div().text_xs().text_color(muted).child(count)),
            )
            .child(
                div()
                    .h(px(300.))
                    .border_1()
                    .border_color(theme.border)
                    .rounded(theme.radius)
                    .child(
                        List::new(&self.list)
                            .search_placeholder("Search statements and connections"),
                    ),
            )
            .child(
                v_flex()
                    .id("history-preview")
                    .h(px(150.))
                    .p_2()
                    .gap_1()
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(theme.border)
                    .rounded(theme.radius)
                    .bg(theme.muted)
                    .map(|this| match &selected {
                        None => this.child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child("Select a statement to see it whole."),
                        ),
                        Some(entry) => this
                            .when(entry.cut, |this| {
                                this.child(div().text_xs().text_color(theme.warning).child(
                                    format!(
                                        "Only the first {} KB of this statement were kept, \
                                         so it can't be put back or run.",
                                        store::MAX_STATEMENT_BYTES / 1024
                                    ),
                                ))
                            })
                            .when_some(entry.error.clone(), |this, error| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.danger)
                                        .font_family(theme.mono_font_family.clone())
                                        .child(error),
                                )
                            })
                            .child(
                                div()
                                    .text_sm()
                                    .font_family(theme.mono_font_family.clone())
                                    .whitespace_normal()
                                    .child(entry.statement.clone()),
                            ),
                    }),
            )
    }
}

/// Open the Query History dialog. `connection` is the current connection's
/// key and label, for the "Only this connection" filter.
pub fn open(
    connection: (String, SharedString),
    handlers: Handlers,
    window: &mut Window,
    cx: &mut App,
) {
    let handlers = Rc::new(handlers);
    let panel = cx.new(|cx| HistoryPanel::new(connection, handlers, window, cx));
    let first = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            panel.read(cx).list.focus_handle(cx).focus(window, cx);
        }
        let usable = panel.read(cx).selected(cx).is_some_and(|e| !e.cut);
        let empty = History::global(cx).read(cx).len() == 0;
        let on_pick = |pick: Pick| {
            let panel = panel.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                panel.update(cx, |panel, cx| panel.pick(pick, window, cx));
            }
        };
        let clear = {
            let panel = panel.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                confirm_clear(panel.clone(), window, cx)
            }
        };
        dialog
            .title("Query History")
            .w(px(880.))
            .child(panel.clone())
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("history-clear")
                            .label("Clear History…")
                            .ghost()
                            .disabled(empty)
                            .on_click(clear),
                    )
                    .child(div().flex_1())
                    .child(crate::ui::natural(
                        DialogClose::new().trigger(|button| button.label("Close")),
                    ))
                    .child(
                        Button::new("history-run")
                            .icon(icon(LucideIcon::Play))
                            .label("Run")
                            .tooltip("Run it on the current connection (⌘↵)")
                            .disabled(!usable)
                            .on_click(on_pick(Pick::Run)),
                    )
                    .child(
                        Button::new("history-insert")
                            .primary()
                            .label("Insert")
                            .tooltip("Put it into the editor at the caret (↵)")
                            .disabled(!usable)
                            .on_click(on_pick(Pick::Insert)),
                    ),
            )
    });
}

fn confirm_clear(panel: Entity<HistoryPanel>, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let panel = panel.clone();
        alert
            .confirm()
            .title("Clear the query history?")
            .description("Quarry forgets every statement it has recorded. This cannot be undone.")
            .ok_text("Clear")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                History::global(cx).update(cx, |history, cx| history.clear(cx));
                panel.update(cx, |panel, cx| {
                    panel.list.update(cx, |list, cx| {
                        *list.delegate_mut() = HistoryDelegate::new(Vec::new());
                        list.set_selected_index(None, window, cx);
                    });
                    cx.notify();
                });
                true
            })
    });
}
