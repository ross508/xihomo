use crate::settings::{LogLevel, Paths, Settings};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use yaml_serde::{Mapping, Value};

const MAX_YAML: u64 = 32 * 1024 * 1024;

/// Keys xihomo writes for every profile.
const WRITTEN: &[&str] = &[
    "mixed-port",
    "allow-lan",
    "skip-auth-prefixes",
    "external-controller",
    "secret",
    "tun",
    "log-level",
];

/// Keys xihomo removes: other listeners, controllers and UIs it does not manage.
const REMOVED: &[&str] = &[
    "port",
    "socks-port",
    "redir-port",
    "tproxy-port",
    "bind-address",
    "external-controller-tls",
    "external-controller-unix",
    "external-controller-pipe",
    "external-controller-cors",
    "external-ui",
    "external-ui-url",
    "external-ui-name",
    "external-doh-server",
    "tuic-server",
    "ss-config",
    "vmess-config",
];

fn owned(key: &Value) -> Option<&str> {
    key.as_str()
        .filter(|k| WRITTEN.contains(k) || REMOVED.contains(k))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub name: String,
    pub path: PathBuf,
    pub is_link: bool,
    /// Why the profile cannot be used right now (typically a broken link).
    pub problem: Option<String>,
}

/// A profile's identifier is its file stem, which also names its home directory, so `.` and `..`
/// (from `..yaml` and `...yaml`) are never profiles.
fn usable_name(stem: &str) -> bool {
    !matches!(stem, "" | "." | "..")
}

pub fn list(paths: &Paths) -> Result<Vec<Profile>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(paths.profiles()).context("read profiles directory")? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        if !usable_name(&name) {
            continue;
        }
        let is_link = fs::symlink_metadata(&path)?.is_symlink();
        let problem = match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => None,
            Ok(_) => Some("not a regular file".to_owned()),
            Err(e) => Some(e.to_string()),
        };
        out.push(Profile {
            name,
            path,
            is_link,
            problem,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Puts `list` in the order profiles were added and returns that order to record. Names in
/// `order` keep their place whatever happens to their files; names not listed are dropped;
/// new ones go last, oldest file or link first, then (as `list` gives them) by name.
pub fn arrange(list: &mut [Profile], order: &[String]) -> Vec<String> {
    list.sort_by_cached_key(|p| match order.iter().position(|n| *n == p.name) {
        Some(i) => (i, None),
        None => (
            order.len(),
            fs::symlink_metadata(&p.path).and_then(|m| m.created()).ok(),
        ),
    });
    list.iter().map(|p| p.name.clone()).collect()
}

pub fn profile_path(paths: &Paths, name: &str) -> Result<PathBuf> {
    list(paths)?
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| p.path)
        .with_context(|| format!("profile {name} not found"))
}

/// The profile a file would become: its stem, and where it goes in `profiles/`.
fn new_profile(paths: &Paths, file: &Path) -> Result<(String, PathBuf)> {
    let name = file
        .file_stem()
        .and_then(|n| n.to_str())
        .filter(|n| usable_name(n))
        .context("file has no usable name")?
        .to_owned();
    let dest = paths.profiles().join(format!("{name}.yaml"));
    if fs::symlink_metadata(&dest).is_ok() {
        bail!("a profile named {name} already exists");
    }
    Ok((name, dest))
}

/// Add an independently maintained YAML file as a profile without importing its contents.
pub fn link(paths: &Paths, target: &Path) -> Result<String> {
    let (name, dest) = new_profile(paths, target)?;
    let target = std::path::absolute(target).context("resolve the linked file")?;
    std::os::unix::fs::symlink(target, &dest).context("create profile link")?;
    Ok(name)
}

/// Copies a YAML mapping into `profiles/` as a new regular-file profile. The copy is checked
/// like any profile when it is selected.
pub fn import(paths: &Paths, source: &Path) -> Result<String> {
    let (name, dest) = new_profile(paths, source)?;
    let bytes = read_bounded(source)?;
    mapping(&bytes, &format!("{name}.yaml"))?;
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dest)
    {
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            bail!("a profile named {name} already exists")
        }
        other => other.with_context(|| format!("create {}", dest.display()))?,
    };
    if let Err(e) = std::io::Write::write_all(&mut file, &bytes) {
        let _ = fs::remove_file(&dest);
        return Err(e).with_context(|| format!("write {}", dest.display()));
    }
    Ok(name)
}

