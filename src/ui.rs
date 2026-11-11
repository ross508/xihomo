use crate::api::{Connection, LogLine};
use crate::backend::{Area, Cmd, Handle, Phase, Snapshot};
use crate::icons;
use crate::settings::Theme;
use crate::tray::phase_label;
use gpui::prelude::*;
use gpui::{
    App, ClickEvent, Context, ElementId, FocusHandle, Hsla, KeyDownEvent, MouseButton,
    PathPromptOptions, Pixels, Point, ScrollStrategy, SharedString, Stateful,
    UniformListScrollHandle, Window, WindowAppearance, WindowHandle, actions, anchored, deferred,
    div, px, rgb, uniform_list,
};
use std::collections::HashSet;
use std::sync::Arc;

actions!(
    xihomo,
    [
        CloseWindow,
        Quit,
        ShowControl,
        ShowConnections,
        ShowLogs,
        ShowSettings
    ]
);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tab {
    Control,
    Connections,
    Logs,
    Settings,
}

#[derive(Clone, Copy, PartialEq)]
enum LogLevel {
    All,
    Warning,
    Error,
}

impl LogLevel {
    fn matches(self, level: &str) -> bool {
        match self {
            Self::All => true,
            Self::Warning => level == "warning" || level == "error",
            Self::Error => level == "error",
        }
    }
}

