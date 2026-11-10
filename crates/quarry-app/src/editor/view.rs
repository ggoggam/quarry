//! Drawing the tab strip and the editor, and the actions, key bindings and
//! menu items that drive them.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Editor;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};
use gpui_kit::{
    Anchor, App, ClickEvent, Context, Entity, Focusable as _, InteractiveElement, IntoElement,
    KeyBinding, MenuItem, MouseButton, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _,
};
use quarry_core::{Dialect, Flavor};

use super::{Editors, TabKind};
use crate::primary;

gpui_kit::actions!(
    quarry,
    [
        NewConsole,
        NewScratch,
        NewSqlFile,
        OpenSqlFile,
        Save,
        SaveAs,
        CloseTab,
        ReopenClosedTab,
        NextTab,
        PreviousTab,
        SelectTab1,
        SelectTab2,
        SelectTab3,
        SelectTab4,
        SelectTab5,
        SelectTab6,
        SelectTab7,
        SelectTab8,
        SelectLastTab,
    ]
);

/// The key context around the editor.
pub const EDITOR_CONTEXT: &str = "SqlEditor";

/// Bind the editor's keys in `context`, the window's.
pub fn init(context: &str, cx: &mut App) {
    let context = Some(context);
    cx.bind_keys([
        KeyBinding::new(primary!("t"), NewConsole, context),
        KeyBinding::new(primary!("shift-n"), NewScratch, context),
        KeyBinding::new(primary!("n"), NewSqlFile, context),
        KeyBinding::new(primary!("alt-o"), OpenSqlFile, context),
        KeyBinding::new(primary!("s"), Save, context),
        KeyBinding::new(primary!("shift-s"), SaveAs, context),
        KeyBinding::new(primary!("w"), CloseTab, context),
        KeyBinding::new(primary!("shift-t"), ReopenClosedTab, context),
        KeyBinding::new(primary!("1"), SelectTab1, context),
        KeyBinding::new(primary!("2"), SelectTab2, context),
        KeyBinding::new(primary!("3"), SelectTab3, context),
        KeyBinding::new(primary!("4"), SelectTab4, context),
        KeyBinding::new(primary!("5"), SelectTab5, context),
        KeyBinding::new(primary!("6"), SelectTab6, context),
        KeyBinding::new(primary!("7"), SelectTab7, context),
        KeyBinding::new(primary!("8"), SelectTab8, context),
        KeyBinding::new(primary!("9"), SelectLastTab, context),
    ]);
    // ⇧⌘] and ⇧⌘[ as on macOS (the shifted key may arrive as `}`), or
    // Ctrl-PageDown and Ctrl-PageUp as on Linux; Ctrl-Tab on both.
    let (next, previous): (&[&str], &[&str]) = if cfg!(target_os = "macos") {
        (&["cmd-shift-]", "cmd-}"], &["cmd-shift-[", "cmd-{"])
    } else {
        (&["ctrl-pagedown"], &["ctrl-pageup"])
    };
    for &key in next.iter().chain(["ctrl-tab"].iter()) {
        cx.bind_keys([KeyBinding::new(key, NextTab, context)]);
    }
    for &key in previous.iter().chain(["ctrl-shift-tab"].iter()) {
        cx.bind_keys([KeyBinding::new(key, PreviousTab, context)]);
    }
}

/// The editor's File menu items.
pub fn menu_items() -> Vec<MenuItem> {
    vec![
        MenuItem::action("New Console", NewConsole),
        MenuItem::action("New SQL File…", NewSqlFile),
        MenuItem::action("New Scratch", NewScratch),
        MenuItem::separator(),
        MenuItem::action("Open SQL File…", OpenSqlFile),
        MenuItem::separator(),
        MenuItem::action("Save", Save),
        MenuItem::action("Save As…", SaveAs),
        MenuItem::separator(),
        MenuItem::action("Close Tab", CloseTab),
        MenuItem::action("Reopen Closed Tab", ReopenClosedTab),
        MenuItem::separator(),
        MenuItem::action("Show Next Tab", NextTab),
        MenuItem::action("Show Previous Tab", PreviousTab),
    ]
}