/// The smallest profile both `compose` and `mihomo -t` accept; an empty file passes neither.
const DEFAULT_PROFILE: &str = "# Created by xihomo because profiles/ had no profile.\n# Replace it, or link another YAML file from Settings.\nmode: direct\n";

/// Gives a first run something to select. Never overwrites: an existing `default.yaml`, valid or
/// not, already counts as a profile.
pub fn ensure_default(paths: &Paths) -> Result<()> {
    if !list(paths)?.is_empty() {
        return Ok(());
    }
    let path = paths.profiles().join("default.yaml");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => std::io::Write::write_all(&mut file, DEFAULT_PROFILE.as_bytes())
            .with_context(|| format!("write {}", path.display())),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
    }
}

/// Deletes a regular file permanently, or only the link for a symlink, then the profile's home
/// unless it is `protected`. The caller refuses the active profile.
pub fn remove(paths: &Paths, name: &str, protected: &[PathBuf]) -> Result<()> {
    let path = profile_path(paths, name)?;
    // On a symlink this unlinks it; the target is never opened.
    fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    let home = paths.home(name);
    if !protected.contains(&home) {
        remove_any(&home).with_context(|| format!("remove {}", home.display()))?;
    }
    Ok(())
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Deletes what nothing refers to: homes without a profile (except `protected`, compared by path),
/// core versions not in `versions`, and atomic-write leftovers of other processes.
/// Removal is best effort; anything left is retried on the next run.
pub fn reconcile(paths: &Paths, profiles: &[Profile], protected: &[PathBuf], versions: &[&str]) {
    for entry in fs::read_dir(paths.homes()).into_iter().flatten().flatten() {
        let path = entry.path();
        let named = entry
            .file_name()
            .to_str()
            .is_some_and(|n| profiles.iter().any(|p| p.name == n));
        if !named && !protected.contains(&path) {
            let _ = remove_any(&path);
        }
    }
    crate::update::prune(paths, versions);
    let own = format!("tmp{}", std::process::id());
    for dir in [paths.runtime(), paths.state()] {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let stale = path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(|e| e.strip_prefix("tmp").map(|pid| (e, pid)))
                .is_some_and(|(ext, pid)| {
                    ext != own && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
                });
            if stale {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .take(MAX_YAML + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_YAML {
        bail!("{} is larger than {MAX_YAML} bytes", path.display());
    }
    Ok(bytes)
}

fn read_overrides(paths: &Paths) -> Result<Vec<u8>> {
    match read_bounded(&paths.overrides()) {
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == ErrorKind::NotFound) =>
        {
            Ok(Vec::new())
        }
        other => other,
    }
}

fn mapping(bytes: &[u8], what: &str) -> Result<Mapping> {
    match yaml_serde::from_slice::<Value>(bytes)
        .with_context(|| format!("{what} is not valid YAML"))?
    {
        Value::Mapping(map) => Ok(map),
        Value::Null if what == "overrides.yaml" => Ok(Mapping::new()),
        _ => bail!("{what} must be a YAML mapping"),
    }
}

pub struct Runtime<'a> {
    pub mixed_port: u16,
    pub controller_port: u16,
    pub allow_lan: bool,
    pub secret: &'a str,
    /// The user's remembered mode, which beats the profile's and the overrides'.
    pub mode: Option<&'a str>,
    pub log_level: LogLevel,
}

impl<'a> Runtime<'a> {
    pub fn new(settings: &Settings, secret: &'a str, mode: Option<&'a str>) -> Self {
        Self {
            mixed_port: settings.mixed_port,
            controller_port: settings.controller_port,
            allow_lan: settings.allow_lan,
            secret,
            mode,
            log_level: settings.log_level,
        }
    }
}

/// `host:port` with the host replaced by 127.0.0.1; text without a port is left alone.
fn loopback(addr: &str) -> String {
    addr.rsplit_once(':')
        .map_or_else(|| addr.to_owned(), |(_, port)| format!("127.0.0.1:{port}"))
}

fn loopback_value(addr: &mut Value) {
    if let Some(text) = addr.as_str() {
        *addr = loopback(text).into();
    }
}

/// Writes xihomo's fields over `map` and returns the owned keys it already had. Also used on
/// `last-good.yaml`, so a recovery launch uses the current ports and secret.
pub fn own(map: &mut Mapping, rt: &Runtime) -> Vec<String> {
    let overridden = map.keys().filter_map(owned).map(str::to_owned).collect();
    for key in WRITTEN.iter().chain(REMOVED) {
        map.remove(*key);
    }
    if let Some(Value::Sequence(listeners)) = map.get_mut("listeners") {
        listeners.retain(|l| l.get("type").and_then(Value::as_str) != Some("tun"));
        // Mihomo binds these to 0.0.0.0 by default, whatever allow-lan says.
        if !rt.allow_lan {
            for listener in listeners.iter_mut().filter_map(Value::as_mapping_mut) {
                listener.insert("listen".into(), "127.0.0.1".into());
            }
        }
    }
    if !rt.allow_lan {
        if let Some(Value::Sequence(tunnels)) = map.get_mut("tunnels") {
            for tunnel in tunnels {
                match tunnel {
                    // network,address,target[,proxy]
                    Value::String(text) => {
                        let mut parts: Vec<String> =
                            text.split(',').map(|p| p.trim().to_owned()).collect();
                        if let Some(address) = parts.get_mut(1) {
                            *address = loopback(address);
                        }
                        *text = parts.join(",");
                    }
                    Value::Mapping(tunnel) => {
                        if let Some(address) = tunnel.get_mut("address") {
                            loopback_value(address);
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(listen) = map.get_mut("dns").and_then(|d| d.get_mut("listen")) {
            loopback_value(listen);
        }
    }
    map.insert("mixed-port".into(), Value::from(rt.mixed_port));
    map.insert("allow-lan".into(), rt.allow_lan.into());
    // Apps on this Mac reach the proxy through the system proxy, which cannot authenticate.
    map.insert(
        "skip-auth-prefixes".into(),
        Value::Sequence(vec!["127.0.0.1/8".into(), "::1/128".into()]),
    );
    map.insert(
        "external-controller".into(),
        format!("127.0.0.1:{}", rt.controller_port).into(),
    );
    map.insert("secret".into(), rt.secret.into());
    let mut tun = Mapping::new();
    tun.insert("enable".into(), false.into());
    map.insert("tun".into(), tun.into());
    map.insert("log-level".into(), rt.log_level.as_str().into());
    if let Some(mode) = rt.mode {
        map.insert("mode".into(), mode.into());
    }
    overridden
}

pub struct Composed {
    pub yaml: String,
    /// Shown at Settings → PROFILES.
    pub notices: Vec<String>,
    /// Owned keys the profile set, which xihomo replaced; written to Logs.
    pub ignored: Vec<String>,
}

/// source → overrides.yaml → xihomo-owned fields. Each step replaces whole top-level values:
/// mappings are never merged and lists never concatenated. An owned key in overrides.yaml is
/// ignored with a notice.
pub fn compose(source: &[u8], overrides: &[u8], rt: &Runtime) -> Result<Composed> {
    let mut map = mapping(source, "profile")?;
    let overrides = mapping(overrides, "overrides.yaml")?;
    let mut refused = Vec::new();
    for (key, value) in overrides {
        match owned(&key) {
            Some(name) => refused.push(name.to_owned()),
            None => {
                map.insert(key, value);
            }
        }
    }
    let mut notices = Vec::new();
    if !refused.is_empty() {
        notices.push(format!(
            "overrides.yaml: {} ignored (set by xihomo)",
            refused.join(", ")
        ));
    }
    // A default, not an owned key: latency tests time one round trip on an open connection,
    // as other Mihomo GUIs report it, unless the profile or overrides.yaml says otherwise.
    map.entry("unified-delay".into())
        .or_insert(Value::Bool(true));
    let had_tun = matches!(map.get("listeners"), Some(Value::Sequence(l))
        if l.iter().any(|l| l.get("type").and_then(Value::as_str) == Some("tun")));
    let ignored = own(&mut map, rt);
    if had_tun {
        notices.push("TUN listeners were removed: xihomo V1 uses the system proxy".to_owned());
    }
    Ok(Composed {
        yaml: yaml_serde::to_string(&map)?,
        notices,
        ignored,
    })
}

/// Writes the owned fields into `last-good.yaml` again, with the current settings.
pub fn reown(path: &Path, rt: &Runtime) -> Result<()> {
    let mut map = mapping(&read_bounded(path)?, "last-good.yaml")?;
    own(&mut map, rt);
    crate::settings::write_private(path, yaml_serde::to_string(&map)?.as_bytes())
}

pub struct Candidate {
    pub home: PathBuf,
    pub fingerprint: String,
    pub notices: Vec<String>,
    pub ignored: Vec<String>,
}

/// Resolve the profile now, then write a private effective snapshot to `candidate.yaml`, which
/// only validation and reloads read. Links are only ever read.
pub fn write_candidate(paths: &Paths, name: &str, rt: &Runtime) -> Result<Candidate> {
    let path = profile_path(paths, name)?;
    let target =
        fs::canonicalize(&path).with_context(|| format!("profile {name} cannot be resolved"))?;
    let source = read_bounded(&target)?;
    let overrides = read_overrides(paths)?;
    let composed = compose(&source, &overrides, rt).with_context(|| format!("profile {name}"))?;
    crate::settings::write_private(&paths.candidate(), composed.yaml.as_bytes())?;
    // Created on first activation, so a never-used profile has no home.
    let home = paths.home(name);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&home)
        .with_context(|| format!("create Mihomo home {}", home.display()))?;
    Ok(Candidate {
        home,
        fingerprint: fingerprint(&source, &overrides),
        notices: composed.notices,
        ignored: composed.ignored,
    })
}

fn fingerprint(source: &[u8], overrides: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update((source.len() as u64).to_be_bytes());
    hash.update(source);
    hash.update(overrides);
    format!("{:x}", hash.finalize())
}

/// Detect external edits by content, so atomic saves and retargeted links both register.
pub fn current_fingerprint(paths: &Paths, name: &str) -> Result<String> {
    let path = profile_path(paths, name)?;
    Ok(fingerprint(&read_bounded(&path)?, &read_overrides(paths)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::temp_paths;

    const RT: Runtime = Runtime {
        mixed_port: 10999,
        controller_port: 11000,
        allow_lan: false,
        secret: "s3",
        mode: None,
        log_level: LogLevel::Info,
    };

    fn rt_default() -> Runtime<'static> {
        Runtime {
            mixed_port: Settings::default().mixed_port,
            ..RT
        }
    }

    fn parsed(yaml: &str) -> Value {
        yaml_serde::from_str(yaml).unwrap()
    }

    #[test]
    fn runtime_fields_beat_source_and_overrides_replace_whole_values() {
        let source = b"mixed-port: 7890\nexternal-controller: 0.0.0.0:9090\nsecret: old\nlog-level: debug\ndns: {enable: true, ipv6: true}\nrules: [\"MATCH,DIRECT\"]\n";
        let debug = Runtime {
            log_level: LogLevel::Debug,
            ..RT
        };
        let c = compose(source, b"dns: {enable: false}\nlog-level: error\n", &debug).unwrap();
        let v = parsed(&c.yaml);
        assert_eq!(v["mixed-port"], Value::from(10999));
        assert_eq!(v["external-controller"].as_str(), Some("127.0.0.1:11000"));
        assert_eq!(v["secret"].as_str(), Some("s3"));
        assert_eq!(
            v["log-level"].as_str(),
            Some("debug"),
            "xihomo's, not the profile's"
        );
        assert!(
            v["dns"].get("ipv6").is_none(),
            "mappings replace, never merge"
        );
        assert_eq!(v["rules"][0].as_str(), Some("MATCH,DIRECT"));
        assert_eq!(
            c.notices,
            ["overrides.yaml: log-level ignored (set by xihomo)"]
        );
        assert_eq!(
            c.ignored,
            ["mixed-port", "external-controller", "secret", "log-level"]
        );
    }

    #[test]
    fn remembered_mode_beats_profile_and_overrides() {
        let mode = |source: &str, overrides: &str, rt: &Runtime| {
            let c = compose(source.as_bytes(), overrides.as_bytes(), rt).unwrap();
            parsed(&c.yaml)["mode"].as_str().map(str::to_owned)
        };
        let global = Runtime {
            mode: Some("global"),
            ..RT
        };
        assert_eq!(mode("mode: rule\n", "", &RT).as_deref(), Some("rule"));
        assert_eq!(
            mode("mode: rule\n", "mode: direct\n", &RT).as_deref(),
            Some("direct")
        );
        assert_eq!(mode("a: 1\n", "", &RT), None);
        assert_eq!(mode("mode: rule\n", "", &global).as_deref(), Some("global"));
        assert_eq!(
            mode("mode: rule\n", "mode: direct\n", &global).as_deref(),
            Some("global")
        );
    }

    #[test]
    fn unified_delay_is_on_unless_the_profile_or_overrides_set_it() {
        let unified = |source: &[u8], overrides: &[u8]| {
            parsed(&compose(source, overrides, &RT).unwrap().yaml)["unified-delay"].as_bool()
        };
        assert_eq!(unified(b"a: 1\n", b""), Some(true));
        assert_eq!(unified(b"unified-delay: false\n", b""), Some(false));
        assert_eq!(
            unified(b"unified-delay: true\n", b"unified-delay: false\n"),
            Some(false)
        );
        assert_eq!(
            unified(b"unified-delay: false\n", b"unified-delay: true\n"),
            Some(true)
        );
    }

    #[test]
    fn owned_overrides_are_ignored_with_a_notice() {
        let c = compose(
            b"a: 1\nmixed-port: 1\n",
            b"mixed-port: 1\nsecret: x\nport: 2\nlog-level: debug\n",
            &RT,
        )
        .unwrap();
        let v = parsed(&c.yaml);
        assert_eq!(v["mixed-port"], Value::from(10999));
        assert_eq!(v["secret"].as_str(), Some("s3"));
        assert!(v.get("port").is_none());
        assert_eq!(v["log-level"].as_str(), Some("info"));
        assert_eq!(
            c.notices,
            ["overrides.yaml: mixed-port, secret, port, log-level ignored (set by xihomo)"]
        );
        assert_eq!(c.ignored, ["mixed-port"], "only the profile's own keys");
    }

    #[test]
    fn extra_controllers_listeners_and_tun_are_disabled() {
        let source = b"tun: {enable: true, stack: system}\nport: 1\nsocks-port: 2\nredir-port: 3\ntproxy-port: 4\nbind-address: '*'\nexternal-controller-tls: 0.0.0.0:9\nexternal-controller-unix: /tmp/x\nexternal-controller-pipe: x\nexternal-controller-cors: {}\nexternal-ui: ui\nexternal-ui-url: u\nexternal-ui-name: n\nexternal-doh-server: /dns\ntuic-server: {enable: true}\nss-config: x\nvmess-config: x\nauthentication: [\"u:p\"]\nlisteners:\n  - {name: t, type: tun}\n  - {name: m, type: mixed, port: 1}\n";
        let c = compose(source, b"", &RT).unwrap();
        let v = parsed(&c.yaml);
        assert_eq!(v["tun"]["enable"], Value::from(false));
        for key in REMOVED {
            assert!(v.get(key).is_none(), "{key}");
        }
        assert_eq!(v["allow-lan"], Value::from(false));
        assert_eq!(v["skip-auth-prefixes"][0].as_str(), Some("127.0.0.1/8"));
        assert_eq!(v["skip-auth-prefixes"][1].as_str(), Some("::1/128"));
        assert_eq!(
            v["authentication"][0].as_str(),
            Some("u:p"),
            "left to the profile"
        );
        assert_eq!(v["listeners"].as_sequence().unwrap().len(), 1);
        assert_eq!(c.notices.len(), 1);
        assert_eq!(c.ignored.len(), 17);
    }

    #[test]
    fn self_addressed_listeners_stay_on_this_mac_unless_shared() {
        let source = b"listeners:\n  - {name: m, type: mixed, port: 1}\n  - {name: s, type: socks, port: 2, listen: 0.0.0.0}\ntunnels:\n  - tcp/udp,0.0.0.0:6553,114.114.114.114:53,proxy\n  - {network: [tcp], address: '[::]:7777', target: 'a:1'}\ndns: {enable: true, listen: ':53'}\n";
        let v = parsed(&compose(source, b"", &RT).unwrap().yaml);
        for i in 0..2 {
            assert_eq!(v["listeners"][i]["listen"].as_str(), Some("127.0.0.1"));
        }
        assert_eq!(
            v["tunnels"][0].as_str(),
            Some("tcp/udp,127.0.0.1:6553,114.114.114.114:53,proxy")
        );
        assert_eq!(v["tunnels"][1]["address"].as_str(), Some("127.0.0.1:7777"));
        assert_eq!(v["tunnels"][1]["target"].as_str(), Some("a:1"));
        assert_eq!(v["dns"]["listen"].as_str(), Some("127.0.0.1:53"));

        let shared = Runtime {
            allow_lan: true,
            ..RT
        };
        let v = parsed(&compose(source, b"", &shared).unwrap().yaml);
        assert_eq!(v["allow-lan"], Value::from(true));
        assert!(v["listeners"][0].get("listen").is_none(), "as written");
        assert_eq!(v["listeners"][1]["listen"].as_str(), Some("0.0.0.0"));
        assert_eq!(
            v["tunnels"][0].as_str(),
            Some("tcp/udp,0.0.0.0:6553,114.114.114.114:53,proxy")
        );
        assert_eq!(v["dns"]["listen"].as_str(), Some(":53"));
    }

    #[test]
    fn last_good_gets_the_current_owned_fields_and_keeps_the_rest() {
        let paths = temp_paths("reown");
        let old = compose(b"log-level: debug\nmode: rule\n", b"", &RT).unwrap();
        fs::write(paths.last_good(), old.yaml).unwrap();
        let now = Runtime {
            mixed_port: 7000,
            controller_port: 7001,
            allow_lan: true,
            secret: "new",
            mode: Some("global"),
            log_level: LogLevel::Warning,
        };
        reown(&paths.last_good(), &now).unwrap();
        let v = parsed(&fs::read_to_string(paths.last_good()).unwrap());
        assert_eq!(v["mixed-port"], Value::from(7000));
        assert_eq!(v["external-controller"].as_str(), Some("127.0.0.1:7001"));
        assert_eq!(v["secret"].as_str(), Some("new"));
        assert_eq!(v["allow-lan"], Value::from(true));
        assert_eq!(v["mode"].as_str(), Some("global"));
        assert_eq!(v["log-level"].as_str(), Some("warning"));
    }

    #[test]
    fn anchors_and_merge_keys_survive_composition() {
        let source = b"base: &b {interval: 300, url: http://t}\nproxy-groups:\n  - &g {name: A, <<: *b}\n  - *g\n";
        let yaml = compose(source, b"", &RT).unwrap().yaml;
        let v = parsed(&yaml);
        assert_eq!(v["proxy-groups"][1]["name"].as_str(), Some("A"));
        assert!(yaml.contains("<<"), "merge key is left for Mihomo to apply");
    }

    #[test]
    fn invalid_sources_are_rejected() {
        assert!(compose(b"- a\n- b\n", b"", &RT).is_err());
        assert!(compose(b"a: [", b"", &RT).is_err());
        assert!(
            compose(b"a: 1\na: 2\n", b"", &RT).is_err(),
            "duplicate keys"
        );
        assert!(compose(b"a: 1", b"- x\n", &RT).is_err());
    }

    #[test]
    fn symlinked_profile_is_read_not_written_and_runs_in_its_own_home() {
        let paths = temp_paths("link");
        let ext = paths.data.join("elsewhere");
        fs::create_dir_all(&ext).unwrap();
        let target = ext.join("home.yaml");
        fs::write(&target, "mixed-port: 1\n").unwrap();
        assert_eq!(link(&paths, &target).unwrap(), "home");
        assert!(
            fs::read_link(paths.profiles().join("home.yaml"))
                .unwrap()
                .is_absolute()
        );

        assert!(
            !paths.home("home").exists(),
            "no home before first activation"
        );
        let candidate = write_candidate(&paths, "home", &rt_default()).unwrap();
        assert_eq!(candidate.home, paths.home("home"));
        assert!(candidate.home.is_dir());
        assert_eq!(fs::read_to_string(&target).unwrap(), "mixed-port: 1\n");
        assert_eq!(
            fs::read_dir(&ext).unwrap().count(),
            1,
            "nothing written next to the target"
        );
        assert!(
            fs::read_to_string(paths.candidate())
                .unwrap()
                .contains(&Settings::default().mixed_port.to_string())
        );
        assert!(link(&paths, &target).is_err(), "name collision is refused");
    }

    #[test]
    fn import_copies_a_mapping_and_refuses_what_it_cannot_add() {
        let paths = temp_paths("import");
        let ext = paths.data.join("elsewhere");
        fs::create_dir_all(&ext).unwrap();
        let source = ext.join("work.yaml");
        fs::write(&source, "mode: rule\n").unwrap();
        assert_eq!(import(&paths, &source).unwrap(), "work");
        let copy = paths.profiles().join("work.yaml");
        assert!(!fs::symlink_metadata(&copy).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&copy).unwrap(), "mode: rule\n");

        fs::write(&source, "mode: global\n").unwrap();
        let again = import(&paths, &source).unwrap_err().to_string();
        assert!(again.contains("already exists"), "{again}");
        assert_eq!(fs::read_to_string(&copy).unwrap(), "mode: rule\n");

        let linked = ext.join("home.yaml");
        fs::write(&linked, "mode: rule\n").unwrap();
        link(&paths, &linked).unwrap();
        assert!(import(&paths, &linked).is_err(), "a linked name is taken");
        assert_eq!(
            fs::read_link(paths.profiles().join("home.yaml")).unwrap(),
            linked
        );

        for (file, text) in [
            ("list.yaml", "- a\n"),
            ("broken.yaml", "a: [\n"),
            ("empty.yaml", ""),
        ] {
            let bad = ext.join(file);
            fs::write(&bad, text).unwrap();
            assert!(import(&paths, &bad).is_err(), "{file}");
            assert!(!paths.profiles().join(file).exists(), "{file}");
        }
    }

    #[test]
    fn only_yaml_files_with_usable_names_are_profiles() {
        let paths = temp_paths("names");
        for file in ["a.yaml", "b.yml", "c.txt", "..yaml", "...yaml", "d.YAML"] {
            fs::write(paths.profiles().join(file), "a: 1\n").unwrap();
        }
        let names: Vec<_> = list(&paths).unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["a"]);
        assert!(link(&paths, &paths.profiles().join("...yaml")).is_err());
    }

    #[test]
    fn default_profile_only_when_there_is_none_and_never_overwritten() {
        let paths = temp_paths("default");
        ensure_default(&paths).unwrap();
        let default = paths.profiles().join("default.yaml");
        let yaml = compose(&fs::read(&default).unwrap(), b"", &RT)
            .unwrap()
            .yaml;
        assert_eq!(parsed(&yaml)["mode"].as_str(), Some("direct"));

        fs::write(&default, "a: [").unwrap();
        ensure_default(&paths).unwrap();
        assert_eq!(fs::read_to_string(&default).unwrap(), "a: [");

        // A broken link is still a profile: nothing is created next to it.
        fs::remove_file(&default).unwrap();
        std::os::unix::fs::symlink(
            paths.data.join("missing.yaml"),
            paths.profiles().join("x.yaml"),
        )
        .unwrap();
        ensure_default(&paths).unwrap();
        assert!(!default.exists());
    }

    #[test]
    fn removal_deletes_file_or_link_and_home_but_never_the_target() {
        let paths = temp_paths("remove");
        let target = paths.data.join("linked.yaml");
        fs::write(&target, "a: 1\n").unwrap();
        link(&paths, &target).unwrap();
        fs::write(paths.profiles().join("own.yaml"), "a: 1\n").unwrap();
        for name in ["linked", "own"] {
            write_candidate(&paths, name, &rt_default()).unwrap();
            fs::write(paths.home(name).join("cache.db"), "x").unwrap();
        }

        remove(&paths, "linked", &[]).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "a: 1\n");
        assert!(!paths.home("linked").exists());

        remove(&paths, "own", &[paths.home("own")]).unwrap();
        assert!(list(&paths).unwrap().is_empty());
        assert!(paths.home("own").exists(), "protected home survives");
        assert!(remove(&paths, "own", &[]).is_err(), "unknown profile");
    }

    #[test]
    fn reconcile_deletes_orphans_and_keeps_protected_paths() {
        let paths = temp_paths("reconcile");
        fs::write(paths.profiles().join("kept.yaml"), "a: 1\n").unwrap();
        for home in ["kept", "orphan", "running", "last-good"] {
            fs::create_dir_all(paths.home(home)).unwrap();
        }
        for v in ["v1.0.0", "v2.0.0", "v3.0.0"] {
            fs::create_dir_all(paths.bin().join(v)).unwrap();
            fs::write(paths.core_exe(v), "x").unwrap();
        }
        let own = format!("tmp{}", std::process::id());
        for file in [
            "runtime/candidate.tmp1",
            "state/app.tmp99999999",
            "state/secret.tmpx",
            "runtime/candidate.yaml",
        ] {
            fs::write(paths.data.join(file), "x").unwrap();
        }
        let mine = paths.state().join(format!("app.{own}"));
        fs::write(&mine, "x").unwrap();

        let profiles = list(&paths).unwrap();
        let protected = [paths.home("running"), paths.home("last-good")];
        reconcile(&paths, &profiles, &protected, &["v2.0.0", "v3.0.0"]);

        let mut homes: Vec<_> = fs::read_dir(paths.homes())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        homes.sort();
        assert_eq!(homes, ["kept", "last-good", "running"]);
        assert_eq!(crate::update::installed(&paths), ["v2.0.0", "v3.0.0"]);
        assert!(!paths.runtime().join("candidate.tmp1").exists());
        assert!(!paths.state().join("app.tmp99999999").exists());
        assert!(
            paths.state().join("secret.tmpx").exists(),
            "not a pid suffix"
        );
        assert!(paths.candidate().exists());
        assert!(mine.exists(), "this process's own write in progress");
    }

    #[test]
    fn broken_and_replaced_links_are_detected() {
        let paths = temp_paths("broken");
        let target = paths.data.join("t.yaml");
        fs::write(&target, "a: 1\n").unwrap();
        link(&paths, &target).unwrap();
        let before = current_fingerprint(&paths, "t").unwrap();

        // An atomic editor save replaces the inode; content comparison still notices.
        let tmp = paths.data.join("t.tmp");
        fs::write(&tmp, "a: 2\n").unwrap();
        fs::rename(&tmp, &target).unwrap();
        assert_ne!(current_fingerprint(&paths, "t").unwrap(), before);

        fs::remove_file(&target).unwrap();
        let listed = list(&paths).unwrap();
        assert!(listed[0].is_link && listed[0].problem.is_some());
        assert!(write_candidate(&paths, "t", &rt_default()).is_err());
        assert!(
            !paths.candidate().exists(),
            "no snapshot from a broken link"
        );
    }

    #[test]
    fn override_edits_change_the_fingerprint() {
        let paths = temp_paths("ovr");
        fs::write(paths.profiles().join("p.yaml"), "a: 1\n").unwrap();
        let before = current_fingerprint(&paths, "p").unwrap();
        fs::write(paths.overrides(), "log-level: info\n").unwrap();
        assert_ne!(current_fingerprint(&paths, "p").unwrap(), before);
    }

    #[test]
    fn arrange_keeps_recorded_places_and_adds_new_profiles_oldest_first() {
        let paths = temp_paths("arrange");
        for name in ["c", "b", "a"] {
            fs::write(paths.profiles().join(format!("{name}.yaml")), "a: 1\n").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let names = |list: &[Profile]| list.iter().map(|p| p.name.clone()).collect::<Vec<_>>();

        // Nothing recorded yet (an upgrade): creation time decides, not the name.
        let mut listed = list(&paths).unwrap();
        assert_eq!(arrange(&mut listed, &[]), ["c", "b", "a"]);

        // Recorded names keep their place, a gone one is dropped, and new ones follow.
        let mut listed = list(&paths).unwrap();
        let order = ["a".to_owned(), "gone".to_owned()];
        assert_eq!(arrange(&mut listed, &order), ["a", "c", "b"]);
        assert_eq!(names(&listed), ["a", "c", "b"]);
    }
}
