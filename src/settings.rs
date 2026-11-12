use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use yaml_serde::{Mapping, Value};

/// Debug builds are the dev channel: their own directories and default ports, so development never
/// touches the release build used day to day.
pub const DEV: bool = cfg!(debug_assertions);
pub const NAME: &str = if DEV { "xihomo-dev" } else { "xihomo" };

/// Everything xihomo reads or owns on disk: `config` is the user's, `data` is xihomo's.
/// Tests point both at a temporary directory.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config: PathBuf,
    pub data: PathBuf,
}

impl Paths {
    pub fn user_default() -> Result<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
        Ok(Self {
            config: home.join(".config").join(NAME),
            data: home.join("Library/Application Support").join(NAME),
        })
    }

    pub fn settings(&self) -> PathBuf {
        self.config.join("settings.yaml")
    }
    pub fn overrides(&self) -> PathBuf {
        self.config.join("overrides.yaml")
    }
    pub fn profiles(&self) -> PathBuf {
        self.config.join("profiles")
    }
    pub fn bin(&self) -> PathBuf {
        self.data.join("bin")
    }
    pub fn homes(&self) -> PathBuf {
        self.data.join("homes")
    }
    /// Mihomo's `-d` for a profile. Profile names never contain `/` and are never `.` or `..`.
    pub fn home(&self, profile: &str) -> PathBuf {
        self.homes().join(profile)
    }
    pub fn runtime(&self) -> PathBuf {
        self.data.join("runtime")
    }
    pub fn state(&self) -> PathBuf {
        self.data.join("state")
    }
    pub fn app_data(&self) -> PathBuf {
        self.state().join("app.json")
    }
    pub fn window(&self) -> PathBuf {
        self.state().join("window.json")
    }
    pub fn candidate(&self) -> PathBuf {
        self.runtime().join("candidate.yaml")
    }
    /// The file a launched core reads with `-f`, and again on a dashboard's Restart or Reload.
    pub fn config(&self) -> PathBuf {
        self.runtime().join("config.yaml")
    }
    pub fn last_good(&self) -> PathBuf {
        self.runtime().join("last-good.yaml")
    }
    pub fn core_exe(&self, version: &str) -> PathBuf {
        self.bin().join(version).join("mihomo")
    }

    /// Generated configs hold credentials, so every owned directory is private.
    pub fn ensure(&self) -> Result<()> {
        for dir in [
            self.config.clone(),
            self.profiles(),
            self.data.clone(),
            self.bin(),
            self.homes(),
            self.runtime(),
            self.state(),
        ] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .with_context(|| format!("create {}", dir.display()))?;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    fn secret_file(&self) -> PathBuf {
        self.state().join("secret")
    }

    /// Stable per-install controller secret, so last-good snapshots stay usable across launches.
    /// `true` when it was missing or invalid and a new one was written.
    pub fn secret(&self) -> Result<(String, bool)> {
        if let Ok(secret) = fs::read_to_string(self.secret_file())
            && valid_secret(&secret)
        {
            return Ok((secret, false));
        }
        let secret = new_secret()?;
        self.save_secret(&secret)?;
        Ok((secret, true))
    }

    pub fn save_secret(&self, secret: &str) -> Result<()> {
        write_private(&self.secret_file(), secret.as_bytes())
    }
}

/// Letters and digits without look-alikes (0 O o, 1 I l), plus the URL-safe marks, so a secret
/// can be read off the screen and pasted into a dashboard's address or a header unescaped.
const SECRET_CHARS: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789-_.~";
/// 20 of 60 characters: about 118 bits.
const SECRET_LEN: usize = 20;

/// Anything URL-safe and long enough, so a secret from an earlier version (64 hex digits) stays.
fn valid_secret(secret: &str) -> bool {
    (16..=128).contains(&secret.len())
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
}

/// A new secret from the system's random source, without modulo bias.
pub fn new_secret() -> Result<String> {
    let mut random = fs::File::open("/dev/urandom")?;
    let limit = (256 / SECRET_CHARS.len() * SECRET_CHARS.len()) as u8;
    let mut secret = String::with_capacity(SECRET_LEN);
    let mut byte = [0u8; 1];
    while secret.len() < SECRET_LEN {
        std::io::Read::read_exact(&mut random, &mut byte)?;
        if byte[0] < limit {
            secret.push(SECRET_CHARS[byte[0] as usize % SECRET_CHARS.len()] as char);
        }
    }
    Ok(secret)
}

