//! The look the editor tabs and the result tabs share: a small tab bar of
//! tabs with an icon, a title cut at a fixed width, a marker (unsaved, pinned)
//! and a close button, closed by a middle click too.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};
use gpui_kit::{
    App, ClickEvent, ElementId, Hsla, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};

/// The widest a tab grows before its title is cut.
pub const MAX_TAB_WIDTH: f32 = 220.;

/// A tab bar in the shared look.
pub fn bar(id: impl Into<ElementId>) -> TabBar {
    TabBar::new(id)
        .small()
        .menu(true)
        .max_width(px(MAX_TAB_WIDTH))
}

/// What a tab shows besides its title.
pub struct StripTab {
    pub title: String,
    pub icon: Icon,
    /// The icon's color; muted when `None`.
    pub icon_color: Option<Hsla>,
    pub tooltip: SharedString,
    /// Drawn before the close button: the unsaved dot, a pin.
    pub marker: Option<gpui_kit::AnyElement>,
    /// The close button's id and tooltip; no button when `None`.
    pub close: Option<(ElementId, SharedString)>,
}

impl StripTab {
    pub fn new(title: impl Into<String>, icon: impl Into<Icon>) -> Self {
        Self {
            title: title.into(),
            icon: icon.into(),
            icon_color: None,
            tooltip: SharedString::default(),
            marker: None,
            close: None,
        }
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = tooltip.into();
        self
    }

    pub fn icon_color(mut self, color: Hsla) -> Self {
        self.icon_color = Some(color);
        self
    }

    pub fn marker(mut self, marker: impl IntoElement) -> Self {
        self.marker = Some(marker.into_any_element());
        self
    }

    pub fn closable(mut self, id: impl Into<ElementId>, tooltip: impl Into<SharedString>) -> Self {
        self.close = Some((id.into(), tooltip.into()));
        self
    }

    /// The tab; `on_close` runs from its close button and a middle click.
    pub fn render(
        self,
        on_close: impl Fn(&mut Window, &mut App) + Clone + 'static,
        cx: &App,
    ) -> Tab {
        let muted = cx.theme().muted_foreground;
        let tooltip = self.tooltip;
        let closable = self.close.is_some();
        let close_click = on_close.clone();
        Tab::new()
            .label(self.title)
            .prefix(
                div().pl_2().child(
                    self.icon
                        .xsmall()
                        .text_color(self.icon_color.unwrap_or(muted)),
                ),
            )
            .suffix(
                h_flex()
                    .pr_1()
                    .gap_0p5()
                    .when(!closable, |this| this.pr_2())
                    .children(self.marker)
                    .when_some(self.close, |this, (id, tooltip)| {
                        this.child(
                            Button::new(id)
                                .icon(IconName::Close)
                                .xsmall()
                                .ghost()
                                .tooltip(tooltip)
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    close_click(window, cx)
                                }),
                        )
                    }),
            )
            .when(!tooltip.is_empty(), |tab| {
                tab.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            })
            .when(closable, |tab| {
                tab.on_mouse_down(
                    MouseButton::Middle,
                    move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                        on_close(window, cx)
                    },
                )
            })
    }
}

/// The dot an unsaved file's tab shows.
pub fn dirty_marker(cx: &App) -> impl IntoElement {
    div().text_xs().text_color(cx.theme().foreground).child("●")
}
