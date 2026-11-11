use crate::settings::{Paths, Settings};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use yaml_serde::{Mapping, Value};

const MAX_YAML: u64 = 32 * 1024 * 1024;

/// Keys xihomo must own; an override naming one is a mistake worth reporting, not ignoring.
const RESERVED: &[&str] = &[
    "mixed-port",
    "external-controller",
    "external-controller-tls",
    "external-controller-unix",
    "external-controller-pipe",
    "external-controller-cors",
    "external-ui",
    "external-ui-url",
    "external-ui-name",
    "external-doh-server",
    "secret",
    "tun",
];

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

pub fn profile_path(paths: &Paths, name: &str) -> Result<PathBuf> {
    list(paths)?
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| p.path)
        .with_context(|| format!("profile {name} not found"))
}

/// Add an independently maintained YAML file as a profile without importing its contents.
pub fn link(paths: &Paths, target: &Path) -> Result<String> {
    let name = target
        .file_stem()
        .and_then(|n| n.to_str())
        .filter(|n| usable_name(n))
        .context("file has no usable name")?
        .to_owned();
    let dest = paths.profiles().join(format!("{name}.yaml"));
    if fs::symlink_metadata(&dest).is_ok() {
        bail!("a profile named {name} already exists");
    }
    let target = std::path::absolute(target).context("resolve the linked file")?;
    std::os::unix::fs::symlink(target, &dest).context("create profile link")?;
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
    pub secret: &'a str,
    /// The user's remembered mode, which beats the profile's and the overrides'.
    pub mode: Option<&'a str>,
}

/// source → overrides.yaml → xihomo-owned fields. Each step replaces whole top-level values:
/// mappings are never merged and lists never concatenated. Returns the YAML and any notices.
pub fn compose(source: &[u8], overrides: &[u8], rt: &Runtime) -> Result<(String, Vec<String>)> {
    let mut map = mapping(source, "profile")?;
    let overrides = mapping(overrides, "overrides.yaml")?;
    for (key, value) in overrides {
        if let Some(name) = key.as_str()
            && RESERVED.contains(&name)
        {
            bail!(
                "overrides.yaml may not set {name}: xihomo owns it (ports live in settings.yaml)"
            );
        }
        map.insert(key, value);
    }

    let mut notices = Vec::new();
    for key in RESERVED {
        map.remove(*key);
    }
    if let Some(Value::Sequence(listeners)) = map.get_mut("listeners") {
        let before = listeners.len();
        listeners.retain(|l| l.get("type").and_then(Value::as_str) != Some("tun"));
        if listeners.len() != before {
            notices.push("TUN listeners were removed: xihomo V1 uses the system proxy".to_owned());
        }
    }
    map.insert("mixed-port".into(), Value::from(rt.mixed_port));
    map.insert(
        "external-controller".into(),
        format!("127.0.0.1:{}", rt.controller_port).into(),
    );
    map.insert("secret".into(), rt.secret.into());
    let mut tun = Mapping::new();
    tun.insert("enable".into(), false.into());
    map.insert("tun".into(), tun.into());
    if let Some(mode) = rt.mode {
        map.insert("mode".into(), mode.into());
    }
    Ok((yaml_serde::to_string(&map)?, notices))
}

pub struct Candidate {
    pub home: PathBuf,
    pub fingerprint: String,
    pub notices: Vec<String>,
}

