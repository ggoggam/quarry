//! The app's assets: the whole Lucide set from gpui-icons under
//! `icons/lucide/`, in front of GPUI Kit's bundle, whose icons its widgets
//! (checkboxes, dropdown carets, close buttons, notifications) load by path.

use std::borrow::Cow;

use gpui_icons::LucideAssetSource;
use gpui_kit::{AssetSource, Result, SharedString};

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match LucideAssetSource.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = LucideAssetSource.list(path)?;
        paths.extend(gpui_kit::assets::Assets.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icons::LucideIcon;

    /// `icons::icon` takes any `LucideIcon`, so every one has to load: a
    /// missing asset draws as an empty box, which no other test would notice.
    #[test]
    fn serves_every_lucide_icon() {
        for glyph in LucideIcon::ALL {
            let bytes = Assets
                .load(glyph.asset_path())
                .unwrap_or_else(|err| panic!("{glyph}: {err}"))
                .unwrap_or_else(|| panic!("{glyph} is missing"));
            assert_eq!(bytes.as_ref(), glyph.svg_bytes(), "{glyph}");
        }
        assert_eq!(
            Assets.list("icons/lucide/").unwrap().len(),
            LucideIcon::ALL.len()
        );
    }

    /// GPUI Kit's widgets load their own icons by path, from its bundle.
    #[test]
    fn still_serves_the_widgets_icons() {
        let paths = gpui_kit::assets::Assets.list("icons/").unwrap();
        assert!(paths.iter().any(|path| path == "icons/chevron-down.svg"));
        for path in &paths {
            assert!(
                Assets.load(path).unwrap().is_some_and(|b| !b.is_empty()),
                "{path} is missing"
            );
        }
        assert!(Assets.list("icons/").unwrap().len() >= paths.len());
    }
}
