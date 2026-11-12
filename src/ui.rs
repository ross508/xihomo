use crate::api::{Connection, LogLine};
use crate::backend::{Area, Cmd, Handle, Message, Phase, Snapshot};
use crate::icons;
use crate::input::{Cancelled, Changed, Submitted, TextInput};
use crate::placement::{self, Placement};
use crate::settings::{self, Theme};
use crate::tray::phase_label;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, ElementId, Entity, FocusHandle,
    FontWeight, Hsla, MouseButton, PathBuilder, PathPromptOptions, Pixels, Point, ScrollStrategy,
    SharedString, Stateful, Task, UniformListScrollHandle, Window, WindowAppearance, WindowHandle,
    actions, anchored, canvas, deferred, div, point, px, rgb, uniform_list,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
enum Tab {
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

/// Case-insensitive match on what a connection row shows, plus its rule.
fn connection_matches(conn: &Connection, needle: &str) -> bool {
    needle.is_empty()
        || [
            conn.destination(),
            conn.metadata.network.clone(),
            conn.metadata.process.clone(),
            conn.chains.join(" "),
            conn.rule.clone(),
        ]
        .iter()
        .any(|field| field.to_lowercase().contains(needle))
}

pub struct AppState {
    pub handle: Handle,
    pub snap: Arc<Snapshot>,
    pub conns: Arc<Vec<Connection>>,
    tab: Tab,
    pub window: Option<WindowHandle<AppState>>,
    expanded: HashSet<String>,
    log_view: Vec<LogLine>,
    log_level: &'static str,
    log_filter: String,
    log_input: Entity<TextInput>,
    /// Lowercased, like the log filter.
    conn_filter: String,
    conn_input: Entity<TextInput>,
    log_paused: bool,
    log_scroll: UniformListScrollHandle,
    pub focus: FocusHandle,
    /// What the open right-click menu would delete, and where it is.
    context_menu: Option<(Deletable, Point<Pixels>)>,
    /// Where the window's placement is kept; tests leave it unset and write nothing.
    pub placement_file: Option<PathBuf>,
    /// The latest placement and its delayed save; replacing it cancels the earlier save.
    placement_save: Option<(Placement, Task<()>)>,
    proxy_port: PortField,
    api_port: PortField,
    /// The secret is shown in full; hidden again on a tab change or when the window closes.
    secret_shown: bool,
    /// The button waiting for its confirming click, and the timer token that may disarm it.
    confirm: Option<(String, u64)>,
    /// "Copied" next to a section title.
    copied: Option<(Area, u64)>,
    timers: u64,
    /// Messages that have been on screen, by id; `true` once they timed out.
    shown: HashMap<u64, bool>,
    /// The palette of the last frame, so tests can see what was drawn.
    #[cfg(test)]
    palette: Option<Palette>,
}

/// The two port fields in Settings.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Port {
    Proxy,
    Api,
}

/// A port input and whether the user has typed in it since it last showed the saved value.
struct PortField {
    input: Entity<TextInput>,
    edited: bool,
}

/// How long a success or information message stays next to its section title.
const MESSAGE_SHOWN: Duration = Duration::from_secs(4);
/// How long Regenerate waits for its confirming click.
const CONFIRM_WINDOW: Duration = Duration::from_secs(3);

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
    /// Text that acts as a button: the accent's hue, with more contrast than the accent fill.
    link: Hsla,
    warning: Hsla,
    danger: Hsla,
}

/// Two fixed palettes after an OS X El Capitan-inspired light theme and a Vibrant Dark-inspired
/// dark one, with Aqua's Graphite appearance as the accent. `Auto` follows the window's appearance.
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
            accent: c(0x66778D),
            on_accent: c(0xFFFFFF),
            link: c(0x9DB0C8),
            warning: c(0xE3C37A),
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
            accent: c(0x5F6F84),
            on_accent: c(0xFFFFFF),
            link: c(0x34465E),
            warning: c(0x795300),
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

/// Node cards are at least this wide, in points; how many fit sets the column count.
const CARD_MIN_WIDTH: f32 = 180.;
/// `gap_1` between cards.
const CARD_GAP: f32 = 4.;

/// As many equal columns as fit cards of the minimum width, and never fewer than two. A card is
/// then below `(n + 1) / n` of the minimum, so at most about 1.5 times it.
fn node_columns(width: f32) -> u16 {
    (((width + CARD_GAP) / (CARD_MIN_WIDTH + CARD_GAP)).floor() as u16).max(2)
}