/// Resolve the profile now, then write a private effective snapshot. Links are only ever read.
pub fn write_candidate(
    paths: &Paths,
    settings: &Settings,
    name: &str,
    secret: &str,
    mode: Option<&str>,
) -> Result<Candidate> {
    let path = profile_path(paths, name)?;
    let target =
        fs::canonicalize(&path).with_context(|| format!("profile {name} cannot be resolved"))?;
    let source = read_bounded(&target)?;
    let overrides = read_overrides(paths)?;
    let (yaml, notices) = compose(
        &source,
        &overrides,
        &Runtime {
            mixed_port: settings.mixed_port,
            controller_port: settings.controller_port,
            secret,
            mode,
        },
    )
    .with_context(|| format!("profile {name}"))?;
    crate::settings::write_private(&paths.candidate(), yaml.as_bytes())?;
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
        notices,
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
        secret: "s3",
        mode: None,
    };

    fn parsed(yaml: &str) -> Value {
        yaml_serde::from_str(yaml).unwrap()
    }

    #[test]
    fn runtime_fields_beat_source_and_overrides_replace_whole_values() {
        let source = b"mixed-port: 7890\nexternal-controller: 0.0.0.0:9090\nsecret: old\nlog-level: debug\ndns: {enable: true, ipv6: true}\nrules: [\"MATCH,DIRECT\"]\n";
        let (yaml, notices) =
            compose(source, b"dns: {enable: false}\nlog-level: info\n", &RT).unwrap();
        let v = parsed(&yaml);
        assert_eq!(v["mixed-port"], Value::from(10999));
        assert_eq!(v["external-controller"].as_str(), Some("127.0.0.1:11000"));
        assert_eq!(v["secret"].as_str(), Some("s3"));
        assert_eq!(v["log-level"].as_str(), Some("info"));
        assert!(
            v["dns"].get("ipv6").is_none(),
            "mappings replace, never merge"
        );
        assert_eq!(v["rules"][0].as_str(), Some("MATCH,DIRECT"));
        assert!(notices.is_empty());
    }

    #[test]
    fn remembered_mode_beats_profile_and_overrides() {
        let mode = |source: &str, overrides: &str, rt: &Runtime| {
            let (yaml, _) = compose(source.as_bytes(), overrides.as_bytes(), rt).unwrap();
            parsed(&yaml)["mode"].as_str().map(str::to_owned)
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
    fn reserved_overrides_are_rejected() {
        for bad in [
            "mixed-port: 1",
            "secret: x",
            "tun: {enable: true}",
            "external-controller-unix: /tmp/x",
        ] {
            let err = compose(b"a: 1", bad.as_bytes(), &RT)
                .unwrap_err()
                .to_string();
            assert!(err.contains("may not set"), "{err}");
        }
    }

    #[test]
    fn extra_controllers_and_tun_are_disabled() {
        let source = b"tun: {enable: true, stack: system}\nexternal-controller-tls: 0.0.0.0:9\nexternal-controller-unix: /tmp/x\nexternal-ui: ui\nexternal-doh-server: /dns\nlisteners:\n  - {name: t, type: tun}\n  - {name: m, type: mixed, port: 1}\n";
        let (yaml, notices) = compose(source, b"", &RT).unwrap();
        let v = parsed(&yaml);
        assert_eq!(v["tun"]["enable"], Value::from(false));
        for key in [
            "external-controller-tls",
            "external-controller-unix",
            "external-ui",
            "external-doh-server",
        ] {
            assert!(v.get(key).is_none(), "{key}");
        }
        assert_eq!(v["listeners"].as_sequence().unwrap().len(), 1);
        assert_eq!(notices.len(), 1);
    }

    #[test]
    fn anchors_and_merge_keys_survive_composition() {
        let source = b"base: &b {interval: 300, url: http://t}\nproxy-groups:\n  - &g {name: A, <<: *b}\n  - *g\n";
        let (yaml, _) = compose(source, b"", &RT).unwrap();
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
        let candidate = write_candidate(&paths, &Settings::default(), "home", "x", None).unwrap();
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
                .contains("10999")
        );
        assert!(link(&paths, &target).is_err(), "name collision is refused");
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
        let (yaml, _) = compose(&fs::read(&default).unwrap(), b"", &RT).unwrap();
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
            write_candidate(&paths, &Settings::default(), name, "x", None).unwrap();
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
        assert!(write_candidate(&paths, &Settings::default(), "t", "x", None).is_err());
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
}