/// Atomic and private. `rename` replaces a symlink at `path` rather than writing through it.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("write {}", path.display()))
}

/// A missing file gives defaults. An unparseable one is set aside rather than overwritten, and
/// reported to the caller.
fn load_or_set_aside<T: Default>(
    path: &Path,
    parse: impl FnOnce(&[u8]) -> Result<T>,
) -> Result<(T, Option<String>)> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    match parse(&bytes) {
        Ok(value) => Ok((value, None)),
        Err(e) => {
            let mut aside = path.as_os_str().to_owned();
            aside.push(".invalid");
            fs::rename(path, &aside)?;
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            Ok((
                T::default(),
                Some(format!(
                    "{name} was invalid ({e:#}); moved to {}",
                    Path::new(&aside).display()
                )),
            ))
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Auto,
    Light,
    Dark,
}

/// `settings.yaml`: only what the user edits. xihomo changes only the key edited in the GUI.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Settings {
    pub mixed_port: u16,
    pub controller_port: u16,
    /// Other devices may use the proxy.
    pub allow_lan: bool,
    pub icon_set: String,
    pub theme: Theme,
}

impl Default for Settings {
    fn default() -> Self {
        let (mixed_port, controller_port) = if cfg!(debug_assertions) {
            (20999, 21000)
        } else {
            (10999, 11000)
        };
        Self {
            mixed_port,
            controller_port,
            allow_lan: false,
            icon_set: "circle".into(),
            theme: Theme::Auto,
        }
    }
}

/// Ports below 1024 need root; xihomo's are always unprivileged.
pub fn valid_port(port: u16) -> bool {
    port >= 1024
}

/// One key's value, or its default when it is absent; an invalid one is named in `invalid`.
fn field<T: DeserializeOwned>(
    map: &Mapping,
    key: &str,
    default: T,
    invalid: &mut Vec<String>,
) -> T {
    match map.get(key) {
        None => default,
        Some(value) => yaml_serde::from_value(value.clone()).unwrap_or_else(|_| {
            invalid.push(key.to_owned());
            default
        }),
    }
}

/// Where a write to `path` lands: through a symlink to its target, so dotfile links survive.
fn write_target(path: &Path) -> PathBuf {
    fs::canonicalize(path)
        .or_else(|_| fs::read_link(path).map(|t| path.parent().unwrap_or(Path::new("")).join(t)))
        .unwrap_or_else(|_| path.to_owned())
}

impl Settings {
    /// Field by field: an invalid value takes its default and is reported, the others are kept.
    /// Only a file that is not YAML, or not a mapping, is set aside.
    pub fn load(paths: &Paths) -> Result<(Self, Option<String>)> {
        let mut invalid = Vec::new();
        let (settings, notice) = load_or_set_aside(&paths.settings(), |bytes| {
            // An empty or comment-only file is a valid "all defaults".
            let map = match yaml_serde::from_slice::<Value>(bytes)? {
                Value::Null => Mapping::new(),
                Value::Mapping(map) => map,
                _ => bail!("not a YAML mapping"),
            };
            let d = Self::default();
            let mut s = Self {
                mixed_port: field(&map, "mixed_port", d.mixed_port, &mut invalid),
                controller_port: field(&map, "controller_port", d.controller_port, &mut invalid),
                allow_lan: field(&map, "allow_lan", d.allow_lan, &mut invalid),
                icon_set: field(&map, "icon_set", d.icon_set, &mut invalid),
                theme: field(&map, "theme", d.theme, &mut invalid),
            };
            for (key, port, default) in [
                ("mixed_port", &mut s.mixed_port, d.mixed_port),
                ("controller_port", &mut s.controller_port, d.controller_port),
            ] {
                if !valid_port(*port) {
                    invalid.push(key.to_owned());
                    *port = default;
                }
            }
            if s.mixed_port == s.controller_port {
                invalid.push("controller_port (same as mixed_port)".to_owned());
                (s.mixed_port, s.controller_port) = (d.mixed_port, d.controller_port);
            }
            Ok(s)
        })?;
        let invalid = (!invalid.is_empty()).then(|| {
            format!(
                "settings.yaml: invalid {}; using the default",
                invalid.join(", ")
            )
        });
        Ok((settings, notice.or(invalid)))
    }

