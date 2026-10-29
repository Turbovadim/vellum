#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod assets;
mod document;
mod fonts;
mod theme;
mod viewer;

use std::path::PathBuf;

use gpui::{
    App, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions, point,
    prelude::*, px, size,
};
use gpui_platform::application;

use crate::viewer::*;

/// Cmd on macOS, Ctrl elsewhere.
macro_rules! primary {
    ($key:literal) => {
        if cfg!(target_os = "macos") {
            concat!("cmd-", $key)
        } else {
            concat!("ctrl-", $key)
        }
    };
}

fn key_bindings() -> Vec<KeyBinding> {
    const APP: Option<&str> = Some("VellumApp");
    const VIEWER: Option<&str> = Some("Viewer");

    let bindings = vec![
        KeyBinding::new(primary!("o"), Open, APP),
        KeyBinding::new(primary!("w"), CloseWindow, APP),
        KeyBinding::new(primary!("q"), Quit, APP),
        KeyBinding::new(primary!("="), ZoomIn, APP),
        KeyBinding::new(primary!("+"), ZoomIn, APP),
        KeyBinding::new(primary!("shift-="), ZoomIn, APP),
        KeyBinding::new(primary!("-"), ZoomOut, APP),
        KeyBinding::new(primary!("0"), ActualSize, APP),
        KeyBinding::new(primary!("9"), ZoomToFit, APP),
        KeyBinding::new(primary!("l"), RotateLeft, APP),
        KeyBinding::new(primary!("r"), RotateRight, APP),
        KeyBinding::new(primary!("alt-1"), HideSidebar, APP),
        KeyBinding::new(primary!("alt-2"), ShowThumbnails, APP),
        KeyBinding::new(primary!("alt-3"), ShowTableOfContents, APP),
        KeyBinding::new(primary!("alt-s"), ToggleSidebar, APP),
        KeyBinding::new(primary!("f"), Find, APP),
        KeyBinding::new(primary!("g"), FindNext, APP),
        KeyBinding::new(primary!("shift-g"), FindPrevious, APP),
        KeyBinding::new("f3", FindNext, APP),
        KeyBinding::new("shift-f3", FindPrevious, APP),
        KeyBinding::new(primary!("i"), ToggleInspector, APP),
        KeyBinding::new(primary!("alt-g"), GoToPage, APP),
        KeyBinding::new(primary!("1"), ContinuousScroll, APP),
        KeyBinding::new(primary!("2"), SinglePage, APP),
        KeyBinding::new(primary!("3"), TwoPages, APP),
        KeyBinding::new(primary!("c"), CopyText, APP),
        KeyBinding::new("alt-down", NextPage, APP),
        KeyBinding::new("alt-up", PreviousPage, APP),
        KeyBinding::new("escape", Dismiss, VIEWER),
        KeyBinding::new("pagedown", NextPage, VIEWER),
        KeyBinding::new("pageup", PreviousPage, VIEWER),
        KeyBinding::new("right", NextPage, VIEWER),
        KeyBinding::new("left", PreviousPage, VIEWER),
        KeyBinding::new("down", ScrollDown, VIEWER),
        KeyBinding::new("up", ScrollUp, VIEWER),
        KeyBinding::new("space", ScrollPageDown, VIEWER),
        KeyBinding::new("shift-space", ScrollPageUp, VIEWER),
        KeyBinding::new("home", FirstPage, VIEWER),
        KeyBinding::new("end", LastPage, VIEWER),
        KeyBinding::new(primary!("up"), FirstPage, VIEWER),
        KeyBinding::new(primary!("down"), LastPage, VIEWER),
    ];
    bindings
}

fn menus() -> Vec<Menu> {
    vec![
        Menu::new("Vellum").items([MenuItem::action("Quit Vellum", Quit)]),
        Menu::new("File").items([
            MenuItem::action("Open…", Open),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]),
        Menu::new("Edit").items([
            MenuItem::action("Copy", CopyText),
            MenuItem::separator(),
            MenuItem::action("Find…", Find),
            MenuItem::action("Find Next", FindNext),
            MenuItem::action("Find Previous", FindPrevious),
        ]),
        Menu::new("View").items([
            MenuItem::action("Hide Sidebar", HideSidebar),
            MenuItem::action("Thumbnails", ShowThumbnails),
            MenuItem::action("Table of Contents", ShowTableOfContents),
            MenuItem::separator(),
            MenuItem::action("Continuous Scroll", ContinuousScroll),
            MenuItem::action("Single Page", SinglePage),
            MenuItem::action("Two Pages", TwoPages),
            MenuItem::separator(),
            MenuItem::action("Actual Size", ActualSize),
            MenuItem::action("Zoom to Fit", ZoomToFit),
            MenuItem::action("Zoom In", ZoomIn),
            MenuItem::action("Zoom Out", ZoomOut),
        ]),
        Menu::new("Go").items([
            MenuItem::action("Previous Page", PreviousPage),
            MenuItem::action("Next Page", NextPage),
            MenuItem::action("First Page", FirstPage),
            MenuItem::action("Last Page", LastPage),
            MenuItem::action("Go to Page…", GoToPage),
        ]),
        Menu::new("Tools").items([
            MenuItem::action("Rotate Left", RotateLeft),
            MenuItem::action("Rotate Right", RotateRight),
            MenuItem::separator(),
            MenuItem::action("Show Inspector", ToggleInspector),
        ]),
    ]
}

fn main() {
    let initial: Option<PathBuf> = std::env::args_os().nth(1).map(PathBuf::from);

    application().with_assets(assets::Assets).run(move |cx: &mut App| {
        cx.bind_keys(key_bindings());
        cx.set_menus(menus());
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1080.), px(820.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Vellum".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(18.), px(20.))),
                }),
                window_min_size: Some(size(px(520.), px(360.))),
                app_id: Some("vellum".into()),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| VellumApp::new(window, cx));
                if let Some(path) = initial.clone() {
                    view.update(cx, |app, cx| app.open_path(path, window, cx));
                }
                view
            },
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
