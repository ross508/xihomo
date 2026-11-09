use crate::backend::{Cmd, Phase, Snapshot};
use crate::icons::{self, State};
use crate::ui::Tab;
use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{TrayIcon, TrayIconBuilder};

pub const CORE: &str = "core";
pub const PROXY: &str = "proxy";
pub const RELOAD: &str = "reload";
pub const OPEN: &str = "open";
pub const SETTINGS: &str = "settings";
pub const QUIT: &str = "quit";
pub const PROFILE_PREFIX: &str = "profile:";

/// Owns the status item; dropping it would remove the icon, so it lives as long as the app.
pub struct Tray {
    icon: TrayIcon,
    shown_icon: Option<(String, State)>,
    shown_menu: Option<Vec<Entry>>,
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

fn status_line(s: &Snapshot) -> String {
    let proxy = if s.proxy_on { "proxy on" } else { "proxy off" };
    let profile = s.active.as_deref().unwrap_or("no profile");
    format!("{} · {proxy} · {profile}", phase_label(&s.phase))
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
        Entry::Item {
            id: None,
            label: status_line(s),
            enabled: false,
            checked: None,
        },
        Entry::Separator,
        item(CORE, label, enabled),
        Entry::Item {
            id: Some(PROXY.to_owned()),
            label: "System Proxy".to_owned(),
            // Turning it off stays possible whatever the core is doing.
            enabled: s.phase == Phase::Running || s.proxy_on,
            checked: Some(s.proxy_on),
        },
        Entry::Submenu {
            label: "Profile".to_owned(),
            enabled: !profiles.is_empty(),
            items: profiles,
        },
    ];
    if s.edited {
        menu.push(item(RELOAD, "Reload Edited Profile", true));
    }
    menu.extend([
        Entry::Separator,
        item(OPEN, "Open xihomo", true),
        item(SETTINGS, "Settings…", true),
        Entry::Separator,
        item(QUIT, "Quit xihomo", true),
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
        OPEN => MenuAction::Open(None),
        SETTINGS => MenuAction::Open(Some(Tab::Settings)),
        QUIT => MenuAction::Quit,
        other => MenuAction::Send(Cmd::Activate(
            other.strip_prefix(PROFILE_PREFIX)?.to_owned(),
        )),
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

impl Tray {
    /// Must run on the main thread after the application has finished launching.
    pub fn new(snapshot: &Snapshot, events: UnboundedSender<String>) -> Result<Self> {
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let _ = events.send(event.id.0);
        }));
        let icon = TrayIconBuilder::new()
            .with_id("xihomo")
            .with_tooltip("xihomo")
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
                self.shown_icon = Some(wanted);
            }
        }
        let signature = menu_model(s);
        if self.shown_menu.as_ref() != Some(&signature)
            && let Ok(menu) = build_menu(&signature)
        {
            self.icon.set_menu(Some(Box::new(menu)));
            let _ = self
                .icon
                .set_tooltip(Some(format!("xihomo: {}", status_line(s))));
            self.shown_menu = Some(signature);
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
    }
}
