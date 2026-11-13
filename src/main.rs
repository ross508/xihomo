mod api;
mod backend;
mod core;
mod dock;
mod icons;
mod input;
mod placement;
mod profiles;
mod proxy;
mod settings;
mod tray;
mod ui;
mod update;

use anyhow::{Context, Result};
use backend::{Cmd, Handle};
use gpui::{
    App, AppContext, Application, Entity, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions,
    px, size,
};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication};
use settings::{Paths, Theme};
use std::cell::RefCell;
use std::fs::File;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use tray::Tray;
use ui::AppState;

/// Long enough for networksetup across many services plus the child's graceful-stop timeout.
const QUIT_TIMEOUT: Duration = Duration::from_secs(20);
const WINDOW_SIZE: (f32, f32) = (640., 680.);
const WINDOW_MIN_SIZE: (f32, f32) = (480., 420.);

fn open_window(state: &Entity<AppState>, cx: &mut App) {
    // An accessory app does not come to the front, so it turns regular first.
    dock::apply(true);
    cx.activate(true);
    let existing = state.read(cx).window;
    if let Some(window) = existing
        && window
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    // Back where it was on the same display; otherwise centered on the main one at its last size.
    let saved = state
        .read(cx)
        .placement_file
        .as_deref()
        .and_then(placement::load);
    let found = saved.as_ref().and_then(|saved| {
        cx.displays()
            .into_iter()
            .find(|d| d.uuid().is_ok_and(|uuid| uuid.to_string() == saved.display))
    });
    let origin = saved
        .as_ref()
        .filter(|_| found.is_some())
        .map(|s| (s.x, s.y));
    let display = found.or_else(|| cx.primary_display());
    let wanted = saved.map_or(WINDOW_SIZE, |s| (s.width, s.height));
    let bounds = display.as_ref().map_or(
        gpui::Bounds::centered(None, size(px(wanted.0), px(wanted.1)), cx),
        |d| placement::fit(origin, wanted, WINDOW_MIN_SIZE, d.bounds().size),
    );
    let view = state.clone();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            display_id: display.map(|d| d.id()),
            // The content draws under a transparent title bar, so it takes the theme's background.
            titlebar: Some(TitlebarOptions {
                // Empty, so the window does not name the app in Mission Control or window pickers.
                title: Some("".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            window_min_size: Some(size(px(WINDOW_MIN_SIZE.0), px(WINDOW_MIN_SIZE.1))),
            is_minimizable: true,
            ..Default::default()
        },
        move |window, cx| {
            view.update(cx, |this, cx| {
                this.focus.focus(window);
                // Editing detection is on activation; the 30s tick covers a window left in front.
                cx.observe_window_activation(window, |this, window, _| {
                    if window.is_window_active() {
                        this.handle.send(Cmd::Refresh);
                    }
                })
                .detach();
                cx.observe_window_bounds(window, |this, window, cx| {
                    this.remember_placement(window, cx)
                })
                .detach();
            });
            view.clone()
        },
    );
    match opened {
        Ok(window) => state.update(cx, |this, cx| {
            this.window = Some(window);
            this.sync_visibility();
            this.rebuild_logs();
            cx.notify();
        }),
        Err(e) => eprintln!("xihomo: cannot open window: {e:#}"),
    }
}

/// GPUI does not theme the title bar, so a fixed theme is set app-wide; `Auto` clears it to follow
/// the system. GPUI's `window.appearance()` then reports the same choice.
fn apply_theme(theme: Theme) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // SAFETY: AppKit's appearance name constants are immutable statics.
    let name = unsafe {
        match theme {
            Theme::Auto => None,
            Theme::Light => Some(NSAppearanceNameAqua),
            Theme::Dark => Some(NSAppearanceNameDarkAqua),
        }
    };
    let appearance = name.and_then(NSAppearance::appearanceNamed);
    NSApplication::sharedApplication(mtm).setAppearance(appearance.as_deref());
}

