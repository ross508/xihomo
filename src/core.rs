use crate::api::LineSplitter;
use crate::settings::{Paths, write_private};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

const VALIDATE_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const TAIL_LINES: usize = 40;
const TAIL_LINE_BYTES: usize = 4096;

/// Only touch the ports through a bind attempt, so an unrelated listener is refused, never signalled.
/// Binds wildcard too: on BSD sockets a specific bind can succeed alongside a wildcard listener.
/// A brief retry rides out a probe socket held for an instant by a concurrently forked child.
pub async fn ensure_ports_free(ports: &[u16]) -> Result<()> {
    for &port in ports {
        for addr in [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED] {
            let mut attempt = 0;
            while let Err(e) = TcpListener::bind((addr, port)) {
                attempt += 1;
                if attempt == 6 {
                    bail!(
                        "port {port} is in use by another process ({e}); xihomo will not control it"
                    );
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
    }
    Ok(())
}

fn command(exe: &Path, paths: &Paths) -> Command {
    let mut cmd = Command::new(exe);
    // Reloads pass a path under runtime/; Mihomo rejects paths outside its home or SAFE_PATHS.
    cmd.env("SAFE_PATHS", paths.runtime())
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

/// `mihomo -t`: checks the generated file with the selected core. It may read resources from home.
pub async fn validate(
    exe: &Path,
    paths: &Paths,
    home: &Path,
    file: &Path,
    secret: &str,
) -> Result<()> {
    let output = command(exe, paths)
        .args(["-t", "-d"])
        .arg(home)
        .arg("-f")
        .arg(file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let output = tokio::time::timeout(VALIDATE_TIMEOUT, output)
        .await
        .context("configuration test timed out")?
        .with_context(|| format!("cannot run {}", exe.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&output.stdout) + String::from_utf8_lossy(&output.stderr);
    bail!(
        "Mihomo rejected the configuration: {}",
        rejection_reason(&text).replace(secret, "***")
    )
}

/// Mihomo's own error lines, without logfmt noise; the last lines if it printed none.
fn rejection_reason(output: &str) -> String {
    let errors: Vec<&str> = output
        .lines()
        .filter(|l| l.contains("level=error") || l.contains("level=fatal"))
        .map(|l| {
            l.split_once("msg=")
                .map_or(l, |(_, msg)| msg.trim_matches('"'))
        })
        .collect();
    if errors.is_empty() {
        let lines: Vec<&str> = output.lines().collect();
        return lines[lines.len().saturating_sub(3)..].join(" ");
    }
    errors.join("; ")
}

#[derive(Serialize, Deserialize)]
struct Journal {
    pid: i32,
    exe: PathBuf,
}

fn journal_path(paths: &Paths) -> PathBuf {
    paths.state().join("core.json")
}

fn process_path(pid: i32) -> Option<PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    buf.truncate(len as usize);
    String::from_utf8(buf).ok().map(PathBuf::from)
}

/// After a crash of xihomo, stop the core it left behind. The executable path check keeps a reused
/// PID from being signalled, so only a process running our own binary can ever be touched.
pub async fn recover_orphan(paths: &Paths) -> Result<bool> {
    let path = journal_path(paths);
    let Ok(bytes) = fs::read(&path) else {
        return Ok(false);
    };
    let journal: Journal = serde_json::from_slice(&bytes).context("parse core journal")?;
    let same = process_path(journal.pid)
        .zip(fs::canonicalize(&journal.exe).ok())
        .is_some_and(|(running, exe)| fs::canonicalize(running).is_ok_and(|r| r == exe));
    if same {
        unsafe { libc::kill(journal.pid, libc::SIGTERM) };
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if process_path(journal.pid).is_none() {
                break;
            }
        }
        if process_path(journal.pid).is_some() {
            unsafe { libc::kill(journal.pid, libc::SIGKILL) };
        }
    }
    fs::remove_file(&path)?;
    Ok(same)
}

/// The owned child. Dropping it kills the process (`kill_on_drop`).
pub struct Core {
    child: Child,
    pid: i32,
    pub home: PathBuf,
    tail: Arc<Mutex<VecDeque<String>>>,
    journal: PathBuf,
}

/// Reads raw bytes so neither invalid UTF-8 nor a huge line can stop draining and block the child.
fn drain(mut stream: impl AsyncRead + Unpin + 'static, tail: Arc<Mutex<VecDeque<String>>>) {
    tokio::task::spawn_local(async move {
        let mut splitter = LineSplitter::new(TAIL_LINE_BYTES);
        let mut buf = [0u8; 8192];
        while let Ok(n @ 1..) = stream.read(&mut buf).await {
            let mut tail = tail.lock().unwrap();
            splitter.feed(&buf[..n], |line| {
                if tail.len() == TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(String::from_utf8_lossy(line).into_owned());
            });
        }
    });
}

impl Core {
    pub fn spawn(exe: &Path, paths: &Paths, home: &Path, file: &Path) -> Result<Self> {
        fs::create_dir_all(home)
            .with_context(|| format!("create Mihomo home {}", home.display()))?;
        let mut child = command(exe, paths)
            .arg("-d")
            .arg(home)
            .arg("-f")
            .arg(file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .with_context(|| format!("cannot start {}", exe.display()))?;
        let pid = child.id().context("core exited immediately")? as i32;
        let journal = journal_path(paths);
        write_private(
            &journal,
            &serde_json::to_vec(&Journal {
                pid,
                exe: exe.to_owned(),
            })?,
        )?;
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        drain(child.stdout.take().context("core stdout")?, tail.clone());
        drain(child.stderr.take().context("core stderr")?, tail.clone());
        Ok(Self {
            child,
            pid,
            home: home.to_owned(),
            tail,
            journal,
        })
    }

    pub fn try_exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait().await
    }

    /// The drain tasks run concurrently; give them a moment to read what a dead child last wrote.
    pub async fn settle_output(&self) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Recent core output, used to explain startup failures and unexpected exits.
    pub fn tail(&self, lines: usize) -> String {
        let tail = self.tail.lock().unwrap();
        let skip = tail.len().saturating_sub(lines);
        tail.iter()
            .skip(skip)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Graceful signal to our own PID, then kill and reap. Never by process name.
    pub async fn stop(mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            unsafe { libc::kill(self.pid, libc::SIGTERM) };
            if tokio::time::timeout(STOP_TIMEOUT, self.child.wait())
                .await
                .is_err()
            {
                self.child.kill().await.context("kill core")?;
            }
        }
        let _ = fs::remove_file(&self.journal);
        Ok(())
    }

    #[cfg(test)]
    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn forget_journal(&self) {
        let _ = fs::remove_file(&self.journal);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::settings::temp_paths;

    #[test]
    fn rejection_reason_keeps_only_mihomo_errors() {
        let out = "time=\"1\" level=info msg=\"Start\"\ntime=\"2\" level=error msg=\"proxy group[0]: bad\"\nconfiguration file x test failed\n";
        assert_eq!(rejection_reason(out), "proxy group[0]: bad");
        assert_eq!(rejection_reason("a\nb\nc\nd\n"), "b c d");
        assert_eq!(rejection_reason(""), "");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn occupied_port_is_refused_and_a_briefly_held_one_is_not() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let err = ensure_ports_free(&[port]).await.unwrap_err().to_string();
        assert!(err.contains("in use"), "{err}");

        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(listener);
        });
        ensure_ports_free(&[port]).await.unwrap();
        release.join().unwrap();
    }

    fn reap_in_background(mut child: std::process::Child) -> i32 {
        let pid = child.id() as i32;
        std::thread::spawn(move || child.wait());
        pid
    }

    fn write_journal(paths: &Paths, pid: i32, exe: &str) {
        fs::write(
            journal_path(paths),
            serde_json::to_vec(&Journal {
                pid,
                exe: exe.into(),
            })
            .unwrap(),
        )
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn orphan_recovery_only_signals_our_executable() {
        let paths = temp_paths("orphan");
        let sleep = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = reap_in_background(sleep);

        // A reused PID: same number, different executable. It must survive.
        write_journal(&paths, pid, "/bin/cat");
        assert!(!recover_orphan(&paths).await.unwrap());
        assert!(process_path(pid).is_some());
        assert!(!journal_path(&paths).exists());

        write_journal(&paths, pid, "/bin/sleep");
        assert!(recover_orphan(&paths).await.unwrap());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(process_path(pid).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stop_reaps_a_child_that_ignores_sigterm() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let paths = temp_paths("stop");
                let exe = paths.data.join("stubborn");
                write_private(
                    &exe,
                    b"#!/bin/sh\ntrap '' TERM\necho hello\nwhile :; do sleep 0.1; done\n",
                )
                .unwrap();
                fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                    .unwrap();
                let core = Core::spawn(&exe, &paths, &paths.data, &paths.candidate()).unwrap();
                let pid = core.pid;
                assert!(journal_path(&paths).exists());
                core.stop().await.unwrap();
                assert!(process_path(pid).is_none(), "killed and reaped");
                assert!(!journal_path(&paths).exists());
            })
            .await;
    }
}
