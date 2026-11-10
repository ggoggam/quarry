//! Quarry's icons: the Lucide set from gpui-icons.
//!
//! Every icon Quarry draws goes through [`icon`], which points a GPUI Kit
//! [`Icon`] at the glyph's path in gpui-icons' asset namespace. Keeping the
//! `Icon` type rather than a bare SVG means buttons and tabs size it the way
//! they size their own icons, `.xsmall()` and `.small()` work beside text, and
//! it takes the surrounding text color (a button's hover and disabled states,
//! a danger line's red) unless it is given one. The widgets' own icons
//! (checkboxes, dropdown carets, dialog close buttons) still come from GPUI
//! Kit's bundle: see `assets.rs`.

pub use gpui_icons::LucideIcon;
use gpui_kit::component::Icon;

/// The Lucide `glyph` as an icon, ready for `Button::icon` or a row.
pub fn icon(glyph: LucideIcon) -> Icon {
    Icon::empty().path(glyph.asset_path())
}
