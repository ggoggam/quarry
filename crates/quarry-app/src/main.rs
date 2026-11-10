//! Quarry: a fast, native SQL client.
//!
//! Usage: `quarry [file or connection URL]`, e.g. `quarry shop.duckdb`,
//! `quarry app.sqlite` or `quarry postgres://alice@localhost/app`. Without one
//! Quarry opens an in-memory DuckDB database.

mod assets;
mod connect;
mod fonts;
mod highlight;
mod results;
mod schema_tree;
mod update;
mod workspace;

use gpui_kit::component::TitleBar;
use gpui_kit::{
    App, AppContext as _, Bounds, KeyBinding, Menu, MenuItem, WindowBounds, WindowOptions, px, size,
};

gpui_kit::actions!(quarry, [Quit]);

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let target = std::env::args().nth(1);

    gpui_kit::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            fonts::init(cx);
            workspace::init(cx);
            update::init(cx);

            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([KeyBinding::new(primary!("q"), Quit, None)]);
            cx.set_menus(vec![Menu {
                name: "Quarry".into(),
                items: vec![
                    MenuItem::action("Check for Updates…", update::CheckForUpdates),
                    MenuItem::separator(),
                    MenuItem::action("Quit Quarry", Quit),
                ],
                disabled: false,
            }]);

            let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
            let options = WindowOptions {
                titlebar: Some(TitleBar::title_bar_options()),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(720.), px(480.))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| workspace::Workspace::new(target.clone(), window, cx))
            })
            .expect("failed to open the Quarry window");
            cx.activate(true);
        });
}
