//! Icons: the component library's defaults plus the few Quarry adds.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(
    QuarryIcons,
    [
        Database,
        DatabasePlus,
        FolderOpen,
        Folder,
        Play,
        ListEnd,
        Square,
        RefreshCw,
        Sun,
        Moon,
        Table,
        Eye,
        Columns3,
        TriangleAlert,
    ]
);

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = QuarryIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(QuarryIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
