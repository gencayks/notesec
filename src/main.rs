mod agenda;
mod ai;
mod app;
mod assets;
mod autotag;
mod backup;
mod capture;
mod clipper;
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
mod import;
mod mentions;
mod model;
mod plugins;
mod publish;
mod search;
mod semantic;
mod state;
mod storage;
mod table;
mod tabs;
mod ui;
mod vault;
mod vim;
mod voice;
mod whiteboard;

use app::NoteSec;
use gpui::{px, size, App, AppContext, Bounds, WindowBounds, WindowOptions};
use gpui_platform::application;
use storage::Storage;

fn main() {
    let cli = match capture::parse_cli(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("notesec: {err}");
            eprintln!("usage: notesec [--notes-dir <dir>] [--capture \"text\"]");
            std::process::exit(2);
        }
    };

    // Quick capture: append the text to today's journal and exit, with no
    // window. Bind this to a system-wide shortcut (e.g. KDE Settings >
    // Shortcuts) for capture from anywhere: on Wayland an app cannot grab
    // a true global hotkey by itself.
    if let Some(text) = cli.capture {
        let storage = Storage::open(cli.root).unwrap_or_else(|err| {
            eprintln!("notesec: cannot open graph: {err}");
            std::process::exit(1);
        });
        match capture::append_to_journal(&storage, &text) {
            Ok(day) => println!("Captured to {day}"),
            Err(err) => {
                eprintln!("notesec: cannot capture: {err}");
                std::process::exit(1);
            }
        }
        return;
    }

    let root = cli.root;
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
