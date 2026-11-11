use crate::backend::{Cmd, Phase, Snapshot};
use crate::icons::{self, State};
use crate::settings::Theme;
use crate::ui::Tab;
use anyhow::Result;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSAppearance, NSAppearanceCustomization, NSApplication, NSImage, NSMenu};
use tokio::sync::mpsc::UnboundedSender;
use tray_icon::menu::{
    CheckMenuItem, ContextMenu, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{TrayIcon, TrayIconBuilder};

pub const CORE: &str = "core";
pub const PROXY: &str = "proxy";
pub const RELOAD: &str = "reload";
pub const CHECK_NOW: &str = "check-now";
pub const OPEN: &str = "open";
pub const SETTINGS: &str = "settings";
pub const QUIT: &str = "quit";
pub const PROFILE_PREFIX: &str = "profile:";
pub const MODE_PREFIX: &str = "mode:";
const MODES: [(&str, &str); 3] = [("rule", "Rule"), ("global", "Global"), ("direct", "Direct")];

/// Owns the status item; dropping it would remove the icon, so it lives as long as the app.
pub struct Tray {
    icon: TrayIcon,
    shown_icon: Option<(String, State)>,
    shown_menu: Option<(Vec<Entry>, Theme)>,
}

pub fn phase_label(phase: &Phase) -> &'static str {
    match phase {
        Phase::Stopped => "Stopped",
        Phase::Starting => "Starting…",
        Phase::Running => "Running",
        Phase::Stopping => "Stopping…",
        Phase::Failed(_) => "Failed",
    }
}

/// The menu's own System Proxy item shows the proxy state, so this line leaves it out.
fn status_line(s: &Snapshot) -> String {
    let profile = s.active.as_deref().unwrap_or("no profile");
    format!("{} · {profile}", phase_label(&s.phase))
}

/// What the menu shows, as plain data: testable without AppKit, and compared to skip rebuilds.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Separator,
    Item {
        id: Option<String>,
        label: String,
        enabled: bool,
        checked: Option<bool>,
    },
    Submenu {
        label: String,
        enabled: bool,
        items: Vec<Entry>,
    },
}

fn item(id: &str, label: &str, enabled: bool) -> Entry {
    Entry::Item {
        id: Some(id.to_owned()),
        label: label.to_owned(),
        enabled,
        checked: None,
    }
}

pub fn menu_model(s: &Snapshot) -> Vec<Entry> {
    let (label, enabled) = match s.phase {
        Phase::Stopped => ("Start Core", true),
        Phase::Failed(_) => ("Retry Start", true),
        Phase::Running | Phase::Starting => ("Stop Core", true),
        Phase::Stopping => ("Stopping…", false),
    };
    let profiles = s
        .profiles
        .iter()
        .map(|p| Entry::Item {
            id: Some(format!("{PROFILE_PREFIX}{}", p.name)),
            label: p.name.clone(),
            enabled: p.problem.is_none(),
            checked: Some(s.active.as_deref() == Some(&p.name)),
        })
        .collect::<Vec<_>>();
    let mut menu = vec![
        item(OPEN, "Open Window", true),
        item(SETTINGS, "Settings…", true),
        Entry::Separator,
        Entry::Item {
            id: None,
            label: status_line(s),
            enabled: false,
            checked: None,
        },
        item(CORE, label, enabled),
        Entry::Separator,
    ];
    // Enabled while stopped: the choice is remembered for the next start.
    menu.extend(MODES.map(|(mode, label)| Entry::Item {
        id: Some(format!("{MODE_PREFIX}{mode}")),
        label: label.to_owned(),
        enabled: s.phase != Phase::Stopping,
        checked: Some(s.mode == mode),
    }));
    menu.extend([
        Entry::Separator,
        Entry::Item {
            id: Some(PROXY.to_owned()),
            label: match s.proxy_busy {
                Some(true) => "System Proxy: Turning On…",
                Some(false) => "System Proxy: Turning Off…",
                None => "System Proxy",
            }
            .to_owned(),
            // Turning it off stays possible whatever the core is doing.
            enabled: s.proxy_busy.is_none() && (s.phase == Phase::Running || s.proxy_on),
            checked: Some(s.proxy_on),
        },
        Entry::Separator,
        Entry::Submenu {
            label: "Profile".to_owned(),
            enabled: !profiles.is_empty(),
            items: profiles,
        },
    ]);
    if s.edited {
        menu.push(item(RELOAD, "Reload Edited Profile", true));
    }
    menu.extend([
        item(CHECK_NOW, "Check Now", true),
        Entry::Separator,
        item(QUIT, "Quit", true),
    ]);
    menu
}

