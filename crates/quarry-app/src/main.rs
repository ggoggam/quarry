//! Quarry: a fast, native SQL client.
//!
//! Usage: `quarry [file or connection URL] [file.sql…]`, e.g. `quarry
//! shop.duckdb`, `quarry app.sqlite` or `quarry postgres://alice@localhost/app`,
//! or the name of a saved connection. Without one Quarry opens an in-memory
//! DuckDB database. `.sql` files open in editor tabs.

mod assets;
mod connect;
mod editor;
mod fonts;
mod format;
mod highlight;
mod history;
mod icons;
mod results;
mod saved;
mod schema_tree;
mod tab_strip;
mod theme;
mod ui;
mod update;
mod workspace;

use gpui_kit::component::TitleBar;
use gpui_kit::{
    App, AppContext as _, Bounds, KeyBinding, Menu, MenuItem, WindowBounds, WindowOptions, px, size,
};
use std::path::PathBuf;

gpui_kit::actions!(quarry, [Quit]);

/// Quarry's settings folder: `~/Library/Application Support/Quarry` on
/// macOS, `~/.config/quarry` on Linux, or `QUARRY_CONFIG_DIR` when set.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("QUARRY_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let folder = if cfg!(target_os = "macos") {
        "Quarry"
    } else {
        "quarry"
    };
    Some(dirs::config_dir()?.join(folder))
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Opened from the Finder or the Dock, Quarry starts in `/`. DuckDB reads
    // relative paths (`FROM 'orders.csv'`) from here, and the editor
    // completes them from here, so make it the home folder instead.
    if std::env::current_dir().is_ok_and(|dir| dir == std::path::Path::new("/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        let _ = std::env::set_current_dir(home);
    }

    let app = gpui_kit::application().with_assets(assets::Assets);
    // Closing the window leaves Quarry running (the macOS way), so clicking
    // its Dock icon must bring a window back.
    let reopen_args = args.clone();
    app.on_reopen(move |cx| {
        if cx.windows().is_empty() {
            open_main_window(reopen_args.clone(), cx);
        }
    });
    app.run(move |cx: &mut App| {
        gpui_kit::init(cx);
        fonts::init(cx);
        theme::init(cx);
        workspace::init(cx);
        update::init(cx);
        history::init(cx);
        results::init(cx);

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new(primary!("q"), Quit, None)]);
        cx.set_menus(vec![
            Menu {
                name: "Quarry".into(),
                items: vec![
                    MenuItem::action("Check for Updates…", update::CheckForUpdates),
                    MenuItem::separator(),
                    MenuItem::action("Quit Quarry", Quit),
                ],
                disabled: false,
            },
            Menu {
                name: "File".into(),
                items: workspace::file_menu_items(),
                disabled: false,
            },
            Menu {
                name: "Query".into(),
                items: workspace::query_menu_items(),
                disabled: false,
            },
            Menu {
                name: "Result".into(),
                items: results::result_menu_items(),
                disabled: false,
            },
        ]);

        open_main_window(args.clone(), cx);
    });
}

/// Open Quarry's window on `args` and bring Quarry to the front.
fn open_main_window(args: Vec<String>, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
    let options = WindowOptions {
        titlebar: Some(TitleBar::title_bar_options()),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(720.), px(480.))),
        ..Default::default()
    };
    gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| workspace::Workspace::new(args, window, cx))
    })
    .expect("failed to open the Quarry window");
    cx.activate(true);
}
