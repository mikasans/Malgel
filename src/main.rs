//! Malgel — a fast, native Markdown editor and previewer.

// Keep release builds on Windows from opening a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod actions;
mod analysis;
mod commands;
mod default_app;
mod diagram;
mod diagram_viewer;
mod document;
mod document_view;
mod docx;
mod export;
mod file_icon;
mod file_tree;
mod format;
mod image_view;
mod images;
mod links;
mod math;
mod menus;
mod pdf;
pub mod pdf_view;
mod preview_ext;
mod quick_open;
mod rich_copy;
mod search;
mod session;
mod settings;
mod smart_edit;
mod spell;
mod themes;
mod typst_preview;
mod typst_world;
mod workspace;
mod zoom;

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;
use std::{path::PathBuf, sync::Arc};

use futures::StreamExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Global, WeakEntity, WindowBounds, WindowKind,
    WindowOptions, component::TitleBar, px, size,
};

use crate::{actions::*, settings::Settings, workspace::Workspace};

const MARKDOWN_GUIDE_URL: &str = "https://commonmark.org/help/";

/// The window's workspace, for files the system asks Malgel to open.
struct MainWindow {
    handle: AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
}

impl Global for MainWindow {}

struct StdErrLogger;
impl log::Log for StdErrLogger {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.level() <= log::Level::Trace
    }
    fn log(&self, record: &log::Record) {
        eprintln!(
            "[{}::{}] {}",
            record.level(),
            record.target(),
            record.args()
        );
    }
    fn flush(&self) {}
}

fn main() {
    let _ = log::set_boxed_logger(Box::new(StdErrLogger));
    let level = match std::env::var("RUST_LOG") {
        Ok(_) => log::LevelFilter::Trace,
        Err(_) => log::LevelFilter::Warn,
    };
    log::set_max_level(level);
    let paths: Vec<PathBuf> = std::env::args_os()
        .skip(1)
        .filter(|arg| !arg.to_string_lossy().starts_with('-'))
        .map(PathBuf::from)
        .map(|path| path.canonicalize().unwrap_or(path))
        .collect();

    // A second launch hands its arguments to the running instance and exits,
    // the way VS Code wakes the existing window.
    if forward_to_running_instance(&paths) {
        return;
    }

    // macOS delivers "Open with" and double-clicked files as URLs, outside
    // the app's context; they are handed over through a channel.
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<Vec<PathBuf>>();
    {
        let cli_tx = open_tx.clone();
        std::thread::spawn(move || listen_for_cli(cli_tx));
    }
    let application = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);
    application.on_open_urls(move |urls| {
        let paths: Vec<PathBuf> = urls
            .iter()
            .filter_map(|url| url::Url::parse(url).ok()?.to_file_path().ok())
            .collect();
        if !paths.is_empty() {
            let _ = open_tx.unbounded_send(paths);
        }
    });

    application.run(move |cx| {
        gpui_kit::init(cx);
        match reqwest_client::ReqwestClient::user_agent(concat!(
            "Malgel/",
            env!("CARGO_PKG_VERSION")
        )) {
            Ok(client) => cx.set_http_client(Arc::new(client)),
            Err(err) => eprintln!("malgel: remote images are unavailable: {err}"),
        }

        themes::init(Settings::load(), cx);
        // Bind keys before building menus so the menus show the shortcuts.
        actions::bind_keys(cx);
        diagram_viewer::bind_keys(cx);
        let app_menu_bar = menus::init(cx);

        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.on_action(|_: &OpenMarkdownGuide, cx: &mut App| cx.open_url(MARKDOWN_GUIDE_URL));
        cx.on_action(|action: &SetAppearance, cx: &mut App| {
            let appearance = action.0;
            themes::AppSettings::update(cx, |settings| settings.appearance = appearance);
        });
        cx.on_action(|action: &SetTheme, cx: &mut App| themes::select_theme(&action.0, cx));
        cx.on_action(|_: &About, cx: &mut App| with_window(cx, workspace::open_about));
        cx.on_action(|_: &OpenSettings, cx: &mut App| with_window(cx, workspace::open_settings));

        // Malgel is a single-window app: closing the window ends it on
        // every platform, as the menu offers no way to open a new one.
        // Diagram windows belong to it and close with it.
        cx.on_window_closed(|cx, _| {
            let main_open = cx.try_global::<MainWindow>().is_some_and(|main| {
                cx.windows()
                    .iter()
                    .any(|window| window.window_id() == main.handle.window_id())
            });
            if !main_open {
                diagram_viewer::close_all(cx);
                cx.quit();
            }
        })
        .detach();

        let menu_bar = app_menu_bar.clone();
        cx.spawn(async move |cx| {
            while let Some(paths) = open_rx.next().await {
                let menu_bar = menu_bar.clone();
                cx.update(|cx| open_paths(paths, menu_bar, cx));
            }
        })
        .detach();

        open_window(paths, app_menu_bar, cx);
        cx.activate(true);
    });
}