/// Control's first line: phase, core, ports and whether the proxy is shared.
fn status_text(s: &Snapshot) -> String {
    let channel = if settings::DEV { " · dev" } else { "" };
    let lan = if s.allow_lan { " · LAN" } else { "" };
    match &s.phase {
        Phase::Failed(reason) => format!("Failed: {reason}{channel}"),
        phase => format!(
            "{} · {} · proxy {}{lan} · API {}{channel}",
            phase_label(phase),
            s.core_version.as_deref().unwrap_or("no core"),
            s.mixed_port,
            s.controller_port,
        ),
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
    chip(id, p, selected, enabled, on_click).child(label.into())
}

/// A button's frame, states and click handling, without its content.
fn chip(
    id: impl Into<SharedString>,
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
        .flex_none();
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

/// Symbols for the icon buttons at the right of a row, drawn on a 14-point grid.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Icon {
    /// Two half-circle arrows: fetch again, a profile's providers or a newer core.
    Update,
    /// A circular arrow: make a new one.
    Regenerate,
    Delete,
    /// Look for something new.
    Search,
    /// An open eye: show what is masked.
    Show,
    /// A struck-out eye: mask it again.
    Hide,
    /// A destructive button waiting for its second click.
    Confirm,
}

fn symbol(icon: Icon, color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let at = |x: f32, y: f32| {
                point(
                    bounds.left() + bounds.size.width * (x / 14.),
                    bounds.top() + bounds.size.height * (y / 14.),
                )
            };
            let unit = bounds.size.width / 14.;
            let line = |points: &[(f32, f32)]| {
                let mut path = PathBuilder::stroke(unit * 1.4);
                path.add_polygon(
                    &points.iter().map(|&(x, y)| at(x, y)).collect::<Vec<_>>(),
                    false,
                );
                path
            };
            let polar = |degrees: f32, r: f32| {
                let a = degrees.to_radians();
                (7. + r * a.cos(), 7. + r * a.sin())
            };
            let mut strokes = Vec::new();
            let mut fills = Vec::new();
            // An arc around the centre, clockwise from `from` to `to` degrees (0 is right, 90 is
            // down), with an arrowhead at its end pointing the way it turns.
            let mut arrow_arc = |from: f32, to: f32, r: f32| {
                let (start, end) = (polar(from, r), polar(to, r));
                let mut arc = PathBuilder::stroke(unit * 1.4);
                arc.move_to(at(start.0, start.1));
                arc.arc_to(
                    point(unit * r, unit * r),
                    px(0.),
                    to - from > 180.,
                    true,
                    at(end.0, end.1),
                );
                strokes.push(arc);
                let (dx, dy) = ((to + 90.).to_radians().cos(), (to + 90.).to_radians().sin());
                let (nx, ny) = (-dy, dx);
                let tip = (end.0 + dx * 1.8, end.1 + dy * 1.8);
                let back = (end.0 - dx * 0.8, end.1 - dy * 0.8);
                let mut head = PathBuilder::fill();
                head.add_polygon(
                    &[
                        at(tip.0, tip.1),
                        at(back.0 + nx * 2.2, back.1 + ny * 2.2),
                        at(back.0 - nx * 2.2, back.1 - ny * 2.2),
                    ],
                    true,
                );
                fills.push(head);
            };
            match icon {
                Icon::Update => {
                    arrow_arc(-165., -25., 4.6);
                    arrow_arc(15., 155., 4.6);
                }
                // Most of a circle, clockwise, ending in an arrowhead near the top.
                Icon::Regenerate => arrow_arc(-30., 250., 4.6),
                // As wide as the round icons and a little shorter, so it does not look narrower
                // than them in a button of the same size.
                Icon::Delete => {
                    strokes.push(line(&[(1.6, 3.6), (12.4, 3.6)]));
                    strokes.push(line(&[(5.2, 3.6), (5.2, 1.9), (8.8, 1.9), (8.8, 3.6)]));
                    strokes.push(line(&[(3., 3.6), (3.7, 12.1), (10.3, 12.1), (11., 3.6)]));
                    strokes.push(line(&[(5.6, 6.), (5.8, 9.8)]));
                    strokes.push(line(&[(8.4, 6.), (8.2, 9.8)]));
                }
                Icon::Search => {
                    let r = unit * 3.9;
                    let mut ring = PathBuilder::stroke(unit * 1.4);
                    ring.move_to(at(2., 6.));
                    ring.arc_to(point(r, r), px(0.), false, true, at(9.8, 6.));
                    ring.arc_to(point(r, r), px(0.), false, true, at(2., 6.));
                    strokes.push(ring);
                    strokes.push(line(&[(8.8, 8.8), (12.3, 12.3)]));
                }
                // Wider than it is tall, like an eye, spanning the round icons' width so it
                // weighs about the same.
                Icon::Show | Icon::Hide => {
                    let r = unit * 6.2;
                    let mut lid = PathBuilder::stroke(unit * 1.4);
                    lid.move_to(at(1.2, 7.));
                    lid.arc_to(point(r, r), px(0.), false, true, at(12.8, 7.));
                    lid.arc_to(point(r, r), px(0.), false, true, at(1.2, 7.));
                    strokes.push(lid);
                    let r = unit * 2.1;
                    let mut pupil = PathBuilder::fill();
                    pupil.move_to(at(4.9, 7.));
                    pupil.arc_to(point(r, r), px(0.), false, true, at(9.1, 7.));
                    pupil.arc_to(point(r, r), px(0.), false, true, at(4.9, 7.));
                    fills.push(pupil);
                    if icon == Icon::Hide {
                        strokes.push(line(&[(2.4, 1.6), (11.6, 12.4)]));
                    }
                }
                Icon::Confirm => {
                    let mut check = PathBuilder::stroke(unit * 1.8);
                    check.add_polygon(&[at(2.8, 7.4), at(5.8, 10.4), at(11.4, 3.8)], false);
                    strokes.push(check);
                }
            }
            for path in strokes.into_iter().chain(fills) {
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .size(px(14.))
}

/// A hover label for an icon button.
struct Tip {
    text: SharedString,
    p: Palette,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(px(5.))
            .border_1()
            .border_color(self.p.border)
            .bg(self.p.panel)
            .shadow_md()
            .text_size(px(11.))
            .text_color(self.p.text)
            .child(self.text.clone())
    }
}

/// A borderless button for secondary actions: accent text, a background only under the pointer.
fn text_button(
    id: &'static str,
    label: &'static str,
    p: Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .px(px(6.))
        .py(px(3.))
        .rounded(px(5.))
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(p.link)
        .cursor_pointer()
        .hover(move |s| s.bg(p.hover))
        .on_click(on_click)
        .child(label)
}

/// A square button with a symbol, so the buttons at the right of the rows line up whatever they
/// do; the tooltip says what.
fn icon_button(
    id: impl Into<SharedString>,
    icon: Icon,
    tip: impl Into<SharedString>,
    p: Palette,
    selected: bool,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<gpui::Div> {
    let color = match (selected, enabled) {
        (true, _) => p.on_accent,
        (false, true) => p.text,
        (false, false) => p.dim.opacity(0.6),
    };
    let (id, tip): (SharedString, SharedString) = (id.into(), tip.into());
    // A tooltip already waiting to show keeps the text it was built with, so a button that
    // changes what it does gets a new element for each state; the selector stays the same.
    let selector = id.to_string();
    chip(format!("{id}:{tip}"), p, selected, enabled, on_click)
        .debug_selector(|| selector)
        .w(px(28.))
        .h(px(24.))
        .px_0()
        .py_0()
        .flex()
        .items_center()
        .justify_center()
        .tooltip(move |_, cx| {
            let text = tip.clone();
            cx.new(|_| Tip { text, p }).into()
        })
        .child(symbol(icon, color))
}

/// An icon set's solid shape, drawn as vector paths in `color`.
fn glyph(set: &str, color: Hsla) -> impl IntoElement {
    let shape = icons::outline(set);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            for polygon in &shape {
                let at = |&(x, y): &(f32, f32)| {
                    point(
                        bounds.left() + bounds.size.width * x,
                        bounds.top() + bounds.size.height * y,
                    )
                };
                let mut path = PathBuilder::fill();
                path.add_polygon(&polygon.iter().map(at).collect::<Vec<_>>(), true);
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .size(px(14.))
}

/// A filter input framed like the other controls; accent-bordered while it filters.
fn filter_field(
    id: &'static str,
    input: &Entity<TextInput>,
    active: bool,
    p: Palette,
) -> gpui::Div {
    div()
        .debug_selector(|| id.to_owned())
        .flex_1()
        .min_w_0()
        .rounded(px(5.))
        .bg(p.panel)
        .border_1()
        .border_color(if active { p.accent } else { p.border })
        .text_size(px(12.))
        .text_color(p.text)
        .overflow_hidden()
        .child(input.clone())
}

fn port_value(text: &str, default: u16) -> u32 {
    text.parse().unwrap_or(default.into())
}

/// The secret's last four characters, so it can be told apart without being shown.
/// As long as the secret itself, so Show does not move what follows it.
fn masked(secret: &str) -> String {
    let split = secret.len().saturating_sub(4);
    let tail = secret.get(split..).unwrap_or_default();
    format!("{}{tail}", "•".repeat(split))
}

/// A footnote under the controls it explains: smaller and fainter than a label.
fn hint(text: String, p: Palette) -> gpui::Div {
    div()
        .mt(px(6.))
        .text_size(px(11.))
        .text_color(p.dim.opacity(0.8))
        .child(text)
}

/// Width of the label column, so every row's controls start at the same edge.
const LABEL_WIDTH: Pixels = px(104.);

/// The height of a button or field, the line every part of a Settings row is centred on.
const CONTROL_HEIGHT: Pixels = px(27.);

/// A Settings row in three parts: the label, the controls (wrapping inside their own area), and
/// the row's icon buttons pinned to the right of the first line.
fn field(
    text: &'static str,
    controls: impl IntoElement,
    action: Option<AnyElement>,
    p: Palette,
) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap_2()
        .py(px(3.))
        .child(
            div()
                .debug_selector(move || format!("label-{text}"))
                .w(LABEL_WIDTH)
                .h(CONTROL_HEIGHT)
                .flex_none()
                .flex()
                .items_center()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.dim)
                .child(text),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                // Text alone, without a control, still lines up with the label.
                .min_h(CONTROL_HEIGHT)
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .child(controls),
        )
        // Icon buttons are shorter than the other controls; they stay on the first line.
        .children(action.map(|action| {
            div()
                .h(CONTROL_HEIGHT)
                .flex_none()
                .flex()
                .items_center()
                .child(action)
        }))
}

/// A hairline between groups of sections, fainter than control borders, across the content.
fn separator(p: Palette) -> gpui::Div {
    div().my(px(8.)).h(px(1.)).bg(p.border.opacity(0.55))
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
        let log_input = cx.new(|cx| TextInput::new("Filter logs", cx));
        let conn_input = cx.new(|cx| TextInput::new("Filter connections", cx));
        cx.subscribe(&log_input, |this, input, _: &Changed, cx| {
            this.log_filter = input.read(cx).text().to_owned();
            this.rebuild_logs();
            cx.notify();
        })
        .detach();
        cx.subscribe(&conn_input, |this, input, _: &Changed, cx| {
            this.conn_filter = input.read(cx).text().to_lowercase();
            cx.notify();
        })
        .detach();
        let defaults = settings::Settings::default();
        let proxy_port = Self::port_field(Port::Proxy, defaults.mixed_port, cx);
        let api_port = Self::port_field(Port::Api, defaults.controller_port, cx);
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
            log_input,
            conn_filter: String::new(),
            conn_input,
            log_paused: false,
            log_scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            context_menu: None,
            placement_file: None,
            placement_save: None,
            proxy_port,
            api_port,
            secret_shown: false,
            confirm: None,
            copied: None,
            timers: 0,
            shown: HashMap::new(),
            #[cfg(test)]
            palette: None,
        }
    }

    fn port_field(port: Port, default: u16, cx: &mut Context<Self>) -> PortField {
        let input = cx.new(|cx| TextInput::digits(default.to_string(), 5, cx));
        cx.subscribe(&input, move |this, _, _: &Changed, cx| {
            this.field(port).edited = true;
            cx.notify();
        })
        .detach();
        cx.subscribe(&input, move |this, _, _: &Submitted, cx| {
            this.apply_port(port, cx)
        })
        .detach();
        cx.subscribe(&input, move |this, input, _: &Cancelled, cx| {
            let saved = this.saved_port(port).to_string();
            input.update(cx, |i, cx| i.set_text(&saved, cx));
            this.field(port).edited = false;
            cx.notify();
        })
        .detach();
        PortField {
            input,
            edited: false,
        }
    }

    fn field(&mut self, port: Port) -> &mut PortField {
        match port {
            Port::Proxy => &mut self.proxy_port,
            Port::Api => &mut self.api_port,
        }
    }

    fn saved_port(&self, port: Port) -> u16 {
        match port {
            Port::Proxy => self.snap.mixed_port,
            Port::Api => self.snap.controller_port,
        }
    }

    fn default_port(port: Port) -> u16 {
        let d = settings::Settings::default();
        match port {
            Port::Proxy => d.mixed_port,
            Port::Api => d.controller_port,
        }
    }

    /// The typed value; an empty field means the channel default.
    fn draft(&mut self, port: Port, cx: &App) -> u32 {
        port_value(
            &self.field(port).input.read(cx).text(),
            Self::default_port(port),
        )
    }

    /// Nothing can be applied while the core starts or stops, or another change is applied.
    fn busy(&self) -> bool {
        self.snap.applying.is_some() || matches!(self.snap.phase, Phase::Starting | Phase::Stopping)
    }

    fn apply_port(&mut self, port: Port, cx: &mut Context<Self>) {
        let value = self.draft(port, cx);
        if self.busy() || value == u32::from(self.saved_port(port)) {
            return;
        }
        // An applied empty field shows the default it stands for.
        let input = self.field(port).input.clone();
        if input.read(cx).text().is_empty() {
            input.update(cx, |i, cx| i.set_text(&value.to_string(), cx));
        }
        self.handle.send(match port {
            Port::Proxy => Cmd::SetProxyPort(value),
            Port::Api => Cmd::SetApiPort(value),
        });
        cx.notify();
    }

    /// An unedited field follows the saved value; an edited one keeps the draft until it matches.
    fn sync_port(&mut self, port: Port, cx: &mut Context<Self>) {
        let saved = self.saved_port(port).to_string();
        let field = self.field(port);
        let text = field.input.read(cx).text();
        if field.edited && text == saved {
            field.edited = false;
        }
        if !field.edited && text != saved {
            field.input.update(cx, |i, cx| i.set_text(&saved, cx));
        }
    }

    /// Calls `done` after `delay` with a token; a later timer makes earlier tokens stale.
    fn timer(
        &mut self,
        delay: Duration,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, u64) + 'static,
    ) -> u64 {
        self.timers += 1;
        let token = self.timers;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                done(this, token);
                cx.notify();
            });
        })
        .detach();
        token
    }

    fn copy(&mut self, area: Area, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        let token = self.timer(MESSAGE_SHOWN, cx, move |this, token| {
            if this.copied == Some((area, token)) {
                this.copied = None;
            }
        });
        self.copied = Some((area, token));
        cx.notify();
    }

    /// A destructive button: the first click turns it into a red Confirm for a few seconds, the
    /// second sends `make()`.
    fn confirmable(
        &mut self,
        id: String,
        (icon, tip): (Icon, &'static str),
        enabled: bool,
        make: impl Fn() -> Cmd + 'static,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let armed = enabled && self.confirm.as_ref().is_some_and(|(key, _)| *key == id);
        let key = id.clone();
        let click = cx.listener(move |this, _, _, cx| {
            if this.confirm.as_ref().is_some_and(|(k, _)| *k == key) {
                this.confirm = None;
                this.handle.send(make());
            } else {
                let token = this.timer(CONFIRM_WINDOW, cx, |this, token| {
                    if this.confirm.as_ref().is_some_and(|(_, t)| *t == token) {
                        this.confirm = None;
                    }
                });
                this.confirm = Some((key.clone(), token));
            }
            cx.notify();
        });
        // Drawn as selected, which keeps its fill under the pointer, then made red.
        let button = if armed {
            icon_button(
                id,
                Icon::Confirm,
                "Click again to confirm",
                p,
                true,
                enabled,
                click,
            )
        } else {
            icon_button(id, icon, tip, p, false, enabled, click)
        };
        if armed {
            button.bg(p.danger).border_color(p.danger)
        } else {
            button
        }
    }

    /// The area's message, unless it is a success or information message that has had its time.
    fn current(&mut self, area: Area, cx: &mut Context<Self>) -> Option<Message> {
        let message = self.snap.message(area)?.clone();
        if message.error {
            return Some(message);
        }
        let id = message.id;
        if !self.shown.contains_key(&id) {
            self.timer(MESSAGE_SHOWN, cx, move |this, _| {
                this.shown.insert(id, true);
            });
        }
        (!*self.shown.entry(id).or_insert(false)).then_some(message)
    }

    /// A Settings group's title with its message to the right.
    fn titled(
        &mut self,
        title: &'static str,
        area: Area,
        busy: Option<String>,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let copied = matches!(self.copied, Some((a, _)) if a == area);
        let message = match busy {
            Some(text) => Some((text, p.accent)),
            None if copied => Some(("Copied".to_owned(), p.accent)),
            None => self
                .current(area, cx)
                .map(|m| (m.text, if m.error { p.danger } else { p.accent })),
        };
        let selector = format!("{}-message", title.to_lowercase());
        div()
            .flex()
            .items_end()
            .gap(px(6.))
            .child(
                div()
                    .flex_none()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .children(message.map(|(text, color)| {
                div()
                    .debug_selector(|| selector)
                    .mb(px(2.))
                    .min_w_0()
                    .truncate()
                    .text_size(px(10.))
                    .italic()
                    .text_color(color)
                    .child(text)
            }))
    }

    /// The window closed: what it showed is reset for the next time.
    pub fn closed(&mut self) {
        self.flush_placement();
        self.window = None;
        self.secret_shown = false;
        self.sync_visibility();
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.secret_shown = false;
        self.sync_visibility();
        self.rebuild_logs();
        cx.notify();
    }

    /// Saves the window's place once it has rested for a moment, so dragging or tiling writes once.
    pub fn remember_placement(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(file) = self.placement_file.clone() else {
            return;
        };
        let Some(display) = window.display(cx).and_then(|d| d.uuid().ok()) else {
            return;
        };
        // Full screen reports the windowed bounds it returns to.
        let bounds = window.window_bounds().get_bounds();
        let placement = Placement {
            display: display.to_string(),
            x: bounds.origin.x.into(),
            y: bounds.origin.y.into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
        };
        if placement.width <= 0.0 || placement.height <= 0.0 {
            return;
        }
        let (executor, saving) = (cx.background_executor().clone(), placement.clone());
        let task = cx.background_spawn(async move {
            executor.timer(Duration::from_millis(500)).await;
            if let Err(e) = placement::save(&file, &saving) {
                eprintln!("xihomo: {e:#}");
            }
        });
        self.placement_save = Some((placement, task));
    }

    /// Writes a pending placement now, for when the window closes or the app quits.
    pub fn flush_placement(&mut self) {
        if let (Some(file), Some((placement, _))) =
            (&self.placement_file, self.placement_save.take())
            && let Err(e) = placement::save(file, &placement)
        {
            eprintln!("xihomo: {e:#}");
        }
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

    fn render_control(
        &mut self,
        p: Palette,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // Less the window's padding and the node grid's own.
        let inner = f32::from(width) - 40.;
        let columns = node_columns(inner);
        // Text measured inside a grid track gets no width to truncate to, so cards carry theirs.
        let card_width = ((inner - (columns - 1) as f32 * CARD_GAP) / columns as f32).floor();
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
        let status = status_text(&s);
        let status_color = if matches!(s.phase, Phase::Failed(_)) {
            p.danger
        } else {
            p.text
        };

        let mut column = div().flex().flex_col();
        column = column
            .child(
                div()
                    .debug_selector(|| "status".to_owned())
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
                    .child("No profiles. Import or link one in Settings."),
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
                    let mut options = div().grid().grid_cols(columns).gap_1().px_2().pb_2();
                    // Display only: REJECT as the whole machine's route is never wanted.
                    for proxy in group
                        .all
                        .iter()
                        .filter(|proxy| !(name == "GLOBAL" && *proxy == "REJECT"))
                    {
                        let (g, pick) = (name.clone(), proxy.clone());
                        let id = SharedString::from(format!("pick-{name}-{proxy}"));
                        let card = chip(
                            id,
                            p,
                            *proxy == group.now,
                            group.selectable(),
                            self.cmd(move || Cmd::Select {
                                group: g.clone(),
                                proxy: pick.clone(),
                            }),
                        );
                        // A long name ends in an ellipsis instead of widening its column.
                        options = options.child(
                            card.w(px(card_width))
                                .child(div().truncate().child(proxy.clone())),
                        );
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
        let shown: Arc<Vec<usize>> = Arc::new(
            (0..count)
                .filter(|&ix| connection_matches(&self.conns[ix], &self.conn_filter))
                .collect(),
        );
        let summary = if self.conn_filter.is_empty() {
            format!("{count} active")
        } else {
            format!("{} of {count} active", shown.len())
        };
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .mb_2()
            .text_size(px(12.))
            .text_color(p.dim)
            .child(filter_field(
                "conn-filter",
                &self.conn_input,
                !self.conn_filter.is_empty(),
                p,
            ))
            .child(div().flex_none().child(summary))
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
            shown.len(),
            cx.processor(move |this, range: std::ops::Range<usize>, _, _| {
                range
                    .map(|row| {
                        let ix = shown[row];
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
        bar = bar
            .child(filter_field(
                "log-filter",
                &self.log_input,
                !self.log_filter.is_empty(),
                p,
            ))
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
                            "warning" => p.warning,
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

        // Appearance. Settings file problems show here: most writes to it come from these.
        let mut sets = div().flex().flex_wrap().gap_1().items_center();
        for set in icons::SETS {
            let selected = s.icon_set == *set;
            sets = sets.child(
                chip(
                    *set,
                    p,
                    selected,
                    true,
                    self.cmd(move || Cmd::SetIconSet((*set).to_owned())),
                )
                .py(px(4.))
                .child(glyph(set, if selected { p.on_accent } else { p.text })),
            );
        }
        let mut themes = div().flex().gap_1();
        for (theme, id, label) in [
            (Theme::Auto, "theme-auto", "Auto"),
            (Theme::Light, "theme-light", "Light"),
            (Theme::Dark, "theme-dark", "Dark"),
        ] {
            themes = themes.child(button(
                id,
                label,
                p,
                s.theme == theme,
                true,
                self.cmd(move || Cmd::SetTheme(theme)),
            ));
        }
        column = column
            .child(self.titled("Appearance", Area::Files, None, p, cx))
            .child(field("Menu Bar Icon", sets, None, p))
            .child(field("Theme", themes, None, p))
            .child(separator(p));

        column = self.render_profiles(column, p, cx).child(separator(p));
        column = self.render_proxy_api(column, p, cx);

        // Click switches; right-click offers Delete.
        let busy = s.update_busy.is_some();
        let mut versions = div().flex().flex_wrap().gap_1().items_center();
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
        // One action: Check for Update becomes a highlighted Update to vX once a newer
        // release is known.
        let action = match &s.latest {
            Some(latest) if s.core_version.as_deref() != Some(&latest.version) => icon_button(
                "update",
                Icon::Update,
                format!("Update to {}", latest.version),
                p,
                true,
                !busy,
                self.cmd(|| Cmd::Download),
            ),
            _ => icon_button(
                "check",
                Icon::Search,
                "Check for Update",
                p,
                false,
                !busy,
                self.cmd(|| Cmd::CheckUpdate),
            ),
        };
        column = column
            .child(self.titled("Core", Area::Update, s.update_busy.clone(), p, cx))
            .child(field(
                "Versions",
                versions,
                Some(action.into_any_element()),
                p,
            ));
        div()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .child(column)
            .into_any_element()
    }

    /// Each profile with Update (its providers, while it runs) and Delete (two clicks).
    fn render_profiles(
        &mut self,
        mut column: gpui::Div,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let s = self.snap.clone();
        let busy = s.providers_busy.then(|| "Updating providers…".to_owned());
        column = column.child(self.titled("Profiles", Area::Profiles, busy, p, cx));
        // The list and, below it, a toolbar for the whole list: new actions join the toolbar.
        let mut list = div().flex().flex_col();
        for profile in s.profiles.iter() {
            let mut name = profile.name.clone();
            if profile.is_link {
                name.push_str(" (linked)");
            }
            if let Some(problem) = &profile.problem {
                name.push_str(&format!(" — {problem}"));
            }
            let active = s.active.as_deref() == Some(&profile.name);
            // The active profile can only be updated, the others only deleted: one button each.
            let action = if active {
                icon_button(
                    format!("update-{}", profile.name),
                    Icon::Update,
                    "Update providers",
                    p,
                    false,
                    s.phase == Phase::Running && !s.providers_busy,
                    self.cmd(|| Cmd::UpdateProviders),
                )
            } else {
                let target = profile.name.clone();
                self.confirmable(
                    format!("delete-{}", profile.name),
                    (Icon::Delete, "Delete profile"),
                    true,
                    move || Cmd::RemoveProfile(target.clone()),
                    p,
                    cx,
                )
            };
            // A double-click on the name selects the profile, as a click in Control does.
            let pick = (!active && profile.problem.is_none()).then(|| profile.name.clone());
            let handle = self.handle.clone();
            let id = format!("profile-name-{}", profile.name);
            let selector = id.clone();
            let title = div()
                .id(ElementId::Name(id.into()))
                .debug_selector(|| selector)
                .flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .on_click(move |event, _, _| {
                    if event.click_count() == 2
                        && let Some(name) = &pick
                    {
                        handle.send(Cmd::Activate(name.clone()));
                    }
                })
                .child(
                    div()
                        .flex_none()
                        .w(px(12.))
                        .text_color(if active { p.accent } else { p.dim })
                        .child("•"),
                )
                .child(div().min_w_0().truncate().child(name));
            // Every row keeps the bar's width, so the names line up; only the active one shows it.
            // No right padding: the buttons share the right edge of the other groups' rows.
            let row = div()
                .flex()
                .items_center()
                .gap_1()
                .py(px(2.))
                .pl(px(4.))
                .border_l_2()
                .rounded_r(px(5.))
                .child(title)
                .child(action);
            list = list.child(if active {
                row.border_color(p.accent)
                    .bg(p.accent.opacity(0.12))
                    .font_weight(FontWeight::MEDIUM)
            } else {
                row.border_color(gpui::transparent_black())
            });
        }
        if s.profiles.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py(px(6.))
                    .text_color(p.dim)
                    .child("No profiles"),
            );
        }
        // Both pick one file; the backend checks its name and contents.
        let pick = |prompt: &'static str, make: fn(PathBuf) -> Cmd| {
            let handle = self.handle.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let paths = cx.prompt_for_paths(PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: false,
                    prompt: Some(prompt.into()),
                });
                let handle = handle.clone();
                cx.spawn(async move |_| {
                    if let Ok(Ok(Some(paths))) = paths.await
                        && let Some(path) = paths.into_iter().next()
                    {
                        handle.send(make(path));
                    }
                })
                .detach();
            }
        };
        let config_dir = s.config_dir.clone();
        // Actions on the whole list, as plain text under it: they do not compete with its rows.
        let toolbar = div()
            .flex()
            .flex_wrap()
            .gap_1()
            .mt_1()
            // The first label lines up with the names, not with its own hover padding.
            .ml(px(-2.))
            .child(text_button(
                "import",
                "Import Profile…",
                p,
                pick("Import", Cmd::ImportProfile),
            ))
            .child(text_button(
                "link",
                "Link Profile…",
                p,
                pick("Link", Cmd::LinkProfile),
            ))
            .child(text_button(
                "check-now",
                "Check Now",
                p,
                self.cmd(|| Cmd::CheckNow),
            ))
            .child(text_button(
                "config-dir",
                "Config Folder",
                p,
                move |_, _, cx| cx.open_with_system(&config_dir),
            ));
        column
            .child(list)
            .child(toolbar)
            .child(hint(
                "overrides.yaml in the config folder applies to every profile.".to_owned(),
                p,
            ))
            .child(
                hint(
                    "Ports, API, LAN access and TUN come from xihomo; profiles cannot change them."
                        .to_owned(),
                    p,
                )
                .mt_0(),
            )
    }
}

impl AppState {
    /// The input, and Apply while the typed value differs from the saved one.
    fn port_controls(
        &mut self,
        port: Port,
        id: &'static str,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> (gpui::Div, Option<Stateful<gpui::Div>>) {
        let input = div()
            .debug_selector(move || format!("{id}-input"))
            .w(px(58.))
            .rounded(px(5.))
            .bg(p.panel)
            .border_1()
            .border_color(p.border)
            .font_family("Menlo")
            .text_color(p.text)
            .overflow_hidden()
            .child(self.field(port).input.clone());
        let changed = self.draft(port, cx) != u32::from(self.saved_port(port));
        let apply = changed.then(|| {
            button(
                format!("{id}-apply"),
                "Apply",
                p,
                false,
                !self.busy(),
                cx.listener(move |this, _, _, cx| this.apply_port(port, cx)),
            )
        });
        (input, apply)
    }

    fn render_proxy_api(
        &mut self,
        mut column: gpui::Div,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let s = self.snap.clone();
        let enabled = !self.busy();
        let mono = |text: String| div().font_family("Menlo").child(text);
        for port in [Port::Proxy, Port::Api] {
            self.sync_port(port, cx);
        }

        let busy = (s.applying == Some(Area::Listen)).then(|| "Applying…".to_owned());
        column = column.child(self.titled("Proxy", Area::Listen, busy, p, cx));
        let (input, apply) = self.port_controls(Port::Proxy, "proxy-port", p, cx);
        column = column.child(field(
            "Port",
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .child(input)
                .children(apply),
            None,
            p,
        ));
        let lan = s.allow_lan;
        // The address sits on the same row, so choosing This network adds no line.
        let mut choice = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(button(
                "lan-off",
                "This device",
                p,
                !lan,
                enabled && lan,
                self.cmd(|| Cmd::SetAllowLan(false)),
            ))
            .child(button(
                "lan-on",
                "This network",
                p,
                lan,
                enabled && !lan,
                self.cmd(|| Cmd::SetAllowLan(true)),
            ));
        if lan {
            choice = match s.lan_ip.map(|ip| format!("{ip}:{}", s.mixed_port)) {
                Some(address) => choice.child(mono(address).ml_2()),
                None => choice.child(div().ml_2().text_color(p.dim).child("No network")),
            };
        }
        let warning = if lan {
            " Anyone on this network can use it."
        } else {
            ""
        };
        column = column
            .child(field("Available To", choice, None, p))
            .child(hint(format!("HTTP and SOCKS5.{warning}"), p))
            .child(separator(p));

        let busy = (s.applying == Some(Area::Api)).then(|| "Restarting core…".to_owned());
        column = column.child(self.titled("API", Area::Api, busy, p, cx));
        let (input, apply) = self.port_controls(Port::Api, "api-port", p, cx);
        column = column.child(field(
            "Address",
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .child(mono("http://127.0.0.1:".to_owned()))
                .child(input)
                .children(apply),
            None,
            p,
        ));
        let secret = s.secret.clone();
        let shown = if self.secret_shown {
            secret.clone()
        } else {
            masked(&secret)
        };
        let regenerate = self.confirmable(
            "regenerate".to_owned(),
            (Icon::Regenerate, "Regenerate secret"),
            enabled,
            || Cmd::RegenerateSecret,
            p,
            cx,
        );
        column = column.child(field(
            "Secret",
            div().flex().flex_wrap().items_center().gap_1().child(
                // Shown in full, a double-click copies it.
                mono(shown)
                    .id("secret")
                    .debug_selector(|| "secret".to_owned())
                    .min_w_0()
                    .truncate()
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        if this.secret_shown && event.click_count() == 2 {
                            this.copy(Area::Api, secret.clone(), cx);
                        }
                    })),
            ),
            Some(
                div()
                    .flex()
                    .flex_none()
                    .gap_1()
                    .child(icon_button(
                        "secret-show",
                        if self.secret_shown {
                            Icon::Hide
                        } else {
                            Icon::Show
                        },
                        if self.secret_shown {
                            "Hide secret"
                        } else {
                            "Show secret"
                        },
                        p,
                        false,
                        true,
                        cx.listener(|this, _, _, cx| {
                            this.secret_shown = !this.secret_shown;
                            cx.notify();
                        }),
                    ))
                    .child(regenerate)
                    .into_any_element(),
            ),
            p,
        ));
        column
            .child(hint(
                "For dashboards and scripts. Changes restart the running core automatically."
                    .to_owned(),
                p,
            ))
            .child(separator(p))
    }
}

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(self.snap.theme, window.appearance());
        // Typing filters right away on Logs and Connections; elsewhere focus returns to the root,
        // so a hidden input never swallows the tab and window shortcuts.
        let wanted = match self.tab {
            Tab::Logs => self.log_input.read(cx).focus.clone(),
            Tab::Connections => self.conn_input.read(cx).focus.clone(),
            _ => self.focus.clone(),
        };
        if !wanted.contains_focused(window, cx) {
            window.focus(&wanted);
        }
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
            Tab::Control => self.render_control(p, window.viewport_size().width, cx),
            Tab::Connections => self.render_connections(p, cx),
            Tab::Logs => self.render_logs(p, cx),
            Tab::Settings => self.render_settings(p, cx),
        };
        // One segmented control, so switching panels does not look like the action buttons. Each
        // segment draws its own frame, so the selected one is a solid block up to its outer edges.
        let mut tabs = div().flex();
        let all = [
            (Tab::Control, "Control", "tab-control"),
            (Tab::Connections, "Connections", "tab-connections"),
            (Tab::Logs, "Logs", "tab-logs"),
            (Tab::Settings, "Settings", "tab-settings"),
        ];
        let last = all.len() - 1;
        for (i, (tab, label, id)) in all.into_iter().enumerate() {
            let selected = self.tab == tab;
            if i > 0 {
                let beside_selected = selected || self.tab == all[i - 1].0;
                tabs = tabs.child(div().w(px(1.)).bg(if beside_selected {
                    p.accent
                } else {
                    p.border
                }));
            }
            let mut segment = div()
                .id(id)
                .debug_selector(|| id.to_owned())
                .px_3()
                .py(px(3.))
                .text_size(px(12.))
                .cursor_pointer()
                .border_t_1()
                .border_b_1()
                .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
                .child(label);
            if i == 0 {
                segment = segment.border_l_1().rounded_l(px(5.));
            }
            if i == last {
                segment = segment.border_r_1().rounded_r(px(5.));
            }
            tabs = tabs.child(if selected {
                segment
                    .bg(p.accent)
                    .border_color(p.accent)
                    .text_color(p.on_accent)
            } else {
                segment
                    .bg(p.panel)
                    .border_color(p.border)
                    .hover(move |s| s.bg(p.hover))
            });
        }
        div()
            .track_focus(&self.focus)
            .key_context("xihomo")
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
            .child(div().flex().justify_center().mb_3().child(tabs))
            .child(div().flex_1().min_h_0().child(body))
            .children(menu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_cards_stay_between_the_minimum_and_one_and_a_half_times_it() {
        let two = (CARD_MIN_WIDTH * 2. + CARD_GAP) as u32;
        for width in (two..3000).map(|w| w as f32) {
            let n = node_columns(width) as f32;
            let card = (width - (n - 1.) * CARD_GAP) / n;
            assert!(card >= CARD_MIN_WIDTH, "{width}: {card}");
            assert!(card < CARD_MIN_WIDTH * 1.5 + CARD_GAP, "{width}: {card}");
        }
        assert_eq!(node_columns(200.), 2);
        assert_eq!(node_columns(600.), 3);
    }

    #[test]
    fn connection_filter_matches_shown_fields_and_rule() {
        let conn = Connection {
            metadata: crate::api::Metadata {
                host: "api.github.com".into(),
                destination_port: "443".into(),
                network: "tcp".into(),
                process: "Safari".into(),
                ..Default::default()
            },
            chains: vec!["node-a".into(), "Proxy".into()],
            rule: "DomainSuffix".into(),
            ..Connection::default()
        };
        for needle in [
            "",
            "github.com:443",
            "tcp",
            "safari",
            "proxy",
            "domainsuffix",
        ] {
            assert!(connection_matches(&conn, needle), "{needle}");
        }
        assert!(!connection_matches(&conn, "chrome"));
    }

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
    use std::net::Ipv4Addr;
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
            ]);
            cx.bind_keys(crate::input::bindings());
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
    fn connections_filter_by_typed_text_and_shortcuts_still_work(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        let conn = |id: &str| Connection {
            id: id.into(),
            metadata: Metadata {
                host: format!("{id}.example"),
                destination_port: "443".into(),
                ..Metadata::default()
            },
            ..Connection::default()
        };
        ui.set(|s| s.conns = Arc::new(vec![conn("c1"), conn("c2")]));
        ui.keys("cmd-2");
        ui.drain();
        ui.cx.simulate_input("C2");
        assert_eq!(ui.read(|s| s.conn_filter.clone()), "c2");
        assert_eq!(
            ui.read(|s| s.log_filter.clone()),
            "",
            "only the visible tab's filter"
        );
        // The first row is now c2: its Close button sits where c1's was.
        ui.click("close-c1");
        assert!(matches!(&ui.sent()[..], [Cmd::CloseConnection(id)] if id == "c2"));

        // Window shortcuts pass through the focused input; other tabs take no typing.
        ui.keys("cmd-1");
        assert_eq!(ui.read(|s| s.tab), Tab::Control);
        ui.cx.simulate_input("x");
        assert_eq!(ui.read(|s| s.conn_filter.clone()), "c2");
        ui.keys("cmd-2 escape");
        assert_eq!(ui.read(|s| s.conn_filter.clone()), "");
    }

    #[gpui::test]
    fn a_click_in_the_filter_padding_places_the_cursor(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        ui.keys("cmd-3");
        ui.cx.simulate_input("git");
        let field = ui.cx.debug_bounds("log-filter").unwrap();
        // Inside the border, left of the text: the start of the line.
        let edge = gpui::point(field.left() + px(3.), field.center().y);
        ui.cx.simulate_click(edge, Modifiers::default());
        ui.frame();
        ui.cx.simulate_input("x");
        assert_eq!(ui.read(|s| s.log_filter.clone()), "xgit");
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

    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789ab3f9a";

    /// Settings with the dev channel's ports, as the debug test build sees them.
    fn settings_tab(cx: &mut TestAppContext) -> Ui<'_> {
        let snapshot = Snapshot {
            mixed_port: 20999,
            controller_port: 21000,
            secret: SECRET.into(),
            ..running()
        };
        let mut ui = open(cx, snapshot);
        ui.keys("cmd-4");
        ui.drain();
        ui
    }

    impl Ui<'_> {
        fn text(&mut self, port: Port) -> String {
            self.view
                .update(self.cx, |this, cx| this.field(port).input.read(cx).text())
        }

        fn snap(&mut self, f: impl FnOnce(&mut Snapshot)) {
            self.set(|s| {
                let mut snap = (*s.snap).clone();
                f(&mut snap);
                s.snap = Arc::new(snap);
            });
        }

        fn double_click(&mut self, selector: &'static str) {
            let position = self.cx.debug_bounds(selector).unwrap().center();
            for click_count in [1, 2] {
                self.cx.simulate_event(gpui::MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count,
                    first_mouse: false,
                });
                self.cx.simulate_event(gpui::MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count,
                });
            }
            self.frame();
        }

        fn clipboard(&mut self) -> Option<String> {
            self.cx
                .update(|_, cx| cx.read_from_clipboard())
                .and_then(|item| item.text())
        }

        fn wait(&mut self, delay: Duration) {
            self.cx.executor().advance_clock(delay);
            self.frame();
        }
    }

    #[test]
    fn control_status_names_both_ports_and_sharing() {
        let mut s = Snapshot {
            mixed_port: 10999,
            controller_port: 11000,
            ..running()
        };
        let dev = if settings::DEV { " · dev" } else { "" };
        assert_eq!(
            status_text(&s),
            format!("Running · v1.19.33 · proxy 10999 · API 11000{dev}")
        );
        s.allow_lan = true;
        assert_eq!(
            status_text(&s),
            format!("Running · v1.19.33 · proxy 10999 · LAN · API 11000{dev}")
        );
        assert_eq!(
            masked(SECRET),
            format!("{}3f9a", "•".repeat(SECRET.len() - 4))
        );
        assert_eq!(
            masked("abcd2345").chars().count(),
            8,
            "as long as the secret"
        );
    }

    #[gpui::test]
    fn port_fields_take_digits_apply_on_return_and_revert_on_escape(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        assert_eq!(ui.text(Port::Proxy), "20999");
        ui.click("proxy-port-input");
        ui.keys("cmd-a");
        ui.cx.simulate_input("7a8-90");
        assert_eq!(ui.text(Port::Proxy), "7890");
        ui.click("proxy-port-apply");
        assert!(matches!(ui.sent()[..], [Cmd::SetProxyPort(7890)]));

        ui.click("proxy-port-input");
        ui.keys("cmd-a");
        ui.cx.simulate_input("123456");
        assert_eq!(ui.text(Port::Proxy), "12345", "five digits at most");
        ui.keys("escape");
        assert_eq!(ui.text(Port::Proxy), "20999", "back to the saved value");
        ui.keys("enter");
        assert!(ui.sent().is_empty(), "nothing to apply");
        // Apply is gone with the change: a click where it was does nothing.
        ui.click("proxy-port-input");

        ui.click("api-port-input");
        ui.keys("cmd-a");
        ui.cx.simulate_input("9090");
        ui.keys("enter");
        assert!(matches!(ui.sent()[..], [Cmd::SetApiPort(9090)]));
    }

    #[gpui::test]
    fn an_empty_port_field_means_the_default(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        ui.snap(|s| s.mixed_port = 7000);
        assert_eq!(ui.text(Port::Proxy), "7000");
        ui.click("proxy-port-input");
        ui.keys("cmd-a backspace");
        assert_eq!(ui.text(Port::Proxy), "", "an empty field is not refilled");
        ui.keys("enter");
        assert!(matches!(ui.sent()[..], [Cmd::SetProxyPort(20999)]));
        assert_eq!(ui.text(Port::Proxy), "20999");
        // Saved as the default: no Apply, and an empty field stands for the same value.
        ui.snap(|s| s.mixed_port = 20999);
        ui.keys("cmd-a backspace enter");
        assert!(ui.sent().is_empty());
    }

    #[gpui::test]
    fn an_edited_field_keeps_its_draft_when_the_saved_value_changes(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        ui.click("proxy-port-input");
        ui.keys("cmd-a");
        ui.cx.simulate_input("7890");
        // Another tool changes both ports.
        ui.snap(|s| {
            s.mixed_port = 8000;
            s.controller_port = 9000;
        });
        assert_eq!(ui.text(Port::Proxy), "7890", "the draft stays");
        assert_eq!(ui.text(Port::Api), "9000", "an unedited field follows");
        // Once the saved value matches the draft, the field follows again.
        ui.snap(|s| s.mixed_port = 7890);
        ui.snap(|s| s.mixed_port = 7891);
        assert_eq!(ui.text(Port::Proxy), "7891");
    }

    #[gpui::test]
    fn available_to_applies_on_click_and_shows_the_lan_address(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        ui.click("lan-off");
        assert!(ui.sent().is_empty(), "already this device");
        ui.click("lan-on");
        assert!(matches!(ui.sent()[..], [Cmd::SetAllowLan(true)]));
        ui.snap(|s| {
            s.allow_lan = true;
            s.lan_ip = Some(Ipv4Addr::new(192, 168, 1, 23));
        });
        ui.click("lan-off");
        assert!(matches!(ui.sent()[..], [Cmd::SetAllowLan(false)]));
    }

    #[gpui::test]
    fn the_secret_is_masked_copied_and_regenerated_on_a_second_click(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        ui.double_click("secret");
        assert_eq!(ui.clipboard(), None, "a masked secret is not copied");
        ui.click("secret-show");
        ui.double_click("secret");
        assert_eq!(ui.clipboard().as_deref(), Some(SECRET));
        assert!(ui.read(|s| matches!(s.copied, Some((Area::Api, _)))));
        ui.wait(MESSAGE_SHOWN + Duration::from_millis(10));
        assert!(ui.read(|s| s.copied.is_none()), "Copied clears itself");
        ui.click("secret-show");

        ui.click("secret-show");
        assert!(ui.read(|s| s.secret_shown));
        ui.keys("cmd-1 cmd-4");
        assert!(!ui.read(|s| s.secret_shown), "hidden again on a tab change");
        ui.click("secret-show");
        ui.set(|s| s.closed());
        assert!(!ui.read(|s| s.secret_shown), "and when the window closes");
        ui.drain();

        ui.click("regenerate");
        assert!(ui.sent().is_empty(), "the first click only arms it");
        ui.click("regenerate");
        assert!(matches!(ui.sent()[..], [Cmd::RegenerateSecret]));
        ui.click("regenerate");
        ui.wait(CONFIRM_WINDOW + Duration::from_millis(10));
        ui.click("regenerate");
        assert!(
            ui.sent().is_empty(),
            "armed again after the confirmation lapsed"
        );
    }

    #[gpui::test]
    fn profiles_are_deleted_on_a_second_click_and_updated_only_while_running(
        cx: &mut TestAppContext,
    ) {
        let mut ui = settings_tab(cx);
        assert!(
            ui.cx.debug_bounds("delete-home").is_none(),
            "the active one has Update"
        );
        assert!(
            ui.cx.debug_bounds("update-work").is_none(),
            "the others have Delete"
        );
        ui.click("delete-work");
        assert!(ui.sent().is_empty(), "the first click only arms it");
        ui.click("delete-work");
        assert!(matches!(&ui.sent()[..], [Cmd::RemoveProfile(n)] if n == "work"));
        ui.click("delete-gone");
        ui.wait(CONFIRM_WINDOW + Duration::from_millis(10));
        ui.click("delete-gone");
        assert!(
            ui.sent().is_empty(),
            "armed again after the confirmation lapsed"
        );

        ui.click("update-home");
        assert!(matches!(ui.sent()[..], [Cmd::UpdateProviders]));
        ui.snap(|s| s.providers_busy = true);
        ui.click("update-home");
        ui.snap(|s| {
            s.providers_busy = false;
            s.phase = Phase::Stopped;
        });
        ui.click("update-home");
        assert!(ui.sent().is_empty(), "not while updating or stopped");
    }

    #[gpui::test]
    fn a_double_click_on_a_profile_name_selects_it(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        ui.click("profile-name-work");
        assert!(ui.sent().is_empty(), "a single click selects nothing");
        ui.double_click("profile-name-work");
        assert!(matches!(&ui.sent()[..], [Cmd::Activate(n)] if n == "work"));
        ui.double_click("profile-name-home");
        assert!(ui.sent().is_empty(), "already selected");
        ui.double_click("profile-name-gone");
        assert!(ui.sent().is_empty(), "a broken link cannot be selected");
    }

    #[gpui::test]
    fn messages_beside_titles_clear_themselves_unless_they_are_errors(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        let message = |text: &str, error: bool, id: u64| Message {
            text: text.into(),
            error,
            id,
        };
        ui.snap(|s| {
            s.messages
                .insert(Area::Listen, message("Applied", false, 1));
            s.messages
                .insert(Area::Api, message("Port 21000 is in use", true, 2));
        });
        let current = |ui: &mut Ui, area| {
            ui.view
                .update(ui.cx, |this, cx| this.current(area, cx).map(|m| m.text))
        };
        assert_eq!(current(&mut ui, Area::Listen).as_deref(), Some("Applied"));
        ui.wait(MESSAGE_SHOWN + Duration::from_millis(10));
        assert_eq!(current(&mut ui, Area::Listen), None);
        assert!(current(&mut ui, Area::Api).is_some(), "errors stay");
        // The same text again is a new message.
        ui.snap(|s| {
            s.messages
                .insert(Area::Listen, message("Applied", false, 3));
        });
        assert_eq!(current(&mut ui, Area::Listen).as_deref(), Some("Applied"));
    }

    #[gpui::test]
    fn changes_wait_while_one_is_applied(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        ui.click("api-port-input");
        ui.keys("cmd-a");
        ui.cx.simulate_input("9090");
        ui.snap(|s| s.applying = Some(Area::Api));
        for control in ["api-port-apply", "lan-on", "regenerate"] {
            ui.click(control);
        }
        ui.keys("enter");
        assert!(ui.sent().is_empty());
        assert!(ui.read(|s| s.confirm.is_none()));
        ui.snap(|s| {
            s.applying = None;
            s.phase = Phase::Starting;
        });
        ui.click("api-port-apply");
        assert!(ui.sent().is_empty(), "nor while the core starts");
    }

    #[gpui::test]
    fn the_buttons_at_the_right_of_rows_are_the_same_size(cx: &mut TestAppContext) {
        let ui = settings_tab(cx);
        let right = ui.cx.debug_bounds("regenerate").unwrap().right();
        for id in [
            "update-home",
            "delete-work",
            "regenerate",
            "check",
            "secret-show",
        ] {
            let bounds = ui.cx.debug_bounds(id).unwrap();
            assert_eq!(
                (bounds.size.width, bounds.size.height),
                (px(28.), px(24.)),
                "{id}"
            );
            if id != "secret-show" {
                assert_eq!(bounds.right(), right, "{id} shares the right edge");
            }
        }
        let show = ui.cx.debug_bounds("secret-show").unwrap();
        let regenerate = ui.cx.debug_bounds("regenerate").unwrap();
        assert!(show.right() < regenerate.left());
        // Text, buttons and icon buttons are centred on their row's label.
        for (label, ids) in [
            ("label-Secret", &["secret", "secret-show", "regenerate"][..]),
            ("label-Available To", &["lan-off"]),
            ("label-Versions", &["check"]),
        ] {
            let label = ui.cx.debug_bounds(label).unwrap();
            for id in ids {
                let offset = (ui.cx.debug_bounds(id).unwrap().center().y - label.center().y).abs();
                assert!(offset < px(1.), "{id} is off its label by {offset:?}");
            }
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
