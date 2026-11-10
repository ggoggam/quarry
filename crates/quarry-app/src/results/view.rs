//! Drawing the results pane (the strip, the Output view, a result's grid),
//! its context menus and rename dialog, and the actions and menu items that
//! drive it.

use std::cell::Cell;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::tab::Tab;
use gpui_kit::component::table::DataTable;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, Focusable as _,
    InteractiveElement, IntoElement, MenuItem, MouseButton, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px,
};

use super::{MEMORY_BUDGET, OutputLine, ResultState, ResultTabId, Results, Tone, View, title};
use crate::format::clock;
use crate::tab_strip::{self, StripTab};

gpui_kit::actions!(
    quarry,
    [
        ShowOutput,
        NextResultTab,
        PreviousResultTab,
        RenameResultTab,
        TogglePinResultTab,
        RerunResult,
        CloseResultTab,
        CloseUnpinnedResultTabs,
    ]
);

/// The result tabs' items, for the Query menu.
pub fn actions_menu_items() -> Vec<MenuItem> {
    vec![
        MenuItem::action("Show Output", ShowOutput),
        MenuItem::action("Show Next Result", NextResultTab),
        MenuItem::action("Show Previous Result", PreviousResultTab),
        MenuItem::separator(),
        MenuItem::action("Rerun Result", RerunResult),
        MenuItem::action("Rename Result Tab…", RenameResultTab),
        MenuItem::action("Pin or Unpin Result Tab", TogglePinResultTab),
        MenuItem::action("Close Result Tab", CloseResultTab),
        MenuItem::action("Close Unpinned Result Tabs", CloseUnpinnedResultTabs),
    ]
}

/// Handle the result tabs' actions on `element`, the window's root, so they
/// work wherever the focus is.
pub fn bind_actions<E: InteractiveElement>(element: E, results: &Entity<Results>) -> E {
    macro_rules! on {
        ($element:expr, $action:ty, |$this:ident, $window:ident, $cx:ident| $body:expr) => {{
            let results = results.clone();
            $element.on_action(move |_: &$action, $window: &mut Window, cx: &mut App| {
                results.update(cx, |$this, $cx| $body)
            })
        }};
    }
    let element = on!(element, ShowOutput, |this, _window, cx| this
        .select(View::Output, cx));
    let element = on!(element, NextResultTab, |this, _window, cx| this
        .cycle(1, cx));
    let element = on!(element, PreviousResultTab, |this, _window, cx| this
        .cycle(-1, cx));
    let element = on!(element, RerunResult, |this, _window, cx| {
        if let Some(id) = this.active_id() {
            this.rerun(id, cx)
        }
    });
    let element = on!(element, RenameResultTab, |this, window, cx| {
        if let Some(id) = this.active_id() {
            this.prompt_rename(id, window, cx)
        }
    });
    let element = on!(element, TogglePinResultTab, |this, _window, cx| {
        if let Some(tab) = this.active_id().and_then(|id| this.tabs.get(id)) {
            let (id, pinned) = (tab.id(), tab.pinned);
            this.set_pinned(id, !pinned, cx)
        }
    });
    let element = on!(element, CloseResultTab, |this, _window, cx| {
        if let Some(id) = this.active_id() {
            this.close(id, cx)
        }
    });
    on!(element, CloseUnpinnedResultTabs, |this, _window, cx| this
        .close_unpinned(cx))
}