pub fn filter_logs<'a>(
    lines: impl Iterator<Item = &'a LogLine>,
    level: &str,
    needle: &str,
) -> Vec<LogLine> {
    let level = match level {
        "warning" => LogLevel::Warning,
        "error" => LogLevel::Error,
        _ => LogLevel::All,
    };
    let needle = needle.to_lowercase();
    lines
        .filter(|l| level.matches(&l.level))
        .filter(|l| needle.is_empty() || l.text.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

pub struct AppState {
    pub handle: Handle,
    pub snap: Arc<Snapshot>,
    pub conns: Arc<Vec<Connection>>,
    pub tab: Tab,
    pub window: Option<WindowHandle<AppState>>,
    expanded: HashSet<String>,
    log_view: Vec<LogLine>,
    log_level: &'static str,
    log_filter: String,
    log_paused: bool,
    log_scroll: UniformListScrollHandle,
    pub focus: FocusHandle,
    /// What the open right-click menu would delete, and where it is.
    context_menu: Option<(Deletable, Point<Pixels>)>,
    /// The palette of the last frame, so tests can see what was drawn.
    #[cfg(test)]
    palette: Option<Palette>,
}

#[derive(Clone, Debug, PartialEq)]
enum Deletable {
    Version(String),
    Profile(String),
}

/// Room for the transparent title bar's traffic lights, measured on macOS 27; GPUI 0.2.2 does not
/// expose the real height.
const TITLEBAR_HEIGHT: Pixels = px(32.);

#[derive(Clone, Copy, Debug, PartialEq)]
struct Palette {
    bg: Hsla,
    panel: Hsla,
    hover: Hsla,
    text: Hsla,
    dim: Hsla,
    border: Hsla,
    accent: Hsla,
    on_accent: Hsla,
    danger: Hsla,
}

/// Two fixed palettes after an OS X El Capitan-inspired light theme and a Vibrant Dark-inspired
/// dark one. `Auto` follows the window's appearance.
fn palette(theme: Theme, appearance: WindowAppearance) -> Palette {
    let dark = match theme {
        Theme::Light => false,
        Theme::Dark => true,
        Theme::Auto => matches!(
            appearance,
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ),
    };
    let c = |hex: u32| rgb(hex).into();
    if dark {
        Palette {
            bg: c(0x2B2B2B),
            panel: c(0x424242),
            hover: c(0x4B4B4B),
            text: c(0xEEEEEE),
            dim: c(0xB8B8B8),
            border: c(0x4D4D4D),
            accent: c(0x286AC7),
            on_accent: c(0xFFFFFF),
            danger: c(0xF08178),
        }
    } else {
        Palette {
            bg: c(0xECECEC),
            panel: c(0xFFFFFF),
            hover: c(0xF5F5F5),
            text: c(0x222222),
            dim: c(0x636363),
            border: c(0xC2C2C2),
            accent: c(0x0067D9),
            on_accent: c(0xFFFFFF),
            danger: c(0xB52F2B),
        }
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `selected` draws the control filled, for tabs, segments and chosen values.
fn button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    p: Palette,
    selected: bool,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<gpui::Div> {
    let id: SharedString = id.into();
    let selector = id.to_string();
    let base = div()
        .id(ElementId::Name(id))
        .debug_selector(|| selector)
        .px_2()
        .py(px(3.))
        .rounded(px(5.))
        .border_1()
        .text_size(px(12.))
        .flex_none()
        .child(label.into());
    let styled = if selected {
        base.bg(p.accent)
            .border_color(p.accent)
            .text_color(p.on_accent)
    } else {
        base.bg(p.panel)
            .border_color(p.border)
            .text_color(if enabled { p.text } else { p.dim })
    };
    if enabled {
        styled
            .cursor_pointer()
            .hover(move |s| if selected { s } else { s.bg(p.hover) })
            .on_click(on_click)
    } else {
        styled
    }
}

fn section(title: &'static str, p: Palette) -> gpui::Div {
    div()
        .text_size(px(11.))
        .text_color(p.dim)
        .mt_3()
        .mb_1()
        .child(title)
}

impl AppState {
    pub fn new(handle: Handle, cx: &mut Context<Self>) -> Self {
        let snap = handle.snapshot.borrow().clone();
        Self {
            handle,
            snap,
            conns: Arc::new(Vec::new()),
            tab: Tab::Control,
            window: None,
            expanded: HashSet::new(),
            log_view: Vec::new(),
            log_level: "all",
            log_filter: String::new(),
            log_paused: false,
            log_scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            context_menu: None,
            #[cfg(test)]
            palette: None,
        }
    }

    pub fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.sync_visibility();
        self.rebuild_logs();
        cx.notify();
    }

    /// Connections are polled only while they are on screen.
    pub fn sync_visibility(&self) {
        let visible = self.window.is_some() && self.tab == Tab::Connections;
        self.handle.send(Cmd::WatchConnections(visible));
    }

    /// Runs only while Logs is on screen, at the coalesced rate of its caller.
    pub fn rebuild_logs(&mut self) {
        if self.tab != Tab::Logs || self.window.is_none() || self.log_paused {
            return;
        }
        let logs = self.handle.logs.lock().unwrap();
        self.log_view = filter_logs(logs.iter(), self.log_level, &self.log_filter);
        drop(logs);
        if let Some(last) = self.log_view.len().checked_sub(1) {
            self.log_scroll.scroll_to_item(last, ScrollStrategy::Bottom);
        }
    }

    fn cmd(
        &self,
        make: impl Fn() -> Cmd + 'static,
    ) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
        let handle = self.handle.clone();
        move |_, _, _| handle.send(make())
    }

    /// Right-click opens the delete menu for `target` at the pointer.
    fn open_menu(
        target: Deletable,
        cx: &mut Context<Self>,
    ) -> impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static {
        cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
            this.context_menu = Some((target.clone(), event.position));
            cx.notify();
        })
    }

    /// One small line next to the area it is about: red for errors, dim otherwise.
    fn note(&self, area: Area, p: Palette) -> Option<gpui::Div> {
        let message = self.snap.message(area)?;
        Some(
            div()
                .mt_1()
                .text_size(px(12.))
                .text_color(if message.error { p.danger } else { p.dim })
                .child(message.text.clone()),
        )
    }

    fn render_control(&mut self, p: Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = self.snap.clone();
        let running = s.phase == Phase::Running;
        let (core_label, core_cmd): (&str, fn() -> Cmd) = match s.phase {
            Phase::Running | Phase::Starting => ("Stop", || Cmd::Stop),
            Phase::Failed(_) => ("Retry", || Cmd::Start),
            _ => ("Start", || Cmd::Start),
        };
        let proxy_on = s.proxy_on;
        let (proxy_label, proxy_enabled) = match s.proxy_busy {
            Some(true) => ("System Proxy: Turning On…", false),
            Some(false) => ("System Proxy: Turning Off…", false),
            None if proxy_on => ("System Proxy: On", true),
            None => ("System Proxy: Off", running),
        };
        let (status, status_color) = match &s.phase {
            Phase::Failed(reason) => (format!("Failed: {reason}"), p.danger),
            phase => (
                format!(
                    "{} · {} · mixed {} · controller {}",
                    phase_label(phase),
                    s.core_version.as_deref().unwrap_or("no core"),
                    s.mixed_port,
                    s.controller_port,
                ),
                p.text,
            ),
        };

        let mut column = div().flex().flex_col();
        column = column
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(status_color)
                    .child(status),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .mt_2()
                    .child(button(
                        "core",
                        core_label,
                        p,
                        false,
                        s.phase != Phase::Stopping,
                        self.cmd(core_cmd),
                    ))
                    .child(button(
                        "proxy",
                        proxy_label,
                        p,
                        proxy_on,
                        proxy_enabled,
                        self.cmd(move || Cmd::SetProxy(!proxy_on)),
                    )),
            )
            .children(self.note(Area::Core, p))
            .children(self.note(Area::Proxy, p));

        let mut profiles = div().flex().flex_wrap().gap_1();
        for profile in s.profiles.iter() {
            let name = profile.name.clone();
            let id = SharedString::from(format!("profile-{name}"));
            let selected = s.active.as_deref() == Some(&name);
            let label = if profile.problem.is_some() {
                format!("{name} (broken)")
            } else {
                name.clone()
            };
            let target = Deletable::Profile(name.clone());
            profiles = profiles.child(
                button(
                    id,
                    label,
                    p,
                    selected,
                    profile.problem.is_none(),
                    self.cmd(move || Cmd::Activate(name.clone())),
                )
                .on_mouse_down(MouseButton::Right, Self::open_menu(target, cx)),
            );
        }
        if s.profiles.is_empty() {
            profiles = profiles.child(
                div()
                    .text_size(px(12.))
                    .text_color(p.dim)
                    .child("No profiles. Link a YAML file in Settings."),
            );
        }
        column = column
            .child(section("PROFILE", p))
            .child(profiles)
            .children(self.note(Area::Profiles, p));
        if s.edited {
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .mt_1()
                    .text_size(px(12.))
                    .text_color(p.dim)
                    .child("The profile or overrides changed on disk.")
                    .child(button(
                        "reload",
                        "Reload",
                        p,
                        false,
                        true,
                        self.cmd(|| Cmd::Reload),
                    )),
            );
        }

        // Shown while stopped too: a choice is remembered and applied at the next start.
        let mut modes = div().flex().gap_1();
        for mode in ["rule", "global", "direct"] {
            modes = modes.child(button(
                mode,
                mode,
                p,
                s.mode == mode,
                s.phase != Phase::Stopping,
                self.cmd(move || Cmd::SetMode(mode.to_owned())),
            ));
        }
        column = column.child(section("MODE", p)).child(modes);
        if running {
            column = column.child(section("GROUPS", p));
            for group in s
                .groups
                .iter()
                .filter(|g| g.name != "GLOBAL" || s.mode == "global")
            {
                let name = group.name.clone();
                let expanded = self.expanded.contains(&name);
                let toggle = name.clone();
                let header = div()
                    .id(SharedString::from(format!("group-{name}")))
                    .debug_selector(|| format!("group-{name}"))
                    .flex()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .hover(move |st| st.bg(p.hover))
                    .text_size(px(12.))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded.remove(&toggle) {
                            this.expanded.insert(toggle.clone());
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_color(p.text)
                            .child(format!("{} {name}", if expanded { "▾" } else { "▸" })),
                    )
                    .child(div().text_color(p.dim).truncate().child(group.now.clone()));
                column = column.child(header);
                if expanded {
                    let mut options = div().flex().flex_wrap().gap_1().px_2().pb_2();
                    // Display only: REJECT as the whole machine's route is never wanted.
                    for proxy in group
                        .all
                        .iter()
                        .filter(|proxy| !(name == "GLOBAL" && *proxy == "REJECT"))
                    {
                        let (g, pick) = (name.clone(), proxy.clone());
                        let id = SharedString::from(format!("pick-{name}-{proxy}"));
                        options = options.child(button(
                            id,
                            proxy.clone(),
                            p,
                            *proxy == group.now,
                            group.selectable(),
                            self.cmd(move || Cmd::Select {
                                group: g.clone(),
                                proxy: pick.clone(),
                            }),
                        ));
                    }
                    column = column.child(options);
                }
            }
        }
        div()
            .id("control")
            .size_full()
            .overflow_y_scroll()
            .child(column)
            .into_any_element()
    }

    fn render_connections(&mut self, p: Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let count = self.conns.len();
        let header = div()
            .flex()
            .justify_between()
            .items_center()
            .mb_2()
            .text_size(px(12.))
            .text_color(p.dim)
            .child(format!("{count} active"))
            .child(button(
                "close-all",
                "Close All",
                p,
                false,
                count > 0,
                self.cmd(|| Cmd::CloseAll),
            ));
        let list = uniform_list(
            "connections",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _, _| {
                range
                    .map(|ix| {
                        let conn = &this.conns[ix];
                        let chain: Vec<&str> =
                            conn.chains.iter().rev().map(String::as_str).collect();
                        let process = if conn.metadata.process.is_empty() {
                            "-"
                        } else {
                            &conn.metadata.process
                        };
                        let id = conn.id.clone();
                        div()
                            .id(ix)
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .h(px(44.))
                            .gap_2()
                            .border_b_1()
                            .border_color(p.border)
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .child(div().text_color(p.text).truncate().child(format!(
                                        "{} ({})",
                                        conn.destination(),
                                        conn.metadata.network
                                    )))
                                    .child(div().text_color(p.dim).truncate().child(format!(
                                        "{process} · {} · ↑{} ↓{}",
                                        chain.join(" → "),
                                        human(conn.upload),
                                        human(conn.download),
                                    ))),
                            )
                            .child(button(
                                SharedString::from(format!("close-{id}")),
                                "Close",
                                p,
                                false,
                                true,
                                this.cmd(move || Cmd::CloseConnection(id.clone())),
                            ))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .w_full()
        .flex_1();
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(list)
            .into_any_element()
    }

    fn render_logs(&mut self, p: Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut bar = div().flex().items_center().gap_1().mb_2();
        for (level, label) in [("all", "All"), ("warning", "Warnings"), ("error", "Errors")] {
            bar = bar.child(button(
                level,
                label,
                p,
                self.log_level == level,
                true,
                cx.listener(move |this, _, _, cx| {
                    this.log_level = level;
                    this.rebuild_logs();
                    cx.notify();
                }),
            ));
        }
        // shortcut: typed characters only (no IME composition or selection), a full input needs GPUI's EntityInputHandler.
        let filter_text = if self.log_filter.is_empty() {
            "Type to filter".to_owned()
        } else {
            self.log_filter.clone()
        };
        bar = bar
            .child(
                div()
                    .id("log-filter")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .px_2()
                    .py(px(3.))
                    .rounded(px(5.))
                    .bg(p.panel)
                    .border_1()
                    .border_color(if self.log_filter.is_empty() {
                        p.border
                    } else {
                        p.accent
                    })
                    .text_size(px(12.))
                    .text_color(if self.log_filter.is_empty() {
                        p.dim
                    } else {
                        p.text
                    })
                    .child(filter_text),
            )
            .child(button(
                "pause",
                if self.log_paused { "Resume" } else { "Pause" },
                p,
                self.log_paused,
                true,
                cx.listener(|this, _, _, cx| {
                    this.log_paused = !this.log_paused;
                    this.rebuild_logs();
                    cx.notify();
                }),
            ))
            .child(button(
                "clear",
                "Clear",
                p,
                false,
                true,
                cx.listener(|this, _, _, cx| {
                    this.handle.logs.lock().unwrap().clear();
                    this.log_view.clear();
                    cx.notify();
                }),
            ));
        let count = self.log_view.len();
        let list = uniform_list(
            "logs",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _, _| {
                range
                    .map(|ix| {
                        let line = &this.log_view[ix];
                        let color = match &*line.level {
                            "error" => p.danger,
                            "warning" => p.accent,
                            _ => p.text,
                        };
                        div()
                            .id(ix)
                            .w_full()
                            .h(px(18.))
                            .truncate()
                            .text_size(px(11.))
                            .text_color(color)
                            .child(SharedString::from(line.text.clone()))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.log_scroll.clone())
        .w_full()
        .flex_1();
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(bar)
            .child(list)
            .into_any_element()
    }

    fn render_settings(&mut self, p: Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = self.snap.clone();
        let mut column = div()
            .flex()
            .flex_col()
            .text_size(px(12.))
            .text_color(p.text);

        let mut sets = div().flex().gap_1();
        for set in icons::SETS {
            sets = sets.child(button(
                *set,
                *set,
                p,
                s.icon_set == *set,
                true,
                self.cmd(move || Cmd::SetIconSet((*set).to_owned())),
            ));
        }
        column = column.child(section("MENU BAR ICON", p)).child(sets);

        let mut themes = div().flex().gap_1();
        for (theme, label) in [
            (Theme::Auto, "auto"),
            (Theme::Light, "light"),
            (Theme::Dark, "dark"),
        ] {
            themes = themes.child(button(
                format!("theme-{label}"),
                label,
                p,
                s.theme == theme,
                true,
                self.cmd(move || Cmd::SetTheme(theme)),
            ));
        }
        column = column.child(section("APPEARANCE", p)).child(themes);

        column = column.child(section("PROFILES", p));
        for profile in s.profiles.iter() {
            let path = profile.path.clone();
            let open = profile.path.clone();
            let mut name = profile.name.clone();
            if profile.is_link {
                name.push_str(" (linked)");
            }
            if let Some(problem) = &profile.problem {
                name.push_str(&format!(" — {problem}"));
            }
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .py_1()
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .child(button(
                        SharedString::from(format!("edit-{}", profile.name)),
                        "Open",
                        p,
                        false,
                        profile.problem.is_none(),
                        move |_, _, cx| cx.open_with_system(&open),
                    ))
                    .child(button(
                        SharedString::from(format!("reveal-{}", profile.name)),
                        "Reveal",
                        p,
                        false,
                        true,
                        move |_, _, cx| cx.reveal_path(&path),
                    )),
            );
        }
        column = column.children(self.note(Area::Profiles, p));
        let handle = self.handle.clone();
        let link = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let paths = cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("Link Profile".into()),
            });
            let handle = handle.clone();
            cx.spawn(async move |_| {
                if let Ok(Ok(Some(paths))) = paths.await
                    && let Some(path) = paths.into_iter().next()
                {
                    handle.send(Cmd::LinkProfile(path));
                }
            })
            .detach();
        };
        let config_dir = s.config_dir.clone();
        column = column.child(
            div()
                .flex()
                .gap_1()
                .mt_1()
                .child(button("link", "Link YAML File…", p, false, true, link))
                .child(button(
                    "check-now",
                    "Check Now",
                    p,
                    false,
                    true,
                    self.cmd(|| Cmd::CheckNow),
                ))
                .child(button(
                    "config-dir",
                    "Reveal Config Folder",
                    p,
                    false,
                    true,
                    move |_, _, cx| cx.reveal_path(&config_dir),
                )),
        );
        column = column.child(
            div().mt_1().text_color(p.dim).child("Overrides: overrides.yaml in the config folder. Ports: settings.yaml there, read at launch."),
        ).children(self.note(Area::Files, p));

        // Click switches; right-click offers Delete.
        let busy = s.update_busy.is_some();
        let mut versions = div().flex().flex_wrap().gap_1();
        for version in &s.installed {
            let pick = version.clone();
            versions = versions.child(
                button(
                    format!("core-{version}"),
                    version.clone(),
                    p,
                    s.core_version.as_deref() == Some(version),
                    !busy,
                    self.cmd(move || Cmd::ActivateVersion(pick.clone())),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    Self::open_menu(Deletable::Version(version.clone()), cx),
                ),
            );
        }
        if s.installed.is_empty() {
            versions = versions.child(div().text_color(p.dim).child("No core installed"));
        }
        let mut actions = div().flex().flex_wrap().gap_1().mt_1();
        actions = actions.child(button(
            "check",
            "Check for Update",
            p,
            false,
            !busy,
            self.cmd(|| Cmd::CheckUpdate),
        ));
        if let Some(latest) = &s.latest
            && s.core_version.as_deref() != Some(&latest.version)
        {
            actions = actions.child(button(
                "update",
                format!("Update to {}", latest.version),
                p,
                false,
                !busy,
                self.cmd(|| Cmd::Download),
            ));
        }
        column = column
            .child(section("MIHOMO CORE", p))
            .child(versions)
            .child(actions);
        column = match &s.update_busy {
            Some(status) => column.child(div().mt_1().text_color(p.dim).child(status.clone())),
            None => column.children(self.note(Area::Update, p)),
        };
        div()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .child(column)
            .into_any_element()
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.tab != Tab::Logs
            || event.keystroke.modifiers.platform
            || event.keystroke.modifiers.control
        {
            return;
        }
        match event.keystroke.key.as_str() {
            "backspace" => {
                self.log_filter.pop();
            }
            "escape" => self.log_filter.clear(),
            _ => match &event.keystroke.key_char {
                Some(text) if !text.chars().any(char::is_control) => self.log_filter.push_str(text),
                _ => return,
            },
        }
        self.rebuild_logs();
        cx.notify();
    }
}

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(self.snap.theme, window.appearance());
        #[cfg(test)]
        {
            self.palette = Some(p);
        }
        let menu = self.context_menu.clone().map(|(target, position)| {
            // The version in use and the active profile cannot be deleted.
            let (selector, label, in_use) = match &target {
                Deletable::Version(v) => (
                    "delete-version",
                    "Delete",
                    self.snap.core_version.as_ref() == Some(v),
                ),
                Deletable::Profile(name) => (
                    "remove-profile",
                    "Remove",
                    self.snap.active.as_ref() == Some(name),
                ),
            };
            let delete = div()
                .id(selector)
                .debug_selector(|| selector.to_owned())
                .px_2()
                .py_1()
                .rounded(px(4.))
                .text_size(px(12.))
                .text_color(if in_use { p.dim } else { p.danger })
                .child(label);
            let delete = if in_use {
                delete
            } else {
                delete
                    .cursor_pointer()
                    .hover(move |st| st.bg(p.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.handle.send(match target.clone() {
                            Deletable::Version(v) => Cmd::DeleteVersion(v),
                            Deletable::Profile(name) => Cmd::RemoveProfile(name),
                        });
                        this.context_menu = None;
                        cx.notify();
                    }))
            };
            deferred(
                anchored().position(position).child(
                    div()
                        // Clicks on the menu must not reach the controls under it.
                        .occlude()
                        .min_w(px(120.))
                        .p_1()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(p.border)
                        .bg(p.panel)
                        .shadow_md()
                        // A right-click elsewhere may be opening this menu on another item.
                        .on_mouse_down_out(cx.listener(
                            |this, event: &gpui::MouseDownEvent, _, cx| {
                                if event.button == MouseButton::Left {
                                    this.context_menu = None;
                                    cx.notify();
                                }
                            },
                        ))
                        .child(delete),
                ),
            )
        });
        let body = match self.tab {
            Tab::Control => self.render_control(p, cx),
            Tab::Connections => self.render_connections(p, cx),
            Tab::Logs => self.render_logs(p, cx),
            Tab::Settings => self.render_settings(p, cx),
        };
        let mut tabs = div().flex().gap_1().mb_3();
        for (tab, label, id) in [
            (Tab::Control, "Control", "tab-control"),
            (Tab::Connections, "Connections", "tab-connections"),
            (Tab::Logs, "Logs", "tab-logs"),
            (Tab::Settings, "Settings", "tab-settings"),
        ] {
            tabs = tabs.child(button(
                id,
                label,
                p,
                self.tab == tab,
                true,
                cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)),
            ));
        }
        div()
            .track_focus(&self.focus)
            .key_context("xihomo")
            .on_key_down(cx.listener(|this, event, _, cx| this.on_key(event, cx)))
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(cx.listener(|this, _: &ShowControl, _, cx| this.set_tab(Tab::Control, cx)))
            .on_action(
                cx.listener(|this, _: &ShowConnections, _, cx| this.set_tab(Tab::Connections, cx)),
            )
            .on_action(cx.listener(|this, _: &ShowLogs, _, cx| this.set_tab(Tab::Logs, cx)))
            .on_action(cx.listener(|this, _: &ShowSettings, _, cx| this.set_tab(Tab::Settings, cx)))
            .flex()
            .flex_col()
            .size_full()
            .p_3()
            .pt_0()
            .bg(p.bg)
            .text_color(p.text)
            .child(
                // AppKit still drags the window here; GPUI only forwards the double-click.
                div()
                    .id("titlebar")
                    .flex_none()
                    .h(TITLEBAR_HEIGHT)
                    .on_click(|event, window, _| {
                        if event.click_count() == 2 {
                            window.titlebar_double_click();
                        }
                    }),
            )
            .child(tabs)
            .child(div().flex_1().min_h_0().child(body))
            .children(menu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_filter_combines_level_and_case_insensitive_text() {
        let line = |level: &str, text: &str| LogLine {
            level: level.into(),
            text: text.into(),
        };
        let lines = [
            line("info", "GitHub ok"),
            line("warning", "github slow"),
            line("error", "dns fail"),
        ];
        assert_eq!(filter_logs(lines.iter(), "all", "").len(), 3);
        assert_eq!(filter_logs(lines.iter(), "warning", "").len(), 2);
        assert_eq!(filter_logs(lines.iter(), "error", "").len(), 1);
        assert_eq!(filter_logs(lines.iter(), "all", "GITHUB").len(), 2);
        assert_eq!(filter_logs(lines.iter(), "error", "github").len(), 0);
    }

    #[test]
    fn theme_overrides_the_window_appearance_only_when_fixed() {
        let light = palette(Theme::Light, WindowAppearance::Light);
        let dark = palette(Theme::Dark, WindowAppearance::Dark);
        assert_ne!(light, dark);
        for appearance in [WindowAppearance::Light, WindowAppearance::VibrantDark] {
            assert_eq!(palette(Theme::Light, appearance), light);
            assert_eq!(palette(Theme::Dark, appearance), dark);
        }
        assert_eq!(palette(Theme::Auto, WindowAppearance::VibrantLight), light);
        assert_eq!(palette(Theme::Auto, WindowAppearance::VibrantDark), dark);
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(2048), "2.0 KB");
        assert_eq!(human(5 * 1024 * 1024), "5.0 MB");
    }
}

