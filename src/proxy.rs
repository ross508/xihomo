//! System proxy, write-only like Bettbox: on writes xihomo's HTTP, HTTPS and SOCKS proxy (Mihomo's
//! mixed port serves all three) into every enabled network service, off turns all three off. Prior
//! values are neither read, checked nor restored.
//!
//! Writes go through Apple's `/usr/sbin/networksetup`. Tested on the target Mac: authorized
//! SCPreferences from this unentitled app prompts for an administrator password again once the 30s
//! `authenticate-admin-nonshared` right lapses, so every toggle would block on a dialog.
//! networksetup carries Apple's private entitlement for the same right and needs neither a prompt
//! nor any privileged code of ours. configd serializes the writes (about 0.12s each), so their
//! number is what costs time: three per service in each direction.
use crate::settings::{Paths, write_private};
use anyhow::{Context, Result, bail};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, CFTypeRef, TCFType, kCFAllocatorDefault};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::task::JoinSet;

const TOOL: &str = "/usr/sbin/networksetup";
const HOST: &str = "127.0.0.1";

/// On disk while the proxy is on, so a crashed or killed xihomo turns it off on the next launch.
#[derive(Debug, Serialize, Deserialize)]
pub struct Marker {
    pub port: u16,
    pub services: Vec<String>,
}

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

fn marker_path(paths: &Paths) -> PathBuf {
    paths.state().join("proxy-on.json")
}

pub fn read_marker(paths: &Paths) -> Result<Option<Marker>> {
    match fs::read(marker_path(paths)) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("parse proxy marker")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_marker(paths: &Paths, marker: &Marker) -> Result<()> {
    write_private(&marker_path(paths), &serde_json::to_vec_pretty(marker)?)
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

async fn set_on(services: &[String], port: u16) -> Result<()> {
    run_all(services.iter().flat_map(|s| on_args(s, port)).collect()).await
}

/// Marker first, so a crash between writes is still turned off on the next launch.
pub async fn enable(paths: &Paths, port: u16) -> Result<()> {
    let services = services()?;
    write_marker(
        paths,
        &Marker {
            port,
            services: services.clone(),
        },
    )?;
    if let Err(e) = set_on(&services, port).await {
        let _ = disable(paths).await;
        return Err(e);
    }
    Ok(())
}

/// A network service that appeared while the proxy is on gets it too.
pub async fn sync_new_services(paths: &Paths) -> Result<()> {
    let Some(mut marker) = read_marker(paths)? else {
        return Ok(());
    };
    let new: Vec<String> = services()?
        .into_iter()
        .filter(|s| !marker.services.contains(s))
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    marker.services.extend(new.iter().cloned());
    write_marker(paths, &marker)?;
    set_on(&new, marker.port).await
}

/// Turns HTTP, HTTPS and SOCKS proxy off on every enabled service, but only if xihomo turned it on:
/// stopping the core must not clear another tool's proxy. The marker stays if a write fails.
pub async fn disable(paths: &Paths) -> Result<()> {
    if read_marker(paths)?.is_none() {
        return Ok(());
    }
    run_all(services()?.iter().flat_map(|s| off_args(s)).collect()).await?;
    match fs::remove_file(marker_path(paths)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::temp_paths;

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
    fn missing_marker_means_proxy_off() {
        let paths = temp_paths("proxy");
        assert!(read_marker(&paths).unwrap().is_none());
        fs::write(marker_path(&paths), "{bad").unwrap();
        assert!(
            read_marker(&paths).is_err(),
            "corrupt marker is reported, not ignored"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn disable_without_marker_writes_nothing() {
        // `run` panics under test, so any networksetup write would fail this test.
        disable(&temp_paths("proxy-off")).await.unwrap();
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
