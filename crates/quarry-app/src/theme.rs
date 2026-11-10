//! Quarry's look on top of GPUI Kit's default light and dark themes, set in
//! one place and kept across a switch between them.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Window};

/// Apply Quarry's settings to the theme the system picked.
pub fn init(cx: &mut App) {
    apply(cx);
}

/// Switch to `mode`, keeping Quarry's settings.
pub fn set_mode(mode: ThemeMode, window: &mut Window, cx: &mut App) {
    Theme::change(mode, Some(window), cx);
    crate::fonts::apply(cx);
    apply(cx);
}

fn apply(cx: &mut App) {
    Theme::update(cx, |theme| {
        // Focus shows as the control's border in the ring color rather than
        // as a ring painted 3px outside it. The outside ring is cut off by
        // whatever clips around the control (a dialog's body, a bar's
        // height, a scroll view), and Quarry's dense bars and dialogs clip
        // everywhere.
        theme.focus_ring = false;
    });
}