/// In-process UI tests: no real window, mouse, keyboard or focus change on the desktop.
#[cfg(test)]
mod gpui_tests {
    use super::*;
    use crate::api::{Group, Metadata};
    use crate::backend::Snapshot;
    use crate::profiles::Profile;
    use gpui::{Entity, KeyBinding, Modifiers, TestAppContext, VisualTestContext};
    use tokio::sync::mpsc::UnboundedReceiver;

    fn running() -> Snapshot {
        let profile = |name: &str, broken: bool| Profile {
            name: name.into(),
            path: format!("/p/{name}.yaml").into(),
            is_link: false,
            problem: broken.then(|| "missing".to_owned()),
        };
        Snapshot {
            phase: Phase::Running,
            core_version: Some("v1.19.33".into()),
            active: Some("home".into()),
            profiles: vec![
                profile("home", false),
                profile("work", false),
                profile("gone", true),
            ],
            mode: "rule".into(),
            groups: vec![
                Group {
                    name: "PICK".into(),
                    kind: "Selector".into(),
                    now: "DIRECT".into(),
                    all: vec!["DIRECT".into(), "REJECT".into()],
                },
                Group {
                    name: "AUTO".into(),
                    kind: "URLTest".into(),
                    now: "A".into(),
                    all: vec!["A".into(), "B".into()],
                },
            ],
            ..Snapshot::default()
        }
    }

