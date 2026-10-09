mod agenda;
mod app;
mod assets;
mod backup;
mod code;
mod commands;
mod config;
mod display;
mod editor;
mod embed;
mod export;
mod graph;
mod graph_view;
mod hotkeys;
mod model;
mod search;
mod state;
mod storage;
mod table;
mod tabs;
mod ui;
mod vim;

use app::NoteSec;
use gpui::{px, size, App, AppContext, Bounds, WindowBounds, WindowOptions};
use gpui_platform::application;
use storage::Storage;

fn main() {
    let root = Storage::default_root();
    let storage = Storage::open(root.clone()).unwrap_or_else(|err| {
        eprintln!("notesec: cannot open graph at {}: {err}", root.display());
        std::process::exit(1);
    });

    let config = config::Config::load(storage.root());

    application().run(move |cx: &mut App| {
        app::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // Wayland compositors (and X11 window managers) match this to
                // the `.desktop` file's StartupWMClass / name to pick the
                // launcher icon.
                app_id: Some("notesec".into()),
                ..Default::default()
            },
            // The closure builds the root view entity.
            |window, cx| cx.new(|cx| NoteSec::new(storage, config, window, cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}