/// Handle the editor's actions on `element`, the window's root, so they
/// work wherever the focus is.
pub fn bind_actions<E: InteractiveElement>(element: E, editors: &Entity<Editors>) -> E {
    macro_rules! on {
        ($element:expr, $action:ty, |$this:ident, $window:ident, $cx:ident| $body:expr) => {{
            let editors = editors.clone();
            $element.on_action(move |_: &$action, $window: &mut Window, cx: &mut App| {
                editors.update(cx, |$this, $cx| $body)
            })
        }};
    }
    let element = on!(element, NewConsole, |this, window, cx| this
        .new_console(window, cx));
    let element = on!(element, NewScratch, |this, window, cx| this
        .new_scratch(window, cx));
    let element = on!(element, NewSqlFile, |this, window, cx| this
        .prompt_new_file(window, cx));
    let element = on!(element, OpenSqlFile, |this, window, cx| this
        .prompt_open(window, cx));
    let element = on!(element, Save, |this, window, cx| this
        .save_active(window, cx));
    let element = on!(element, SaveAs, |this, window, cx| this
        .save_active_as(window, cx));
    let element = on!(element, CloseTab, |this, window, cx| this
        .close_active(window, cx));
    let element = on!(element, ReopenClosedTab, |this, window, cx| this
        .reopen_closed(window, cx));
    let element = on!(element, NextTab, |this, window, cx| this
        .next_tab(1, window, cx));
    let element = on!(element, PreviousTab, |this, window, cx| this
        .next_tab(-1, window, cx));
    let element = on!(element, SelectTab1, |this, window, cx| this.select_tab(
        Some(0),
        window,
        cx
    ));
    let element = on!(element, SelectTab2, |this, window, cx| this.select_tab(
        Some(1),
        window,
        cx
    ));
    let element = on!(element, SelectTab3, |this, window, cx| this.select_tab(
        Some(2),
        window,
        cx
    ));
    let element = on!(element, SelectTab4, |this, window, cx| this.select_tab(
        Some(3),
        window,
        cx
    ));
    let element = on!(element, SelectTab5, |this, window, cx| this.select_tab(
        Some(4),
        window,
        cx
    ));
    let element = on!(element, SelectTab6, |this, window, cx| this.select_tab(
        Some(5),
        window,
        cx
    ));
    let element = on!(element, SelectTab7, |this, window, cx| this.select_tab(
        Some(6),
        window,
        cx
    ));
    let element = on!(element, SelectTab8, |this, window, cx| this.select_tab(
        Some(7),
        window,
        cx
    ));
    on!(element, SelectLastTab, |this, window, cx| this
        .select_tab(None, window, cx))
}

impl Editors {
    /// The active tab's editor, read-only while its text loads.
    pub fn editor_element(&self) -> impl IntoElement {
        let buffer = self.active_buffer();
        div().size_full().key_context(EDITOR_CONTEXT).child(
            Editor::new(&buffer.editor)
                .bordered(false)
                .readonly(buffer.loading)
                .h_full(),
        )
    }

    fn render_tab(&self, id: super::TabId, title: String, cx: &mut Context<Self>) -> Tab {
        let tab = self.tabs.get(id).expect("titles come from the tabs");
        let muted = cx.theme().muted_foreground;
        let (icon, tooltip): (_, SharedString) = match &tab.kind {
            TabKind::Console { .. } => (
                gpui_kit::assets::IconName::SquareTerminal,
                format!("Console on {}", self.connection_label).into(),
            ),
            TabKind::Scratch { .. } => (
                gpui_kit::assets::IconName::NotepadText,
                self.store
                    .path_of(&tab.kind)
                    .map_or_else(|| "Scratch".into(), |p| p.display().to_string().into()),
            ),
            TabKind::File { path } => (
                gpui_kit::assets::IconName::FileCode,
                path.display().to_string().into(),
            ),
        };
        let dirty = tab.buffer.dirty && !tab.kind.is_autosaved();
        Tab::new()
            .label(title)
            .prefix(
                div()
                    .pl_2()
                    .child(Icon::new(icon).xsmall().text_color(muted)),
            )
            .suffix(
                h_flex()
                    .pr_1()
                    .gap_0p5()
                    .when(dirty, |this| {
                        this.child(div().text_xs().text_color(cx.theme().foreground).child("●"))
                    })
                    .child(
                        Button::new(("close-tab", id.0))
                            .icon(IconName::Close)
                            .xsmall()
                            .ghost()
                            .tooltip(if dirty {
                                "Close (unsaved changes)"
                            } else {
                                "Close (⌘W)"
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.close(id, window, cx)
                            })),
                    ),
            )
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, window, cx| this.close(id, window, cx)),
            )
    }

    /// The "+" menu: what can be opened in a new tab.
    fn render_new_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.active_editor().focus_handle(cx);
        Button::new("new-tab")
            .icon(IconName::Plus)
            .xsmall()
            .ghost()
            .tooltip("New console, scratch or file")
            .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, _| {
                menu.action_context(focus.clone())
                    .menu("New Console", Box::new(NewConsole))
                    .menu("New Scratch", Box::new(NewScratch))
                    .menu("New SQL File…", Box::new(NewSqlFile))
                    .separator()
                    .menu("Open SQL File…", Box::new(OpenSqlFile))
                    .menu("Reopen Closed Tab", Box::new(ReopenClosedTab))
            })
    }
}

