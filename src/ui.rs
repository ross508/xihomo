use crate::api::{Connection, Latency, LogLine};
use crate::backend::{Area, Cmd, Handle, Message, Phase, Snapshot, Topic, TunBusy};
use crate::icons;
use crate::input::{Cancelled, Changed, Submitted, TextInput};
use crate::placement::{self, Placement};
use crate::settings::{self, Theme};
use crate::tray::phase_label;
use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, AnyElement, App, ClickEvent, ClipboardItem, Context, ElementId,
    Entity, FocusHandle, FontWeight, Hsla, MouseButton, PathBuilder, PathPromptOptions, Pixels,
    Point, ScrollStrategy, SharedString, Stateful, Task, TextRun, UniformListScrollHandle, Window,
    WindowAppearance, WindowHandle, WindowTextSystem, actions, anchored, canvas, deferred, div,
    point, pulsating_between, px, rgb, uniform_list,
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
pub enum Tab {
    Control,
    Connections,
    Logs,
    Settings,
}

impl Tab {
    /// Where the tab shows messages; Profiles is in both Control and Settings.
    fn areas(self) -> &'static [Area] {
        match self {
            Self::Control => &[Area::Core, Area::Proxy, Area::Profiles, Area::Groups],
            Self::Settings => &[
                Area::Files,
                Area::Profiles,
                Area::Listen,
                Area::Api,
                Area::Update,
            ],
            Self::Connections | Self::Logs => &[],
        }
    }
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
    latency_url: PortField,
    /// The secret is shown in full; hidden again on a tab change or when the window closes.
    secret_shown: bool,
    /// The icon whose note is open. A click anywhere else closes it.
    note: Option<&'static str>,
    /// The button waiting for its confirming click, and the timer token that may disarm it.
    confirm: Option<(String, u64)>,
    /// "Copied" next to a section title.
    copied: Option<(Area, u64)>,
    timers: u64,
    /// The busy look: its level moves toward `snap.busy` over `BUSY_FADE`, a frame at a time.
    busy: Busy,
    busy_level: f32,
    busy_target: bool,
    busy_since: std::time::Instant,
    /// Messages that have been on screen, by id; `true` once one that is not an error timed out.
    shown: HashMap<u64, bool>,
    /// Measures what fits beside a title; set at each frame with the window's font.
    measure: Option<(Arc<WindowTextSystem>, gpui::Font)>,
    /// The width inside the window's padding, at the last frame.
    content_width: Pixels,
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

/// A port or Latency URL input and whether the user has typed in it since it last showed the
/// saved value.
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
    /// A fast latency.
    good: Hsla,
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
            good: c(0x8CCB8A),
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
            good: c(0x2D6A2F),
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
/// Control's profile buttons: laid out like node cards, but sized on their own.
const PROFILE_MIN_WIDTH: f32 = 180.;
/// `gap_1` between cards.
const CARD_GAP: f32 = 4.;

/// As many equal columns as fit cards `min` wide in `width`, never fewer than two, and the cards'
/// width. A card is then below `(n + 1) / n` of the minimum, so at most about 1.5 times it.
/// Text measured inside a grid track gets no width to truncate to, so cards carry theirs.
fn card_grid(width: f32, min: f32) -> (u16, f32) {
    let columns = (((width + CARD_GAP) / (min + CARD_GAP)).floor() as u16).max(2);
    let card = ((width - (columns - 1) as f32 * CARD_GAP) / columns as f32).floor();
    (columns, card)
}

/// What a card's latency slot shows.
#[derive(Clone, Debug, PartialEq)]
enum Slot {
    /// Never tested: no ⚡.
    Empty,
    Bolt {
        pulsing: bool,
    },
    Text(String, Hsla),
}

/// The slot for `member` and whether a click on it tests. On a selected card the accent's text
/// color replaces the latency colors.
fn slot(s: &Snapshot, member: &str, selected: bool, p: Palette) -> (Slot, bool) {
    let Some(node) = s.target(member) else {
        return (Slot::Empty, false);
    };
    let testing = s.testing.contains(&node);
    let usable = !testing && s.testing_group.is_none();
    let color = |c: Hsla| if selected { p.on_accent } else { c };
    let slot = match s.nodes.get(&node).and_then(|n| n.latency) {
        _ if testing => Slot::Bolt { pulsing: true },
        None => Slot::Bolt { pulsing: false },
        Some(Latency::Ms(ms)) => Slot::Text(
            format!("{ms} ms"),
            color(if ms < 600 { p.good } else { p.warning }),
        ),
        Some(Latency::Failed) => Slot::Text("Timeout".into(), color(p.danger)),
    };
    (slot, usable)
}

/// Fades `element` in and out while `on`, for a test that is running.
fn pulse<E: IntoElement + Styled + 'static>(id: String, element: E, on: bool) -> AnyElement {
    if !on {
        return element.into_any_element();
    }
    let animation = Animation::new(Duration::from_millis(1200))
        .repeat()
        .with_easing(pulsating_between(0.3, 1.));
    element
        .with_animation(ElementId::Name(id.into()), animation, |e, t| e.opacity(t))
        .into_any_element()
}

/// Beside Control's phase: core, ports and whether the proxy is shared.
fn status_text(s: &Snapshot) -> String {
    let channel = if settings::DEV { " · dev" } else { "" };
    let lan = if s.allow_lan { " · LAN" } else { "" };
    match &s.phase {
        // The reason takes the room, as the heading's message.
        Phase::Failed(_) => channel.trim_start_matches(" · ").to_owned(),
        _ => format!(
            "{} · proxy {}{lan} · API {}{}{channel}",
            s.core_version.as_deref().unwrap_or("no core"),
            s.mixed_port,
            s.controller_port,
            s.tun_device
                .as_ref()
                .map(|d| format!(" · TUN {d}"))
                .unwrap_or_default(),
        ),
    }
}

/// The tooltip of the warning beside the LAN address.
fn dns_port_text(port: u16) -> String {
    format!(
        "This profile's DNS port {port} is in use by another app, so other devices can't use the core for DNS."
    )
}

/// The TUN button's label and whether it is enabled. It shows what `/configs` reports, not the
/// remembered choice, so a TUN asked for but not running reads Off.
fn tun_button(s: &Snapshot) -> (&'static str, bool) {
    let running = s.phase == Phase::Running;
    match s.tun_busy {
        Some(TunBusy::Authorizing) => ("TUN: Authorizing…", false),
        Some(TunBusy::TurningOn) => ("TUN: Turning On…", false),
        Some(TunBusy::TurningOff) => ("TUN: Turning Off…", false),
        None if s.tun_device.is_some() => ("TUN: On", running),
        None => ("TUN: Off", running),
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
        // GPUI rounds a text input's line to whole points but not a label's. This one is rounded
        // too, so a button is as tall as a field, CONTROL_HEIGHT, and Apply appearing beside a
        // port field leaves the rows below where they are.
        .line_height(CONTROL_HEIGHT - px(8.))
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
    /// Two half-circle arrows: read the profile from disk again and apply it.
    Reload,
    /// An arrow into a tray: fetch from the internet, a profile's providers or a newer core.
    Download,
    /// A die: make a new random one.
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
    /// A lightning bolt: test the latency.
    Bolt,
    /// An `i` in a ring: more about the line it ends.
    Info,
    /// A `!` cut out of a triangle: a problem with the line it ends.
    Warning,
}

fn symbol(icon: Icon, color: Hsla) -> gpui::Canvas<()> {
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
            // A filled circle, for the pupil and the pips.
            let disc = |x: f32, y: f32, r: f32| {
                let radii = point(unit * r, unit * r);
                let mut disc = PathBuilder::fill();
                disc.move_to(at(x - r, y));
                disc.arc_to(radii, px(0.), false, true, at(x + r, y));
                disc.arc_to(radii, px(0.), false, true, at(x - r, y));
                disc
            };
            match icon {
                Icon::Reload => {
                    arrow_arc(-165., -25., 4.6);
                    arrow_arc(15., 155., 4.6);
                }
                // A shaft and a head like the round arrows', into a tray as wide as the round icons.
                Icon::Download => {
                    strokes.push(line(&[(7., 1.6), (7., 6.8)]));
                    let mut head = PathBuilder::fill();
                    head.add_polygon(&[at(7., 9.4), at(4.2, 6.2), at(9.8, 6.2)], true);
                    fills.push(head);
                    strokes.push(line(&[(2.2, 8.6), (2.2, 12.), (11.8, 12.), (11.8, 8.6)]));
                }
                // Five pips in a rounded square no wider, outside, than the round icons.
                Icon::Regenerate => {
                    let (lo, hi, r) = (2., 12., 2.2);
                    let radii = point(unit * r, unit * r);
                    let mut face = PathBuilder::stroke(unit * 1.4);
                    face.move_to(at(lo + r, lo));
                    for (to, corner) in [
                        ((hi - r, lo), (hi, lo + r)),
                        ((hi, hi - r), (hi - r, hi)),
                        ((lo + r, hi), (lo, hi - r)),
                        ((lo, lo + r), (lo + r, lo)),
                    ] {
                        face.line_to(at(to.0, to.1));
                        face.arc_to(radii, px(0.), false, true, at(corner.0, corner.1));
                    }
                    face.close();
                    strokes.push(face);
                    for (x, y) in [
                        (4.75, 4.75),
                        (9.25, 4.75),
                        (7., 7.),
                        (4.75, 9.25),
                        (9.25, 9.25),
                    ] {
                        fills.push(disc(x, y, 0.9));
                    }
                }
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
                    fills.push(disc(7., 7., 2.1));
                    if icon == Icon::Hide {
                        strokes.push(line(&[(2.4, 1.6), (11.6, 12.4)]));
                    }
                }
                Icon::Bolt => {}
                Icon::Info => {
                    let r = unit * 5.6;
                    let mut ring = PathBuilder::stroke(unit * 1.4);
                    ring.move_to(at(1.4, 7.));
                    ring.arc_to(point(r, r), px(0.), false, true, at(12.6, 7.));
                    ring.arc_to(point(r, r), px(0.), false, true, at(1.4, 7.));
                    strokes.push(ring);
                    fills.push(disc(7., 4.2, 1.));
                    strokes.push(line(&[(7., 6.4), (7., 10.4)]));
                }
                // The `!` is a second and third outline in the triangle's path, and the even-odd
                // fill rule (the default) leaves them out.
                Icon::Warning => {
                    let mut sign = PathBuilder::fill();
                    sign.add_polygon(&[at(7., 1.), at(13.6, 12.8), at(0.4, 12.8)], true);
                    sign.add_polygon(
                        &[at(6.2, 4.6), at(7.8, 4.6), at(7.6, 8.8), at(6.4, 8.8)],
                        true,
                    );
                    let radii = point(unit * 0.9, unit * 0.9);
                    sign.move_to(at(6.1, 10.6));
                    sign.arc_to(radii, px(0.), false, true, at(7.9, 10.6));
                    sign.arc_to(radii, px(0.), false, true, at(6.1, 10.6));
                    sign.close();
                    fills.push(sign);
                }
                Icon::Confirm => {
                    let mut check = PathBuilder::stroke(unit * 1.8);
                    check.add_polygon(&[at(2.8, 7.4), at(5.8, 10.4), at(11.4, 3.8)], false);
                    strokes.push(check);
                }
            }
            if icon == Icon::Bolt {
                let mut bolt = PathBuilder::fill();
                bolt.add_polygon(
                    &[
                        at(8.4, 1.),
                        at(2.8, 8.),
                        at(6.4, 8.),
                        at(5.4, 13.),
                        at(11.2, 5.8),
                        at(7.6, 5.8),
                    ],
                    true,
                );
                fills.push(bolt);
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
        tip_box(self.text.clone(), self.p)
    }
}

