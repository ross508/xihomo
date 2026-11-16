//! The Dock icon, Cmd-Tab entry and app menu: shown while a window is open, or always when the
//! user keeps them.
use crate::{icons, input, ui};
use gpui::{Menu, MenuItem, OsAction, actions};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSAboutPanelOptionApplicationVersion, NSAboutPanelOptionVersion, NSApplication,
    NSApplicationActivationPolicy, NSImage,
};
use objc2_foundation::{NSData, NSDictionary, NSString, ns_string};
use std::cell::RefCell;

actions!(xihomo, [About, Hide, HideOthers, ShowAll]);

thread_local! {
    /// The chosen set's icon: the Dock drops it whenever the app leaves it, so it is set again.
    static ICON: RefCell<Option<Retained<NSImage>>> = const { RefCell::new(None) };
}

pub fn dock_shown(keep: bool, window_open: bool) -> bool {
    keep || window_open
}

/// `Regular` for a Dock icon, Cmd-Tab entry and app menu; `Accessory` for the menu bar only.
pub fn apply(shown: bool) {
    // Tests must not change this Mac's Dock.
    if cfg!(test) {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let policy = if shown {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(policy);
    if shown {
        // SAFETY: a valid image or none, set on the main thread.
        ICON.with_borrow(|icon| unsafe { app.setApplicationIconImage(icon.as_deref()) });
    }
}

/// The Dock and Cmd-Tab icon for `set` while xihomo runs; the bundle's icon returns on quit.
pub fn set_icon(set: &str) {
    if cfg!(test) {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let image = image(set);
    // SAFETY: a valid image, set on the main thread.
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(image.as_deref()) };
    ICON.set(image);
}

/// The chosen set's app icon for AppKit.
fn image(set: &str) -> Option<Retained<NSImage>> {
    const SIZE: u32 = 512;
    let png = icons::png(
        &icons::app_icon(Some(set), cfg!(debug_assertions), SIZE),
        SIZE,
    );
    NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(&png))
}

/// The standard About panel. Its icon and name are the app's own, as the Dock shows them; the rest
/// is passed in, because the bare binary has no Info.plist.
pub fn about() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let version = NSString::from_str(env!("CARGO_PKG_VERSION"));
    // SAFETY: AppKit's option key constants are immutable statics.
    let keys = unsafe {
        [
            NSAboutPanelOptionApplicationVersion,
            // Empty: no build number in parentheses after the version.
            NSAboutPanelOptionVersion,
            ns_string!("Copyright"),
        ]
    };
    let values: [&AnyObject; 3] = [
        &version,
        ns_string!(""),
        ns_string!("Copyright © 2026 ross508"),
    ];
    let options = NSDictionary::from_slices(&keys, &values);
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: every value has the type its key documents.
    unsafe { app.orderFrontStandardAboutPanelWithOptions(&options) };
}

/// The first menu is the application menu; macOS titles it with the process or bundle name.
pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "Xihomo".into(),
            items: vec![
                MenuItem::action("About Xihomo", About),
                MenuItem::separator(),
                MenuItem::action("Hide Xihomo", Hide),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit Xihomo", ui::Quit),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Cut", input::Cut, OsAction::Cut),
                MenuItem::os_action("Copy", input::Copy, OsAction::Copy),
                MenuItem::os_action("Paste", input::Paste, OsAction::Paste),
                MenuItem::os_action("Select All", input::SelectAll, OsAction::SelectAll),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Control", ui::ShowControl),
                MenuItem::action("Connections", ui::ShowConnections),
                MenuItem::action("Logs", ui::ShowLogs),
                MenuItem::action("Settings", ui::ShowSettings),
                MenuItem::separator(),
                MenuItem::action("Close", ui::CloseWindow),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dock_shows_with_a_window_or_when_kept() {
        assert!(!dock_shown(false, false));
        assert!(dock_shown(false, true));
        assert!(dock_shown(true, false));
        assert!(dock_shown(true, true));
    }

    #[test]
    fn app_menus_in_order() {
        let menus: Vec<_> = app_menus()
            .into_iter()
            .map(|menu| {
                let items: Vec<_> = menu
                    .items
                    .iter()
                    .map(|item| match item {
                        MenuItem::Action { name, .. } => name.to_string(),
                        MenuItem::Separator => "-".into(),
                        _ => "?".into(),
                    })
                    .collect();
                (menu.name.to_string(), items.join(", "))
            })
            .collect();
        let expected = [
            (
                "Xihomo",
                "About Xihomo, -, Hide Xihomo, Hide Others, Show All, -, Quit Xihomo",
            ),
            ("Edit", "Cut, Copy, Paste, Select All"),
            ("Window", "Control, Connections, Logs, Settings, -, Close"),
        ];
        assert_eq!(
            menus,
            expected.map(|(a, b)| (a.to_owned(), b.to_owned())).to_vec()
        );
    }
}
