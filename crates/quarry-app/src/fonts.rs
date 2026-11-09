//! JetBrains Mono, embedded so the editor and grid look the same everywhere.
//! The font is under the SIL Open Font License; see
//! `assets/fonts/jetbrains-mono/OFL.txt`.

use std::borrow::Cow;

use gpui_kit::component::Theme;
use gpui_kit::{App, px};

pub const MONO: &str = "JetBrains Mono";

macro_rules! font {
    ($file:literal) => {
        Cow::Borrowed(
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../assets/fonts/jetbrains-mono/",
                $file
            ))
            .as_slice(),
        )
    };
}

pub fn init(cx: &mut App) {
    if let Err(err) = cx.text_system().add_fonts(vec![
        font!("JetBrainsMono-Regular.ttf"),
        font!("JetBrainsMono-Italic.ttf"),
        font!("JetBrainsMono-Bold.ttf"),
        font!("JetBrainsMono-BoldItalic.ttf"),
    ]) {
        log::warn!("could not load JetBrains Mono: {err:#}");
        return;
    }
    apply(cx);
}

/// Point the theme's monospace font at JetBrains Mono. Switching between
/// light and dark reloads the theme config, so call this again afterwards.
pub fn apply(cx: &mut App) {
    Theme::update(cx, |theme| {
        theme.mono_font_family = MONO.into();
        theme.mono_font_size = px(13.);
    });
}
