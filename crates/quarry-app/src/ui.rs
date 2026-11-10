//! The sizes and small building blocks the window's chrome shares, so the
//! dialogs, bars and panes line up and look alike.

use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    App, Div, IntoElement, ParentElement as _, Pixels, SharedString, Styled as _, div,
    prelude::FluentBuilder as _, px,
};

/// The height of every bar: the tab strips, the toolbar, the sidebar's
/// section headers, and the filter and find bars over a grid. Controls in a
/// bar are small (24px) buttons and inputs, or extra-small (20px) icon
/// buttons.
pub const BAR_HEIGHT: Pixels = px(32.);

/// The height of the status bar.
pub const STATUS_HEIGHT: Pixels = px(24.);

/// A bar: one row of controls, [`BAR_HEIGHT`] tall, with the panel divider
/// under it.
pub fn bar(cx: &App) -> Div {
    h_flex()
        .flex_shrink_0()
        .w_full()
        .h(BAR_HEIGHT)
        .px_2()
        .gap_1()
        .border_b_1()
        .border_color(cx.theme().border)
}

/// A sidebar section header: its name in small muted text, with room for
/// the section's buttons after it.
pub fn section_header(label: impl Into<SharedString>, cx: &App) -> Div {
    bar(cx).pl_3().child(
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_xs()
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(label.into()),
    )
}

/// Muted text in a bar: a count, a state.
pub fn bar_text(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex_shrink_0()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// A small quiet chip in a bar, such as the sort a grid is under: give it
/// its text and an extra-small button to clear it.
pub fn chip(cx: &App) -> Div {
    h_flex()
        .flex_shrink_0()
        .h(px(24.))
        .pl_2()
        .gap_0p5()
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .text_xs()
        .text_color(cx.theme().foreground)
}

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
