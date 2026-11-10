//! One actor on one Tokio thread owns the child, settings and system proxy, so lifecycle work is
//! serialized. The UI only sends commands and observes coalescing `watch` snapshots.
use crate::api::{Api, Connection, Group, LineSplitter, LogBuffer, LogLine, parse_log_line};
use crate::core::{self, Core};
use crate::profiles::{self, Profile};
use crate::proxy;
use crate::settings::{AppData, Paths, Settings, Theme, write_private};
use crate::update::{self, Release};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::fs;
use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const PORT_GRACE: Duration = Duration::from_secs(5);
const LOG_LINES: usize = 2000;
const LOG_BYTES: usize = 2 * 1024 * 1024;
const LOG_LINE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Phase {
    #[default]
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed(String),
}

/// Where a message is shown: next to the part of the UI it is about. There is no global banner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Area {
    Core,
    Proxy,
    Profiles,
    Files,
    Update,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub text: String,
    pub error: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub phase: Phase,
    pub core_version: Option<String>,
    pub previous_version: Option<String>,
    pub installed: Vec<String>,
    pub profiles: Vec<Profile>,
    pub active: Option<String>,
    /// The active profile or overrides changed on disk since activation; a reload is on offer.
    pub edited: bool,
    pub mode: String,
    pub groups: Vec<Group>,
    /// The system proxy is on, whoever set it.
    pub proxy_on: bool,
    /// A proxy write is running: `Some(true)` turning on, `Some(false)` turning off.
    pub proxy_busy: Option<bool>,
    /// At most one message per area; a later user action in that area replaces it.
    pub messages: BTreeMap<Area, Message>,
    pub icon_set: String,
    pub theme: Theme,
    pub mixed_port: u16,
    pub controller_port: u16,
    pub latest: Option<Release>,
    pub update_busy: Option<String>,
    pub config_dir: PathBuf,
}

impl Snapshot {
    pub fn message(&self, area: Area) -> Option<&Message> {
        self.messages.get(&area)
    }

    #[cfg(test)]
    pub fn error(&self) -> Option<String> {
        self.messages
            .values()
            .find(|m| m.error)
            .map(|m| m.text.clone())
    }
}

pub enum Cmd {
    Start,
    Stop,
    Activate(String),
    Reload,
    SetMode(String),
    Select {
        group: String,
        proxy: String,
    },
    Refresh,
    CloseConnection(String),
    CloseAll,
    SetProxy(bool),
    WatchConnections(bool),
    CheckUpdate,
    Download,
    ActivateVersion(String),
    DeleteVersion(String),
    SetIconSet(String),
    SetTheme(Theme),
    LinkProfile(PathBuf),
    RemoveProfile(String),
    /// The 30-second periodic work, now.
    CheckNow,
    Checked(Result<Release, String>),
    Staged(Result<String, String>),
    /// Restores the remembered core and proxy state; queued once at launch.
    Restore,
    /// Turns off a system proxy that points at us while no core of ours serves it.
    GuardProxy,
    Shutdown(std_mpsc::Sender<()>),
}

impl Cmd {
    fn area(&self) -> Area {
        match self {
            Cmd::SetProxy(_) | Cmd::GuardProxy => Area::Proxy,
            Cmd::Activate(_)
            | Cmd::Reload
            | Cmd::Refresh
            | Cmd::LinkProfile(_)
            | Cmd::RemoveProfile(_)
            | Cmd::CheckNow => Area::Profiles,
            Cmd::CheckUpdate
            | Cmd::Download
            | Cmd::ActivateVersion(_)
            | Cmd::DeleteVersion(_)
            | Cmd::Checked(_)
            | Cmd::Staged(_) => Area::Update,
            Cmd::SetIconSet(_) | Cmd::SetTheme(_) => Area::Files,
            _ => Area::Core,
        }
    }

    /// Background work must not wipe a message the user has not read yet.
    fn is_user_action(&self) -> bool {
        !matches!(
            self,
            Cmd::Refresh
                | Cmd::WatchConnections(_)
                | Cmd::Checked(_)
                | Cmd::Staged(_)
                | Cmd::Restore
                | Cmd::GuardProxy
                | Cmd::Shutdown(_)
        )
    }
}

/// The UI's side. Stop and quit also raise `cancel`, which interrupts a pending readiness wait.
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Cmd>,
    cancel: Arc<AtomicBool>,
    pub snapshot: watch::Receiver<Arc<Snapshot>>,
    pub connections: watch::Receiver<Arc<Vec<Connection>>>,
    pub log_generation: watch::Receiver<u64>,
    pub logs: Arc<Mutex<LogBuffer>>,
    /// Raised by SIGTERM/SIGINT so they take the same restore-and-reap path as Quit.
    pub quit_requested: watch::Receiver<bool>,
}

