//! Staged core updates. Versions live outside the app bundle in bin/<version>/mihomo; a staged
//! binary only becomes selectable after verification, and settings switch only after it runs.
use crate::settings::Paths;
use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const LATEST: &str = "https://api.github.com/repos/MetaCubeX/mihomo/releases/latest";
const DOWNLOAD_PREFIX: &str = "https://github.com/MetaCubeX/mihomo/releases/download/";
const MAX_GZ: u64 = 128 * 1024 * 1024;
const MAX_BIN: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

#[derive(Deserialize)]
struct RawRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    assets: Vec<RawAsset>,
}

#[derive(Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

/// The version becomes a path component, so it is checked like any other untrusted path.
pub fn valid_version(version: &str) -> bool {
    version.starts_with('v')
        && version.len() < 40
        && version[1..].starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

fn pick_release(raw: RawRelease) -> Result<Release> {
    ensure!(
        !raw.prerelease && !raw.draft,
        "latest release is not a stable release"
    );
    ensure!(
        valid_version(&raw.tag_name),
        "unexpected release tag {:?}",
        raw.tag_name
    );
    let name = format!("mihomo-darwin-arm64-{}.gz", raw.tag_name);
    let asset = raw
        .assets
        .into_iter()
        .find(|a| a.name == name)
        .with_context(|| format!("release has no {name}"))?;
    ensure!(
        asset.browser_download_url.starts_with(DOWNLOAD_PREFIX),
        "unexpected download URL"
    );
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()))
        .context("release asset has no SHA-256 digest")?
        .to_ascii_lowercase();
    Ok(Release {
        version: raw.tag_name,
        url: asset.browser_download_url,
        sha256,
    })
}

/// Downloads go through the running core when there is one. Otherwise reqwest follows the
/// environment and system proxy settings, so first-run bootstrap works behind another proxy.
fn client(via_mixed_port: Option<u16>) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent("xihomo")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600));
    if let Some(port) = via_mixed_port {
        builder = builder.proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{port}"))?);
    }
    Ok(builder.build()?)
}

pub async fn check_latest(via: Option<u16>) -> Result<Release> {
    let resp = client(via)?
        .get(LATEST)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    ensure!(
        resp.status().is_success(),
        "GitHub returned {}",
        resp.status()
    );
    pick_release(resp.json().await?)
}

pub async fn download(release: &Release, via: Option<u16>) -> Result<Vec<u8>> {
    let mut resp = client(via)?.get(&release.url).send().await?;
    ensure!(
        resp.status().is_success(),
        "download returned {}",
        resp.status()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        bytes.extend_from_slice(&chunk);
        ensure!(
            bytes.len() as u64 <= MAX_GZ,
            "download exceeds {MAX_GZ} bytes"
        );
    }
    Ok(bytes)
}

fn is_arm64_macho(head: &[u8]) -> bool {
    head.len() >= 8
        && head[..4] == 0xfeed_facf_u32.to_le_bytes()
        && head[4..8] == 0x0100_000c_u32.to_le_bytes()
}

fn staging_dir(paths: &Paths, version: &str) -> PathBuf {
    paths.bin().join(format!(".stage-{version}"))
}

