//! Malgel — a fast, native Markdown editor and previewer.

// Keep release builds on Windows from opening a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod actions;
mod analysis;
mod commands;
mod crash;
mod default_app;
mod diagram;
mod diagram_viewer;
mod dialog_focus;
mod diff;
mod display_raster;
mod document;
mod document_view;
mod docx;
mod export;
mod file_icon;
mod file_tree;
mod format;
#[cfg(test)]
mod fuzz_tests;
mod image_view;
mod images;
mod links;
mod logfile;
mod math;
mod menus;
mod paths;
mod pdf;
pub mod pdf_view;
mod perf;
mod preview_ext;
mod quick_open;
mod render_budget;
mod rich_copy;
mod search;
mod session;
mod settings;
mod settings_ui;
mod shortcuts;
mod single_instance;
mod smart_edit;
mod spell;
mod term_session;
mod terminal;
mod themes;
mod title_menu_bar;
mod typst_live;
mod typst_preview;
mod typst_world;
mod workspace;
mod zoom;

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

/// Platform messages logged as errors on a normal run (an X server without
/// a pointer device, a window already gone at exit); shown only when
/// `RUST_LOG` asks for logs.
const BENIGN_PLATFORM_ERRORS: &[&str] = &[
    "Found no xinput mouse pointers",
    "DestroyWindow",
    "clipboard",
];

fn is_benign_platform_error(target: &str, message: &str) -> bool {
    target.starts_with("gpui")
        && BENIGN_PLATFORM_ERRORS
            .iter()
            .any(|benign| message.contains(benign))
}

struct StdErrLogger {
    quiet: bool,
}
impl log::Log for StdErrLogger {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.level() <= log::Level::Trace
    }
    fn log(&self, record: &log::Record) {
        if self.quiet && is_benign_platform_error(record.target(), &record.args().to_string()) {
            return;
        }
        let line = format!(
            "[{}::{}] {}",
            record.level(),
            record.target(),
            record.args()
        );
        eprintln!("{line}");
        // Warnings and errors also go to the rotating log file.
        if record.level() <= log::Level::Warn
            && !is_benign_platform_error(record.target(), &record.args().to_string())
            && let Some(file) = crate::logfile::RotatingLog::app()
        {
            file.write_line(&format!("[{}] {line}", crate::logfile::timestamp()));
        }
    }
    fn flush(&self) {}
}

/// The command line, parsed.
#[derive(Debug, Default, PartialEq)]
struct Cli {
    help: bool,
    version: bool,
    new_instance: bool,
    paths: Vec<PathBuf>,
}