impl Handle {
    /// A handle with no backend: tests read the commands the UI sends.
    #[cfg(test)]
    pub fn detached(snapshot: Snapshot) -> (Self, mpsc::UnboundedReceiver<Cmd>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = Self {
            tx,
            cancel: Arc::new(AtomicBool::new(false)),
            snapshot: watch::channel(Arc::new(snapshot)).1,
            connections: watch::channel(Arc::new(Vec::new())).1,
            log_generation: watch::channel(0).1,
            logs: Arc::new(Mutex::new(LogBuffer::new(LOG_LINES, LOG_BYTES))),
            quit_requested: watch::channel(false).1,
        };
        (handle, rx)
    }

    pub fn send(&self, cmd: Cmd) {
        if matches!(cmd, Cmd::Stop) {
            self.cancel.store(true, Ordering::SeqCst);
        }
        let _ = self.tx.send(cmd);
    }

    /// Blocks the caller until the proxy is restored and the child reaped, or `timeout` passes.
    pub fn shutdown_blocking(&self, timeout: Duration) {
        self.cancel.store(true, Ordering::SeqCst);
        let (done, wait) = std_mpsc::channel();
        if self.tx.send(Cmd::Shutdown(done)).is_ok() {
            let _ = wait.recv_timeout(timeout);
        }
    }
}

pub fn spawn(paths: Paths) -> Result<Handle> {
    paths.ensure()?;
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let (actor, snapshot, connections, log_generation, logs) =
        Actor::new(paths, tx.clone(), cancel.clone())?;
    let (quit_tx, quit_requested) = watch::channel(false);
    std::thread::Builder::new()
        .name("xihomo-backend".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            tokio::task::LocalSet::new().block_on(&runtime, actor.run(rx, quit_tx));
        })?;
    Ok(Handle {
        tx,
        cancel,
        snapshot,
        connections,
        log_generation,
        logs,
        quit_requested,
    })
}

struct Actor {
    paths: Paths,
    settings: Settings,
    app: AppData,
    secret: String,
    api: Api,
    tx: mpsc::UnboundedSender<Cmd>,
    cancel: Arc<AtomicBool>,
    core: Option<Core>,
    fingerprint: Option<String>,
    snap: Snapshot,
    snap_tx: watch::Sender<Arc<Snapshot>>,
    conn_tx: watch::Sender<Arc<Vec<Connection>>>,
    log_tx: watch::Sender<u64>,
    logs: Arc<Mutex<LogBuffer>>,
    log_task: Option<JoinHandle<()>>,
    conn_task: Option<JoinHandle<()>>,
    watching_connections: bool,
    /// Profile names at the last reconciliation; `None` forces the next one.
    reconciled: Option<Vec<String>>,
    proxy: proxy::State,
    /// Services that got our proxy, so ones appearing later can be added.
    proxied: Vec<String>,
    read_proxy: fn(u16) -> proxy::State,
}

type Parts = (
    Actor,
    watch::Receiver<Arc<Snapshot>>,
    watch::Receiver<Arc<Vec<Connection>>>,
    watch::Receiver<u64>,
    Arc<Mutex<LogBuffer>>,
);

async fn wait_core(core: &mut Option<Core>) -> std::io::Result<std::process::ExitStatus> {
    match core {
        Some(core) => core.wait().await,
        None => pending().await,
    }
}

