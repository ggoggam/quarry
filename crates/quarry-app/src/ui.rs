//! The sizes and small building blocks the window's chrome shares, so the
//! dialogs, bars and panes line up and look alike.

use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, v_flex};
use gpui_kit::{
    App, Div, IntoElement, ParentElement as _, SharedString, Styled as _, div,
    prelude::FluentBuilder as _, px,
};

/// `element` at its natural width in a dialog footer. The footer's close
/// and confirm wrappers fill their parent, which in a row stretches them.
pub fn natural(element: impl IntoElement) -> Div {
    div().flex_none().child(element)
}

/// What a pane shows while it has nothing to show: an icon, a line saying
/// so, and an optional hint under it, centered in the space it is given.
pub fn empty_state(
    glyph: impl Into<Icon>,
    title: impl Into<SharedString>,
    hint: Option<SharedString>,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .p_4()
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(36.))
                .rounded(theme.radius_lg)
                .bg(theme.muted)
                .text_color(theme.muted_foreground)
                .child(glyph.into().small()),
        )
        .child(
            div()
                .text_sm()
                .font_medium()
                .text_color(theme.foreground)
                .child(title.into()),
        )
        .when_some(hint, |this, hint| {
            this.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(hint),
            )
        })
}