impl Results {
    /// Ask for a tab's new name.
    pub fn prompt_rename(&mut self, id: ResultTabId, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(current), Some(own)) = (
            self.tabs.title(id).map(str::to_owned),
            self.tabs.get(id).map(|tab| tab.title.clone()),
        ) else {
            return;
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(own)
                .default_value(current)
        });
        // Enter renames, as the button does.
        cx.subscribe_in(&input, window, move |this, input, event, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                let name = input.read(cx).value().to_string();
                this.rename(id, &name, cx);
                window.close_dialog(cx);
            }
        })
        .detach();
        let results = cx.entity().downgrade();
        let first = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                input.focus_handle(cx).focus(window, cx);
                input.update(cx, |input, cx| input.select_all(window, cx));
            }
            let (input_ok, results) = (input.clone(), results.clone());
            dialog
                .title("Rename Result Tab")
                .w(px(380.))
                .child(
                    v_flex().gap_2().child(Input::new(&input)).child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Leave it empty for the name its statement gives it."),
                    ),
                )
                .footer(
                    DialogFooter::new()
                        .child(div().flex_1())
                        .child(DialogClose::new().trigger(|button| button.label("Cancel")))
                        .child(
                            DialogAction::new()
                                .child(Button::new("rename-ok").primary().label("Rename")),
                        ),
                )
                .on_ok(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let name = input_ok.read(cx).value().to_string();
                    let _ = results.update(cx, |this, cx| this.rename(id, &name, cx));
                    true
                })
        });
    }

    fn render_output_tab(&self, cx: &mut Context<Self>) -> Tab {
        let failed = self
            .tabs
            .output(self.owner)
            .last()
            .is_some_and(|line| line.tone == Tone::Error);
        let mut tab = StripTab::new("Output", Icon::new(gpui_kit::assets::IconName::ScrollText))
            .tooltip("What each statement did");
        if failed {
            tab = tab.icon_color(cx.theme().danger);
        }
        tab.render(|_, _| {}, cx).on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, _, _, _| this.menu_target = Some(View::Output)),
        )
    }

    fn render_result_tab(&self, id: ResultTabId, title: String, cx: &mut Context<Self>) -> Tab {
        let tab = self.tabs.get(id).expect("titles come from the tabs");
        let theme = cx.theme();
        let (icon, color) = match &tab.data.state {
            ResultState::Running { .. } => (gpui_kit::assets::IconName::LoaderCircle, None),
            ResultState::Failed { .. } => (
                gpui_kit::assets::IconName::TriangleAlert,
                Some(theme.danger),
            ),
            _ if tab.pinned => (gpui_kit::assets::IconName::Pin, Some(theme.foreground)),
            _ => (gpui_kit::assets::IconName::Table, None),
        };
        let mut tooltip = format!(
            "{}\nRan on {}",
            title::preview(&tab.data.statement, 240),
            tab.data.connection
        );
        if tab.pinned {
            tooltip.push_str("\nPinned: the next run keeps it");
        }
        let mut strip = StripTab::new(title, Icon::new(icon))
            .tooltip(tooltip)
            .closable(("close-result", id.raw()), "Close");
        if let Some(color) = color {
            strip = strip.icon_color(color);
        }
        let results = cx.entity().downgrade();
        strip
            .render(
                move |_, cx| {
                    let _ = results.update(cx, |this, cx| this.close(id, cx));
                },
                cx,
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, _| this.menu_target = Some(View::Result(id))),
            )
    }

    fn render_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let titles = self.tabs.titles(self.owner);
        let views: Vec<View> = std::iter::once(View::Output)
            .chain(titles.iter().map(|(id, _)| View::Result(*id)))
            .collect();
        let selected = views.iter().position(|&v| v == self.view()).unwrap_or(0);
        let mut tabs = vec![self.render_output_tab(cx)];
        tabs.extend(
            titles
                .into_iter()
                .map(|(id, title)| self.render_result_tab(id, title, cx)),
        );
        let results = cx.entity().downgrade();
        div()
            .id("result-strip")
            .w_full()
            // A right click anywhere forgets the last tab right-clicked; the
            // tab under it, if any, sets itself after this.
            .capture_any_mouse_down(cx.listener(|this, _, _, _| this.menu_target = None))
            .context_menu(move |menu, _, cx| strip_menu(menu, &results, cx))
            .child(
                tab_strip::bar("result-tabs")
                    .selected_index(selected)
                    .children(tabs)
                    .on_click(cx.listener(move |this, ix: &usize, _, cx| {
                        if let Some(&view) = views.get(*ix) {
                            this.select(view, cx);
                        }
                    })),
            )
    }

    fn render_output(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger, mono) = (
            theme.muted_foreground,
            theme.danger,
            theme.mono_font_family.clone(),
        );
        let hover = theme.list_hover;
        if self.tabs.output_len(self.owner) == 0 {
            return v_flex()
                .id("output")
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .text_sm()
                .text_color(muted)
                .child("Run a query to see results")
                .child(
                    div()
                        .text_xs()
                        .child("⌘↵ runs the statement at the caret, ⇧⌘↵ the whole script"),
                );
        }
        let mut lines = Vec::with_capacity(self.tabs.output_len(self.owner));
        for (ix, line) in self.tabs.output(self.owner).enumerate() {
            let result = line.result.filter(|&id| self.tabs.get(id).is_some());
            lines.push(self.render_line(
                ix,
                line,
                result,
                (muted, danger, hover),
                mono.clone(),
                cx,
            ));
        }
        v_flex()
            .id("output")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.output_scroll)
            .p_2()
            .gap_0p5()
            .children(lines)
    }

    fn render_line(
        &self,
        ix: usize,
        line: &OutputLine,
        result: Option<ResultTabId>,
        (muted, danger, hover): (gpui_kit::Hsla, gpui_kit::Hsla, gpui_kit::Hsla),
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let tone = match line.tone {
            Tone::Normal => cx.theme().foreground,
            Tone::Muted => muted,
            Tone::Error => danger,
        };
        let detail = line.detail.clone();
        let copy = detail.clone();
        v_flex()
            .id(("output-line", ix))
            .w_full()
            .px_2()
            .py_0p5()
            .rounded(cx.theme().radius)
            .when_some(result, |this, id| {
                this.cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.select(View::Result(id), cx)
                    }))
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .text_sm()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(muted)
                            .font_family(mono.clone())
                            .child(clock(line.at)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(mono.clone())
                            .child(line.statement.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(tone)
                            .child(line.message.clone()),
                    ),
            )
            .when_some(detail, |this, detail| {
                this.child(
                    div()
                        .pl(px(68.))
                        .pt_0p5()
                        .text_sm()
                        .text_color(danger)
                        .font_family(mono)
                        .whitespace_normal()
                        .child(detail),
                )
            })
            .context_menu(move |menu, _, _| match copy.clone() {
                Some(message) => menu.item(PopupMenuItem::new("Copy Message").on_click(
                    move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(message.clone()))
                    },
                )),
                None => menu,
            })
    }

    fn render_result(&self, id: ResultTabId, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(tab) = self.tabs.get(id) else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let grid = || {
            DataTable::new(&tab.data.table)
                .stripe(true)
                .bordered(false)
                .small()
        };
        match &tab.data.state {
            ResultState::Released { .. } => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .child("This result was let go of to keep memory in check.")
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "Results take up to {} GB together; past that, the ones \
                             shown least recently go first.",
                            MEMORY_BUDGET >> 30
                        )),
                )
                .child(
                    Button::new(("rerun-released", id.raw()))
                        .icon(Icon::new(gpui_kit::assets::IconName::Play))
                        .label("Run Again")
                        .small()
                        .on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| this.rerun(id, cx)),
                        ),
                )
                .into_any_element(),
            ResultState::Failed { message, .. } => v_flex()
                .size_full()
                .child(
                    h_flex()
                        .px_3()
                        .py_1p5()
                        .gap_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .text_sm()
                        .text_color(theme.danger)
                        .child(Icon::new(gpui_kit::assets::IconName::TriangleAlert).small())
                        .child(
                            div()
                                .font_family(theme.mono_font_family.clone())
                                .whitespace_normal()
                                .child(message.clone()),
                        ),
                )
                .child(div().flex_1().min_h_0().child(grid()))
                .into_any_element(),
            _ => grid().into_any_element(),
        }
    }
}