fn tip_box(text: SharedString, p: Palette) -> gpui::Div {
    div()
        .px_2()
        .py_1()
        .rounded(px(5.))
        .border_1()
        .border_color(p.border)
        .bg(p.panel)
        .shadow_md()
        .text_size(px(11.))
        .text_color(p.text)
        .child(text)
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

/// How long the busy look takes to fade in or out.
const BUSY_FADE: Duration = Duration::from_millis(200);

/// What a busy backend does to an area of the window: `on` blocks hover and clicks at once, and
/// `fade` (0 to 1, eased in) dims it, so a busy moment of a few milliseconds never blinks. Every
/// control inside a covered area gets both, so new rows and buttons need nothing of their own.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Busy {
    on: bool,
    fade: f32,
}

impl Busy {
    fn cover(self, content: impl IntoElement) -> gpui::Div {
        div()
            .relative()
            .opacity(1. - 0.5 * self.fade)
            .child(content)
            .when(self.on, |area| {
                area.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .block_mouse_except_scroll(),
                )
            })
    }
}

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

/// Between messages on a title's line.
const SEPARATOR: &str = " · ";
/// Between the last message shown and `+N`.
const MORE_GAP: f32 = 6.;

/// How many of the messages, `widths` wide, fit whole in `room` with `sep` between them and,
/// when some are left over, `more(left)` after them.
fn fitting(widths: &[f32], sep: f32, more: impl Fn(usize) -> f32, room: f32) -> usize {
    let mut used = 0.;
    for (i, width) in widths.iter().enumerate() {
        used += width + if i > 0 { sep } else { 0. };
        let left = widths.len() - i - 1;
        let tail = if left > 0 { MORE_GAP + more(left) } else { 0. };
        if used + tail > room {
            return i;
        }
    }
    widths.len()
}

/// A hover label listing every message beside a title in full, newest first.
struct Tips {
    items: Vec<(String, Hsla)>,
    p: Palette,
}

impl Render for Tips {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .max_w(px(360.))
            .px_2()
            .py_1()
            .rounded(px(5.))
            .border_1()
            .border_color(self.p.border)
            .bg(self.p.panel)
            .shadow_md()
            .text_size(px(11.))
            .children(
                self.items
                    .iter()
                    .map(|(text, color)| div().text_color(*color).child(text.clone())),
            )
    }
}

/// A hairline between groups of sections, fainter than control borders, across the content.
fn separator(p: Palette) -> gpui::Div {
    div().my(px(8.)).h(px(1.)).bg(p.border.opacity(0.55))
}