pub enum MenuAction {
    Send(Cmd),
    Open(Option<Tab>),
    Quit,
}

/// Decided from the snapshot the user saw when clicking, not from a later one.
pub fn menu_action(id: &str, s: &Snapshot) -> Option<MenuAction> {
    Some(match id {
        CORE => MenuAction::Send(match s.phase {
            Phase::Running | Phase::Starting => Cmd::Stop,
            Phase::Stopping => return None,
            _ => Cmd::Start,
        }),
        PROXY => MenuAction::Send(Cmd::SetProxy(!s.proxy_on)),
        RELOAD => MenuAction::Send(Cmd::Reload),
        CHECK_NOW => MenuAction::Send(Cmd::CheckNow),
        OPEN => MenuAction::Open(None),
        SETTINGS => MenuAction::Open(Some(Tab::Settings)),
        QUIT => MenuAction::Quit,
        other => {
            if let Some(mode) = other.strip_prefix(MODE_PREFIX) {
                if s.phase == Phase::Stopping {
                    return None;
                }
                MenuAction::Send(Cmd::SetMode(mode.to_owned()))
            } else {
                MenuAction::Send(Cmd::Activate(
                    other.strip_prefix(PROFILE_PREFIX)?.to_owned(),
                ))
            }
        }
    })
}

fn append(
    menu: &dyn Fn(&dyn IsMenuItem) -> tray_icon::menu::Result<()>,
    entry: &Entry,
) -> Result<()> {
    match entry {
        Entry::Separator => menu(&PredefinedMenuItem::separator())?,
        Entry::Item {
            id,
            label,
            enabled,
            checked,
        } => match (id, checked) {
            (Some(id), Some(checked)) => {
                menu(&CheckMenuItem::with_id(id, label, *enabled, *checked, None))?
            }
            (Some(id), None) => menu(&MenuItem::with_id(id, label, *enabled, None))?,
            (None, _) => menu(&MenuItem::new(label, *enabled, None))?,
        },
        Entry::Submenu {
            label,
            enabled,
            items,
        } => {
            let submenu = Submenu::new(label, *enabled);
            for entry in items {
                append(&|item| submenu.append(item), entry)?;
            }
            menu(&submenu)?;
        }
    }
    Ok(())
}

fn build_menu(model: &[Entry]) -> Result<Menu> {
    let menu = Menu::new();
    for entry in model {
        append(&|item| menu.append(item), entry)?;
    }
    Ok(menu)
}

/// macOS drops a menu's check mark column while no item in it is checked, which shifts every
/// label left. A blank off-state image the size of the check mark keeps the column everywhere.
/// A status menu follows the menu bar, not the app, so the app's fixed theme is copied onto it.
fn style_menu(menu: &NSMenu, appearance: Option<&NSAppearance>) {
    menu.setAppearance(appearance);
    for item in menu.itemArray() {
        if let Some(submenu) = item.submenu() {
            style_menu(&submenu, appearance);
        }
        if let Some(check) = item.onStateImage()
            && !item.isSeparatorItem()
        {
            let blank = NSImage::initWithSize(NSImage::alloc(), check.size());
            item.setOffStateImage(Some(&blank));
        }
    }
}

