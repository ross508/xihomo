use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Everything xihomo owns on disk. Tests point this at a temporary directory.
#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn user_default() -> Result<Self> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(Self {
            root: PathBuf::from(home).join(".config/xihomo"),
        })
    }

    pub fn settings(&self) -> PathBuf {
        self.root.join("settings.json")
    }
    pub fn overrides(&self) -> PathBuf {
        self.root.join("overrides.yaml")
    }
    pub fn profiles(&self) -> PathBuf {
        self.root.join("profiles")
    }
    pub fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }
    pub fn runtime(&self) -> PathBuf {
        self.root.join("runtime")
    }
    pub fn state(&self) -> PathBuf {
        self.root.join("state")
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
            self.root.clone(),
            self.profiles(),
            self.bin(),
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub active_profile: Option<String>,
    pub icon_set: String,
    pub core_version: Option<String>,
    pub previous_core_version: Option<String>,
    pub mixed_port: u16,
    pub controller_port: u16,
    /// Optional per-profile Mihomo home; defaults to the resolved profile's parent directory.
    pub homes: BTreeMap<String, PathBuf>,
    pub last_good_home: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            active_profile: None,
            icon_set: "orbit".into(),
            core_version: None,
            previous_core_version: None,
            mixed_port: 10999,
            controller_port: 11000,
            homes: BTreeMap::new(),
            last_good_home: None,
        }
    }
}

impl Settings {
    /// Unparseable settings are set aside rather than overwritten, and reported to the caller.
    pub fn load(paths: &Paths) -> Result<(Self, Option<String>)> {
        let path = paths.settings();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
            Err(e) => return Err(e).context("read settings.json"),
        };
        match serde_json::from_slice(&bytes) {
            Ok(settings) => Ok((settings, None)),
            Err(e) => {
                let aside = path.with_extension("json.invalid");
                fs::rename(&path, &aside)?;
                Ok((
                    Self::default(),
                    Some(format!(
                        "settings.json was invalid ({e}); moved to {}",
                        aside.display()
                    )),
                ))
            }
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_private(&paths.settings(), &serde_json::to_vec_pretty(self)?)
    }
}

#[cfg(test)]
pub fn temp_paths(name: &str) -> Paths {
    let root = std::env::temp_dir().join(format!("xihomo-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let paths = Paths { root };
    paths.ensure().unwrap();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_settings_are_set_aside_not_overwritten() {
        let paths = temp_paths("settings");
        fs::write(paths.settings(), "{nope").unwrap();
        let (settings, notice) = Settings::load(&paths).unwrap();
        assert_eq!(settings, Settings::default());
        assert!(notice.unwrap().contains("invalid"));
        assert_eq!(
            fs::read_to_string(paths.settings().with_extension("json.invalid")).unwrap(),
            "{nope"
        );
    }

    #[test]
    fn write_private_replaces_symlink_without_touching_target() {
        let paths = temp_paths("write");
        let target = paths.root.join("target");
        let link = paths.root.join("link");
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