/// A group's title: Settings puts its message beside it, Control's Groups list goes under it.
fn heading(title: &'static str) -> gpui::Div {
    div()
        .flex_none()
        .text_size(px(14.))
        .font_weight(FontWeight::SEMIBOLD)
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
        let latency_url = Self::latency_field(cx);
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
            latency_url,
            secret_shown: false,
            note: None,
            confirm: None,
            copied: None,
            timers: 0,
            busy: Busy::default(),
            busy_level: 0.,
            busy_target: false,
            busy_since: cx.background_executor().now(),
            shown: HashMap::new(),
            measure: None,
            content_width: px(0.),
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

    fn latency_field(cx: &mut Context<Self>) -> PortField {
        let input = cx.new(|cx| TextInput::setting(settings::DEFAULT_LATENCY_URL, cx));
        cx.subscribe(&input, |this, _, _: &Changed, cx| {
            this.latency_url.edited = true;
            cx.notify();
        })
        .detach();
        cx.subscribe(&input, |this, _, _: &Submitted, cx| {
            this.apply_latency_url(cx)
        })
        .detach();
        cx.subscribe(&input, |this, input, _: &Cancelled, cx| {
            let saved = this.snap.latency_url.clone();
            input.update(cx, |i, cx| i.set_text(&saved, cx));
            this.latency_url.edited = false;
            cx.notify();
        })
        .detach();
        PortField {
            input,
            edited: false,
        }
    }

    /// The URL the field stands for; empty means the default.
    fn latency_draft(&self, cx: &App) -> String {
        match self.latency_url.input.read(cx).text().trim() {
            "" => settings::DEFAULT_LATENCY_URL.to_owned(),
            text => text.to_owned(),
        }
    }

    fn apply_latency_url(&mut self, cx: &mut Context<Self>) {
        let draft = self.latency_draft(cx);
        if draft == self.snap.latency_url {
            return self.settle_latency_url(cx);
        }
        let input = self.latency_url.input.clone();
        if input.read(cx).text().trim().is_empty() {
            input.update(cx, |i, cx| i.set_text(&draft, cx));
        }
        self.handle.send(Cmd::SetLatencyUrl(draft));
        cx.notify();
    }

    /// Like a port field: a field that stands for the saved URL shows it again, and a message
    /// an earlier draft left goes.
    fn settle_latency_url(&mut self, cx: &mut Context<Self>) {
        let saved = self.snap.latency_url.clone();
        if self.latency_draft(cx) != saved {
            return;
        }
        self.latency_url.edited = false;
        let input = self.latency_url.input.clone();
        input.update(cx, |i, cx| i.set_text(&saved, cx));
        if self.snap.message(Topic::LatencyUrl).is_some() {
            self.handle.send(Cmd::SetLatencyUrl(saved));
        }
        cx.notify();
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

    /// Moves the busy level toward the backend's state by the time since the last frame, and asks
    /// for another frame until it gets there.
    fn update_busy(&mut self, window: &mut Window, cx: &App) {
        let now = cx.background_executor().now();
        let on = self.snap.busy;
        if on != self.busy_target {
            self.busy_target = on;
            self.busy_since = now;
        }
        let step = (now - self.busy_since).as_secs_f32() / BUSY_FADE.as_secs_f32();
        self.busy_since = now;
        self.busy_level = if on {
            (self.busy_level + step).min(1.)
        } else {
            (self.busy_level - step).max(0.)
        };
        if self.busy_level != f32::from(u8::from(on)) {
            window.request_animation_frame();
        }
        self.busy = Busy {
            on,
            fade: self.busy_level * self.busy_level,
        };
    }

    /// Nothing can be applied while the core starts or stops, or another change is applied.
    fn busy(&self) -> bool {
        self.snap.applying.is_some() || matches!(self.snap.phase, Phase::Starting | Phase::Stopping)
    }

    fn apply_port(&mut self, port: Port, cx: &mut Context<Self>) {
        let value = self.draft(port, cx);
        if value == u32::from(self.saved_port(port)) {
            return self.settle_port(port, cx);
        }
        if self.busy() {
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

    /// A field that stands for the saved port, even empty, shows it again; a message an earlier
    /// draft left goes, as the backend clears its topic on a port it already has.
    fn settle_port(&mut self, port: Port, cx: &mut Context<Self>) {
        let saved = self.saved_port(port);
        if self.draft(port, cx) != u32::from(saved) {
            return;
        }
        let field = self.field(port);
        field.edited = false;
        let input = field.input.clone();
        input.update(cx, |i, cx| i.set_text(&saved.to_string(), cx));
        let (topic, cmd) = match port {
            Port::Proxy => (Topic::ProxyPort, Cmd::SetProxyPort(saved.into())),
            Port::Api => (Topic::ApiPort, Cmd::SetApiPort(saved.into())),
        };
        if self.snap.message(topic).is_some() && !self.busy() {
            self.handle.send(cmd);
        }
        cx.notify();
    }

    /// Leaving a port field settles it as Return does; a new port still waits for Return or Apply.
    pub fn watch_port_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for port in [Port::Proxy, Port::Api] {
            let focus = self.field(port).input.read(cx).focus.clone();
            cx.on_focus_out(&focus, window, move |this, _, _, cx| {
                this.settle_port(port, cx)
            })
            .detach();
        }
        let focus = self.latency_url.input.read(cx).focus.clone();
        cx.on_focus_out(&focus, window, |this, _, _, cx| this.settle_latency_url(cx))
            .detach();
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

    /// Whether a message is still shown: errors always, information until it has had its time.
    fn visible(&mut self, message: &Message, cx: &mut Context<Self>) -> bool {
        let id = message.id;
        if message.error {
            // Seen, which clears its dot on the other tabs; it never times out.
            self.shown.entry(id).or_insert(false);
            return true;
        }
        if !self.shown.contains_key(&id) {
            self.timer(MESSAGE_SHOWN, cx, move |this, _| {
                this.shown.insert(id, true);
            });
        }
        !*self.shown.entry(id).or_insert(false)
    }

    /// The area's messages still shown, newest first.
    fn current(&mut self, area: Area, cx: &mut Context<Self>) -> Vec<Message> {
        let snap = self.snap.clone();
        snap.messages_in(area)
            .into_iter()
            .filter(|m| self.visible(m, cx))
            .cloned()
            .collect()
    }

    fn colored(messages: Vec<Message>, p: Palette) -> Vec<(String, Hsla)> {
        messages
            .into_iter()
            .map(|m| (m.text, if m.error { p.danger } else { p.accent }))
            .collect()
    }

    /// How wide `text` is drawn, in the window's font.
    fn width(&self, text: &str, size: f32, weight: FontWeight, italic: bool) -> f32 {
        let Some((system, font)) = &self.measure else {
            return 0.;
        };
        let mut font = font.clone();
        font.weight = weight;
        if italic {
            font.style = gpui::FontStyle::Italic;
        }
        let run = TextRun {
            len: text.len(),
            font,
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = system.shape_line(text.to_owned().into(), px(size), &[run], None);
        f32::from(line.width)
    }

    /// The messages on a title's line in `room`: newest first, ` · ` between them, as many as
    /// fit whole and then `+N`; the newest alone is cut short. A hover lists them all.
    fn message_line(
        &self,
        selector: &str,
        items: Vec<(String, Hsla)>,
        room: f32,
        p: Palette,
    ) -> Option<Stateful<gpui::Div>> {
        if items.is_empty() {
            return None;
        }
        let measure = |text: &str| self.width(text, 10., FontWeight::NORMAL, true);
        let widths: Vec<f32> = items.iter().map(|(text, _)| measure(text)).collect();
        // A little slack, so rounding never pushes the last one out of the line.
        let fit = fitting(
            &widths,
            measure(SEPARATOR),
            |left| measure(&format!("+{left}")),
            room - 1.,
        );
        let shown = fit.max(1);
        let id = format!(
            "{selector}:{}",
            items
                .iter()
                .map(|(t, _)| t.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
        let name = selector.to_owned();
        let tips = items.clone();
        let mut line = div()
            .id(ElementId::Name(id.into()))
            .debug_selector(move || name)
            .flex()
            .min_w_0()
            .overflow_hidden()
            .mb(px(2.))
            .text_size(px(10.))
            .italic()
            .whitespace_nowrap()
            .tooltip(move |_, cx| {
                let items = tips.clone();
                cx.new(|_| Tips { items, p }).into()
            });
        for (i, (text, color)) in items.iter().take(shown).enumerate() {
            if i > 0 {
                line = line.child(div().flex_none().text_color(p.dim).child(SEPARATOR));
            }
            let item = div()
                .debug_selector(|| format!("{selector}-{i}"))
                .text_color(*color)
                .child(text.clone());
            line = line.child(if fit == 0 {
                item.min_w_0().truncate()
            } else {
                item.flex_none()
            });
        }
        if items.len() > shown {
            line = line.child(
                div()
                    .debug_selector(|| format!("{selector}-more"))
                    .flex_none()
                    .ml(px(MORE_GAP))
                    .text_color(p.dim)
                    .child(format!("+{}", items.len() - shown)),
            );
        }
        Some(line)
    }

    /// Reload, in Control and on the active profile in Settings. Always there, so an edit on disk
    /// only fills it in, like Check for Update; usable while the core runs, since it also
    /// restores a port another tool changed. It waits while providers update, as the backend does.
    fn reload_button(&self, id: impl Into<SharedString>, p: Palette) -> Stateful<gpui::Div> {
        let s = &self.snap;
        let usable = s.phase == Phase::Running && !self.busy() && !s.providers_busy;
        let edited = s.edited;
        icon_button(
            id,
            Icon::Reload,
            if edited {
                "Reload: the profile or overrides changed on disk"
            } else {
                "Reload profile"
            },
            p,
            edited,
            usable,
            self.cmd(|| Cmd::Reload),
        )
    }

    /// An error `tab` would show that has not been on screen yet: a dot on its segment.
    fn unseen_error(&self, tab: Tab) -> bool {
        tab.areas()
            .iter()
            .flat_map(|&area| self.snap.messages_in(area))
            .any(|m| m.error && !self.shown.contains_key(&m.id))
    }

    /// Beside Control's phase, Core's and the system proxy's messages, newest first, then a
    /// failure's reason. `true` marks an error.
    fn status_messages(&mut self, cx: &mut Context<Self>) -> Vec<(String, bool)> {
        let mut list: Vec<Message> = [Area::Core, Area::Proxy]
            .into_iter()
            .flat_map(|area| self.current(area, cx))
            .collect();
        list.sort_by_key(|m| std::cmp::Reverse(m.id));
        let mut list: Vec<(String, bool)> = list.into_iter().map(|m| (m.text, m.error)).collect();
        if let Phase::Failed(reason) = &self.snap.phase {
            list.push((reason.clone(), true));
        }
        list
    }

    /// A group's title with its messages to the right; `beside` is the width of what the row
    /// holds after them.
    fn titled(
        &mut self,
        title: &'static str,
        area: Area,
        busy: Option<String>,
        beside: f32,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let copied = matches!(self.copied, Some((a, _)) if a == area);
        let mut items: Vec<(String, Hsla)> = busy
            .into_iter()
            .chain(copied.then(|| "Copied".to_owned()))
            .map(|text| (text, p.accent))
            .collect();
        let current = self.current(area, cx);
        items.extend(Self::colored(current, p));
        let room = f32::from(self.content_width)
            - self.width(title, 14., FontWeight::SEMIBOLD, false)
            - 6.
            - beside;
        let selector = format!("{}-message", title.to_lowercase());
        div()
            .flex()
            .items_end()
            .gap(px(6.))
            .child(heading(title))
            .children(self.message_line(&selector, items, room, p))
    }

    /// The window closed: what it showed is reset for the next time.
    pub fn closed(&mut self) {
        self.flush_placement();
        self.window = None;
        self.secret_shown = false;
        self.sync_visibility();
    }

    pub fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.secret_shown = false;
        self.note = None;
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

    fn render_control(
        &mut self,
        p: Palette,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // Less the window's padding; the node grid also has its own.
        let content = f32::from(width) - 24.;
        let (columns, card_width) = card_grid(content - 16., CARD_MIN_WIDTH);
        let (profile_columns, profile_width) = card_grid(content, PROFILE_MIN_WIDTH);
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
        let tun_on = s.tun_device.is_some();
        let (tun_label, tun_enabled) = tun_button(&s);
        // The phase is the heading.
        let failed = matches!(s.phase, Phase::Failed(_));
        let items: Vec<(String, Hsla)> = self
            .status_messages(cx)
            .into_iter()
            .map(|(text, error)| (text, if error { p.danger } else { p.accent }))
            .collect();
        let details = status_text(&s);
        let phase = phase_label(&s.phase);
        let room = f32::from(self.content_width)
            - self.width(phase, 14., FontWeight::SEMIBOLD, false)
            - if details.is_empty() {
                0.
            } else {
                self.width(&details, 12., FontWeight::NORMAL, false) + 6.
            }
            - 6.;
        let message = self.message_line("status-message", items, room, p);
        let status = div()
            .flex()
            .items_end()
            .gap(px(6.))
            .child(
                heading(phase)
                    .debug_selector(|| "status".to_owned())
                    .text_color(if failed { p.danger } else { p.text }),
            )
            .when(!details.is_empty(), |row| {
                row.child(
                    div()
                        .flex_none()
                        .mb(px(1.))
                        .text_color(p.dim)
                        .child(details),
                )
            })
            .children(message);

        // A list gets a heading, a setting a label beside it, as in Settings. Messages go beside
        // the headings, so none of them adds a line.
        // Stop stays usable while busy: it also interrupts a start. Below this row, everything
        // is covered at once.
        let cover = self.busy;
        let core = button(
            "core",
            core_label,
            p,
            false,
            s.phase != Phase::Stopping,
            self.cmd(core_cmd),
        );
        let head = div()
            .flex()
            .flex_col()
            .text_size(px(12.))
            .child(status)
            .child(field(
                "Core",
                div()
                    .flex()
                    .gap_2()
                    .child(match s.phase {
                        Phase::Running | Phase::Starting => core.into_any_element(),
                        _ => cover.cover(core).into_any_element(),
                    })
                    .child(cover.cover(button(
                        "proxy",
                        proxy_label,
                        p,
                        proxy_on,
                        proxy_enabled,
                        self.cmd(move || Cmd::SetProxy(!proxy_on)),
                    )))
                    .child(cover.cover(button(
                        "tun",
                        tun_label,
                        p,
                        tun_on,
                        tun_enabled,
                        self.cmd(move || Cmd::SetTun(!tun_on)),
                    ))),
                None,
                p,
            ));
        let mut column = div().flex().flex_col().text_size(px(12.));

        // Shown while stopped too: a choice is remembered and applied at the next start.
        let mut modes = div().flex().gap_1();
        for (mode, label) in [("rule", "Rule"), ("global", "Global"), ("direct", "Direct")] {
            modes = modes.child(button(
                mode,
                label,
                p,
                s.mode == mode,
                s.phase != Phase::Stopping,
                self.cmd(move || Cmd::SetMode(mode.to_owned())),
            ));
        }
        column = column
            .child(field("Mode", modes, None, p))
            .child(separator(p));

        let reload = self.reload_button("reload", p);
        let busy = s.providers_busy.then(|| "Updating providers…".to_owned());
        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    // Reload and the gap before it.
                    self.titled("Profiles", Area::Profiles, busy, 36., p, cx)
                        .debug_selector(|| "profiles".to_owned())
                        .flex_1()
                        .min_w_0(),
                )
                .child(reload),
        );

        // Equal columns across the content, like the node cards.
        let mut profiles = div().grid().grid_cols(profile_columns).gap_1().py(px(3.));
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
            // Its own frame, not a node card's: the two may change apart.
            profiles = profiles.child(
                chip(
                    id,
                    p,
                    selected,
                    profile.problem.is_none(),
                    self.cmd(move || Cmd::Activate(name.clone())),
                )
                .w(px(profile_width))
                .on_mouse_down(MouseButton::Right, Self::open_menu(target, cx))
                .child(div().truncate().child(label)),
            );
        }
        if s.profiles.is_empty() {
            profiles = div()
                .py(px(6.))
                .text_color(p.dim)
                .child("No profiles. Import or link one in Settings.");
        }
        column = column.child(profiles);

        let groups: Vec<_> = s
            .groups
            .iter()
            .filter(|g| running && (g.name != "GLOBAL" || s.mode == "global"))
            .collect();
        // A profile without groups, like the default direct one, gets no empty heading.
        if !groups.is_empty() {
            column = column.child(separator(p)).child(
                self.titled("Groups", Area::Groups, None, 0., p, cx)
                    .debug_selector(|| "groups".to_owned())
                    // The first name sits as far below as a Settings row's label does.
                    .mb(px(3.)),
            );
            for group in groups {
                let name = group.name.clone();
                let expanded = self.expanded.contains(&name);
                let toggle = name.clone();
                let bolt = self.group_bolt(&name, expanded, p);
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
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .min_w_0()
                            .child(div().text_color(p.dim).truncate().child(group.now.clone()))
                            .child(bolt),
                    );
                column = column.child(header);
                if expanded {
                    let mut options = div().grid().grid_cols(columns).gap_1().px_2().pb_2();
                    for proxy in group.shown() {
                        let (g, pick) = (name.clone(), proxy.clone());
                        let id = SharedString::from(format!("pick-{name}-{proxy}"));
                        let selected = *proxy == group.now;
                        let kind = s.nodes.get(proxy).map(|n| n.kind.clone());
                        let card = chip(
                            id,
                            p,
                            selected,
                            group.selectable(),
                            self.cmd(move || Cmd::Select {
                                group: g.clone(),
                                proxy: pick.clone(),
                            }),
                        );
                        // A long name ends in an ellipsis instead of widening its column.
                        let details = div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_1()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(if selected { p.on_accent } else { p.dim })
                                    .children(kind),
                            )
                            .child(self.latency_slot(&name, proxy, selected, p));
                        options = options.child(
                            card.w(px(card_width))
                                .child(div().truncate().child(proxy.clone()))
                                .child(details),
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
            .child(head)
            .child(self.busy.cover(column))
            .into_any_element()
    }

    /// A card's latency slot. It reaches into the card's padding at the right and bottom, so it is
    /// easy to hit, and its clicks never reach the card.
    fn latency_slot(&self, group: &str, member: &str, selected: bool, p: Palette) -> AnyElement {
        let (content, usable) = slot(&self.snap, member, selected, p);
        let id = format!("latency-{group}-{member}");
        let selector = id.clone();
        let handle = self.handle.clone();
        let member = member.to_owned();
        let frame = div()
            .id(ElementId::Name(id.clone().into()))
            .debug_selector(move || selector)
            .flex()
            .flex_none()
            .items_center()
            .justify_end()
            .min_w(px(28.))
            // The row's line plus the card's bottom padding, which it reaches into.
            .h(CONTROL_HEIGHT - px(5.))
            .mr(px(-8.))
            .mb(px(-3.))
            .pl_1()
            .pr(px(8.))
            .pb(px(3.))
            .text_size(px(11.))
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                if usable {
                    handle.send(Cmd::TestNode(member.clone()));
                }
            })
            .when(usable, |frame| frame.cursor_pointer());
        let bolt = if selected { p.on_accent } else { p.link };
        match content {
            Slot::Empty => frame.into_any_element(),
            Slot::Text(text, color) => frame.text_color(color).child(text).into_any_element(),
            Slot::Bolt { pulsing } => frame
                .child(pulse(
                    format!("pulse-{id}"),
                    div().child(symbol(Icon::Bolt, bolt)),
                    pulsing,
                ))
                .into_any_element(),
        }
    }

    /// Tests every member of an expanded group; collapsed, it only says so. Its clicks never
    /// expand or collapse the group.
    fn group_bolt(&self, group: &str, expanded: bool, p: Palette) -> AnyElement {
        let s = &self.snap;
        let usable = expanded && s.testing_group.is_none() && !s.group_targets(group).is_empty();
        let id = format!("test-group-{group}");
        let selector = id.clone();
        let handle = self.handle.clone();
        let target = group.to_owned();
        let testing = s.testing_group.as_deref() == Some(group);
        // Its own test running keeps the active color, so the pulse shows.
        let color = if usable || testing {
            p.link
        } else {
            p.dim.opacity(0.6)
        };
        let bolt = div()
            .id(ElementId::Name(id.clone().into()))
            .debug_selector(move || selector)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .w(px(20.))
            .h(px(16.))
            .rounded(px(4.))
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                if usable {
                    handle.send(Cmd::TestGroup(target.clone()));
                }
            })
            .when(usable, |b| {
                b.cursor_pointer().hover(move |st| st.bg(p.panel))
            })
            .when(!expanded, |b| {
                b.tooltip(move |_, cx| {
                    let text = "Expand to test".into();
                    cx.new(|_| Tip { text, p }).into()
                })
            })
            .child(symbol(Icon::Bolt, color));
        pulse(format!("pulse-{id}"), bolt, testing)
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
        self.busy
            .cover(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .child(header)
                    .child(list),
            )
            .size_full()
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
        // A bad value read at launch is fixed by choosing again, even the option already shown.
        let has_error = |topic| s.message(topic).is_some_and(|m| m.error);
        let dock_error = has_error(Topic::DockIcon);
        let mut dock = div().flex().gap_1();
        for (keep, id, label) in [
            (true, "dock-always", "Always"),
            (false, "dock-open", "While Open"),
        ] {
            dock = dock.child(button(
                id,
                label,
                p,
                s.keep_dock_icon == keep,
                s.keep_dock_icon != keep || dock_error,
                self.cmd(move || Cmd::SetKeepDockIcon(keep)),
            ));
        }
        column = column
            .child(self.titled("Appearance", Area::Files, None, 0., p, cx))
            .child(field("Icon", sets, None, p))
            .child(field("Dock Icon", dock, None, p))
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
                Icon::Download,
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
        let level_error = has_error(Topic::LogLevel);
        let mut levels = div().flex().flex_wrap().gap_1();
        for (level, label) in [
            (settings::LogLevel::Info, "Info"),
            (settings::LogLevel::Warning, "Warning"),
            (settings::LogLevel::Error, "Error"),
            (settings::LogLevel::Silent, "Silent"),
            (settings::LogLevel::Debug, "Debug"),
        ] {
            levels = levels.child(button(
                format!("log-level-{}", level.as_str()),
                label,
                p,
                s.log_level == level,
                s.log_level != level || level_error,
                self.cmd(move || Cmd::SetLogLevel(level)),
            ));
        }
        let stack_error = has_error(Topic::TunStack);
        let mut stacks = div().flex().flex_wrap().gap_1();
        for (stack, label) in [
            (settings::TunStack::Mips, "Mips"),
            (settings::TunStack::Gvisor, "gVisor"),
            (settings::TunStack::System, "System"),
        ] {
            stacks = stacks.child(button(
                format!("tun-stack-{}", stack.as_str()),
                label,
                p,
                s.tun_stack == stack,
                s.tun_stack != stack || stack_error,
                self.cmd(move || Cmd::SetTunStack(stack)),
            ));
        }
        column = column
            .child(self.titled("Runtime", Area::Update, s.update_busy.clone(), 0., p, cx))
            .child(field("Latency URL", self.latency_controls(p, cx), None, p))
            .child(field("Log Level", levels, None, p))
            .child(field("TUN Stack", stacks, None, p))
            .child(field("Core", versions, Some(action.into_any_element()), p));
        div()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            // A click outside any field leaves the one being edited; a field's own click is
            // already handled and stays in it.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    if !window.default_prevented() {
                        this.focus.focus(window);
                    }
                }),
            )
            .child(self.busy.cover(column))
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
        column = column.child(self.titled("Profiles", Area::Profiles, busy, 0., p, cx));
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
            // The active profile can be reloaded and its providers updated; the others can only
            // be deleted. Update and Delete share the right edge.
            let action = if active {
                div()
                    .flex()
                    .flex_none()
                    .gap_1()
                    .child(self.reload_button(format!("reload-{}", profile.name), p))
                    .child(icon_button(
                        format!("update-{}", profile.name),
                        Icon::Download,
                        "Update providers: download node lists and rule sets",
                        p,
                        false,
                        s.phase == Phase::Running && !s.providers_busy,
                        self.cmd(|| Cmd::UpdateProviders),
                    ))
                    .into_any_element()
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
                .into_any_element()
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
        column.child(list).child(toolbar).child(
            hint(
                "overrides.yaml in the config folder applies to every profile.".to_owned(),
                p,
            )
            .flex()
            .items_center()
            .gap_1()
            .child(
                self.note_icon(
                    "profiles-info",
                    "Ports, API, LAN access and TUN come from xihomo. Profiles cannot change them.",
                    p,
                    cx,
                )
                // The hover area reaches 6px past the 11px icon; the margins keep the line
                // laid out as if it held the icon alone.
                .size(px(23.))
                .p(px(6.))
                .m(px(-6.))
                .child(symbol(Icon::Info, p.dim.opacity(0.8)).size_full()),
            ),
        )
    }
}