/// The strip's context menu, for the tab right-clicked.
fn strip_menu(menu: PopupMenu, results: &WeakEntity<Results>, cx: &App) -> PopupMenu {
    let Some(entity) = results.upgrade() else {
        return menu;
    };
    let this = entity.read(cx);
    let on = |run: fn(&mut Results, &mut Window, &mut Context<Results>)| {
        let entity = entity.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            entity.update(cx, |this, cx| run(this, window, cx))
        }
    };
    let close_unpinned = PopupMenuItem::new("Close Unpinned Tabs")
        .on_click(on(|this, _, cx| this.close_unpinned(cx)));
    match this.menu_target {
        Some(View::Result(id)) if this.tabs.get(id).is_some() => {
            let pinned = this.tabs.get(id).is_some_and(|t| t.pinned);
            let with = |run: fn(&mut Results, ResultTabId, &mut Window, &mut Context<Results>)| {
                let entity = entity.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    entity.update(cx, |this, cx| run(this, id, window, cx))
                }
            };
            menu.item(
                PopupMenuItem::new("Rerun").on_click(with(|this, id, _, cx| this.rerun(id, cx))),
            )
            .separator()
            .item(
                PopupMenuItem::new("Rename…").on_click(with(|this, id, window, cx| {
                    this.prompt_rename(id, window, cx)
                })),
            )
            .item(
                PopupMenuItem::new(if pinned { "Unpin Tab" } else { "Pin Tab" }).on_click(with(
                    |this, id, _, cx| {
                        let pinned = this.tabs.get(id).is_some_and(|t| t.pinned);
                        this.set_pinned(id, !pinned, cx)
                    },
                )),
            )
            .separator()
            .item(PopupMenuItem::new("Close").on_click(with(|this, id, _, cx| this.close(id, cx))))
            .item(
                PopupMenuItem::new("Close Other Tabs")
                    .on_click(with(|this, id, _, cx| this.close_others(id, cx))),
            )
            .item(close_unpinned)
        }
        Some(View::Output) => menu
            .item(
                PopupMenuItem::new("Clear Output")
                    .on_click(on(|this, _, cx| this.clear_output(cx))),
            )
            .separator()
            .item(close_unpinned),
        _ => menu.item(close_unpinned),
    }
}

impl Render for Results {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.view() {
            View::Output => self.render_output(cx).into_any_element(),
            View::Result(id) => self.render_result(id, cx).into_any_element(),
        };
        v_flex()
            .size_full()
            .child(self.render_strip(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}