fn on_menu(id: &str, state: &Entity<AppState>, cx: &mut App) {
    let (handle, snap) = {
        let this = state.read(cx);
        (this.handle.clone(), this.snap.clone())
    };
    match tray::menu_action(id, &snap) {
        Some(tray::MenuAction::Send(cmd)) => handle.send(cmd),
        Some(tray::MenuAction::Open) => open_window(state, cx),
        Some(tray::MenuAction::Quit) => cx.quit(),
        None => {}
    }
}

fn show_tab(tab: ui::Tab, cx: &mut App) {
    let state = cx.global::<StateGlobal>().0.clone();
    open_window(&state, cx);
    state.update(cx, |this, cx| this.set_tab(tab, cx));
}

fn run_app(handle: Handle, window_file: PathBuf, cx: &mut App) -> Result<()> {
    // GPUI sets the regular policy during launch; without a window the Dock icon is optional.
    let (mut theme, mut keep, mut icon_set) = {
        let snap = handle.snapshot.borrow();
        (snap.theme, snap.keep_dock_icon, snap.icon_set.clone())
    };
    dock::apply(dock::dock_shown(keep, false));
    dock::set_icon(&icon_set);
    apply_theme(theme);
    cx.set_menus(dock::app_menus());

    cx.bind_keys([
        KeyBinding::new("cmd-w", ui::CloseWindow, None),
        KeyBinding::new("cmd-q", ui::Quit, None),
        KeyBinding::new("cmd-1", ui::ShowControl, None),
        KeyBinding::new("cmd-2", ui::ShowConnections, None),
        KeyBinding::new("cmd-3", ui::ShowLogs, None),
        KeyBinding::new("cmd-4", ui::ShowSettings, None),
        KeyBinding::new("cmd-,", ui::ShowSettings, None),
        KeyBinding::new("cmd-h", dock::Hide, None),
        KeyBinding::new("alt-cmd-h", dock::HideOthers, None),
    ]);
    cx.bind_keys(input::bindings());
    cx.on_action(|_: &ui::Quit, cx| cx.quit());
    cx.on_action(|_: &dock::About, _| dock::about());
    cx.on_action(|_: &dock::Hide, cx| cx.hide());
    cx.on_action(|_: &dock::HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &dock::ShowAll, cx| cx.unhide_other_apps());
    // With the Dock icon kept and no window, the menus open one on their tab.
    cx.on_action(|_: &ui::ShowControl, cx| show_tab(ui::Tab::Control, cx));
    cx.on_action(|_: &ui::ShowConnections, cx| show_tab(ui::Tab::Connections, cx));
    cx.on_action(|_: &ui::ShowLogs, cx| show_tab(ui::Tab::Logs, cx));
    cx.on_action(|_: &ui::ShowSettings, cx| show_tab(ui::Tab::Settings, cx));

    let state = cx.new(|cx| {
        let mut state = AppState::new(handle.clone(), cx);
        state.placement_file = Some(window_file);
        state
    });
    cx.set_global(StateGlobal(state.clone()));

    let (menu_tx, mut menu_rx) = tokio::sync::mpsc::unbounded_channel();
    let tray = Rc::new(RefCell::new(
        Tray::new(&handle.snapshot.borrow().clone(), menu_tx).context("create status item")?,
    ));

    // Every long-lived task below ends when the app quits, as its executor shuts down.
    {
        let (state, tray) = (state.clone(), tray.clone());
        cx.spawn(async move |cx| {
            while let Some(id) = menu_rx.recv().await {
                let _ = cx.update(|cx| {
                    on_menu(&id, &state, cx);
                    tray.borrow_mut().resync(&state.read(cx).snap);
                });
            }
        })
        .detach();
    }
    {
        let (state, tray, mut rx) = (state.clone(), tray.clone(), handle.snapshot.clone());
        cx.spawn(async move |cx| {
            while rx.changed().await.is_ok() {
                let snap = rx.borrow_and_update().clone();
                if snap.theme != theme {
                    theme = snap.theme;
                    apply_theme(theme);
                }
                if snap.icon_set != icon_set {
                    icon_set.clone_from(&snap.icon_set);
                    dock::set_icon(&icon_set);
                }
                if snap.keep_dock_icon != keep {
                    keep = snap.keep_dock_icon;
                    let _ = cx
                        .update(|cx| dock::apply(dock::dock_shown(keep, !cx.windows().is_empty())));
                }
                tray.borrow_mut().update(&snap);
                let _ = state.update(cx, |this, cx| {
                    this.snap = snap;
                    cx.notify();
                });
            }
        })
        .detach();
    }
    {
        let (state, mut rx) = (state.clone(), handle.connections.clone());
        cx.spawn(async move |cx| {
            while rx.changed().await.is_ok() {
                let conns = rx.borrow_and_update().clone();
                let _ = state.update(cx, |this, cx| {
                    this.conns = conns;
                    cx.notify();
                });
            }
        })
        .detach();
    }
    {
        let (state, mut rx) = (state.clone(), handle.log_generation.clone());
        cx.spawn(async move |cx| {
            // Coalesce: however fast the core logs, the view rebuilds at most 5 times a second.
            while rx.changed().await.is_ok() {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                rx.borrow_and_update();
                let _ = state.update(cx, |this, cx| {
                    this.rebuild_logs();
                    cx.notify();
                });
            }
        })
        .detach();
    }
    {
        let mut rx = handle.quit_requested.clone();
        cx.spawn(async move |cx| {
            let _ = rx.changed().await;
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    }

    {
        let state = state.clone();
        cx.on_window_closed(move |cx| {
            if cx.windows().is_empty() {
                dock::apply(dock::dock_shown(state.read(cx).snap.keep_dock_icon, false));
                state.update(cx, |this, _| this.closed());
            }
        })
        .detach();
    }
    // The closure runs synchronously on quit; GPUI gives the returned future only 100ms.
    cx.on_app_quit(move |cx| {
        state.update(cx, |this, _| this.flush_placement());
        handle.shutdown_blocking(QUIT_TIMEOUT);
        async {}
    })
    .detach();

    // A menu bar app starts without a window; the Metal surface and glyph caches wait until asked for.
    Ok(())
}

fn main() -> Result<()> {
    // Used by the bundle script; it must not touch the data directories or the instance lock.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--write-iconset") {
        let dir = args.next().context("--write-iconset needs a directory")?;
        return icons::write_iconset(dir.as_ref()).context("write iconset");
    }
    // Any panic is a bug that may already have taken the core down with it (an unwinding backend
    // drops it; a panic in a platform callback aborts the app), so a proxy of ours goes too.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        proxy::release_after_panic();
    }));
    let paths = Paths::user_default()?;
    paths.ensure()?;
    // Held for the whole process: one owner of the child, the ports and the proxy journal.
    let lock = File::create(paths.state().join("instance.lock")).context("open instance lock")?;
    if lock.try_lock().is_err() {
        eprintln!("xihomo is already running");
        return Ok(());
    }
    let window_file = paths.window();
    let handle = backend::spawn(paths)?;
    let app = Application::new();
    // Relaunching the app from Finder while it runs with no window lands here.
    app.on_reopen(|cx| {
        if let Some(state) = cx.try_global::<StateGlobal>().map(|g| g.0.clone()) {
            open_window(&state, cx);
        }
    });
    app.run(move |cx| {
        if let Err(e) = run_app(handle, window_file, cx) {
            eprintln!("xihomo: {e:#}");
            cx.quit();
        }
    });
    drop(lock);
    Ok(())
}

struct StateGlobal(Entity<AppState>);
impl gpui::Global for StateGlobal {}