impl AppState {
    /// An icon that explains itself in a note under it on a click, not on hover.
    fn note_icon(
        &self,
        id: &'static str,
        text: impl Into<SharedString>,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let icon = div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .relative()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.note = (this.note != Some(id)).then_some(id);
                cx.notify();
            }));
        if self.note != Some(id) {
            return icon;
        }
        let note = tip_box(text.into(), p)
            .debug_selector(|| "note".to_owned())
            // A click on the note closes it and must not reach the controls under it.
            .occlude()
            .mt_1();
        icon.on_mouse_down_out(cx.listener(|this, _, _, cx| {
            this.note = None;
            cx.notify();
        }))
        .child(
            div()
                .absolute()
                .top_full()
                .left_0()
                .child(deferred(anchored().child(note))),
        )
    }

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

    /// The field across the row, and Apply while it stands for another URL than the saved one.
    fn latency_controls(&mut self, p: Palette, cx: &mut Context<Self>) -> gpui::Div {
        let saved = self.snap.latency_url.clone();
        let field = &mut self.latency_url;
        let text = field.input.read(cx).text();
        if field.edited && text == saved {
            field.edited = false;
        }
        if !field.edited && text != saved {
            field.input.update(cx, |i, cx| i.set_text(&saved, cx));
        }
        let changed = self.latency_draft(cx) != saved;
        div()
            .flex()
            .items_center()
            .gap_1()
            // The row's width, so the field gives way to Apply in a narrow window.
            .w_full()
            .child(
                div()
                    .debug_selector(|| "latency-url-input".to_owned())
                    // Room for the default URL; a longer one scrolls.
                    .w(px(300.))
                    .flex_shrink()
                    .min_w_0()
                    .rounded(px(5.))
                    .bg(p.panel)
                    .border_1()
                    .border_color(p.border)
                    .font_family("Menlo")
                    .text_color(p.text)
                    .overflow_hidden()
                    .child(self.latency_url.input.clone()),
            )
            .children(changed.then(|| {
                button(
                    "latency-url-apply",
                    "Apply",
                    p,
                    false,
                    true,
                    cx.listener(|this, _, _, cx| this.apply_latency_url(cx)),
                )
            }))
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
        column = column.child(self.titled("Proxy", Area::Listen, busy, 0., p, cx));
        let (input, apply) = self.port_controls(Port::Proxy, "proxy-port", p, cx);
        column = column.child(field(
            "Mixed Port",
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
        // A bad value read at launch is fixed by choosing again, even the option already shown.
        let lan_error = s.message(Topic::Lan).is_some_and(|m| m.error);
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
                enabled && (lan || lan_error),
                self.cmd(|| Cmd::SetAllowLan(false)),
            ))
            .child(button(
                "lan-on",
                "This network",
                p,
                lan,
                enabled && (!lan || lan_error),
                self.cmd(|| Cmd::SetAllowLan(true)),
            ));
        if lan {
            choice = match s.lan_ip.map(|ip| format!("{ip}:{}", s.mixed_port)) {
                Some(address) => choice.child(mono(address).ml_2()),
                None => choice.child(div().ml_2().text_color(p.dim).child("No network")),
            };
            if let Some(port) = s.dns_port_taken {
                choice = choice.child(
                    self.note_icon("dns-port-taken", dns_port_text(port), p, cx)
                        // As tall as the buttons beside it, so the triangle sits on their centre
                        // line, with a few pixels either side to hover.
                        .h(CONTROL_HEIGHT)
                        .px_1()
                        .flex()
                        .items_center()
                        .child(symbol(Icon::Warning, p.warning).size(px(13.))),
                );
            }
        }
        column = column
            .child(field("Available To", choice, None, p))
            .child(separator(p));

        let busy = (s.applying == Some(Area::Api)).then(|| "Restarting core…".to_owned());
        column = column.child(self.titled("API", Area::Api, busy, 0., p, cx));
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
        column.child(separator(p))
    }
}

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(self.snap.theme, window.appearance());
        self.measure = Some((window.text_system().clone(), window.text_style().font()));
        // Less the window's padding.
        self.content_width = window.viewport_size().width - px(24.);
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
                    "Delete",
                    self.snap.active.as_ref() == Some(name),
                ),
            };
            let in_use = in_use || self.snap.busy;
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
        self.update_busy(window, cx);
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
            // The body above has been drawn, so what it showed already counts as seen.
            let dot = !selected && self.unseen_error(tab);
            let mut segment = div()
                .id(id)
                .debug_selector(|| id.to_owned())
                .relative()
                .px_3()
                .py(px(3.))
                .text_size(px(12.))
                .cursor_pointer()
                .border_t_1()
                .border_b_1()
                .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
                .child(label)
                // In the corner of the padding, so it changes no width.
                .when(dot, |segment| {
                    segment.child(
                        div()
                            .debug_selector(move || format!("{id}-dot"))
                            .absolute()
                            .top(px(3.))
                            .right(px(3.))
                            .size(px(6.))
                            .rounded_full()
                            .bg(p.danger),
                    )
                });
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
    fn cards_stay_between_the_minimum_and_one_and_a_half_times_it_and_fit() {
        let two = (CARD_MIN_WIDTH * 2. + CARD_GAP) as u32;
        for width in (two..3000).map(|w| w as f32) {
            let (n, card) = card_grid(width, CARD_MIN_WIDTH);
            assert!(card >= CARD_MIN_WIDTH, "{width}: {card}");
            assert!(card < CARD_MIN_WIDTH * 1.5 + CARD_GAP, "{width}: {card}");
            assert!(
                card * n as f32 + (n - 1) as f32 * CARD_GAP <= width,
                "{width}"
            );
        }
        assert_eq!(card_grid(200., CARD_MIN_WIDTH).0, 2);
        assert_eq!(card_grid(600., CARD_MIN_WIDTH).0, 3);
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
            let mut state = AppState::new(handle, cx);
            state.watch_port_focus(window, cx);
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
        /// Each frame also stands for a backend that has handled what was sent.
        fn frame(&mut self) {
            self.view.update(self.cx, |this, _| this.handle.settle());
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
    fn groups_heading_is_shown_only_above_groups(cx: &mut TestAppContext) {
        {
            let ui = open(cx, running());
            assert!(ui.cx.debug_bounds("groups").is_some());
        }
        // The default profile runs direct and lists nothing but GLOBAL, which is hidden then.
        let only_global = Snapshot {
            mode: "direct".into(),
            groups: vec![Group {
                name: "GLOBAL".into(),
                kind: "Selector".into(),
                now: "DIRECT".into(),
                all: vec!["DIRECT".into()],
            }],
            ..running()
        };
        let ui = open(cx, only_global);
        assert!(ui.cx.debug_bounds("groups").is_none());
    }

    #[gpui::test]
    fn control_rows_share_the_settings_label_column(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        let bounds = |ui: &mut Ui, id| ui.cx.debug_bounds(id).unwrap();
        let start = bounds(&mut ui, "core").left();
        assert_eq!(bounds(&mut ui, "rule").left(), start);
        for (label, id) in [
            ("label-Core", "core"),
            ("label-Core", "proxy"),
            ("label-Mode", "rule"),
        ] {
            let offset = (bounds(&mut ui, id).center().y - bounds(&mut ui, label).center().y).abs();
            assert!(offset < px(1.), "{id} is off its label by {offset:?}");
        }
        ui.click("tab-settings");
        assert_eq!(bounds(&mut ui, "theme-auto").left(), start);
    }

    #[gpui::test]
    fn profile_buttons_share_a_width_and_wrap_under_the_first(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        // The window's minimum width: two columns, so the third profile starts a new line.
        ui.cx.simulate_resize(gpui::size(px(480.), px(680.)));
        ui.frame();
        let bounds = |ui: &mut Ui, id| ui.cx.debug_bounds(id).unwrap();
        let [home, work, gone] =
            ["profile-home", "profile-work", "profile-gone"].map(|id| bounds(&mut ui, id));
        assert_eq!(home.size.width, work.size.width);
        assert_eq!(home.size.width, gone.size.width);
        assert_eq!(home.top(), work.top());
        assert!(gone.top() > home.bottom(), "a new line");
        assert_eq!(gone.left(), home.left(), "under the first button");
        // A list under its heading spans the content: from the headings' edge out to the
        // window's 12-point padding, where Reload sits.
        assert_eq!(home.left(), bounds(&mut ui, "profiles").left());
        assert_eq!(work.right(), px(480. - 12.));
        assert_eq!(bounds(&mut ui, "reload").right(), work.right());
    }

    #[gpui::test]
    fn control_messages_sit_beside_headings_and_add_no_line(cx: &mut TestAppContext) {
        let ids = ["rule", "profile-home", "group-PICK"];
        let tops = |ui: &mut Ui| ids.map(|id| ui.cx.debug_bounds(id).unwrap().top());
        let quiet = {
            let mut ui = open(cx, running());
            for id in ["status-message", "profiles-message", "groups-message"] {
                assert!(ui.cx.debug_bounds(id).is_none(), "{id}");
            }
            tops(&mut ui)
        };
        let message = |text: &str, error, id| Message {
            text: text.into(),
            error,
            id,
        };
        let mut busy = running();
        busy.edited = true;
        busy.messages.extend([
            (
                Topic::LastGood,
                message("Running the last-good configuration", true, 1),
            ),
            (Topic::SystemProxy, message("networksetup failed", true, 2)),
            (Topic::Providers, message("Updated 3 providers", false, 3)),
            (Topic::Groups, message("PICK has no member B", true, 4)),
        ]);
        let mut ui = open(cx, busy);
        for id in ["status-message", "profiles-message", "groups-message"] {
            assert!(ui.cx.debug_bounds(id).is_some(), "{id}");
        }
        assert_eq!(tops(&mut ui), quiet);
    }

    #[gpui::test]
    fn the_status_line_shows_the_newest_message_then_an_older_error(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        let shown = |ui: &mut Ui| ui.view.update(ui.cx, |this, cx| this.status_messages(cx));
        let message = |text: &str, error, id| Message {
            text: text.into(),
            error,
            id,
        };
        ui.snap(|s| {
            s.messages
                .insert(Topic::Core, message("Mode failed", true, 1));
            s.messages
                .insert(Topic::SystemProxy, message("Proxy note", false, 2));
        });
        assert_eq!(
            shown(&mut ui),
            [("Proxy note".into(), false), ("Mode failed".into(), true)],
            "both, newest first"
        );
        ui.wait(MESSAGE_SHOWN + Duration::from_millis(10));
        assert_eq!(
            shown(&mut ui),
            [("Mode failed".into(), true)],
            "errors stay"
        );
        // A failed start says why on the heading's line, as the last, oldest item.
        ui.snap(|s| s.phase = Phase::Failed("port 20999 is in use".into()));
        assert_eq!(
            shown(&mut ui),
            [
                ("Mode failed".into(), true),
                ("port 20999 is in use".into(), true)
            ]
        );
    }

    #[gpui::test]
    fn a_dot_marks_a_tab_with_an_error_not_seen_yet(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        let message = |text: &str, error, id| Message {
            text: text.into(),
            error,
            id,
        };
        let dots =
            |ui: &mut Ui| ui.read(|s| [Tab::Control, Tab::Settings].map(|t| s.unseen_error(t)));
        // An error for Settings while Control is on screen.
        ui.snap(|s| {
            s.messages
                .insert(Topic::ApiPort, message("Port 21000 is in use", true, 1));
        });
        assert_eq!(dots(&mut ui), [false, true]);
        assert!(ui.cx.debug_bounds("tab-settings-dot").is_some());
        // Nothing times out before it has been seen.
        ui.wait(MESSAGE_SHOWN * 10);
        assert_eq!(dots(&mut ui), [false, true]);
        ui.click("tab-settings");
        assert_eq!(dots(&mut ui), [false, false]);
        assert!(ui.cx.debug_bounds("api-message").is_some(), "still there");
        // A profile error seen here is not new to Control, which shows it too.
        ui.snap(|s| {
            s.messages
                .insert(Topic::Profile, message("Import failed", true, 2));
        });
        assert_eq!(dots(&mut ui), [false, false]);
        // Seen on neither until one of them shows it.
        ui.click("tab-logs");
        ui.snap(|s| {
            s.messages
                .insert(Topic::Profile, message("Activation failed", true, 3));
        });
        assert_eq!(dots(&mut ui), [true, true]);
        ui.click("tab-control");
        assert_eq!(dots(&mut ui), [false, false]);
        // Information gets no dot.
        ui.snap(|s| {
            s.messages
                .insert(Topic::CoreUpdate, message("Up to date", false, 4));
        });
        assert_eq!(dots(&mut ui), [false, false]);
    }

    #[gpui::test]
    fn reload_works_while_running_and_is_filled_after_an_edit(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        ui.click("reload");
        assert!(matches!(ui.sent()[..], [Cmd::Reload]), "no edit needed");
        ui.snap(|s| s.edited = true);
        ui.click("reload");
        assert!(matches!(ui.sent()[..], [Cmd::Reload]));
        ui.snap(|s| s.providers_busy = true);
        ui.click("reload");
        assert!(ui.sent().is_empty(), "waits for the providers update");
        ui.snap(|s| {
            s.providers_busy = false;
            s.phase = Phase::Stopped;
        });
        ui.click("reload");
        assert!(ui.sent().is_empty(), "nothing runs to reload");
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

    #[gpui::test]
    fn log_level_sits_above_core_under_runtime_and_follows_the_snapshot(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        let top = |ui: &mut Ui, id| ui.cx.debug_bounds(id).unwrap().top();
        assert!(top(&mut ui, "label-Log Level") < top(&mut ui, "label-Core"));
        let left = |ui: &mut Ui, id| ui.cx.debug_bounds(id).unwrap().left();
        let order = ["info", "warning", "error", "silent", "debug"].map(|l| {
            let id: &'static str = Box::leak(format!("log-level-{l}").into_boxed_str());
            left(&mut ui, id)
        });
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "Info, Warning, Error, Silent, Debug"
        );
        ui.click("log-level-info");
        assert!(ui.sent().is_empty(), "already Info");
        ui.click("log-level-debug");
        assert!(matches!(
            ui.sent()[..],
            [Cmd::SetLogLevel(settings::LogLevel::Debug)]
        ));
        ui.snap(|s| s.log_level = settings::LogLevel::Debug);
        ui.click("log-level-debug");
        assert!(ui.sent().is_empty(), "already Debug");
        ui.click("log-level-silent");
        assert!(matches!(
            ui.sent()[..],
            [Cmd::SetLogLevel(settings::LogLevel::Silent)]
        ));
    }

    #[test]
    fn the_tun_button_shows_the_actual_state() {
        let with = |phase: Phase, device: Option<&str>, busy| {
            tun_button(&Snapshot {
                phase,
                tun_device: device.map(str::to_owned),
                tun_busy: busy,
                ..Snapshot::default()
            })
        };
        assert_eq!(with(Phase::Stopped, None, None), ("TUN: Off", false));
        assert_eq!(with(Phase::Running, None, None), ("TUN: Off", true));
        assert_eq!(
            with(Phase::Running, None, Some(TunBusy::Authorizing)),
            ("TUN: Authorizing…", false)
        );
        assert_eq!(
            with(Phase::Running, None, Some(TunBusy::TurningOn)),
            ("TUN: Turning On…", false)
        );
        assert_eq!(
            with(Phase::Running, Some("utun9"), Some(TunBusy::TurningOff)),
            ("TUN: Turning Off…", false)
        );
        assert_eq!(with(Phase::Running, Some("utun9"), None), ("TUN: On", true));
    }

    #[gpui::test]
    fn the_tun_button_follows_system_proxy_and_sends_the_opposite(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        let bounds = |ui: &mut Ui, id| ui.cx.debug_bounds(id).unwrap();
        let (proxy, tun) = (bounds(&mut ui, "proxy"), bounds(&mut ui, "tun"));
        assert!(tun.left() > proxy.right() && tun.top() == proxy.top());
        ui.click("tun");
        assert!(matches!(ui.sent()[..], [Cmd::SetTun(true)]));
        ui.snap(|s| s.tun_device = Some("utun9".into()));
        ui.click("tun");
        assert!(matches!(ui.sent()[..], [Cmd::SetTun(false)]));
        ui.snap(|s| s.tun_busy = Some(TunBusy::TurningOff));
        ui.click("tun");
        assert!(ui.sent().is_empty(), "disabled while switching");
        ui.snap(|s| {
            s.tun_busy = None;
            s.tun_device = None;
            s.phase = Phase::Stopped;
        });
        ui.click("tun");
        assert!(ui.sent().is_empty(), "disabled while stopped");
    }

    #[gpui::test]
    fn while_busy_only_stop_can_be_clicked(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        ui.click("group-PICK");
        ui.drain();
        ui.snap(|s| s.busy = true);
        for id in ["tun", "proxy", "global", "profile-work", "pick-PICK-REJECT"] {
            ui.click(id);
            assert!(ui.sent().is_empty(), "{id}");
        }
        ui.click("core");
        assert!(matches!(ui.sent()[..], [Cmd::Stop]));
        ui.snap(|s| s.busy = false);
        ui.click("global");
        assert!(matches!(&ui.sent()[..], [Cmd::SetMode(m)] if m == "global"));
    }

    #[gpui::test]
    fn the_busy_look_fades_so_a_moment_of_busy_never_blinks(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        let fade = |ui: &mut Ui| ui.read(|s| s.busy.fade);
        let frame = Duration::from_millis(16);
        // A command of one frame: the area hardly changes, then is back.
        ui.snap(|s| s.busy = true);
        ui.wait(frame);
        assert!(fade(&mut ui) < 0.01, "{}", fade(&mut ui));
        ui.snap(|s| s.busy = false);
        ui.wait(frame);
        assert_eq!(fade(&mut ui), 0.);
        // A long one fades in fully over BUSY_FADE, and out again.
        ui.snap(|s| s.busy = true);
        for _ in 0..14 {
            ui.wait(frame);
        }
        assert_eq!(fade(&mut ui), 1.);
        ui.snap(|s| s.busy = false);
        ui.wait(BUSY_FADE / 2);
        let halfway = fade(&mut ui);
        assert!(0. < halfway && halfway < 1., "{halfway}");
        ui.wait(BUSY_FADE);
        assert_eq!(fade(&mut ui), 0.);
    }

    #[gpui::test]
    fn a_taken_dns_port_shows_a_warning_beside_the_lan_address(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        ui.snap(|s| {
            s.allow_lan = true;
            s.lan_ip = Some(Ipv4Addr::new(192, 168, 1, 23));
        });
        assert!(ui.cx.debug_bounds("dns-port-taken").is_none());
        ui.snap(|s| s.dns_port_taken = Some(1053));
        assert!(ui.cx.debug_bounds("dns-port-taken").is_some());
        assert_eq!(
            dns_port_text(1053),
            "This profile's DNS port 1053 is in use by another app, so other devices can't use the core for DNS."
        );
    }

    #[gpui::test]
    fn an_icon_shows_its_note_on_a_click_not_on_hover(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        let icon = ui.cx.debug_bounds("profiles-info").unwrap().center();
        ui.cx.simulate_mouse_move(icon, None, Modifiers::default());
        ui.frame();
        assert!(ui.cx.debug_bounds("note").is_none());
        ui.click("profiles-info");
        assert!(ui.cx.debug_bounds("note").is_some());
        ui.click("profiles-info");
        assert_eq!(ui.read(|a| a.note), None);
        ui.click("profiles-info");
        assert_eq!(ui.read(|a| a.note), Some("profiles-info"));
        ui.click("theme-dark");
        assert_eq!(ui.read(|a| a.note), None);
    }

    #[gpui::test]
    fn no_dns_warning_without_lan_access(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        ui.snap(|s| {
            s.allow_lan = false;
            s.lan_ip = Some(Ipv4Addr::new(192, 168, 1, 23));
            s.dns_port_taken = Some(53);
        });
        assert!(ui.cx.debug_bounds("dns-port-taken").is_none());
    }

    #[gpui::test]
    fn settings_are_covered_while_busy(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        ui.snap(|s| s.busy = true);
        for id in [
            "theme-dark",
            "tun-stack-gvisor",
            "log-level-debug",
            "import",
        ] {
            ui.click(id);
            assert!(ui.sent().is_empty(), "{id}");
        }
        ui.snap(|s| s.busy = false);
        ui.click("theme-dark");
        assert!(matches!(ui.sent()[..], [Cmd::SetTheme(Theme::Dark)]));
    }

    #[gpui::test]
    fn a_tun_reason_shows_on_controls_status_line_with_a_dot(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        ui.click("tab-logs");
        let text = "TUN did not start: Start TUN listening error: operation not permitted";
        ui.snap(|s| {
            s.messages.insert(
                Topic::Tun,
                Message {
                    text: text.into(),
                    error: true,
                    id: 1,
                },
            );
        });
        assert!(ui.read(|s| s.unseen_error(Tab::Control)));
        assert!(ui.cx.debug_bounds("tab-control-dot").is_some());
        ui.click("tab-control");
        let shown = ui.view.update(ui.cx, |this, cx| this.status_messages(cx));
        assert_eq!(shown, [(text.to_owned(), true)]);
        assert!(!ui.read(|s| s.unseen_error(Tab::Control)));
    }

    #[gpui::test]
    fn tun_stack_sits_between_log_level_and_core(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        let top = |ui: &mut Ui, id| ui.cx.debug_bounds(id).unwrap().top();
        assert!(top(&mut ui, "label-Log Level") < top(&mut ui, "label-TUN Stack"));
        assert!(top(&mut ui, "label-TUN Stack") < top(&mut ui, "label-Core"));
        let left = |ui: &mut Ui, id| ui.cx.debug_bounds(id).unwrap().left();
        let order =
            ["tun-stack-mips", "tun-stack-gvisor", "tun-stack-system"].map(|id| left(&mut ui, id));
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "Mips, gVisor, System"
        );
        ui.click("tun-stack-mips");
        assert!(ui.sent().is_empty(), "already Mips");
        ui.click("tun-stack-gvisor");
        assert!(matches!(
            ui.sent()[..],
            [Cmd::SetTunStack(settings::TunStack::Gvisor)]
        ));
        ui.snap(|s| s.tun_stack = settings::TunStack::System);
        ui.click("tun-stack-system");
        assert!(ui.sent().is_empty(), "already System");
        ui.click("tun-stack-mips");
        assert!(matches!(
            ui.sent()[..],
            [Cmd::SetTunStack(settings::TunStack::Mips)]
        ));
    }

    #[gpui::test]
    fn dock_icon_choice_follows_the_snapshot_on_the_label_column(cx: &mut TestAppContext) {
        let mut ui = open(cx, running());
        ui.keys("cmd-4");
        ui.drain();
        ui.click("dock-open");
        assert!(ui.sent().is_empty(), "already While Open");
        ui.click("dock-always");
        assert!(matches!(ui.sent()[..], [Cmd::SetKeepDockIcon(true)]));
        ui.set(|s| {
            s.snap = Arc::new(Snapshot {
                keep_dock_icon: true,
                ..(*s.snap).clone()
            })
        });
        ui.click("dock-always");
        assert!(ui.sent().is_empty(), "already Always");
        ui.click("dock-open");
        assert!(matches!(ui.sent()[..], [Cmd::SetKeepDockIcon(false)]));

        let bounds = |ui: &mut Ui, id| ui.cx.debug_bounds(id).unwrap();
        assert_eq!(
            bounds(&mut ui, "dock-always").left(),
            bounds(&mut ui, "theme-auto").left()
        );
        let (label, open) = (
            bounds(&mut ui, "label-Dock Icon"),
            bounds(&mut ui, "dock-open"),
        );
        assert!((open.center().y - label.center().y).abs() < px(1.));
        assert_eq!(open.size.height, CONTROL_HEIGHT);
        assert!(bounds(&mut ui, "label-Icon").top() < label.top());
        assert!(label.top() < bounds(&mut ui, "label-Theme").top());
    }

    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789ab3f9a";

    /// This build's default ports, proxy and API, as the backend would report them.
    fn defaults() -> (u16, u16) {
        let d = settings::Settings::default();
        (d.mixed_port, d.controller_port)
    }

    /// Settings with this build's default ports.
    fn settings_tab(cx: &mut TestAppContext) -> Ui<'_> {
        let (mixed_port, controller_port) = defaults();
        let snapshot = Snapshot {
            mixed_port,
            controller_port,
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
        // Beside the phase, which is the heading.
        assert_eq!(
            status_text(&s),
            format!("v1.19.33 · proxy 10999 · API 11000{dev}")
        );
        s.allow_lan = true;
        assert_eq!(
            status_text(&s),
            format!("v1.19.33 · proxy 10999 · LAN · API 11000{dev}")
        );
        s.tun_device = Some("utun9".into());
        assert_eq!(
            status_text(&s),
            format!("v1.19.33 · proxy 10999 · LAN · API 11000 · TUN utun9{dev}")
        );
        // A failure's reason takes the room; only the channel stays.
        s.phase = Phase::Failed("boom".into());
        assert_eq!(status_text(&s), dev.trim_start_matches(" · "));
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
        let (proxy, _) = defaults();
        assert_eq!(ui.text(Port::Proxy), proxy.to_string());
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
        assert_eq!(
            ui.text(Port::Proxy),
            proxy.to_string(),
            "back to the saved value"
        );
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
    fn apply_appearing_leaves_the_rows_where_they_are(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        let below = |ui: &mut Ui| {
            ["lan-off", "regenerate", "check"].map(|id| ui.cx.debug_bounds(id).unwrap().top())
        };
        let before = below(&mut ui);
        for input in ["proxy-port-input", "api-port-input"] {
            ui.click(input);
            ui.keys("cmd-a");
            ui.cx.simulate_input("7890");
        }
        // Not even by the fraction of a point a taller button would add.
        assert_eq!(below(&mut ui), before);
        for id in [
            "proxy-port-input",
            "proxy-port-apply",
            "lan-off",
            "theme-auto",
        ] {
            let height = ui.cx.debug_bounds(id).unwrap().size.height;
            assert_eq!(height, CONTROL_HEIGHT, "{id}");
        }
    }

    #[gpui::test]
    fn an_empty_port_field_means_the_default(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        let (proxy, _) = defaults();
        ui.snap(|s| s.mixed_port = 7000);
        assert_eq!(ui.text(Port::Proxy), "7000");
        ui.click("proxy-port-input");
        ui.keys("cmd-a backspace");
        assert_eq!(ui.text(Port::Proxy), "", "an empty field is not refilled");
        ui.keys("enter");
        assert!(matches!(ui.sent()[..], [Cmd::SetProxyPort(p)] if p == u32::from(proxy)));
        assert_eq!(ui.text(Port::Proxy), proxy.to_string());
        // Saved as the default: no Apply, and an empty field stands for the same value.
        ui.snap(|s| s.mixed_port = proxy);
        ui.keys("cmd-a backspace enter");
        assert!(ui.sent().is_empty());
        assert_eq!(
            ui.text(Port::Proxy),
            proxy.to_string(),
            "refilled with the saved port"
        );
    }

    #[gpui::test]
    fn a_field_settled_at_the_saved_port_refills_and_clears_the_old_error(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        let (proxy, api) = defaults();
        // GPUI reports focus changes only in the active window, as the one being typed in is.
        ui.cx.update(|window, _| window.activate_window());
        let error = || Message {
            text: "Port 80 is below 1024".into(),
            error: true,
            id: 1,
        };
        ui.click("proxy-port-input");
        ui.keys("cmd-a");
        ui.cx.simulate_input("80");
        ui.keys("enter");
        assert!(matches!(ui.sent()[..], [Cmd::SetProxyPort(80)]));
        ui.snap(|s| {
            s.messages.insert(Topic::ProxyPort, error());
        });
        // Emptied, it stands for the saved default: Return refills it and clears the message.
        ui.keys("cmd-a backspace enter");
        assert_eq!(ui.text(Port::Proxy), proxy.to_string());
        assert!(matches!(ui.sent()[..], [Cmd::SetProxyPort(p)] if p == u32::from(proxy)));

        // Leaving the field does the same.
        ui.snap(|s| {
            s.messages.clear();
            s.messages.insert(Topic::ApiPort, error());
        });
        ui.click("api-port-input");
        ui.keys("cmd-a backspace");
        ui.click("label-Theme");
        assert_eq!(ui.text(Port::Api), api.to_string());
        assert!(matches!(ui.sent()[..], [Cmd::SetApiPort(p)] if p == u32::from(api)));

        // Without a message nothing is sent, and a new port is kept, not applied, on leaving.
        ui.snap(|s| s.messages.clear());
        ui.click("api-port-input");
        ui.keys("cmd-a backspace");
        ui.click("label-Theme");
        assert_eq!(ui.text(Port::Api), api.to_string());
        ui.click("proxy-port-input");
        ui.keys("cmd-a");
        ui.cx.simulate_input("7890");
        ui.click("label-Theme");
        assert_eq!(ui.text(Port::Proxy), "7890");
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
        // Reload sits beside it and works as Control's does.
        ui.click("reload-home");
        assert!(matches!(ui.sent()[..], [Cmd::Reload]));
        ui.snap(|s| s.providers_busy = true);
        ui.click("update-home");
        ui.click("reload-home");
        ui.snap(|s| {
            s.providers_busy = false;
            s.phase = Phase::Stopped;
        });
        ui.click("update-home");
        ui.click("reload-home");
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
                .insert(Topic::ProxyPort, message("Applied", false, 1));
            s.messages
                .insert(Topic::ApiPort, message("Port 21000 is in use", true, 2));
        });
        let current = |ui: &mut Ui, area| {
            ui.view.update(ui.cx, |this, cx| {
                this.current(area, cx).first().map(|m| m.text.clone())
            })
        };
        assert_eq!(current(&mut ui, Area::Listen).as_deref(), Some("Applied"));
        ui.wait(MESSAGE_SHOWN + Duration::from_millis(10));
        assert_eq!(current(&mut ui, Area::Listen), None);
        assert!(current(&mut ui, Area::Api).is_some(), "errors stay");
        // The same text again is a new message.
        ui.snap(|s| {
            s.messages
                .insert(Topic::ProxyPort, message("Applied", false, 3));
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
            "reload-home",
        ] {
            let bounds = ui.cx.debug_bounds(id).unwrap();
            assert_eq!(
                (bounds.size.width, bounds.size.height),
                (px(28.), px(24.)),
                "{id}"
            );
            if !["secret-show", "reload-home"].contains(&id) {
                assert_eq!(bounds.right(), right, "{id} shares the right edge");
            }
        }
        // A second button sits just left of the one on the edge.
        for (left, edge) in [
            ("secret-show", "regenerate"),
            ("reload-home", "update-home"),
        ] {
            let gap = ui.cx.debug_bounds(edge).unwrap().left()
                - ui.cx.debug_bounds(left).unwrap().right();
            assert_eq!(gap, px(4.), "{left}");
        }
        // Text, buttons and icon buttons are centred on their row's label.
        for (label, ids) in [
            ("label-Secret", &["secret", "secret-show", "regenerate"][..]),
            ("label-Available To", &["lan-off"]),
            ("label-Core", &["check"]),
            ("label-Log Level", &["log-level-silent"]),
        ] {
            let label = ui.cx.debug_bounds(label).unwrap();
            for id in ids {
                let offset = (ui.cx.debug_bounds(id).unwrap().center().y - label.center().y).abs();
                assert!(offset < px(1.), "{id} is off its label by {offset:?}");
            }
        }
    }

    /// `running()` with each name's kind; PICK selects DIRECT, AUTO picked A.
    fn with_nodes() -> Snapshot {
        let node = |kind: &str, latency| crate::api::Node {
            kind: kind.into(),
            provider: None,
            latency,
        };
        let mut snap = running();
        snap.nodes = [
            ("DIRECT", node("Direct", None)),
            ("REJECT", node("Reject", None)),
            ("A", node("Vless", Some(Latency::Ms(123)))),
            ("B", node("Vless", Some(Latency::Failed))),
            ("PICK", node("Selector", None)),
            ("AUTO", node("URLTest", None)),
        ]
        .into_iter()
        .map(|(n, node)| (n.to_owned(), node))
        .collect();
        snap.latency_url = settings::DEFAULT_LATENCY_URL.into();
        snap
    }

    #[test]
    fn the_slot_shows_the_state_of_the_cards_node() {
        let mut s = with_nodes();
        let p = palette(Theme::Light, WindowAppearance::Light);
        let text = |t: &str, c| (Slot::Text(t.into(), c), true);
        assert_eq!(slot(&s, "A", false, p), text("123 ms", p.good));
        assert_eq!(slot(&s, "B", false, p), text("Timeout", p.danger));
        assert_eq!(
            slot(&s, "DIRECT", false, p),
            (Slot::Bolt { pulsing: false }, true)
        );
        assert_eq!(slot(&s, "REJECT", false, p), (Slot::Empty, false));
        assert_eq!(
            slot(&s, "AUTO", false, p),
            text("123 ms", p.good),
            "AUTO's pick"
        );
        assert_eq!(
            slot(&s, "A", true, p),
            text("123 ms", p.on_accent),
            "on the accent"
        );
        s.nodes.get_mut("A").unwrap().latency = Some(Latency::Ms(600));
        assert_eq!(slot(&s, "A", false, p), text("600 ms", p.warning));
        s.testing.insert("A".into());
        assert_eq!(
            slot(&s, "A", false, p),
            (Slot::Bolt { pulsing: true }, false)
        );
        s.testing.clear();
        s.testing_group = Some("PICK".into());
        assert!(!slot(&s, "B", false, p).1, "not during a group test");
        let dark = palette(Theme::Dark, WindowAppearance::Dark);
        assert_ne!(p.good, dark.good, "a green for each theme");
    }

    #[gpui::test]
    fn a_click_on_the_slot_tests_and_never_selects(cx: &mut TestAppContext) {
        let mut ui = open(cx, with_nodes());
        ui.click("group-PICK");
        ui.click("group-AUTO");
        ui.drain();
        let card = ui.cx.debug_bounds("pick-PICK-DIRECT").unwrap();
        let slot = ui.cx.debug_bounds("latency-PICK-DIRECT").unwrap();
        // Out to the card's right and bottom edges, inside its border.
        assert!(card.right() - slot.right() <= px(1.) && card.bottom() - slot.bottom() <= px(1.));
        assert!(slot.size.width >= px(28.) && slot.size.height >= px(20.));
        assert!(slot.top() > ui.cx.debug_bounds("pick-PICK-REJECT").unwrap().top());
        ui.click("latency-PICK-DIRECT");
        assert!(matches!(&ui.sent()[..], [Cmd::TestNode(m)] if m == "DIRECT"));
        ui.click("latency-PICK-REJECT");
        assert!(
            ui.sent().is_empty(),
            "REJECT is never tested, nor selected by its slot"
        );
        // A URLTest card does nothing itself; its slot still tests.
        ui.click("latency-AUTO-B");
        assert!(matches!(&ui.sent()[..], [Cmd::TestNode(m)] if m == "B"));
        ui.click("pick-PICK-REJECT");
        assert!(
            matches!(&ui.sent()[..], [Cmd::Select { .. }]),
            "the card still selects"
        );

        ui.snap(|s| s.testing_group = Some("AUTO".into()));
        ui.click("latency-PICK-DIRECT");
        ui.click("test-group-PICK");
        assert!(ui.sent().is_empty(), "everything waits for the group test");
    }

    #[gpui::test]
    fn the_group_bolt_tests_only_an_expanded_group_and_never_toggles(cx: &mut TestAppContext) {
        let mut ui = open(cx, with_nodes());
        let row = ui.cx.debug_bounds("group-PICK").unwrap();
        let bolt = ui.cx.debug_bounds("test-group-PICK").unwrap();
        assert!(
            row.right() - bolt.right() < px(12.),
            "at the end of the row"
        );
        ui.click("test-group-PICK");
        assert!(ui.sent().is_empty());
        assert!(
            ui.cx.debug_bounds("pick-PICK-DIRECT").is_none(),
            "still collapsed"
        );
        ui.click("group-PICK");
        ui.click("test-group-PICK");
        assert!(matches!(&ui.sent()[..], [Cmd::TestGroup(g)] if g == "PICK"));
        assert!(ui.read(|s| s.expanded.contains("PICK")), "still expanded");
    }

    #[gpui::test]
    fn the_latency_url_applies_on_return_and_reverts_on_escape(cx: &mut TestAppContext) {
        let snapshot = Snapshot {
            secret: SECRET.into(),
            ..with_nodes()
        };
        let mut ui = open(cx, snapshot);
        ui.keys("cmd-4");
        ui.drain();
        let text = |ui: &mut Ui| {
            ui.view
                .update(ui.cx, |this, cx| this.latency_url.input.read(cx).text())
        };
        assert_eq!(text(&mut ui), settings::DEFAULT_LATENCY_URL);
        let label = ui.cx.debug_bounds("label-Latency URL").unwrap();
        assert!(label.top() < ui.cx.debug_bounds("label-Log Level").unwrap().top());
        let field = ui.cx.debug_bounds("latency-url-input").unwrap();
        assert_eq!(
            field.size.width,
            px(300.),
            "room for the default, not the whole row"
        );
        assert!(ui.cx.debug_bounds("latency-url-apply").is_none());
        ui.click("latency-url-input");
        ui.keys("cmd-a");
        ui.cx.simulate_input("http://127.0.0.1:8080/204");
        assert!(ui.cx.debug_bounds("latency-url-apply").is_some());
        ui.keys("escape");
        assert_eq!(
            text(&mut ui),
            settings::DEFAULT_LATENCY_URL,
            "back to the saved one"
        );
        // Drawn elements stay in the test's map, so Apply's absence is read from the state.
        let apply_shown = |ui: &mut Ui| {
            ui.view.update(ui.cx, |this, cx| {
                this.latency_draft(cx) != this.snap.latency_url
            })
        };
        assert!(!apply_shown(&mut ui));

        ui.keys("cmd-a");
        ui.cx.simulate_input("not a url");
        ui.keys("enter");
        assert!(matches!(&ui.sent()[..], [Cmd::SetLatencyUrl(u)] if u == "not a url"));
        // Refused: the error stays beside Runtime and so does the draft.
        ui.snap(|s| {
            s.messages.insert(
                Topic::LatencyUrl,
                Message {
                    text: "Use an http or https URL".into(),
                    error: true,
                    id: 1,
                },
            );
        });
        assert_eq!(text(&mut ui), "not a url");
        // Emptied, it stands for the saved default: Return refills it and clears the error.
        ui.keys("cmd-a backspace enter");
        assert_eq!(text(&mut ui), settings::DEFAULT_LATENCY_URL);
        assert!(
            matches!(&ui.sent()[..], [Cmd::SetLatencyUrl(u)] if u == settings::DEFAULT_LATENCY_URL)
        );
        ui.keys("cmd-a");
        ui.cx.simulate_input("https://example.com/x");
        ui.click("latency-url-apply");
        assert!(matches!(&ui.sent()[..], [Cmd::SetLatencyUrl(u)] if u == "https://example.com/x"));
        ui.snap(|s| s.latency_url = "https://example.com/x".into());
        assert_eq!(text(&mut ui), "https://example.com/x");
        assert!(!apply_shown(&mut ui));
    }

    #[test]
    fn as_many_whole_messages_fit_as_leave_room_for_the_count() {
        let more = |_| 15.;
        let widths = [50., 50., 50.];
        assert_eq!(fitting(&widths, 10., more, 170.), 3, "all, no count");
        assert_eq!(fitting(&widths, 10., more, 169.), 2, "two, then +1");
        assert_eq!(
            fitting(&widths, 10., more, 130.),
            1,
            "two would leave no room for +1"
        );
        assert_eq!(
            fitting(&widths, 10., more, 70.),
            0,
            "the newest is cut short"
        );
        assert_eq!(
            fitting(&[50.], 10., more, 50.),
            1,
            "alone, it needs no count"
        );
    }

    /// Settings → Proxy with three topics: two errors and the newest, information.
    fn three_proxy_messages() -> Snapshot {
        let (mixed_port, controller_port) = defaults();
        let message = |text: &str, error, id| Message {
            text: text.into(),
            error,
            id,
        };
        let mut snap = Snapshot {
            mixed_port,
            controller_port,
            secret: SECRET.into(),
            ..running()
        };
        snap.messages.extend([
            (Topic::ProxyPort, message("Port 7890 is in use", true, 1)),
            (
                Topic::ExternalPort,
                message("Another tool set port 0. Reload restores 20999", true, 2),
            ),
            (
                Topic::Lan,
                message("Other devices can use 192.168.1.23:20999", false, 3),
            ),
        ]);
        snap
    }

    #[gpui::test]
    fn messages_share_the_title_line_and_count_what_does_not_fit(cx: &mut TestAppContext) {
        let tops = |ui: &mut Ui| {
            ["label-Mixed Port", "lan-off", "label-Secret"]
                .map(|id| ui.cx.debug_bounds(id).unwrap().top())
        };
        let narrow = |ui: &mut Ui| {
            ui.cx.simulate_resize(gpui::size(px(480.), px(900.)));
            ui.keys("cmd-4");
        };
        let quiet = {
            let mut ui = settings_tab(cx);
            narrow(&mut ui);
            tops(&mut ui)
        };
        // Wide: all three, newest first, in one line.
        {
            let mut ui = open(cx, three_proxy_messages());
            ui.cx.simulate_resize(gpui::size(px(1400.), px(900.)));
            ui.keys("cmd-4");
            let at = |ui: &mut Ui, i: usize| {
                let id: &'static str = ["proxy-message-0", "proxy-message-1", "proxy-message-2"][i];
                ui.cx.debug_bounds(id).unwrap()
            };
            let (first, second, third) = (at(&mut ui, 0), at(&mut ui, 1), at(&mut ui, 2));
            assert!(first.right() < second.left() && second.right() < third.left());
            assert_eq!(first.top(), third.top(), "one line");
            assert!(ui.cx.debug_bounds("proxy-message-more").is_none());
            // The newest is the information message, shown before it expires.
            let items = ui
                .view
                .update(ui.cx, |this, cx| this.current(Area::Listen, cx));
            assert_eq!(items[0].text, "Other devices can use 192.168.1.23:20999");
        }
        // At 480 points: what fits, then the count; nothing moves down.
        let mut ui = open(cx, three_proxy_messages());
        narrow(&mut ui);
        let line = ui.cx.debug_bounds("proxy-message").unwrap();
        let more = ui.cx.debug_bounds("proxy-message-more").unwrap();
        assert!(
            more.right() <= line.right() + px(0.5),
            "the count stays inside"
        );
        assert!(
            line.right() <= px(480. - 12.) + px(0.5),
            "the line never overflows"
        );
        assert!(ui.cx.debug_bounds("proxy-message-2").is_none());
        assert_eq!(tops(&mut ui), quiet, "the title line keeps its height");
    }

    #[gpui::test]
    fn the_newest_alone_is_cut_short_before_the_count(cx: &mut TestAppContext) {
        let mut snap = three_proxy_messages();
        snap.messages.get_mut(&Topic::Lan).unwrap().text = "x".repeat(300);
        let mut ui = open(cx, snap);
        ui.cx.simulate_resize(gpui::size(px(480.), px(900.)));
        ui.keys("cmd-4");
        let line = ui.cx.debug_bounds("proxy-message").unwrap();
        let newest = ui.cx.debug_bounds("proxy-message-0").unwrap();
        assert!(newest.right() < ui.cx.debug_bounds("proxy-message-more").unwrap().left());
        assert!(line.right() <= px(480. - 12.) + px(0.5));
        assert!(ui.cx.debug_bounds("proxy-message-1").is_none());
    }

    #[gpui::test]
    fn a_settled_field_clears_only_its_own_topic(cx: &mut TestAppContext) {
        let mut ui = open(cx, three_proxy_messages());
        ui.keys("cmd-4");
        ui.drain();
        ui.snap(|s| {
            s.messages.remove(&Topic::ProxyPort);
        });
        // Another topic in the section is no reason to send anything.
        ui.click("proxy-port-input");
        ui.keys("enter");
        assert!(ui.sent().is_empty());
        ui.snap(|s| {
            s.messages.insert(
                Topic::ProxyPort,
                Message {
                    text: "Port 7890 is in use".into(),
                    error: true,
                    id: 9,
                },
            );
        });
        ui.keys("enter");
        assert!(matches!(ui.sent()[..], [Cmd::SetProxyPort(_)]));
    }

    #[gpui::test]
    fn a_bad_value_is_fixed_by_choosing_the_shown_option_again(cx: &mut TestAppContext) {
        let mut ui = settings_tab(cx);
        ui.snap(|s| s.keep_dock_icon = true);
        for id in ["dock-always", "log-level-info", "lan-off"] {
            ui.click(id);
        }
        assert!(ui.sent().is_empty(), "already chosen");
        ui.snap(|s| {
            for topic in [Topic::DockIcon, Topic::LogLevel, Topic::Lan] {
                s.messages.insert(
                    topic,
                    Message {
                        text: "settings.yaml: invalid theme. Using the default".into(),
                        error: true,
                        id: topic as u64 + 1,
                    },
                );
            }
        });
        for id in ["dock-always", "log-level-info", "lan-off"] {
            ui.click(id);
        }
        assert!(matches!(
            ui.sent()[..],
            [
                Cmd::SetKeepDockIcon(true),
                Cmd::SetLogLevel(settings::LogLevel::Info),
                Cmd::SetAllowLan(false)
            ]
        ));
    }

    #[gpui::test]
    fn cmd_w_closes_the_window(cx: &mut TestAppContext) {
        let ui = open(cx, running());
        ui.cx.simulate_keystrokes("cmd-w");
        ui.cx.run_until_parked();
        assert!(ui.cx.cx.update(|cx| cx.windows().is_empty()));
    }
}
