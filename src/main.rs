mod api;
mod backend;
mod core;
mod icons;
mod input;
mod profiles;
mod proxy;
mod settings;
mod tray;
mod ui;
mod update;

use anyhow::{Context, Result};
use backend::{Cmd, Handle};
use gpui::{
    App, AppContext, Application, Bounds, Entity, KeyBinding, TitlebarOptions, WindowBounds,
    WindowOptions, px, size,
};
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    NSApplicationActivationPolicy,
};
use settings::{Paths, Theme};
use std::cell::RefCell;
use std::fs::File;
use std::rc::Rc;
use std::time::Duration;
use tray::Tray;
use ui::{AppState, Tab};

/// Long enough for networksetup across many services plus the child's graceful-stop timeout.
const QUIT_TIMEOUT: Duration = Duration::from_secs(20);

fn open_window(state: &Entity<AppState>, tab: Option<Tab>, cx: &mut App) {
    cx.activate(true);
    let existing = state.read(cx).window;
    if let Some(window) = existing
        && window
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        if let Some(tab) = tab {
            state.update(cx, |this, cx| this.set_tab(tab, cx));
        }
        return;
    }
    let bounds = Bounds::centered(None, size(px(520.), px(620.)), cx);
    let view = state.clone();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // The content draws under a transparent title bar, so it takes the theme's background.
            titlebar: Some(TitlebarOptions {
                title: Some("xihomo".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            window_min_size: Some(size(px(440.), px(360.))),
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
            });
            view.clone()
        },
    );
    match opened {
        Ok(window) => state.update(cx, |this, cx| {
            this.window = Some(window);
            this.tab = tab.unwrap_or(this.tab);
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
        Some(tray::MenuAction::Open(tab)) => open_window(state, tab, cx),
        Some(tray::MenuAction::Quit) => cx.quit(),
        None => {}
    }
}

fn run_app(handle: Handle, cx: &mut App) -> Result<()> {
    // GPUI sets the regular policy during launch; a menu bar app has no Dock icon.
    let mtm = MainThreadMarker::new().context("not on the main thread")?;
    NSApplication::sharedApplication(mtm)
        .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let mut theme = handle.snapshot.borrow().theme;
    apply_theme(theme);

    cx.bind_keys([
        KeyBinding::new("cmd-w", ui::CloseWindow, None),
        KeyBinding::new("cmd-q", ui::Quit, None),
        KeyBinding::new("cmd-1", ui::ShowControl, None),
        KeyBinding::new("cmd-2", ui::ShowConnections, None),
        KeyBinding::new("cmd-3", ui::ShowLogs, None),
        KeyBinding::new("cmd-4", ui::ShowSettings, None),
    ]);
    cx.bind_keys(input::bindings());
    cx.on_action(|_: &ui::Quit, cx| cx.quit());

    let state = cx.new(|cx| AppState::new(handle.clone(), cx));
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
                state.update(cx, |this, _| {
                    this.window = None;
                    this.sync_visibility();
                });
            }
        })
        .detach();
    }
    // The closure runs synchronously on quit; GPUI gives the returned future only 100ms.
    cx.on_app_quit(move |_| {
        handle.shutdown_blocking(QUIT_TIMEOUT);
        async {}
    })
    .detach();

    // A menu bar app starts without a window; the Metal surface and glyph caches wait until asked for.
    Ok(())
}

fn main() -> Result<()> {
    let paths = Paths::user_default()?;
    paths.ensure()?;
    // Held for the whole process: one owner of the child, the ports and the proxy journal.
    let lock = File::create(paths.state().join("instance.lock")).context("open instance lock")?;
    if lock.try_lock().is_err() {
        eprintln!("xihomo is already running");
        return Ok(());
    }
    let handle = backend::spawn(paths)?;
    let app = Application::new();
    // Relaunching the app from Finder while it runs with no window lands here.
    app.on_reopen(|cx| {
        if let Some(state) = cx.try_global::<StateGlobal>().map(|g| g.0.clone()) {
            open_window(&state, None, cx);
        }
    });
    app.run(move |cx| {
        if let Err(e) = run_app(handle, cx) {
            eprintln!("xihomo: {e:#}");
            cx.quit();
        }
    });
    drop(lock);
    Ok(())
}

struct StateGlobal(Entity<AppState>);
impl gpui::Global for StateGlobal {}