impl Render for Editors {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let titles = self.tabs.titles();
        let ids: Vec<_> = titles.iter().map(|(id, _)| *id).collect();
        let tabs: Vec<Tab> = titles
            .into_iter()
            .map(|(id, title)| self.render_tab(id, title, cx))
            .collect();
        TabBar::new("editor-tabs")
            .small()
            .menu(true)
            .max_width(gpui_kit::px(220.))
            .when_some(self.tabs.active_index(), |bar, ix| bar.selected_index(ix))
            .children(tabs)
            .on_click(cx.listener(move |this, ix: &usize, window, cx| {
                if let Some(&id) = ids.get(*ix) {
                    this.activate(id, window, cx);
                }
            }))
            .suffix(div().px_1().child(self.render_new_menu(cx)))
    }
}

/// The active tab's SQL dialect and release, like DataGrip's per-console
/// dialect: it follows the connection unless the user picks one, e.g. to
/// write for PostgreSQL 13 while connected to PostgreSQL 17.
pub fn flavor_picker(editors: &Entity<Editors>, cx: &App) -> impl IntoElement {
    let this = editors.read(cx);
    let flavor = this.active_flavor();
    let connection = this.connection_flavor().clone();
    let picked = this.picked_flavor().cloned();
    let differs = picked
        .as_ref()
        .is_some_and(|p| p.dialect != connection.dialect);
    let tooltip: SharedString = match &picked {
        None => format!("Writing {} (from the connection)", connection.label()),
        Some(p) if differs => format!(
            "Writing {}; queries still run on {}",
            p.label(),
            connection.label()
        ),
        Some(p) => format!("Writing {}; connected to {}", p.label(), connection.label()),
    }
    .into();
    let editors = editors.downgrade();
    Button::new("flavor")
        .label(flavor.label())
        .icon(Icon::new(gpui_kit::assets::IconName::Database))
        .small()
        .ghost()
        .when(differs, |b| b.text_color(cx.theme().warning))
        .tooltip(tooltip)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
            let owner = editors.clone();
            let pick = move |choice: Option<Flavor>| {
                let editors = owner.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ =
                        editors.update(cx, |this, cx| this.set_picked_flavor(choice.clone(), cx));
                }
            };
            let mut menu = menu.item(
                PopupMenuItem::new(format!("Auto: {}", connection.label()))
                    .checked(picked.is_none())
                    .on_click(pick(None)),
            );
            menu = menu.separator();
            for dialect in Dialect::ALL {
                let picked = picked.clone();
                let pick = pick.clone();
                menu = menu.submenu(dialect.name(), window, cx, move |mut sub, _, _| {
                    let latest = Flavor::unversioned(dialect);
                    sub = sub.item(
                        PopupMenuItem::new("Latest")
                            .checked(picked.as_ref() == Some(&latest))
                            .on_click(pick(Some(latest))),
                    );
                    for release in dialect.releases().iter().rev() {
                        let flavor = Flavor::release(dialect, release);
                        sub = sub.item(
                            PopupMenuItem::new(release.label)
                                .checked(picked.as_ref() == Some(&flavor))
                                .on_click(pick(Some(flavor))),
                        );
                    }
                    sub
                });
            }
            menu
        })
}