/// Parse the arguments after the program name. An unknown option is an
/// error; after `--` everything is a path.
fn parse_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut options_done = false;
    for arg in args {
        let text = arg.to_string_lossy();
        if options_done || !text.starts_with('-') || text == "-" {
            cli.paths.push(PathBuf::from(arg));
            continue;
        }
        match text.as_ref() {
            "--" => options_done = true,
            "-h" | "--help" => cli.help = true,
            "-V" | "--version" => cli.version = true,
            "--new-instance" | "--new-window" => cli.new_instance = true,
            // macOS's launcher used to add a process serial number.
            other if other.starts_with("-psn_") => {}
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    Ok(cli)
}

/// The log level `RUST_LOG` asks for (`error` … `trace`, or `target=level`
/// pairs, of which the most verbose counts); warnings by default.
fn log_level(value: Option<&str>) -> log::LevelFilter {
    value
        .into_iter()
        .flat_map(|value| value.split(','))
        .filter_map(|part| {
            part.rsplit('=')
                .next()?
                .trim()
                .parse::<log::LevelFilter>()
                .ok()
        })
        .max()
        .unwrap_or(log::LevelFilter::Warn)
}

fn main() {
    crate::perf::init();
    crate::crash::install_panic_hook();
    // Which build wrote what follows (crash reports quote the log).
    if let Some(log) = crate::logfile::RotatingLog::app() {
        log.write_line(&format!(
            "[{}] start {}",
            crate::logfile::timestamp(),
            crate::logfile::build_label()
        ));
    }
    let _ = log::set_boxed_logger(Box::new(StdErrLogger {
        quiet: std::env::var_os("RUST_LOG").is_none(),
    }));
    log::set_max_level(log_level(std::env::var("RUST_LOG").ok().as_deref()));
    let cli = match parse_args(std::env::args_os().skip(1)) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("malgel: {message} (see --help)");
            std::process::exit(2);
        }
    };
    if cli.help {
        println!(
            "Malgel {}\n\nUsage: malgel [OPTIONS] [--] [FILE|FOLDER]...\n\n\
             Options:\n  \
             --new-instance  Open a new window instead of handing the files to the running Malgel\n  \
             -h, --help      Show this help\n  \
             -V, --version   Show the version\n  \
             --              Treat every argument after it as a file, even one starting with '-'",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    if cli.version {
        println!("Malgel {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // Without a display the window can never appear; say so at once rather
    // than failing deep in the platform layer.
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_none_or(|v| v.is_empty())
        && std::env::var_os("WAYLAND_DISPLAY").is_none_or(|v| v.is_empty())
    {
        eprintln!("malgel: no display found (DISPLAY/WAYLAND_DISPLAY unset)");
        std::process::exit(1);
    }
    let new_instance = cli.new_instance;
    let paths: Vec<PathBuf> = cli
        .paths
        .into_iter()
        .map(|path| path.canonicalize().unwrap_or(path))
        .collect();

    // A second launch hands its arguments to the running instance and exits,
    // the way VS Code wakes the existing window.
    if !new_instance && single_instance::forward(&paths) {
        eprintln!(
            "malgel: handed over to the Malgel already running in this session; \
             pass --new-instance to open another window"
        );
        return;
    }

    // macOS delivers "Open with" and double-clicked files as URLs, outside
    // the app's context; they are handed over through a channel.
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<Vec<PathBuf>>();
    {
        let cli_tx = open_tx.clone();
        // A new instance still takes later launches of this session.
        std::thread::spawn(move || single_instance::listen(cli_tx));
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
        terminal::init(cx);
        crate::perf::start_ui_lag_probe(cx);
        diagram_viewer::bind_keys(cx);
        let app_menu_bar = menus::init(cx);

        // Never quit here: that skipped the workspace dirty-buffer confirm
        // (Save / Don't save / Cancel) when Ctrl+Q or the Quit menu fired at
        // app scope. Forward to the workspace; only quit if no window exists.
        cx.on_action(|_: &Quit, cx: &mut App| {
            let Some(main) = cx.try_global::<MainWindow>() else {
                cx.quit();
                return;
            };
            if !cx.windows().contains(&main.handle) {
                cx.quit();
                return;
            }
            let (handle, workspace) = (main.handle, main.workspace.clone());
            let forwarded = handle.update(cx, |_, window, cx| {
                let Some(ws) = workspace.upgrade() else {
                    return false;
                };
                ws.update(cx, |ws, cx| ws.request_quit(window, cx));
                true
            });
            if !matches!(forwarded, Ok(true)) {
                cx.quit();
            }
        });
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
    app_menu_bar: gpui_kit::Entity<crate::title_menu_bar::TitleMenuBar>,
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
    app_menu_bar: gpui_kit::Entity<crate::title_menu_bar::TitleMenuBar>,
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
        crate::perf::mark("window_workspace_built");
        workspace
    });
    if let Err(err) = result {
        eprintln!("malgel: could not open a window: {err:#}");
        cx.quit();
    } else {
        crate::perf::mark("open_window_ok");
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

#[cfg(test)]
mod tests {
    #[test]
    fn options_are_known_or_refused() {
        use super::parse_args;
        let args = |list: &[&str]| parse_args(list.iter().map(std::ffi::OsString::from));
        assert_eq!(args(&["--bogus"]).unwrap_err(), "unknown option '--bogus'");
        let cli = args(&["--new-instance", "a.md", "--", "--weird.md", "-h"]).unwrap();
        assert!(cli.new_instance && !cli.help);
        assert_eq!(
            cli.paths,
            ["a.md", "--weird.md", "-h"].map(std::path::PathBuf::from)
        );
        assert!(args(&["-V"]).unwrap().version);
        assert!(args(&["--help"]).unwrap().help);
    }

    #[test]
    fn rust_log_sets_the_level_it_names() {
        use log::LevelFilter::*;
        assert_eq!(super::log_level(None), Warn);
        assert_eq!(super::log_level(Some("info")), Info);
        assert_eq!(super::log_level(Some("malgel=debug,gpui=error")), Debug);
        assert_eq!(super::log_level(Some("nonsense")), Warn);
    }

    #[test]
    fn known_platform_noise_is_recognised() {
        assert!(super::is_benign_platform_error(
            "gpui_linux::linux::x11::client",
            "Found no xinput mouse pointers."
        ));
        assert!(!super::is_benign_platform_error(
            "gpui_linux::linux::x11::client",
            "failed to open display"
        ));
        assert!(!super::is_benign_platform_error("malgel", "clipboard"));
    }
}
