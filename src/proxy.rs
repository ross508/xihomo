//! System proxy. On writes xihomo's HTTP, HTTPS and SOCKS proxy (Mihomo's mixed port serves all
//! three) into every enabled network service, off turns all three off. Both are idempotent. The
//! effective state is read back for display and safety; earlier values are never restored.
//!
//! Writes go through Apple's `/usr/sbin/networksetup`. Tested on the target Mac: authorized
//! SCPreferences from this unentitled app prompts for an administrator password again once the 30s
//! `authenticate-admin-nonshared` right lapses, so every toggle would block on a dialog.
//! networksetup carries Apple's private entitlement for the same right and needs neither a prompt
//! nor any privileged code of ours. configd serializes the writes (about 0.12s each), so their
//! number is what costs time: three per service in each direction.
use anyhow::{Context, Result, bail};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, CFTypeRef, TCFType, kCFAllocatorDefault};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use std::time::Duration;
use tokio::process::Command;
use tokio::task::JoinSet;

const TOOL: &str = "/usr/sbin/networksetup";
const HOST: &str = "127.0.0.1";

async fn run<const N: usize>(args: [&str; N]) -> Result<()> {
    // Layer 1 tests must never change this Mac's network settings.
    if cfg!(test) {
        panic!("test tried to run networksetup {args:?}");
    }
    let output = Command::new(TOOL).args(args).kill_on_drop(true).output();
    let output = tokio::time::timeout(Duration::from_secs(15), output)
        .await
        .context("networksetup timed out")?
        .context("cannot run networksetup")?;
    if !output.status.success() {
        bail!(
            "networksetup {} {}: {}",
            args[0],
            args.get(1).unwrap_or(&""),
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    Ok(())
}

mod sc {
    use core_foundation::array::CFArrayRef;
    use core_foundation::base::{CFAllocatorRef, CFTypeRef};
    use core_foundation::dictionary::CFDictionaryRef;
    use core_foundation::string::CFStringRef;

    #[link(name = "SystemConfiguration", kind = "framework")]
    unsafe extern "C" {
        pub fn SCPreferencesCreate(
            allocator: CFAllocatorRef,
            name: CFStringRef,
            prefs_id: CFStringRef,
        ) -> CFTypeRef;
        pub fn SCPreferencesPathGetValue(prefs: CFTypeRef, path: CFStringRef) -> CFDictionaryRef;
        pub fn SCNetworkSetCopyCurrent(prefs: CFTypeRef) -> CFTypeRef;
        pub fn SCNetworkSetCopyServices(set: CFTypeRef) -> CFArrayRef;
        pub fn SCNetworkServiceGetEnabled(service: CFTypeRef) -> u8;
        pub fn SCNetworkServiceGetName(service: CFTypeRef) -> CFStringRef;
        pub fn SCNetworkServiceGetServiceID(service: CFTypeRef) -> CFStringRef;
        pub fn SCDynamicStoreCopyProxies(store: CFTypeRef) -> CFDictionaryRef;
    }
}

type Dict = CFDictionary<CFString, CFType>;

fn flag(d: &Dict, key: &str) -> bool {
    d.find(CFString::new(key)).is_some_and(|v| {
        v.downcast::<CFNumber>()
            .and_then(|n| n.to_i64())
            .unwrap_or(0)
            != 0
            || v.downcast::<CFBoolean>().is_some_and(bool::from)
    })
}

/// Wraps a +1 reference so it is released; `None` for NULL.
unsafe fn owned(ptr: CFTypeRef) -> Option<CFType> {
    (!ptr.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(ptr) })
}

/// macOS keeps services for some interfaces (`HiddenConfiguration`) out of networksetup and System
/// Settings; they are skipped here too, so the list matches `networksetup -listallnetworkservices`.
unsafe fn hidden(prefs: &CFType, service: CFTypeRef) -> bool {
    let id = unsafe { sc::SCNetworkServiceGetServiceID(service) };
    if id.is_null() {
        return false;
    }
    let id = unsafe { CFString::wrap_under_get_rule(id) };
    let path = CFString::new(&format!("/NetworkServices/{id}/Interface"));
    let interface =
        unsafe { sc::SCPreferencesPathGetValue(prefs.as_CFTypeRef(), path.as_concrete_TypeRef()) };
    !interface.is_null()
        && flag(
            &unsafe { Dict::wrap_under_get_rule(interface) },
            "HiddenConfiguration",
        )
}

/// Names of the enabled services in the current location, listed in-process without authorization
/// instead of spawning networksetup. Only names: no proxy setting is read.
pub fn services() -> Result<Vec<String>> {
    let name = CFString::new("xihomo");
    let mut out = Vec::new();
    unsafe {
        let prefs = owned(sc::SCPreferencesCreate(
            kCFAllocatorDefault,
            name.as_concrete_TypeRef(),
            std::ptr::null(),
        ))
        .context("cannot open network preferences")?;
        let set = owned(sc::SCNetworkSetCopyCurrent(prefs.as_CFTypeRef()))
            .context("no current network location")?;
        let services = sc::SCNetworkSetCopyServices(set.as_CFTypeRef());
        if services.is_null() {
            bail!("cannot list network services");
        }
        let services = CFArray::<CFType>::wrap_under_create_rule(services);
        for service in services.iter() {
            let service = service.as_CFTypeRef();
            if sc::SCNetworkServiceGetEnabled(service) == 0 || hidden(&prefs, service) {
                continue;
            }
            let name = sc::SCNetworkServiceGetName(service);
            if !name.is_null() {
                out.push(CFString::wrap_under_get_rule(name).to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The system's effective proxy, as `scutil --proxy` shows it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct State {
    /// HTTP, HTTPS or SOCKS is enabled, whoever set it.
    pub on: bool,
    /// One of them points at xihomo's mixed port.
    pub ours: bool,
}

fn parse(d: &Dict, port: u16) -> State {
    let mut state = State::default();
    for (enable, host, port_key) in [
        ("HTTPEnable", "HTTPProxy", "HTTPPort"),
        ("HTTPSEnable", "HTTPSProxy", "HTTPSPort"),
        ("SOCKSEnable", "SOCKSProxy", "SOCKSPort"),
    ] {
        if !flag(d, enable) {
            continue;
        }
        state.on = true;
        let host = d
            .find(CFString::new(host))
            .and_then(|v| v.downcast::<CFString>())
            .is_some_and(|h| h == HOST);
        let same_port = d
            .find(CFString::new(port_key))
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
            == Some(port.into());
        state.ours |= host && same_port;
    }
    state
}

/// Read-only and in-process; an unreadable store counts as no proxy.
pub fn state(port: u16) -> State {
    unsafe {
        let dict = sc::SCDynamicStoreCopyProxies(std::ptr::null());
        if dict.is_null() {
            return State::default();
        }
        parse(&Dict::wrap_under_create_rule(dict), port)
    }
}

/// Each set command also turns its proxy on, so no separate state write.
fn on_args(service: &str, port: u16) -> [[String; 4]; 3] {
    let set = |cmd: &str| [cmd, service, HOST, &port.to_string()].map(str::to_owned);
    [
        set("-setwebproxy"),
        set("-setsecurewebproxy"),
        set("-setsocksfirewallproxy"),
    ]
}

fn off_args(service: &str) -> [[String; 3]; 3] {
    let set = |cmd: &str| [cmd, service, "off"].map(str::to_owned);
    [
        set("-setwebproxystate"),
        set("-setsecurewebproxystate"),
        set("-setsocksfirewallproxystate"),
    ]
}

/// Runs every command, concurrently, and keeps the first error.
async fn run_all<const N: usize>(commands: Vec<[String; N]>) -> Result<()> {
    let mut set = JoinSet::new();
    for args in commands {
        set.spawn_local(async move { run(args.each_ref().map(String::as_str)).await });
    }
    let mut first_error = None;
    while let Some(result) = set.join_next().await {
        if let Err(e) = result.map_err(anyhow::Error::from).and_then(|r| r) {
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

pub async fn enable(services: &[String], port: u16) -> Result<()> {
    run_all(services.iter().flat_map(|s| on_args(s, port)).collect()).await
}

/// Turns HTTP, HTTPS and SOCKS proxy off on every enabled service.
pub async fn disable() -> Result<()> {
    run_all(services()?.iter().flat_map(|s| off_args(s)).collect()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_xihomos_http_https_and_socks_proxy() {
        assert_eq!(
            on_args("Wi-Fi", 10999),
            [
                ["-setwebproxy", "Wi-Fi", "127.0.0.1", "10999"],
                ["-setsecurewebproxy", "Wi-Fi", "127.0.0.1", "10999"],
                ["-setsocksfirewallproxy", "Wi-Fi", "127.0.0.1", "10999"],
            ]
        );
        assert_eq!(
            off_args("USB 10/100/1000 LAN"),
            [
                ["-setwebproxystate", "USB 10/100/1000 LAN", "off"],
                ["-setsecurewebproxystate", "USB 10/100/1000 LAN", "off"],
                ["-setsocksfirewallproxystate", "USB 10/100/1000 LAN", "off"],
            ]
        );
    }

    #[test]
    fn state_is_on_for_any_proxy_and_ours_only_for_our_port() {
        let dict = |pairs: &[(&str, CFType)]| {
            let pairs: Vec<_> = pairs
                .iter()
                .map(|(k, v)| (CFString::new(k), v.clone()))
                .collect();
            CFDictionary::from_CFType_pairs(&pairs)
        };
        let num = |n: i64| CFNumber::from(n).as_CFType();
        let text = |s: &str| CFString::new(s).as_CFType();
        assert_eq!(parse(&dict(&[]), 10999), State::default());
        let other = dict(&[
            ("HTTPEnable", num(1)),
            ("HTTPProxy", text("127.0.0.1")),
            ("HTTPPort", num(7890)),
        ]);
        assert_eq!(
            parse(&other, 10999),
            State {
                on: true,
                ours: false
            }
        );
        let ours = dict(&[
            ("HTTPEnable", num(0)),
            ("HTTPProxy", text("127.0.0.1")),
            ("HTTPPort", num(10999)),
            ("SOCKSEnable", num(1)),
            ("SOCKSProxy", text("127.0.0.1")),
            ("SOCKSPort", num(10999)),
        ]);
        assert_eq!(
            parse(&ours, 10999),
            State {
                on: true,
                ours: true
            }
        );
        let disabled_ours = dict(&[
            ("HTTPEnable", num(0)),
            ("HTTPProxy", text("127.0.0.1")),
            ("HTTPPort", num(10999)),
        ]);
        assert_eq!(parse(&disabled_ours, 10999), State::default());
    }

    /// Read-only cross-check on this Mac: the in-process list matches networksetup's.
    #[test]
    fn service_list_matches_networksetup() {
        let output = std::process::Command::new(TOOL)
            .arg("-listallnetworkservices")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        let mut names: Vec<&str> = text
            .lines()
            .skip(1)
            .filter(|l| !l.is_empty() && !l.starts_with('*'))
            .collect();
        names.sort();
        assert_eq!(services().unwrap(), names);
    }
}
