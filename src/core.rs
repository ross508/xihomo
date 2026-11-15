use crate::api::LineSplitter;
use crate::settings::{Paths, write_private};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::ffi::{CStr, OsString};
use std::fs;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::MetadataExt;
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

/// Something accepts connections on the loopback port, as the system proxy would connect.
pub fn listening(port: u16) -> bool {
    let addr = (Ipv4Addr::LOCALHOST, port).into();
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// Connections a stopped root core closed stay in TIME_WAIT for up to 30 s, and macOS refuses the
/// user's bind over a root one meanwhile, although nothing listens. A root core binds anyway.
const CLOSING_WAIT: Duration = Duration::from_secs(36);

/// Only touch the ports through a bind attempt, so an unrelated listener is refused, never signalled.
/// Binds wildcard too: on BSD sockets a specific bind can succeed alongside a wildcard listener.
/// A brief retry rides out a probe socket held for an instant by a concurrently forked child.
/// A port held only by closing connections is free for a core that runs as root (`as_root`);
/// for one that runs as the user, it is waited for.
pub async fn ensure_ports_free(ports: &[u16], as_root: bool) -> Result<()> {
    for &port in ports {
        for addr in [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED] {
            let start = std::time::Instant::now();
            let mut attempt = 0;
            while let Err(e) = TcpListener::bind((addr, port)) {
                let closing = !listening(port);
                if closing && as_root {
                    break;
                }
                attempt += 1;
                if attempt >= 6 && !(closing && start.elapsed() < CLOSING_WAIT) {
                    bail!(
                        "port {port} is in use by another process ({e}), so xihomo will not control it"
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
pub fn rejection_reason(output: &str) -> String {
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
    errors.join(". ")
}

/// The core file runs as root wherever it is started: owned by root, with the setuid bit.
pub fn authorized(exe: &Path) -> bool {
    fs::metadata(exe).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o4000 != 0)
}

/// The path goes in as an argument, so AppleScript needs no escaping, and `quoted form of`
/// quotes it for the shell.
const AUTHORIZE: [&str; 4] = [
    "on run argv",
    "set p to quoted form of item 1 of argv",
    "do shell script \"/usr/sbin/chown root:wheel \" & p & \" && /bin/chmod 4755 \" & p with prompt \"xihomo needs administrator rights so Mihomo can turn on TUN.\" with administrator privileges",
    "end run",
];

fn authorize_args(exe: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = AUTHORIZE
        .iter()
        .flat_map(|line| ["-e".into(), (*line).into()])
        .collect();
    args.push(exe.into());
    args
}

/// The only place xihomo asks for administrator rights: makes `exe` setuid root, once per core
/// file. Blocking while the dialog is open. `false` when the user cancelled.
pub fn authorize(exe: &Path) -> Result<bool> {
    // Layer 1 tests must never ask for a password or change an owner.
    if cfg!(test) {
        panic!("test tried to authorize {}", exe.display());
    }
    let output = std::process::Command::new("/usr/bin/osascript")
        .args(authorize_args(exe))
        .output()
        .context("cannot run osascript")?;
    if output.status.success() {
        return Ok(true);
    }
    let error = String::from_utf8_lossy(&output.stderr);
    if error.contains("(-128)") {
        return Ok(false);
    }
    bail!("Authorization failed: {}", error.trim())
}

fn acl_entry(user: &str) -> String {
    format!(
        "user:{user} allow list,search,add_file,add_subdirectory,delete,delete_child,read,write,append,readattr,writeattr,readextattr,writeextattr,readsecurity,file_inherit,directory_inherit"
    )
}

fn user_name() -> Result<String> {
    // Safety: the entry is copied out at once; nothing else in xihomo calls getpwuid.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() {
            bail!("cannot read this user's name");
        }
        Ok(CStr::from_ptr((*entry).pw_name).to_str()?.to_owned())
    }
}

/// What a root core creates under `homes` inherits an entry that lets the user read, write and
/// delete it. Added to what exists too, since homes made earlier inherit nothing from `homes`.
/// Run as the user; adding the same entry again changes nothing.
pub fn share_homes(homes: &Path) -> Result<()> {
    let output = std::process::Command::new("/bin/chmod")
        .args(["-R", "+a"])
        .arg(acl_entry(&user_name()?))
        .arg(homes)
        .output()
        .context("cannot run chmod")?;
    if !output.status.success() {
        bail!(
            "cannot share {} with this user: {}",
            homes.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// A `cache.db` the user cannot write, left by a root core, makes a core running as the user run
/// without a cache and forget every selection. Opening for append neither creates nor truncates
/// it, and deleting it needs only the home, which is the user's. `true` when it was deleted.
pub fn discard_unwritable_cache(home: &Path) -> Result<bool> {
    let path = home.join("cache.db");
    match fs::OpenOptions::new().append(true).open(&path) {
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            fs::remove_file(&path).with_context(|| format!("delete {}", path.display()))?;
            Ok(true)
        }
        _ => Ok(false),
    }
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

    #[test]
    fn only_a_setuid_root_file_is_authorized() {
        let paths = temp_paths("authorized");
        let exe = paths.data.join("mihomo");
        fs::write(&exe, "x").unwrap();
        let mode = |m| fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(m));
        mode(0o4755).unwrap();
        assert!(!authorized(&exe), "setuid, but owned by this user");
        mode(0o755).unwrap();
        assert!(!authorized(&exe));
        assert!(!authorized(&paths.data.join("missing")));
        // Root-owned without the setuid bit; root-owned with it would be authorized.
        assert!(!authorized(Path::new("/bin/ls")));
    }

    #[test]
    fn the_authorization_passes_the_path_as_an_argument() {
        let exe = Path::new("/Users/a b/it's \"x\"/v1/mihomo");
        let args = authorize_args(exe);
        assert_eq!(args.last().unwrap(), exe.as_os_str(), "passed unchanged");
        let script: Vec<&str> = args[..args.len() - 1]
            .chunks(2)
            .map(|pair| {
                assert_eq!(pair[0], "-e");
                pair[1].to_str().unwrap()
            })
            .collect();
        assert_eq!(script, AUTHORIZE);
        assert!(script[1].contains("quoted form of item 1 of argv"));
        assert!(script[2].starts_with(
            "do shell script \"/usr/sbin/chown root:wheel \" & p & \" && /bin/chmod 4755 \" & p "
        ));
        assert!(script[2].ends_with("with administrator privileges"));
        assert!(
            !script.concat().contains("it's"),
            "the path is not in the script"
        );
    }

    #[test]
    fn homes_pass_their_acl_on_to_what_is_created_later() {
        let paths = temp_paths("acl");
        let earlier = paths.home("earlier");
        fs::create_dir_all(&earlier).unwrap();
        share_homes(&paths.homes()).unwrap();
        share_homes(&paths.homes()).unwrap();
        fs::create_dir_all(earlier.join("providers")).unwrap();
        fs::write(earlier.join("providers/p.yaml"), "x").unwrap();
        let acl = |path: &Path| {
            let out = std::process::Command::new("/bin/ls")
                .arg("-led")
                .arg(path)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        let user = user_name().unwrap();
        let own = acl(&paths.homes());
        assert_eq!(
            own.matches(&format!("user:{user} allow")).count(),
            1,
            "{own}"
        );
        for path in [earlier.join("providers"), earlier.join("providers/p.yaml")] {
            let text = acl(&path);
            assert!(
                text.contains(&format!("user:{user} inherited allow")),
                "{text}"
            );
        }
        assert!(acl(&earlier).contains(&format!("user:{user} allow")));
    }

    #[test]
    fn only_an_unwritable_cache_is_discarded() {
        let paths = temp_paths("cache");
        let home = paths.home("p");
        fs::create_dir_all(&home).unwrap();
        let cache = home.join("cache.db");
        assert!(!discard_unwritable_cache(&home).unwrap(), "missing");
        assert!(!cache.exists(), "not created");
        fs::write(&cache, "selections").unwrap();
        assert!(!discard_unwritable_cache(&home).unwrap());
        assert_eq!(
            fs::read_to_string(&cache).unwrap(),
            "selections",
            "kept whole"
        );
        fs::set_permissions(&cache, std::os::unix::fs::PermissionsExt::from_mode(0o444)).unwrap();
        assert!(discard_unwritable_cache(&home).unwrap());
        assert!(!cache.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn occupied_port_is_refused_and_a_briefly_held_one_is_not() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let err = ensure_ports_free(&[port], true)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("in use"), "{err}");

        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(listener);
        });
        ensure_ports_free(&[port], false).await.unwrap();
        release.join().unwrap();
    }

    /// A socket bound to the port without listening: binds fail, connections are refused, as
    /// with the TIME_WAIT a root core leaves behind.
    fn held(port: u16) -> i32 {
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            let mut addr: libc::sockaddr_in = std::mem::zeroed();
            addr.sin_family = libc::AF_INET as _;
            addr.sin_port = port.to_be();
            addr.sin_addr.s_addr = u32::from(Ipv4Addr::LOCALHOST).to_be();
            let len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            assert_eq!(libc::bind(fd, (&raw const addr).cast(), len), 0);
            fd
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_port_held_without_a_listener_is_free_for_root_and_waited_for_otherwise() {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let fd = held(port);
        assert!(!listening(port));
        ensure_ports_free(&[port], true).await.unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            unsafe { libc::close(fd) };
        });
        let start = std::time::Instant::now();
        ensure_ports_free(&[port], false).await.unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(700),
            "waited, past the brief retry"
        );
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