impl Actor {
    fn new(paths: Paths, tx: mpsc::UnboundedSender<Cmd>, cancel: Arc<AtomicBool>) -> Result<Parts> {
        let (settings, settings_notice) = Settings::load(&paths)?;
        let (app, app_notice) = AppData::load(&paths)?;
        let notices: Vec<String> = settings_notice.into_iter().chain(app_notice).collect();
        let mut messages = BTreeMap::new();
        if !notices.is_empty() {
            let text = notices.join("; ");
            messages.insert(Area::Files, Message { text, error: true });
        }
        let secret = paths.secret()?;
        let api = Api::new(settings.controller_port, &secret)?;
        let snap = Snapshot {
            phase: Phase::Stopped,
            core_version: app.core_version.clone(),
            previous_version: app.previous_core_version.clone(),
            installed: update::installed(&paths),
            profiles: profiles::list(&paths).unwrap_or_default(),
            active: app.active_profile.clone(),
            edited: false,
            mode: String::new(),
            groups: Vec::new(),
            proxy_on: false,
            proxy_busy: None,
            messages,
            icon_set: settings.icon_set.clone(),
            theme: settings.theme,
            mixed_port: settings.mixed_port,
            controller_port: settings.controller_port,
            latest: None,
            update_busy: None,
            config_dir: paths.config.clone(),
        };
        let (snap_tx, snap_rx) = watch::channel(Arc::new(snap.clone()));
        let (conn_tx, conn_rx) = watch::channel(Arc::new(Vec::new()));
        let (log_tx, log_rx) = watch::channel(0);
        let logs = Arc::new(Mutex::new(LogBuffer::new(LOG_LINES, LOG_BYTES)));
        let actor = Actor {
            paths,
            settings,
            app,
            secret,
            api,
            tx,
            cancel,
            core: None,
            fingerprint: None,
            snap,
            snap_tx,
            conn_tx,
            log_tx,
            logs: logs.clone(),
            log_task: None,
            conn_task: None,
            watching_connections: false,
            reconciled: None,
            proxy: proxy::State::default(),
            proxied: Vec::new(),
            // Tests must not depend on this Mac's proxy settings.
            read_proxy: if cfg!(test) {
                |_| proxy::State::default()
            } else {
                proxy::state
            },
        };
        Ok((actor, snap_rx, conn_rx, log_rx, logs))
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>, quit: watch::Sender<bool>) {
        self.startup().await;
        self.publish();
        let signal = |kind| tokio::signal::unix::signal(kind).expect("signal handler");
        let mut sigterm = signal(tokio::signal::unix::SignalKind::terminate());
        let mut sigint = signal(tokio::signal::unix::SignalKind::interrupt());
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.tick().await;
        loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Shutdown(done)) => {
                        self.shutdown().await;
                        let _ = done.send(());
                        return;
                    }
                    Some(cmd) => self.handle(cmd).await,
                    None => {
                        self.shutdown().await;
                        return;
                    }
                },
                status = wait_core(&mut self.core) => self.on_core_exit(status).await,
                _ = tick.tick() => self.periodic().await,
                _ = sigterm.recv() => { let _ = quit.send(true); }
                _ = sigint.recv() => { let _ = quit.send(true); }
            }
            self.publish();
        }
    }

    fn publish(&mut self) {
        self.snap.core_version = self.app.core_version.clone();
        self.snap.previous_version = self.app.previous_core_version.clone();
        self.snap.active = self.app.active_profile.clone();
        self.snap.icon_set = self.settings.icon_set.clone();
        self.snap.theme = self.settings.theme;
        self.snap.installed = update::installed(&self.paths);
        self.snap_tx.send_if_modified(|current| {
            let changed = **current != self.snap;
            if changed {
                *current = Arc::new(self.snap.clone());
            }
            changed
        });
    }

    /// Clean up after a previous xihomo that did not get to quit properly.
    async fn startup(&mut self) {
        match core::recover_orphan(&self.paths).await {
            Ok(true) => self.log(false, "Stopped a core left behind by a previous run"),
            Ok(false) => {}
            Err(e) => self.say(Area::Core, format!("{e:#}"), true),
        }
        if let Err(e) = profiles::ensure_default(&self.paths) {
            self.say(Area::Files, format!("{e:#}"), true);
        }
        self.refresh_profiles();
        self.read_proxy_state();
        // Queued, so the window shows the launch state before a core starts.
        let _ = self.tx.send(Cmd::Restore);
    }

    fn log(&mut self, error: bool, text: &str) {
        let mut logs = self.logs.lock().unwrap();
        logs.push(LogLine {
            level: if error { "error" } else { "info" }.into(),
            text: format!("xihomo: {text}").into(),
        });
        self.log_tx.send_replace(logs.generation);
    }

    /// Shown next to `area` and kept in Logs.
    fn say(&mut self, area: Area, text: String, error: bool) {
        self.log(error, &text);
        self.snap.messages.insert(area, Message { text, error });
    }

    /// Only user choices are remembered; crashes, failed starts and Quit leave them alone.
    fn remember(&mut self, core_on: bool, proxy_on: bool) -> Result<()> {
        self.app.core_on = core_on;
        self.app.proxy_on = proxy_on;
        self.app.save(&self.paths)
    }

    async fn handle(&mut self, cmd: Cmd) {
        let area = cmd.area();
        if cmd.is_user_action() {
            self.snap.messages.remove(&area);
        }
        let result = match cmd {
            Cmd::Start => match self.remember(true, self.app.proxy_on) {
                Ok(()) => self.start().await,
                Err(e) => Err(e),
            },
            Cmd::Stop => match self.remember(false, false) {
                Ok(()) => self.stop().await,
                Err(e) => Err(e),
            },
            Cmd::Activate(name) => self.activate(&name).await,
            Cmd::Reload => match self.app.active_profile.clone() {
                Some(name) => self.activate(&name).await,
                None => Err(anyhow!("Choose a profile first")),
            },
            Cmd::SetMode(mode) => self.set_mode(&mode).await,
            Cmd::Select { group, proxy } => self.select(&group, &proxy).await,
            Cmd::Refresh => {
                self.refresh_profiles();
                self.check_edits();
                self.read_proxy_state();
                if self.core.is_some() {
                    self.refresh_runtime().await
                } else {
                    Ok(())
                }
            }
            Cmd::CloseConnection(id) => self.api.close_connection(&id).await,
            Cmd::CloseAll => self.api.close_all().await,
            Cmd::SetProxy(on) => match self.remember(self.app.core_on, on) {
                Ok(()) => self.apply_proxy(on).await,
                Err(e) => Err(e),
            },
            Cmd::GuardProxy => self.guard_proxy().await,
            Cmd::Restore => self.restore().await,
            Cmd::WatchConnections(on) => {
                self.watching_connections = on;
                self.reconcile_connection_task();
                Ok(())
            }
            Cmd::CheckUpdate => self.check_update(),
            Cmd::Download => match self.snap.latest.clone() {
                Some(r) if update::installed(&self.paths).contains(&r.version) => {
                    self.activate_version(&r.version).await
                }
                _ => self.download(),
            },
            Cmd::ActivateVersion(version) => self.activate_version(&version).await,
            Cmd::DeleteVersion(version) => self.delete_version(&version),
            Cmd::SetIconSet(set) => {
                self.settings.icon_set = set;
                self.settings.save(&self.paths)
            }
            Cmd::SetTheme(theme) => {
                self.settings.theme = theme;
                self.settings.save(&self.paths)
            }
            Cmd::LinkProfile(path) => profiles::link(&self.paths, &path).map(|name| {
                self.log(false, &format!("Linked profile {name}"));
                self.refresh_profiles();
            }),
            Cmd::RemoveProfile(name) => self.remove_profile(&name),
            Cmd::CheckNow => {
                self.reconciled = None;
                self.periodic().await;
                Ok(())
            }
            Cmd::Checked(result) => self.on_checked(result),
            Cmd::Staged(result) => self.on_staged(result).await,
            Cmd::Shutdown(_) => unreachable!("handled by the run loop"),
        };
        if let Err(e) = result {
            let text = format!("{e:#}");
            // A failed start already shows its reason as the phase.
            if self.snap.phase == Phase::Failed(text.clone()) {
                self.log(true, &text);
            } else {
                self.say(area, text, true);
            }
        }
    }

    /// The remembered state at launch. With no profile chosen yet, the generated default is used.
    async fn restore(&mut self) -> Result<()> {
        if self.app.active_profile.is_none()
            && self.snap.profiles.iter().any(|p| p.name == "default")
        {
            self.app.active_profile = Some("default".to_owned());
        }
        let started = if self.app.core_on
            && self.app.core_version.is_some()
            && self.app.active_profile.is_some()
        {
            self.start().await
        } else {
            Ok(())
        };
        let guarded = self.guard_proxy().await;
        started.and(guarded)
    }

    fn exe(&self) -> Result<(PathBuf, String)> {
        let version = self
            .app
            .core_version
            .clone()
            .context("No Mihomo core installed yet: use Settings → Check for Update")?;
        Ok((self.paths.core_exe(&version), version))
    }

    /// Compose and test a candidate with the selected core. Nothing is activated or persisted.
    async fn prepare(&mut self, exe: &Path, name: &str) -> Result<profiles::Candidate> {
        let candidate = profiles::write_candidate(&self.paths, &self.settings, name, &self.secret)?;
        core::validate(
            exe,
            &self.paths,
            &candidate.home,
            &self.paths.candidate(),
            &self.secret,
        )
        .await
        .with_context(|| format!("profile {name}"))?;
        Ok(candidate)
    }

    /// The only place selection is persisted, and it runs after activation has succeeded.
    fn commit(&mut self, name: &str, candidate: profiles::Candidate) -> Result<()> {
        write_private(&self.paths.last_good(), &fs::read(self.paths.candidate())?)?;
        self.app.active_profile = Some(name.to_owned());
        self.app.last_good_home = Some(candidate.home);
        self.app.save(&self.paths)?;
        self.fingerprint = Some(candidate.fingerprint);
        self.snap.edited = false;
        if !candidate.notices.is_empty() {
            self.say(Area::Profiles, candidate.notices.join("; "), false);
        }
        Ok(())
    }

    async fn launch(&mut self, exe: &Path, home: &Path, file: &Path) -> Result<()> {
        core::ensure_ports_free(&[self.settings.mixed_port, self.settings.controller_port]).await?;
        let mut core = Core::spawn(exe, &self.paths, home, file)?;
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut api_ready = None;
        let ready = loop {
            if self.cancel.load(Ordering::SeqCst) {
                break Err(anyhow!("Start was cancelled"));
            }
            if let Some(status) = core.try_exited() {
                core.settle_output().await;
                break Err(anyhow!(
                    "Mihomo exited during startup ({status}): {}",
                    core.tail(6)
                ));
            }
            // Mihomo serves its API before binding listeners, so the port can lag a little.
            if self.api.version().await.is_ok() {
                let api_ready = *api_ready.get_or_insert_with(Instant::now);
                match self.confirm_ports().await {
                    Ok(()) => break Ok(()),
                    Err(e) if api_ready.elapsed() > PORT_GRACE => break Err(e),
                    Err(_) => {}
                }
            }
            if Instant::now() > deadline {
                break Err(anyhow!(
                    "Mihomo did not become ready in time: {}",
                    core.tail(6)
                ));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        };
        match ready {
            Ok(()) => {
                self.core = Some(core);
                self.spawn_log_task();
                self.reconcile_connection_task();
                self.snap.phase = Phase::Running;
                Ok(())
            }
            Err(e) => {
                core.stop().await?;
                Err(e)
            }
        }
    }

    async fn confirm_ports(&self) -> Result<()> {
        let (_, mixed) = self.api.runtime().await?;
        if mixed != self.settings.mixed_port {
            bail!(
                "Mihomo is not serving the mixed port {} (reports {mixed})",
                self.settings.mixed_port
            );
        }
        Ok(())
    }

    async fn start(&mut self) -> Result<()> {
        if self.core.is_some() {
            return Ok(());
        }
        self.cancel.store(false, Ordering::SeqCst);
        self.snap.phase = Phase::Starting;
        self.publish();
        let result = self.start_inner().await;
        if let Err(e) = &result {
            self.snap.phase = if self.cancel.load(Ordering::SeqCst) {
                Phase::Stopped
            } else {
                Phase::Failed(format!("{e:#}"))
            };
            return result;
        }
        self.refresh_runtime().await?;
        if self.app.proxy_on {
            self.apply_proxy(true).await?;
        }
        Ok(())
    }

    async fn start_inner(&mut self) -> Result<()> {
        let (exe, _) = self.exe()?;
        let name = self
            .app
            .active_profile
            .clone()
            .context("Choose a profile first")?;
        core::ensure_ports_free(&[self.settings.mixed_port, self.settings.controller_port]).await?;
        let primary = async {
            let candidate = self.prepare(&exe, &name).await?;
            let file = self.paths.candidate();
            self.launch(&exe, &candidate.home, &file).await?;
            Ok::<_, anyhow::Error>(candidate)
        }
        .await;
        let error = match primary {
            Ok(candidate) => return self.commit(&name, candidate),
            Err(e) => e,
        };
        if self.cancel.load(Ordering::SeqCst) {
            return Err(error);
        }
        // One recovery attempt only: no restart loop.
        let last_good = self.paths.last_good();
        let Some(home) = self
            .app
            .last_good_home
            .clone()
            .filter(|_| last_good.exists())
        else {
            return Err(error);
        };
        self.launch(&exe, &home, &last_good)
            .await
            .map_err(|recovery| {
                anyhow!("{error:#}; last-good recovery also failed: {recovery:#}")
            })?;
        self.fingerprint = None;
        self.snap.edited = true;
        self.say(
            Area::Core,
            format!("Running the last-good configuration because: {error:#}"),
            false,
        );
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if self.core.is_none() {
            self.snap.phase = Phase::Stopped;
            return Ok(());
        }
        self.snap.phase = Phase::Stopping;
        self.publish();
        let proxy_result = self.release_proxy().await;
        let stop_result = self.stop_child().await;
        self.snap.phase = Phase::Stopped;
        proxy_result.and(stop_result)
    }

    async fn stop_child(&mut self) -> Result<()> {
        self.abort_tasks();
        self.clear_runtime();
        match self.core.take() {
            Some(core) => core.stop().await,
            None => Ok(()),
        }
    }

    fn abort_tasks(&mut self) {
        for task in [self.log_task.take(), self.conn_task.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
        self.conn_tx.send_replace(Arc::new(Vec::new()));
    }

    fn clear_runtime(&mut self) {
        self.snap.mode.clear();
        self.snap.groups.clear();
        self.snap.edited = false;
        self.fingerprint = None;
    }

    async fn on_core_exit(&mut self, status: std::io::Result<std::process::ExitStatus>) {
        let Some(core) = self.core.take() else { return };
        core.settle_output().await;
        let tail = core.tail(6);
        core.forget_journal();
        self.abort_tasks();
        self.clear_runtime();
        let status = status.map_or_else(|e| e.to_string(), |s| s.to_string());
        self.snap.phase = Phase::Failed(format!("Mihomo exited unexpectedly ({status}): {tail}"));
        // Queued: the exit handler must not block the select loop on networksetup.
        self.read_proxy_state();
        if self.proxy.ours {
            let _ = self.tx.send(Cmd::GuardProxy);
        }
    }

    fn read_proxy_state(&mut self) {
        self.proxy = (self.read_proxy)(self.settings.mixed_port);
        self.snap.proxy_on = self.proxy.on;
    }

    /// Idempotent: on writes our proxy to every service, off clears every service.
    async fn apply_proxy(&mut self, on: bool) -> Result<()> {
        if on && self.core.is_none() {
            bail!("Start the core before enabling the system proxy");
        }
        self.snap.proxy_busy = Some(on);
        self.publish();
        let result = if on {
            match proxy::services() {
                Ok(services) => {
                    let result = proxy::enable(&services, self.settings.mixed_port).await;
                    self.proxied = services;
                    result
                }
                Err(e) => Err(e),
            }
        } else {
            proxy::disable().await
        };
        self.snap.proxy_busy = None;
        self.read_proxy_state();
        result
    }

    /// Stop and Quit leave another tool's proxy alone.
    async fn release_proxy(&mut self) -> Result<()> {
        self.read_proxy_state();
        if self.proxy.ours {
            self.apply_proxy(false).await?;
        }
        Ok(())
    }

    /// A proxy pointing at our port with no core of ours behind it blackholes the machine.
    async fn guard_proxy(&mut self) -> Result<()> {
        if self.core.is_none() {
            self.release_proxy().await?;
        }
        Ok(())
    }

    /// A network service that appeared while our proxy is on gets it too.
    async fn sync_new_services(&mut self) -> Result<()> {
        if !(self.app.proxy_on && self.core.is_some() && self.proxy.ours) {
            return Ok(());
        }
        let services = proxy::services()?;
        let new: Vec<String> = services
            .iter()
            .filter(|s| !self.proxied.contains(s))
            .cloned()
            .collect();
        if !new.is_empty() {
            proxy::enable(&new, self.settings.mixed_port).await?;
        }
        self.proxied = services;
        Ok(())
    }

    async fn activate(&mut self, name: &str) -> Result<()> {
        let (exe, _) = self.exe()?;
        let candidate = self.prepare(&exe, name).await?;
        let Some(running_home) = self.core.as_ref().map(|c| c.home.clone()) else {
            return self.commit(name, candidate);
        };
        if running_home == candidate.home {
            if let Err(e) = self.reload_running().await {
                let recovery = self.api.reload(&self.paths.last_good()).await;
                let _ = self.refresh_runtime().await;
                return Err(match recovery {
                    Ok(()) => e.context("reload failed; the last-good configuration was reloaded"),
                    Err(r) => e.context(format!(
                        "reload failed and last-good did not reload ({r:#})"
                    )),
                });
            }
            self.commit(name, candidate)?;
            return self.refresh_runtime().await;
        }
        // A different home is a process argument, so this needs a managed restart.
        self.read_proxy_state();
        let proxy_was_on = self.proxy.ours;
        self.release_proxy().await?;
        self.stop_child().await?;
        self.cancel.store(false, Ordering::SeqCst);
        let file = self.paths.candidate();
        let result = match self.launch(&exe, &candidate.home, &file).await {
            Ok(()) => self.commit(name, candidate),
            Err(e) => self.relaunch_last_good(&exe, e).await,
        };
        self.after_restart(proxy_was_on).await;
        result
    }

    async fn reload_running(&mut self) -> Result<()> {
        self.api.reload(&self.paths.candidate()).await?;
        self.confirm_ports().await
    }

    async fn relaunch_last_good(&mut self, exe: &Path, error: anyhow::Error) -> Result<()> {
        let home = self
            .app
            .last_good_home
            .clone()
            .context("no last-good home")?;
        let last_good = self.paths.last_good();
        match self.launch(exe, &home, &last_good).await {
            Ok(()) => Err(error.context("activation failed; running the last-good configuration")),
            Err(recovery) => {
                let message = format!("{error:#}; last-good recovery also failed: {recovery:#}");
                self.snap.phase = Phase::Failed(message.clone());
                Err(anyhow!(message))
            }
        }
    }

    async fn after_restart(&mut self, proxy_was_on: bool) {
        if self.core.is_none() {
            return;
        }
        let _ = self.refresh_runtime().await;
        if proxy_was_on && let Err(e) = self.apply_proxy(true).await {
            self.say(Area::Proxy, format!("{e:#}"), true);
        }
    }

    fn running_or_err(&self) -> Result<()> {
        if self.core.is_none() {
            bail!("The core is not running");
        }
        Ok(())
    }

    async fn set_mode(&mut self, mode: &str) -> Result<()> {
        self.running_or_err()?;
        self.api.set_mode(mode).await?;
        self.refresh_runtime().await
    }

    async fn select(&mut self, group: &str, proxy: &str) -> Result<()> {
        self.running_or_err()?;
        self.api.select(group, proxy).await?;
        self.refresh_runtime().await
    }

    async fn refresh_runtime(&mut self) -> Result<()> {
        let (mode, _) = self.api.runtime().await?;
        self.snap.mode = mode;
        self.snap.groups = self.api.groups().await?;
        Ok(())
    }

    /// False when `profiles/` could not be listed; the previous list stays on screen.
    fn refresh_profiles(&mut self) -> bool {
        match profiles::list(&self.paths) {
            Ok(list) => {
                self.snap.profiles = list;
                true
            }
            Err(e) => {
                self.say(Area::Profiles, format!("{e:#}"), true);
                false
            }
        }
    }

    /// Homes that must survive whatever the profile list says.
    fn protected_homes(&self) -> Vec<PathBuf> {
        let running = self.core.as_ref().map(|c| c.home.clone());
        running
            .into_iter()
            .chain(self.app.last_good_home.clone())
            .collect()
    }

    fn remove_profile(&mut self, name: &str) -> Result<()> {
        if self.app.active_profile.as_deref() == Some(name) {
            bail!("{name} is the active profile; switch to another one first");
        }
        let result = profiles::remove(&self.paths, name, &self.protected_homes());
        self.refresh_profiles();
        result?;
        self.log(false, &format!("Removed profile {name}"));
        Ok(())
    }

    /// Runs inside the actor, so it cannot interleave with an activation creating a home.
    fn reconcile(&mut self) {
        let names: Vec<String> = self.snap.profiles.iter().map(|p| p.name.clone()).collect();
        if self.reconciled.as_ref() == Some(&names) {
            return;
        }
        let versions: Vec<&str> = [
            self.app.core_version.as_deref(),
            self.app.previous_core_version.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        profiles::reconcile(
            &self.paths,
            &self.snap.profiles,
            &self.protected_homes(),
            &versions,
        );
        self.reconciled = Some(names);
    }

    /// Detect only. Applying is always the user's explicit Reload.
    fn check_edits(&mut self) {
        if let (Some(name), true) = (self.app.active_profile.clone(), self.core.is_some()) {
            let current = profiles::current_fingerprint(&self.paths, &name).ok();
            self.snap.edited = current != self.fingerprint;
        }
    }

    async fn periodic(&mut self) {
        let listed = self.refresh_profiles();
        self.check_edits();
        self.read_proxy_state();
        let proxy = match self.guard_proxy().await {
            Ok(()) => self.sync_new_services().await,
            Err(e) => Err(e),
        };
        if let Err(e) = proxy {
            self.say(Area::Proxy, format!("{e:#}"), true);
        }
        // Without a trustworthy list every home would look orphaned.
        if listed {
            self.reconcile();
        }
    }

    fn spawn_log_task(&mut self) {
        let (api, logs, log_tx) = (self.api.clone(), self.logs.clone(), self.log_tx.clone());
        self.log_task = Some(tokio::task::spawn_local(async move {
            loop {
                if let Ok(mut resp) = api.logs().await {
                    let mut splitter = LineSplitter::new(LOG_LINE_BYTES);
                    while let Ok(Some(chunk)) = resp.chunk().await {
                        let mut logs = logs.lock().unwrap();
                        splitter.feed(&chunk, |line| {
                            if let Some(line) = parse_log_line(line) {
                                logs.push(line);
                            }
                        });
                        log_tx.send_replace(logs.generation);
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }));
    }

    /// Polling only runs while Connections is on screen.
    fn reconcile_connection_task(&mut self) {
        if let Some(task) = self.conn_task.take() {
            task.abort();
        }
        if !self.watching_connections || self.core.is_none() {
            return;
        }
        let (api, tx) = (self.api.clone(), self.conn_tx.clone());
        self.conn_task = Some(tokio::task::spawn_local(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                if let Ok(mut list) = api.connections().await {
                    list.sort_by(|a, b| b.start.cmp(&a.start));
                    tx.send_replace(Arc::new(list));
                }
            }
        }));
    }

    fn download_route(&self) -> Option<u16> {
        self.core.is_some().then_some(self.settings.mixed_port)
    }

    fn check_update(&mut self) -> Result<()> {
        self.snap.update_busy = Some("Checking…".to_owned());
        let (tx, via) = (self.tx.clone(), self.download_route());
        tokio::task::spawn_local(async move {
            let result = update::check_latest(via)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Cmd::Checked(result));
        });
        Ok(())
    }

    fn on_checked(&mut self, result: Result<Release, String>) -> Result<()> {
        self.snap.update_busy = None;
        let release = result.map_err(|e| anyhow!("Update check failed: {e}"))?;
        if self.app.core_version.as_deref() == Some(&release.version) {
            self.say(Area::Update, "Up to date".to_owned(), false);
        }
        self.snap.latest = Some(release);
        Ok(())
    }

    fn download(&mut self) -> Result<()> {
        let release = self
            .snap
            .latest
            .clone()
            .context("Check for an update first")?;
        self.snap.update_busy = Some(format!("Downloading {}…", release.version));
        let (tx, via, paths) = (self.tx.clone(), self.download_route(), self.paths.clone());
        tokio::task::spawn_local(async move {
            let result = async {
                let bytes = update::download(&release, via).await?;
                update::stage(&paths, &release, &bytes).await?;
                Ok::<_, anyhow::Error>(release.version)
            }
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Cmd::Staged(result));
        });
        Ok(())
    }

    /// A downloaded core is used right away; the first one starts like a launch would.
    async fn on_staged(&mut self, result: Result<String, String>) -> Result<()> {
        self.snap.update_busy = None;
        let version = result.map_err(|e| anyhow!("Update failed: {e}"))?;
        self.log(false, &format!("Installed {version}"));
        if self.app.core_version.is_none() {
            self.app.core_version = Some(version);
            self.app.save(&self.paths)?;
            return self.restore().await;
        }
        self.snap.update_busy = Some(format!("Switching to {version}…"));
        self.publish();
        let result = self.activate_version(&version).await;
        self.snap.update_busy = None;
        result
    }

    fn delete_version(&mut self, version: &str) -> Result<()> {
        if self.app.core_version.as_deref() == Some(version) {
            bail!("{version} is in use");
        }
        if !update::valid_version(version) {
            bail!("{version} is not a core version");
        }
        fs::remove_dir_all(self.paths.bin().join(version))
            .with_context(|| format!("delete {version}"))?;
        if self.app.previous_core_version.as_deref() == Some(version) {
            self.app.previous_core_version = None;
            self.app.save(&self.paths)?;
        }
        self.log(false, &format!("Deleted core {version}"));
        Ok(())
    }

    /// Switch core versions, after an update or by choice. Settings change only after the new core
    /// runs, so an interruption at any point leaves the previous selection in place.
    async fn activate_version(&mut self, target: &str) -> Result<()> {
        if self.app.core_version.as_deref() == Some(target) {
            return Ok(());
        }
        let exe = self.paths.core_exe(target);
        if !update::valid_version(target) || !exe.is_file() {
            bail!("{target} is not installed");
        }
        let last_good = self.paths.last_good();
        if let Some(home) = self
            .app
            .last_good_home
            .clone()
            .filter(|_| last_good.exists())
        {
            core::validate(&exe, &self.paths, &home, &last_good, &self.secret)
                .await
                .with_context(|| format!("{target} cannot load the current configuration"))?;
        }
        if self.core.is_some() {
            let home = self
                .app
                .last_good_home
                .clone()
                .context("no last-good home")?;
            let (old_exe, old_version) = self.exe()?;
            self.read_proxy_state();
            let proxy_was_on = self.proxy.ours;
            self.release_proxy().await?;
            self.stop_child().await?;
            self.cancel.store(false, Ordering::SeqCst);
            if let Err(e) = self.launch(&exe, &home, &last_good).await {
                let result = match self.launch(&old_exe, &home, &last_good).await {
                    Ok(()) => {
                        Err(e.context(format!("{target} failed to start; back on {old_version}")))
                    }
                    Err(recovery) => {
                        let message = format!("{e:#}; {old_version} also failed: {recovery:#}");
                        self.snap.phase = Phase::Failed(message.clone());
                        Err(anyhow!(message))
                    }
                };
                self.after_restart(proxy_was_on).await;
                return result;
            }
            self.after_restart(proxy_was_on).await;
        }
        self.app.previous_core_version = self.app.core_version.replace(target.to_owned());
        self.app.save(&self.paths)?;
        let previous = self.app.previous_core_version.clone().unwrap_or_default();
        update::prune(&self.paths, &[target, &previous]);
        self.log(false, &format!("Now using {target}"));
        Ok(())
    }

    async fn shutdown(&mut self) {
        if let Err(e) = self.release_proxy().await {
            // The next launch's proxy guard retries.
            eprintln!("xihomo: restoring proxy settings failed: {e:#}");
        }
        if let Err(e) = self.stop_child().await {
            eprintln!("xihomo: stopping the core failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests;