    struct Ui<'a> {
        view: Entity<AppState>,
        cx: &'a mut VisualTestContext,
        rx: UnboundedReceiver<Cmd>,
    }

    fn open(cx: &mut TestAppContext, snapshot: Snapshot) -> Ui<'_> {
        cx.update(|cx| {
            cx.bind_keys([
                KeyBinding::new("cmd-w", CloseWindow, None),
                KeyBinding::new("cmd-1", ShowControl, None),
                KeyBinding::new("cmd-2", ShowConnections, None),
                KeyBinding::new("cmd-3", ShowLogs, None),
                KeyBinding::new("cmd-4", ShowSettings, None),
            ])
        });
        let (handle, rx) = Handle::detached(snapshot);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let state = AppState::new(handle, cx);
            state.focus.focus(window);
            state
        });
        let window = cx.window_handle().downcast::<AppState>();
        view.update(cx, |this, _| this.window = window);
        let mut ui = Ui { view, cx, rx };
        ui.frame();
        ui.drain();
        ui
    }

    impl Ui<'_> {
        /// The test window has no display link, so layout happens only when asked.
        fn frame(&mut self) {
            self.cx.update(|window, cx| window.draw(cx).clear());
            self.cx.run_until_parked();
        }

        fn click(&mut self, selector: &'static str) {
            let bounds = self
                .cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is not on screen"));
            self.cx
                .simulate_click(bounds.center(), Modifiers::default());
            self.frame();
        }

        fn right_click(&mut self, selector: &'static str) {
            let position = self.cx.debug_bounds(selector).unwrap().center();
            self.cx.simulate_event(gpui::MouseDownEvent {
                button: MouseButton::Right,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            });
            self.cx.simulate_event(gpui::MouseUpEvent {
                button: MouseButton::Right,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
            });
            self.frame();
        }

        fn keys(&mut self, keys: &str) {
            self.cx.simulate_keystrokes(keys);
            self.frame();
        }

        fn sent(&mut self) -> Vec<Cmd> {
            std::iter::from_fn(|| self.rx.try_recv().ok()).collect()
        }

        fn drain(&mut self) {
            self.sent();
        }

        fn set(&mut self, f: impl FnOnce(&mut AppState)) {
            self.view.update(self.cx, |this, cx| {
                f(this);
                cx.notify();
            });
            self.frame();
        }

        fn read<R>(&mut self, f: impl FnOnce(&AppState) -> R) -> R {
            self.view.read_with(self.cx, |this, _| f(this))
        }
    }

    #[gpui::test]
    fn tabs_switch_by_click_and_shortcut_and_poll_only_on_connections(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        ui.click("tab-logs");
        assert_eq!(ui.read(|s| s.tab), Tab::Logs);
        assert!(matches!(ui.sent()[..], [Cmd::WatchConnections(false)]));
        ui.keys("cmd-2");
        assert_eq!(ui.read(|s| s.tab), Tab::Connections);
        assert!(matches!(ui.sent()[..], [Cmd::WatchConnections(true)]));
        ui.keys("cmd-4");
        assert_eq!(ui.read(|s| s.tab), Tab::Settings);
        ui.keys("cmd-1");
        assert_eq!(ui.read(|s| s.tab), Tab::Control);
    }

    #[gpui::test]
    fn core_and_proxy_buttons_follow_phase(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        ui.click("core");
        assert!(matches!(ui.sent()[..], [Cmd::Stop]));
        ui.click("proxy");
        assert!(matches!(ui.sent()[..], [Cmd::SetProxy(true)]));

        ui.set(|s| {
            s.snap = Arc::new(Snapshot {
                phase: Phase::Failed("boom".into()),
                ..running()
            })
        });
        ui.click("core");
        assert!(matches!(ui.sent()[..], [Cmd::Start]));
        ui.click("proxy");
        assert!(
            ui.sent().is_empty(),
            "proxy cannot be enabled without a running core"
        );

        // Turning a leftover proxy off stays possible.
        ui.set(|s| {
            s.snap = Arc::new(Snapshot {
                phase: Phase::Stopped,
                proxy_on: true,
                ..running()
            })
        });
        ui.click("proxy");
        assert!(matches!(ui.sent()[..], [Cmd::SetProxy(false)]));
    }

    #[gpui::test]
    fn profiles_modes_and_groups_send_commands(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        ui.click("profile-work");
        assert!(matches!(&ui.sent()[..], [Cmd::Activate(name)] if name == "work"));
        ui.click("profile-gone");
        assert!(ui.sent().is_empty(), "broken profile is disabled");

        ui.click("global");
        assert!(matches!(&ui.sent()[..], [Cmd::SetMode(mode)] if mode == "global"));

        assert!(
            ui.cx.debug_bounds("pick-PICK-REJECT").is_none(),
            "groups start collapsed"
        );
        ui.click("group-PICK");
        ui.click("pick-PICK-REJECT");
        assert!(
            matches!(&ui.sent()[..], [Cmd::Select { group, proxy }] if group == "PICK" && proxy == "REJECT")
        );

        ui.click("group-AUTO");
        ui.click("pick-AUTO-B");
        assert!(ui.sent().is_empty(), "automatic groups are read-only");
    }

    #[gpui::test]
    fn mode_row_is_shown_and_usable_while_stopped(cx: &mut TestAppContext) {
        let stopped = Snapshot {
            phase: Phase::Stopped,
            mode: "direct".into(),
            groups: Vec::new(),
            ..running()
        };
        let mut ui = open(cx, stopped);
        assert!(ui.cx.debug_bounds("group-PICK").is_none(), "no groups");
        ui.click("rule");
        assert!(matches!(&ui.sent()[..], [Cmd::SetMode(mode)] if mode == "rule"));
        ui.set(|s| {
            s.snap = Arc::new(Snapshot {
                phase: Phase::Stopping,
                ..(*s.snap).clone()
            })
        });
        ui.click("global");
        assert!(ui.sent().is_empty(), "disabled while stopping");
    }

    #[gpui::test]
    fn global_is_listed_first_in_global_mode_without_reject(cx: &mut TestAppContext) {
        let global = Group {
            name: "GLOBAL".into(),
            kind: "Selector".into(),
            now: "PICK".into(),
            all: vec!["DIRECT".into(), "REJECT".into(), "PICK".into()],
        };
        let mut snap = running();
        snap.mode = "global".into();
        snap.groups.insert(0, global);
        let mut ui = open(cx, snap);
        let y = |ui: &mut Ui, id| ui.cx.debug_bounds(id).unwrap().origin.y;
        assert!(y(&mut ui, "group-GLOBAL") < y(&mut ui, "group-PICK"));
        ui.click("group-GLOBAL");
        assert!(ui.cx.debug_bounds("pick-GLOBAL-DIRECT").is_some());
        assert!(ui.cx.debug_bounds("pick-GLOBAL-REJECT").is_none());
        ui.click("pick-GLOBAL-DIRECT");
        assert!(
            matches!(&ui.sent()[..], [Cmd::Select { group, proxy }] if group == "GLOBAL" && proxy == "DIRECT")
        );
    }

    #[gpui::test]
    fn global_is_hidden_outside_global_mode(cx: &mut TestAppContext) {
        let mut snap = running();
        snap.groups.insert(
            0,
            Group {
                name: "GLOBAL".into(),
                kind: "Selector".into(),
                now: "DIRECT".into(),
                all: vec!["DIRECT".into()],
            },
        );
        let ui = open(cx, snap);
        assert!(ui.cx.debug_bounds("group-GLOBAL").is_none());
        assert!(ui.cx.debug_bounds("group-PICK").is_some());
    }

    #[gpui::test]
    fn connections_can_be_closed_one_by_one_or_all(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        let conn = |id: &str| Connection {
            id: id.into(),
            metadata: Metadata {
                host: format!("{id}.example"),
                destination_port: "443".into(),
                network: "tcp".into(),
                ..Metadata::default()
            },
            chains: vec!["DIRECT".into(), "PICK".into()],
            ..Connection::default()
        };
        ui.set(|s| s.conns = Arc::new(vec![conn("c1"), conn("c2")]));
        ui.keys("cmd-2");
        ui.drain();
        ui.click("close-c2");
        assert!(matches!(&ui.sent()[..], [Cmd::CloseConnection(id)] if id == "c2"));
        ui.click("close-all");
        assert!(matches!(ui.sent()[..], [Cmd::CloseAll]));
    }

    #[gpui::test]
    fn logs_filter_by_level_and_typed_text(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        ui.read(|s| {
            let mut logs = s.handle.logs.lock().unwrap();
            for (level, text) in [
                ("info", "GitHub ok"),
                ("warning", "github slow"),
                ("error", "dns fail"),
            ] {
                logs.push(LogLine {
                    level: level.into(),
                    text: text.into(),
                });
            }
        });
        ui.keys("cmd-3");
        let shown = |ui: &mut Ui| {
            ui.read(|s| {
                s.log_view
                    .iter()
                    .map(|l| l.text.to_string())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(shown(&mut ui).len(), 3);

        ui.keys("g i t");
        assert_eq!(ui.read(|s| s.log_filter.clone()), "git");
        assert_eq!(shown(&mut ui), ["GitHub ok", "github slow"]);
        ui.click("warning");
        assert_eq!(shown(&mut ui), ["github slow"]);
        ui.keys("backspace backspace backspace");
        assert_eq!(shown(&mut ui), ["github slow", "dns fail"]);
        ui.keys("x escape");
        assert_eq!(ui.read(|s| s.log_filter.clone()), "");

        ui.click("pause");
        ui.read(|s| {
            s.handle.logs.lock().unwrap().push(LogLine {
                level: "error".into(),
                text: "late".into(),
            })
        });
        ui.set(|s| s.rebuild_logs());
        assert_eq!(shown(&mut ui).len(), 2, "paused view does not change");
        ui.click("clear");
        assert!(shown(&mut ui).is_empty());
    }

    #[gpui::test]
    fn settings_send_icon_and_core_commands(cx: &mut TestAppContext) {
        let snapshot = Snapshot {
            previous_version: Some("v1.19.32".into()),
            installed: vec!["v1.19.32".into(), "v1.19.33".into()],
            ..running()
        };
        let mut ui = open(cx, snapshot);
        ui.keys("cmd-4");
        ui.drain();
        ui.click("bars");
        assert!(matches!(&ui.sent()[..], [Cmd::SetIconSet(set)] if set == "bars"));
        ui.click("check");
        assert!(matches!(ui.sent()[..], [Cmd::CheckUpdate]));
        assert!(ui.cx.debug_bounds("rollback").is_none());
        ui.click("core-v1.19.32");
        assert!(matches!(&ui.sent()[..], [Cmd::ActivateVersion(v)] if v == "v1.19.32"));

        // Right-click offers Delete.
        ui.right_click("core-v1.19.32");
        ui.click("delete-version");
        assert!(matches!(&ui.sent()[..], [Cmd::DeleteVersion(v)] if v == "v1.19.32"));
        assert!(ui.read(|s| s.context_menu.is_none()));

        ui.set(|s| {
            let mut snap = (*s.snap).clone();
            snap.latest = Some(crate::update::Release {
                version: "v1.20.0".into(),
                url: String::new(),
                sha256: String::new(),
            });
            s.snap = Arc::new(snap);
        });
        ui.click("update");
        assert!(matches!(ui.sent()[..], [Cmd::Download]));
        ui.set(|s| {
            s.snap = Arc::new(Snapshot {
                update_busy: Some("Downloading".into()),
                ..(*s.snap).clone()
            })
        });
        ui.click("check");
        assert!(
            ui.sent().is_empty(),
            "update actions are disabled while one runs"
        );
    }

    #[gpui::test]
    fn the_core_version_in_use_cannot_be_deleted(cx: &mut TestAppContext) {
        let snapshot = Snapshot {
            installed: vec!["v1.19.32".into(), "v1.19.33".into()],
            ..running()
        };
        let mut ui = open(cx, snapshot);
        ui.keys("cmd-4");
        ui.drain();
        ui.right_click("core-v1.19.33");
        assert!(ui.read(|s| s.context_menu.is_some()));
        ui.click("delete-version");
        assert!(ui.sent().is_empty());
    }

    #[gpui::test]
    fn profiles_are_removed_by_right_click_except_the_active_one(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        ui.right_click("profile-home");
        ui.click("remove-profile");
        assert!(ui.sent().is_empty(), "the active profile stays");
        // A broken link cannot be activated but can be removed.
        ui.right_click("profile-gone");
        ui.click("remove-profile");
        assert!(matches!(&ui.sent()[..], [Cmd::RemoveProfile(n)] if n == "gone"));
        assert!(ui.read(|s| s.context_menu.is_none()));
    }

    #[gpui::test]
    fn proxy_button_is_disabled_while_switching(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        ui.set(|s| {
            s.snap = Arc::new(Snapshot {
                proxy_busy: Some(true),
                ..running()
            })
        });
        ui.click("proxy");
        assert!(ui.sent().is_empty());
        // Another tool's proxy shows as on, and one click turns it off.
        ui.set(|s| {
            s.snap = Arc::new(Snapshot {
                phase: Phase::Stopped,
                proxy_on: true,
                ..running()
            })
        });
        ui.click("proxy");
        assert!(matches!(ui.sent()[..], [Cmd::SetProxy(false)]));
    }

    #[gpui::test]
    fn theme_choice_is_sent_and_the_window_follows_it(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        ui.keys("cmd-4");
        ui.drain();
        ui.click("theme-dark");
        assert!(matches!(ui.sent()[..], [Cmd::SetTheme(Theme::Dark)]));
        ui.click("check-now");
        assert!(matches!(ui.sent()[..], [Cmd::CheckNow]));

        // The backend's answer arrives as a snapshot; the next frame uses its palette.
        let appearance = ui.cx.update(|window, _| window.appearance());
        for theme in [Theme::Dark, Theme::Light, Theme::Auto] {
            ui.set(|s| {
                s.snap = Arc::new(Snapshot {
                    theme,
                    ..(*s.snap).clone()
                })
            });
            assert_eq!(
                ui.read(|s| s.palette),
                Some(palette(theme, appearance)),
                "{theme:?}"
            );
        }
    }

    #[gpui::test]
    fn cmd_w_closes_the_window(cx: &mut TestAppContext) {
        let ui = open(cx, running());
        ui.cx.simulate_keystrokes("cmd-w");
        ui.cx.run_until_parked();
        assert!(ui.cx.cx.update(|cx| cx.windows().is_empty()));
    }
}