/// Open files in the window, or in a new one if it was closed.
fn open_paths(
    paths: Vec<PathBuf>,
    app_menu_bar: gpui_kit::Entity<gpui_kit::component::menu::AppMenuBar>,
    cx: &mut App,
) {
    if paths.is_empty() {
        if let Some(main) = cx.try_global::<MainWindow>() {
            _ = main
                .handle
                .update(cx, |_, window, _| window.activate_window());
        }
        return;
    }
    if let Some(main) = cx.try_global::<MainWindow>() {
        let (handle, workspace) = (main.handle, main.workspace.clone());
        let opened = handle.update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_external(paths.clone(), window, cx)
            })
        });
        if matches!(opened, Ok(Ok(()))) {
            _ = handle.update(cx, |_, window, _| window.activate_window());
            return;
        }
    }
    open_window(paths, app_menu_bar, cx);
}

fn open_window(
    paths: Vec<PathBuf>,
    app_menu_bar: gpui_kit::Entity<gpui_kit::component::menu::AppMenuBar>,
    cx: &mut App,
) {
    let window_bounds = workspace::restored_bounds(cx).unwrap_or_else(|| {
        let mut window_size = size(px(1280.), px(840.));
        if let Some(display) = cx.primary_display() {
            let display_size = display.bounds().size;
            window_size.width = window_size.width.min(display_size.width * 0.9);
            window_size.height = window_size.height.min(display_size.height * 0.9);
        }
        WindowBounds::Windowed(Bounds::centered(None, window_size, cx))
    });
    let options = WindowOptions {
        window_bounds: Some(window_bounds),
        window_min_size: Some(size(px(560.), px(360.))),
        kind: WindowKind::Normal,
        app_id: Some("dev.malgel.Malgel".into()),
        #[cfg(target_os = "linux")]
        window_background: gpui_kit::WindowBackgroundAppearance::Opaque,
        #[cfg(target_os = "linux")]
        window_decorations: Some(gpui_kit::WindowDecorations::Client),
        ..TitleBar::window_options()
    };

    let result = gpui_kit::open_window(options, cx, move |window, cx| {
        themes::apply(Some(window), cx);
        let workspace = cx.new(|cx| Workspace::new(paths, app_menu_bar, window, cx));
        cx.set_global(MainWindow {
            handle: window.window_handle(),
            workspace: workspace.downgrade(),
        });
        workspace
    });
    if let Err(err) = result {
        eprintln!("malgel: could not open a window: {err:#}");
        cx.quit();
    }
}

/// Run `f` in the main window, where dialogs like Settings belong.
fn with_window(cx: &mut App, f: impl FnOnce(&mut gpui_kit::Window, &mut App)) {
    let Some(window) = cx
        .try_global::<MainWindow>()
        .map(|main| main.handle)
        .filter(|main| cx.windows().contains(main))
        .or_else(|| cx.active_window())
        .or_else(|| cx.windows().into_iter().next())
    else {
        return;
    };
    _ = window.update(cx, |_, window, cx| f(window, cx));
}

/// Ports the first instance listens on; a second launch probes these.
const CLI_PORTS: [u16; 10] = [
    47615, 47616, 47617, 47618, 47619, 47620, 47621, 47622, 47623, 47624,
];
const CLI_MAGIC: &str = "malgel-cli-v1";

/// Hand `paths` to the running instance; `true` when it acknowledged.
fn forward_to_running_instance(paths: &[PathBuf]) -> bool {
    for port in CLI_PORTS {
        let Ok(mut stream) = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(250),
        ) else {
            continue;
        };
        if stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .is_err()
        {
            continue;
        }
        let mut payload = String::from(CLI_MAGIC);
        payload.push('\n');
        for path in paths {
            payload.push_str(&path.to_string_lossy());
            payload.push('\n');
        }
        if stream.write_all(payload.as_bytes()).is_err() {
            continue;
        }
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut reply = String::new();
        if stream.read_to_string(&mut reply).is_ok() && reply.contains("malgel-cli-ok") {
            return true;
        }
    }
    false
}

/// Accept forwarded launches on the first free port in `CLI_PORTS`.
fn listen_for_cli(open_tx: futures::channel::mpsc::UnboundedSender<Vec<PathBuf>>) {
    for port in CLI_PORTS {
        let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) else {
            continue;
        };
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                continue;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut payload = String::new();
            if stream.read_to_string(&mut payload).is_err() {
                continue;
            }
            let mut lines = payload.lines();
            if lines.next() != Some(CLI_MAGIC) {
                continue;
            }
            let paths: Vec<PathBuf> = lines
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .map(|path| path.canonicalize().unwrap_or(path))
                .collect();
            let _ = open_tx.unbounded_send(paths);
            let _ = stream.write_all(b"malgel-cli-ok\n");
        }
    }
}
