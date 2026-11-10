use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

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
            config: home.join(".config/xihomo"),
            data: home.join("Library/Application Support/xihomo"),
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
    pub fn candidate(&self) -> PathBuf {
        self.runtime().join("candidate.yaml")
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

    /// Stable per-install controller secret, so last-good snapshots stay usable across launches.
    pub fn secret(&self) -> Result<String> {
        let path = self.state().join("secret");
        if let Ok(secret) = fs::read_to_string(&path)
            && secret.len() == 64
        {
            return Ok(secret);
        }
        let mut bytes = [0u8; 32];
        std::io::Read::read_exact(&mut fs::File::open("/dev/urandom")?, &mut bytes)?;
        let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        write_private(&path, secret.as_bytes())?;
        Ok(secret)
    }
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

/// `settings.yaml`: only what the user edits. xihomo rewrites it only when a preference changes
/// in the GUI.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub mixed_port: u16,
    pub controller_port: u16,
    pub icon_set: String,
    pub theme: Theme,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mixed_port: 10999,
            controller_port: 11000,
            icon_set: "orbit".into(),
            theme: Theme::Auto,
        }
    }
}

impl Settings {
    pub fn load(paths: &Paths) -> Result<(Self, Option<String>)> {
        load_or_set_aside(&paths.settings(), |bytes| {
            // An empty or comment-only file is a valid "all defaults".
            Ok(match yaml_serde::from_slice::<yaml_serde::Value>(bytes)? {
                yaml_serde::Value::Null => Self::default(),
                value => yaml_serde::from_value(value)?,
            })
        })
    }

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
            (paths.settings(), "theme: purple\n"),
            (paths.settings(), "a: ["),
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
