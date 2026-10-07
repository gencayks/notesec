mod app;
mod editor;
mod model;
mod storage;
mod ui;

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

    application().run(move |cx: &mut App| {
        app::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            // The closure builds the root view entity.
            |_window, cx| cx.new(|cx| NoteSec::new(storage, cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}