    /// Re-reads the file and changes only `key`, so edits made since launch are kept.
    pub fn write_key(paths: &Paths, key: &str, value: impl Serialize) -> Result<()> {
        let path = paths.settings();
        let mut map = match fs::read(&path) {
            Ok(bytes) => match yaml_serde::from_slice::<Value>(&bytes) {
                Ok(Value::Mapping(map)) => map,
                Ok(Value::Null) => Mapping::new(),
                _ => bail!("settings.yaml cannot be parsed; fix it before changing settings here"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Mapping::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        map.insert(key.into(), yaml_serde::to_value(value)?);
        write_private(
            &write_target(&path),
            yaml_serde::to_string(&map)?.as_bytes(),
        )
    }

    #[cfg(test)]
    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_private(&paths.settings(), yaml_serde::to_string(self)?.as_bytes())
    }
}

/// `state/app.json`: what xihomo maintains.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppData {
    pub active_profile: Option<String>,
    pub core_version: Option<String>,
    pub previous_core_version: Option<String>,
    pub last_good_home: Option<PathBuf>,
    /// The user's last Start/Stop and System Proxy choices, restored at launch.
    pub core_on: bool,
    pub proxy_on: bool,
    /// The user's outbound mode; `None` keeps each profile's own `mode`.
    pub mode: Option<String>,
}

impl Default for AppData {
    fn default() -> Self {
        Self {
            active_profile: None,
            core_version: None,
            previous_core_version: None,
            last_good_home: None,
            core_on: true,
            proxy_on: false,
            mode: None,
        }
    }
}

impl AppData {
    pub fn load(paths: &Paths) -> Result<(Self, Option<String>)> {
        load_or_set_aside(&paths.app_data(), |bytes| {
            Ok(serde_json::from_slice(bytes)?)
        })
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_private(&paths.app_data(), &serde_json::to_vec_pretty(self)?)
    }
}

#[cfg(test)]
pub fn temp_paths(name: &str) -> Paths {
    let root = std::env::temp_dir().join(format!("xihomo-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let paths = Paths {
        config: root.join("config"),
        data: root.join("data"),
    };
    paths.ensure().unwrap();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(debug_assertions)]
    fn debug_builds_stay_off_the_release_directories_and_ports() {
        let paths = Paths::user_default().unwrap();
        assert!(paths.config.ends_with(".config/xihomo-dev"));
        assert!(
            paths
                .data
                .ends_with("Library/Application Support/xihomo-dev")
        );
        let settings = Settings::default();
        assert_eq!(
            (settings.mixed_port, settings.controller_port),
            (20999, 21000)
        );
    }

    #[test]
    fn missing_keys_and_empty_files_take_defaults() {
        let paths = temp_paths("settings-defaults");
        assert_eq!(Settings::load(&paths).unwrap(), (Settings::default(), None));
        fs::write(paths.settings(), "# only a comment\n").unwrap();
        assert_eq!(Settings::load(&paths).unwrap().0, Settings::default());
        fs::write(
            paths.settings(),
            "theme: dark\nmixed_port: 7000\nunknown: 1\n",
        )
        .unwrap();
        let (settings, notice) = Settings::load(&paths).unwrap();
        assert_eq!(notice, None);
        assert_eq!(
            settings,
            Settings {
                theme: Theme::Dark,
                mixed_port: 7000,
                ..Settings::default()
            }
        );
        settings.save(&paths).unwrap();
        assert_eq!(Settings::load(&paths).unwrap().0, settings);
        assert_eq!(AppData::load(&paths).unwrap(), (AppData::default(), None));
    }

    #[test]
    fn invalid_files_are_set_aside_not_overwritten() {
        let paths = temp_paths("settings-invalid");
        for (path, bad) in [
            (paths.settings(), "a: ["),
            (paths.settings(), "- a\n"),
            (paths.app_data(), "{nope"),
        ] {
            fs::write(&path, bad).unwrap();
            let notice = if path == paths.settings() {
                let (settings, notice) = Settings::load(&paths).unwrap();
                assert_eq!(settings, Settings::default());
                notice
            } else {
                let (data, notice) = AppData::load(&paths).unwrap();
                assert_eq!(data, AppData::default());
                notice
            };
            assert!(notice.unwrap().contains("invalid"));
            let mut aside = path.into_os_string();
            aside.push(".invalid");
            assert_eq!(fs::read_to_string(aside).unwrap(), bad);
        }
    }

    #[test]
    fn an_invalid_value_takes_its_default_and_keeps_the_others() {
        let paths = temp_paths("settings-fields");
        let d = Settings::default();
        let text = "theme: purple\nicon_set: bars\nmixed_port: 80\ncontroller_port: 7001\nallow_lan: yes please\n";
        fs::write(paths.settings(), text).unwrap();
        let (settings, notice) = Settings::load(&paths).unwrap();
        assert_eq!(
            settings,
            Settings {
                icon_set: "bars".into(),
                controller_port: 7001,
                ..d.clone()
            }
        );
        let notice = notice.unwrap();
        for key in ["theme", "mixed_port", "allow_lan"] {
            assert!(notice.contains(key), "{notice}");
        }
        assert!(!notice.contains("icon_set"), "{notice}");
        assert_eq!(fs::read_to_string(paths.settings()).unwrap(), text, "kept");

        fs::write(
            paths.settings(),
            "mixed_port: 7000\ncontroller_port: 7000\n",
        )
        .unwrap();
        let (settings, notice) = Settings::load(&paths).unwrap();
        assert_eq!(
            (settings.mixed_port, settings.controller_port),
            (d.mixed_port, d.controller_port)
        );
        assert!(notice.unwrap().contains("same as"));
    }

    #[test]
    fn a_gui_write_changes_one_key_and_follows_a_symlink() {
        let paths = temp_paths("settings-write");
        Settings::write_key(&paths, "theme", Theme::Dark).unwrap();
        assert_eq!(Settings::load(&paths).unwrap().0.theme, Theme::Dark);

        // Edited by hand since launch: the other keys, even unknown ones, survive.
        let dotfiles = paths.data.join("dotfiles");
        fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join("settings.yaml");
        fs::write(&target, "mixed_port: 7000\nunknown: 1\n").unwrap();
        fs::remove_file(paths.settings()).unwrap();
        std::os::unix::fs::symlink(&target, paths.settings()).unwrap();
        Settings::write_key(&paths, "allow_lan", true).unwrap();
        assert!(fs::symlink_metadata(paths.settings()).unwrap().is_symlink());
        let (settings, notice) = Settings::load(&paths).unwrap();
        assert_eq!(
            (settings.mixed_port, settings.allow_lan, notice),
            (7000, true, None)
        );
        assert!(fs::read_to_string(&target).unwrap().contains("unknown: 1"));

        // A file broken since launch is not replaced.
        fs::write(&target, "a: [").unwrap();
        assert!(Settings::write_key(&paths, "theme", Theme::Light).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "a: [");
    }

    #[test]
    fn a_missing_or_invalid_secret_is_regenerated_and_reported() {
        let paths = temp_paths("secret");
        let (first, created) = paths.secret().unwrap();
        assert!(created && first.len() == SECRET_LEN);
        assert!(first.bytes().all(|b| SECRET_CHARS.contains(&b)));
        assert_eq!(paths.secret().unwrap(), (first.clone(), false));
        for bad in ["short", "has a space in it!!", "quote\"and#hash&amp;sign"] {
            fs::write(paths.state().join("secret"), bad).unwrap();
            let (next, created) = paths.secret().unwrap();
            assert!(created && valid_secret(&next), "{bad}");
        }
        // A secret from an earlier version is kept.
        let old = "0123456789abcdef".repeat(4);
        fs::write(paths.state().join("secret"), &old).unwrap();
        assert_eq!(paths.secret().unwrap(), (old, false));
        assert_ne!(new_secret().unwrap(), new_secret().unwrap());
    }

    #[test]
    fn app_data_lives_in_the_data_root() {
        let paths = temp_paths("app-data");
        let data = AppData {
            active_profile: Some("p".into()),
            core_version: Some("v1".into()),
            ..AppData::default()
        };
        data.save(&paths).unwrap();
        assert!(paths.app_data().starts_with(&paths.data));
        assert!(
            !paths.settings().exists(),
            "app state never writes settings.yaml"
        );
        assert_eq!(AppData::load(&paths).unwrap().0, data);
        assert_eq!(paths.home("p"), paths.data.join("homes/p"));
    }

    #[test]
    fn write_private_replaces_symlink_without_touching_target() {
        let paths = temp_paths("write");
        let target = paths.data.join("target");
        let link = paths.data.join("link");
        fs::write(&target, "original").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_private(&link, b"new").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "original");
        assert!(!fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(
            fs::metadata(&link).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