impl Tray {
    /// Must run on the main thread after the application has finished launching.
    pub fn new(snapshot: &Snapshot, events: UnboundedSender<String>) -> Result<Self> {
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let _ = events.send(event.id.0);
        }));
        let icon = TrayIconBuilder::new()
            .with_id("xihomo")
            .with_menu_on_left_click(false)
            .with_icon_templated(icons::tray_icon(
                &snapshot.icon_set,
                State::of(&snapshot.phase),
            ))
            .build()?;
        let mut tray = Self {
            icon,
            shown_icon: None,
            shown_menu: None,
        };
        tray.update(snapshot);
        Ok(tray)
    }

    pub fn update(&mut self, s: &Snapshot) {
        let wanted = (s.icon_set.clone(), State::of(&s.phase));
        if self.shown_icon.as_ref() != Some(&wanted) {
            let icon = icons::tray_icon(&wanted.0, wanted.1);
            if self.icon.set_icon_templated(Some(icon)).is_ok() {
                self.fit_icon();
                self.shown_icon = Some(wanted);
            }
        }
        let signature = (menu_model(s), s.theme);
        if self.shown_menu.as_ref() != Some(&signature)
            && let Ok(menu) = build_menu(&signature.0)
            && let Some(mtm) = MainThreadMarker::new()
        {
            let appearance = NSApplication::sharedApplication(mtm).appearance();
            // Safety: on macOS `ns_menu` is the menu's live NSMenu, retained by `menu`.
            style_menu(
                unsafe { &*menu.ns_menu().cast::<NSMenu>() },
                appearance.as_deref(),
            );
            self.icon.set_menu(Some(Box::new(menu)));
            // No app name: the menu bar shows only the state, not what is running it.
            let _ = self.icon.set_tooltip(Some(format!(
                "{} · system proxy {}",
                status_line(s),
                if s.proxy_on { "on" } else { "off" }
            )));
            self.shown_menu = Some(signature);
        }
    }

    /// tray-icon takes the bitmap's pixels as points, capped at 22pt; show the 2x bitmap at its real size.
    fn fit_icon(&self) {
        let (Some(mtm), Some(item)) = (MainThreadMarker::new(), self.icon.ns_status_item()) else {
            return;
        };
        if let Some(button) = item.button(mtm)
            && let Some(image) = button.image()
        {
            let mut size = image.size();
            size.width = icons::POINTS;
            size.height = icons::POINTS;
            image.setSize(size);
            button.setImage(Some(&image));
        }
    }

    /// AppKit flips a check item when clicked. Rebuilding restores the true state if the action fails.
    pub fn resync(&mut self, s: &Snapshot) {
        self.shown_menu = None;
        self.update(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::Profile;

    fn snapshot(phase: Phase) -> Snapshot {
        let profile = |name: &str, broken: bool| Profile {
            name: name.into(),
            path: format!("/p/{name}.yaml").into(),
            is_link: false,
            problem: broken.then(|| "missing".to_owned()),
        };
        Snapshot {
            phase,
            active: Some("home".into()),
            profiles: vec![profile("home", false), profile("gone", true)],
            ..Snapshot::default()
        }
    }

    fn find<'a>(model: &'a [Entry], id: &str) -> Option<&'a Entry> {
        model.iter().find_map(|e| match e {
            Entry::Item { id: Some(i), .. } if i == id => Some(e),
            Entry::Submenu { items, .. } => find(items, id),
            _ => None,
        })
    }

    fn state(model: &[Entry], id: &str) -> (String, bool, Option<bool>) {
        match find(model, id) {
            Some(Entry::Item {
                label,
                enabled,
                checked,
                ..
            }) => (label.clone(), *enabled, *checked),
            other => panic!("{id}: {other:?}"),
        }
    }

    #[test]
    fn core_item_follows_phase() {
        let label = |phase| state(&menu_model(&snapshot(phase)), CORE);
        assert_eq!(label(Phase::Stopped), ("Start Core".into(), true, None));
        assert_eq!(
            label(Phase::Failed("x".into())),
            ("Retry Start".into(), true, None)
        );
        assert_eq!(label(Phase::Running), ("Stop Core".into(), true, None));
        assert_eq!(label(Phase::Stopping), ("Stopping…".into(), false, None));
    }

    #[test]
    fn proxy_can_be_turned_off_even_when_core_is_not_running() {
        let mut s = snapshot(Phase::Stopped);
        assert_eq!(
            state(&menu_model(&s), PROXY),
            ("System Proxy".into(), false, Some(false))
        );
        s.proxy_on = true;
        assert_eq!(
            state(&menu_model(&s), PROXY),
            ("System Proxy".into(), true, Some(true))
        );
        s.proxy_busy = Some(false);
        assert_eq!(
            state(&menu_model(&s), PROXY),
            ("System Proxy: Turning Off…".into(), false, Some(true))
        );
    }

    #[test]
    fn profiles_show_selection_and_broken_links() {
        let model = menu_model(&snapshot(Phase::Running));
        assert_eq!(
            state(&model, "profile:home"),
            ("home".into(), true, Some(true))
        );
        assert_eq!(
            state(&model, "profile:gone"),
            ("gone".into(), false, Some(false))
        );
        assert!(find(&model, RELOAD).is_none());
        let edited = Snapshot {
            edited: true,
            ..snapshot(Phase::Running)
        };
        assert!(find(&menu_model(&edited), RELOAD).is_some());
        let empty = Snapshot::default();
        assert!(menu_model(&empty).contains(&Entry::Submenu {
            label: "Profile".into(),
            enabled: false,
            items: vec![]
        }));
    }

    fn labels(model: &[Entry]) -> Vec<&str> {
        model
            .iter()
            .map(|e| match e {
                Entry::Separator => "-",
                Entry::Item { label, .. } | Entry::Submenu { label, .. } => label,
            })
            .collect()
    }

    #[test]
    fn menu_order_and_mode_items() {
        let mut s = Snapshot {
            edited: true,
            mode: "global".into(),
            ..snapshot(Phase::Running)
        };
        let model = menu_model(&s);
        assert_eq!(
            labels(&model),
            [
                "Open Window",
                "Settings…",
                "-",
                "Running · home",
                "Stop Core",
                "-",
                "Rule",
                "Global",
                "Direct",
                "-",
                "System Proxy",
                "-",
                "Profile",
                "Reload Edited Profile",
                "Check Now",
                "-",
                "Quit",
            ]
        );
        let modes = |s: &Snapshot| {
            let model = menu_model(s);
            ["mode:rule", "mode:global", "mode:direct"].map(|id| {
                let (_, enabled, checked) = state(&model, id);
                (enabled, checked)
            })
        };
        assert_eq!(
            modes(&s),
            [(true, Some(false)), (true, Some(true)), (true, Some(false))]
        );
        s.phase = Phase::Stopped;
        s.mode = String::new();
        assert_eq!(modes(&s), [(true, Some(false)); 3], "nothing recorded");
        s.phase = Phase::Stopping;
        assert_eq!(modes(&s), [(false, Some(false)); 3]);
    }

    #[test]
    fn menu_ids_map_to_commands() {
        let running = snapshot(Phase::Running);
        let stopped = snapshot(Phase::Stopped);
        assert!(matches!(
            menu_action(CORE, &running),
            Some(MenuAction::Send(Cmd::Stop))
        ));
        assert!(matches!(
            menu_action(CORE, &stopped),
            Some(MenuAction::Send(Cmd::Start))
        ));
        assert!(matches!(
            menu_action(CORE, &snapshot(Phase::Failed("x".into()))),
            Some(MenuAction::Send(Cmd::Start))
        ));
        assert!(menu_action(CORE, &snapshot(Phase::Stopping)).is_none());
        assert!(matches!(
            menu_action(PROXY, &running),
            Some(MenuAction::Send(Cmd::SetProxy(true)))
        ));
        let on = Snapshot {
            proxy_on: true,
            ..running
        };
        assert!(matches!(
            menu_action(PROXY, &on),
            Some(MenuAction::Send(Cmd::SetProxy(false)))
        ));
        assert!(matches!(
            menu_action(RELOAD, &on),
            Some(MenuAction::Send(Cmd::Reload))
        ));
        assert!(
            matches!(menu_action("profile:a b/c", &on), Some(MenuAction::Send(Cmd::Activate(n))) if n == "a b/c")
        );
        assert!(
            menu_action("remove:x", &on).is_none(),
            "removal lives in the window"
        );
        assert!(matches!(
            menu_action(CHECK_NOW, &on),
            Some(MenuAction::Send(Cmd::CheckNow))
        ));
        assert!(matches!(
            menu_action(OPEN, &on),
            Some(MenuAction::Open(None))
        ));
        assert!(matches!(
            menu_action(SETTINGS, &on),
            Some(MenuAction::Open(Some(Tab::Settings)))
        ));
        assert!(matches!(menu_action(QUIT, &on), Some(MenuAction::Quit)));
        assert!(menu_action("unknown", &on).is_none());
        assert!(
            matches!(menu_action("mode:direct", &stopped), Some(MenuAction::Send(Cmd::SetMode(m))) if m == "direct")
        );
        assert!(menu_action("mode:rule", &snapshot(Phase::Stopping)).is_none());
    }
}