fn unpack(gz: &[u8], sha256: &str, dest: &Path, max_bin: u64) -> Result<()> {
    let digest = format!("{:x}", Sha256::digest(gz));
    ensure!(digest == sha256, "download failed its SHA-256 check");
    let mut bin = Vec::new();
    GzDecoder::new(gz)
        .take(max_bin + 1)
        .read_to_end(&mut bin)
        .context("cannot decompress download")?;
    ensure!(
        bin.len() as u64 <= max_bin,
        "decompressed core exceeds {max_bin} bytes"
    );
    ensure!(
        is_arm64_macho(&bin),
        "download is not an arm64 macOS executable"
    );
    fs::write(dest, bin)?;
    fs::set_permissions(dest, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

async fn verify_runs(exe: &Path, version: &str) -> Result<()> {
    let output = tokio::process::Command::new(exe)
        .arg("-v")
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(15), output)
        .await
        .context("core version check timed out")?
        .with_context(|| {
            format!(
                "cannot run {} (macOS security may have blocked it)",
                exe.display()
            )
        })?;
    let text = String::from_utf8_lossy(&output.stdout);
    ensure!(
        output.status.success() && text.contains(version),
        "core reports an unexpected version"
    );
    Ok(())
}

/// Same filesystem as the final location, so publishing is one atomic rename. Failures leave no
/// selectable version behind, and an interrupted run only leaves a staging directory to overwrite.
pub async fn stage(paths: &Paths, release: &Release, gz: &[u8]) -> Result<PathBuf> {
    ensure!(valid_version(&release.version), "invalid version");
    let staging = staging_dir(paths, &release.version);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let result = async {
        let exe = staging.join("mihomo");
        unpack(gz, &release.sha256, &exe, MAX_BIN)?;
        verify_runs(&exe, &release.version).await?;
        let dest = paths.bin().join(&release.version);
        if dest.exists() {
            bail!("{} is already installed", release.version);
        }
        fs::rename(&staging, &dest)?;
        Ok(dest.join("mihomo"))
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

pub fn installed(paths: &Paths) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(paths.bin())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| valid_version(name) && paths.core_exe(name).is_file())
        .collect();
    out.sort();
    out
}

/// Keep only the running version and its rollback target.
pub fn prune(paths: &Paths, keep: &[&str]) {
    for version in installed(paths) {
        if !keep.contains(&version.as_str()) {
            let _ = fs::remove_dir_all(paths.bin().join(version));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::temp_paths;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    fn gz(bytes: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::fast());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn sha(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn macho(extra: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xfeed_facf_u32.to_le_bytes());
        bytes.extend_from_slice(&0x0100_000c_u32.to_le_bytes());
        bytes.resize(8 + extra, 0);
        bytes
    }

    #[test]
    fn versions_cannot_escape_the_bin_directory() {
        for good in ["v1.19.33", "v2.0.0-rc1"] {
            assert!(valid_version(good));
        }
        for bad in [
            "../evil",
            "v1/../../x",
            "1.19",
            "v",
            "vx",
            ".stage-v1",
            "v1 2",
            "",
        ] {
            assert!(!valid_version(bad), "{bad}");
        }
    }

    #[test]
    fn release_selection_requires_exact_asset_and_digest() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let raw = |tag: &str, pre: bool, url: &str, digest: Option<String>| RawRelease {
            tag_name: tag.into(),
            prerelease: pre,
            draft: false,
            assets: vec![
                RawAsset {
                    name: format!("mihomo-darwin-arm64-go120-{tag}.gz"),
                    browser_download_url: "x".into(),
                    digest: None,
                },
                RawAsset {
                    name: format!("mihomo-darwin-arm64-{tag}.gz"),
                    browser_download_url: url.into(),
                    digest,
                },
            ],
        };
        let url = format!("{DOWNLOAD_PREFIX}v1.2.3/a.gz");
        let got = pick_release(raw("v1.2.3", false, &url, Some(digest.clone()))).unwrap();
        assert_eq!(
            (got.version.as_str(), got.url.as_str()),
            ("v1.2.3", url.as_str())
        );
        assert!(
            pick_release(raw("v1.2.3", true, &url, Some(digest.clone()))).is_err(),
            "prerelease"
        );
        assert!(
            pick_release(raw("v1.2.3", false, &url, None)).is_err(),
            "no digest"
        );
        assert!(
            pick_release(raw("v1.2.3", false, "https://evil/x", Some(digest.clone()))).is_err(),
            "host"
        );
        assert!(
            pick_release(raw("../x", false, &url, Some(digest))).is_err(),
            "tag"
        );
    }

    #[test]
    fn unpack_rejects_corrupt_oversized_and_foreign_binaries() {
        let paths = temp_paths("unpack");
        let dest = paths.root.join("mihomo");
        let good = gz(&macho(16));
        assert!(
            unpack(&good, &"0".repeat(64), &dest, 1024)
                .unwrap_err()
                .to_string()
                .contains("SHA-256")
        );
        let script = gz(b"#!/bin/sh\n");
        assert!(
            unpack(&script, &sha(&script), &dest, 1024)
                .unwrap_err()
                .to_string()
                .contains("arm64")
        );
        let big = gz(&macho(4096));
        assert!(
            unpack(&big, &sha(&big), &dest, 1024)
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
        let truncated = &good[..good.len() / 2];
        assert!(unpack(truncated, &sha(truncated), &dest, 1024).is_err());
        assert!(!dest.exists(), "nothing written from rejected input");
        unpack(&good, &sha(&good), &dest, 1024).unwrap();
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_stage_leaves_nothing_selectable() {
        let paths = temp_paths("stage");
        // A valid header that cannot execute: stops at the run check, like a blocked binary would.
        let bytes = gz(&macho(16));
        let release = Release {
            version: "v9.9.9".into(),
            url: String::new(),
            sha256: sha(&bytes),
        };
        assert!(stage(&paths, &release, &bytes).await.is_err());
        assert!(installed(&paths).is_empty());
        assert!(
            fs::read_dir(paths.bin()).unwrap().next().is_none(),
            "staging removed"
        );
    }

    #[test]
    fn prune_keeps_requested_versions_only() {
        let paths = temp_paths("prune");
        for v in ["v1.0.0", "v2.0.0", "v3.0.0"] {
            fs::create_dir_all(paths.bin().join(v)).unwrap();
            fs::write(paths.core_exe(v), "x").unwrap();
        }
        prune(&paths, &["v2.0.0", "v3.0.0"]);
        assert_eq!(installed(&paths), ["v2.0.0", "v3.0.0"]);
    }
}
