use crate::settings::{Paths, Settings};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
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

pub fn list(paths: &Paths) -> Result<Vec<Profile>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(paths.profiles()).context("read profiles directory")? {
        let path = entry?.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "yaml" && ext != "yml" {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
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
        .context("file has no usable name")?
        .to_owned();
    let dest = paths.profiles().join(format!("{name}.yaml"));
    if fs::symlink_metadata(&dest).is_ok() {
        bail!("a profile named {name} already exists");
    }
    std::os::unix::fs::symlink(target, &dest).context("create profile link")?;
    Ok(name)
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
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
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
                "overrides.yaml may not set {name}: xihomo owns it (ports live in settings.json)"
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
        },
    )
    .with_context(|| format!("profile {name}"))?;
    crate::settings::write_private(&paths.candidate(), yaml.as_bytes())?;
    let home = match settings.homes.get(name) {
        Some(home) => home.clone(),
        None => target
            .parent()
            .context("profile has no parent directory")?
            .to_owned(),
    };
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
    fn symlinked_profile_is_read_not_written_and_home_follows_target() {
        let paths = temp_paths("link");
        let ext = paths.root.join("elsewhere");
        fs::create_dir_all(&ext).unwrap();
        let target = ext.join("home.yaml");
        fs::write(&target, "mixed-port: 1\n").unwrap();
        assert_eq!(link(&paths, &target).unwrap(), "home");

        let candidate = write_candidate(&paths, &Settings::default(), "home", "x").unwrap();
        assert_eq!(candidate.home, fs::canonicalize(&ext).unwrap());
        assert_eq!(fs::read_to_string(&target).unwrap(), "mixed-port: 1\n");
        assert!(
            fs::read_to_string(paths.candidate())
                .unwrap()
                .contains("10999")
        );
        assert!(link(&paths, &target).is_err(), "name collision is refused");
    }

    #[test]
    fn explicit_home_wins_over_target_parent() {
        let paths = temp_paths("home");
        fs::write(paths.profiles().join("p.yaml"), "a: 1\n").unwrap();
        let mut settings = Settings::default();
        settings.homes.insert("p".into(), "/somewhere".into());
        assert_eq!(
            write_candidate(&paths, &settings, "p", "x").unwrap().home,
            PathBuf::from("/somewhere")
        );
    }

    #[test]
    fn broken_and_replaced_links_are_detected() {
        let paths = temp_paths("broken");
        let target = paths.root.join("t.yaml");
        fs::write(&target, "a: 1\n").unwrap();
        link(&paths, &target).unwrap();
        let before = current_fingerprint(&paths, "t").unwrap();

        // An atomic editor save replaces the inode; content comparison still notices.
        let tmp = paths.root.join("t.tmp");
        fs::write(&tmp, "a: 2\n").unwrap();
        fs::rename(&tmp, &target).unwrap();
        assert_ne!(current_fingerprint(&paths, "t").unwrap(), before);

        fs::remove_file(&target).unwrap();
        let listed = list(&paths).unwrap();
        assert!(listed[0].is_link && listed[0].problem.is_some());
        assert!(write_candidate(&paths, &Settings::default(), "t", "x").is_err());
        assert!(
            !paths.candidate().exists(),
            "no snapshot from a broken link"
        );
    }

    #[test]
    fn override_edits_change_the_fingerprint() {
        let paths = temp_paths("ovr");
        fs::write(paths.profiles().join("p.yml"), "a: 1\n").unwrap();
        let before = current_fingerprint(&paths, "p").unwrap();
        fs::write(paths.overrides(), "log-level: info\n").unwrap();
        assert_ne!(current_fingerprint(&paths, "p").unwrap(), before);
    }
}
