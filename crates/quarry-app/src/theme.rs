//! Quarry's look on top of GPUI Kit's default light and dark themes, set in
//! one place and kept across a switch between them.
//!
//! The chrome stays neutral grey, as shadcn's neutral palette has it: the
//! bars share one surface a step off the panels, tabs and list selections
//! are grey, and color is left to what it means (success and failure, the
//! grid's selection and find matches, and the SQL highlighting, which keeps
//! its own theme).

use gpui_kit::component::{Theme, ThemeColor, ThemeMode};
use gpui_kit::{App, Hsla, Window, rgb};

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
        let palette = if theme.mode.is_dark() { DARK } else { LIGHT };
        palette.apply(&mut theme.colors);
    });
}

/// The colors Quarry sets over the default theme, as `0xRRGGBB`.
struct Palette {
    /// The panels: the editor, the grid, the sidebar, dialogs.
    panel: u32,
    /// The bars around them: the title bar, the tab strips, the status bar,
    /// and the grid's header and row numbers.
    bar: u32,
    /// Text on a bar that is not the active thing: inactive tabs.
    bar_text: u32,
    /// Text that is: the active tab.
    text: u32,
    /// A selected row in a list or tree (connections, the schema, history).
    selected: u32,
}

const LIGHT: Palette = Palette {
    panel: 0xffffff,
    bar: 0xf5f5f5,
    bar_text: 0x737373,
    text: 0x0a0a0a,
    selected: 0xebebeb,
};

const DARK: Palette = Palette {
    panel: 0x0a0a0a,
    bar: 0x171717,
    bar_text: 0xa3a3a3,
    text: 0xfafafa,
    selected: 0x262626,
};

impl Palette {
    fn apply(&self, colors: &mut ThemeColor) {
        let color = |hex: u32| -> Hsla { rgb(hex).into() };
        // The active tab takes the panel's color and covers the bar's bottom
        // border, so it reads as the top of the panel under it; the others
        // sit on the bar in muted text. The same in both strips.
        colors.tab_bar = color(self.bar);
        colors.tab_active = color(self.panel);
        colors.tab_foreground = color(self.bar_text);
        colors.tab_active_foreground = color(self.text);
        colors.title_bar = color(self.bar);
        colors.status_bar = color(self.bar);
        colors.table_head = color(self.bar);
        colors.list_active = color(self.selected);
        colors.list_active_border = color(self.selected);
    }
}
