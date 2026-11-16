//! One actor on one Tokio thread owns the child, settings and system proxy, so lifecycle work is
//! serialized. The UI only sends commands and observes coalescing `watch` snapshots.
use crate::api::{
    Api, Connection, Group, Latency, LineSplitter, LogBuffer, LogLine, Node, parse_log_line,
};
use crate::core::{self, Core};
use crate::profiles::{self, Profile};
use crate::proxy;
use crate::settings::{self, AppData, LogLevel, Paths, Settings, Theme, TunStack, write_private};
use crate::update::{self, Release};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::pending;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const PORT_GRACE: Duration = Duration::from_secs(5);
const PROXY_SETTLE: Duration = Duration::from_secs(2);
/// How long a start or reload may take to bring the TUN up.
const TUN_SETTLE: Duration = Duration::from_secs(5);
const LOG_LINES: usize = 2000;
const LOG_BYTES: usize = 2 * 1024 * 1024;
const LOG_LINE_BYTES: usize = 64 * 1024;
const MODES: [&str; 3] = ["rule", "global", "direct"];
/// Built-in outbounds that are never a useful default for GLOBAL.
const BUILTIN: [&str; 5] = ["DIRECT", "REJECT", "REJECT-DROP", "PASS", "COMPATIBLE"];
/// Adapter kinds a latency test means nothing for.
const UNTESTED: [&str; 7] = [
    "Reject",
    "RejectDrop",
    "Pass",
    "PassRule",
    "Compatible",
    "Dns",
    "Rematch",
];
/// Latency tests in flight at once.
const TEST_LIMIT: usize = 32;

/// What a card for `member` stands for: `now` followed through groups to a node. A group without
/// `now`, a cycle or a name in no list stops the walk there.
pub fn resolve(member: &str, groups: &[Group], nodes: &BTreeMap<String, Node>) -> String {
    let mut seen = vec![member];
    while let Some(group) = groups.iter().find(|g| g.name == *seen.last().unwrap()) {
        let next = group.now.as_str();
        if next.is_empty() || seen.contains(&next) || !nodes.contains_key(next) {
            break;
        }
        seen.push(next);
    }
    seen.last().unwrap().to_string()
}

/// Mihomo lists a connection's chain from the node outward, so the entry before a group is the
/// member it chose.
fn routed_elsewhere(chains: &[String], group: &str, proxy: &str) -> bool {
    chains
        .iter()
        .position(|name| name == group)
        .is_some_and(|ix| ix > 0 && chains[ix - 1] != proxy)
}

/// What GLOBAL should select instead of DIRECT: its first member that is a proxy group, else
/// its first member that is not built in. `None` leaves it alone.
pub fn global_default(groups: &[Group]) -> Option<String> {
    let global = groups.iter().find(|g| g.name == "GLOBAL")?;
    if global.now != "DIRECT" {
        return None;
    }
    let is_group = |m: &String| m != "GLOBAL" && groups.iter().any(|g| &g.name == m);
    global
        .all
        .iter()
        .find(|m| is_group(m))
        .or_else(|| global.all.iter().find(|m| !BUILTIN.contains(&m.as_str())))
        .cloned()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Phase {
    #[default]
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed(String),
}

/// A TUN switch in progress.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TunBusy {
    /// The administrator dialog is open.
    Authorizing,
    TurningOn,
    TurningOff,
}

/// Where a message is shown: next to the part of the UI it is about. There is no global banner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Area {
    Core,
    /// The system proxy, in Control.
    Proxy,
    Profiles,
    /// Control → Groups: picking a member.
    Groups,
    /// Settings → PROXY: the port and who may use it.
    Listen,
    /// Settings → API: the controller port and the secret.
    Api,
    Files,
    Update,
}

/// What a message is about. A section shows one message per topic; a new one replaces only the
/// message of its own topic, and a user command clears only its own topic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Topic {
    Core,
    /// Running the last-good configuration instead of the chosen profile.
    LastGood,
    SystemProxy,
    /// Why TUN is not running although `tun_on` asks for it.
    Tun,
    /// The 30-second proxy guard and new-service sync.
    ProxyCheck,
    Profile,
    Providers,
    ProfileList,
    Groups,
    Icon,
    Theme,
    DockIcon,
    /// `settings.yaml` or `app.json` was set aside.
    SettingsFile,
    ProxyPort,
    /// The proxy port another tool set.
    ExternalPort,
    Lan,
    ApiPort,
    Secret,
    CoreUpdate,
    CoreVersion,
    CoreReplaced,
    LogLevel,
    TunStack,
    LatencyUrl,
}

impl Topic {
    pub fn area(self) -> Area {
        match self {
            Self::Core | Self::LastGood | Self::Tun => Area::Core,
            Self::SystemProxy | Self::ProxyCheck => Area::Proxy,
            Self::Profile | Self::Providers | Self::ProfileList => Area::Profiles,
            Self::Groups => Area::Groups,
            Self::Icon | Self::Theme | Self::DockIcon | Self::SettingsFile => Area::Files,
            Self::ProxyPort | Self::ExternalPort | Self::Lan => Area::Listen,
            Self::ApiPort | Self::Secret => Area::Api,
            Self::CoreUpdate
            | Self::CoreVersion
            | Self::CoreReplaced
            | Self::LogLevel
            | Self::TunStack
            | Self::LatencyUrl => Area::Update,
        }
    }

    /// The topic of the control that edits a `settings.yaml` key.
    fn of_setting(key: &str) -> Self {
        match key {
            "mixed_port" => Self::ProxyPort,
            "controller_port" => Self::ApiPort,
            "allow_lan" => Self::Lan,
            "icon_set" => Self::Icon,
            "theme" => Self::Theme,
            "keep_dock_icon" => Self::DockIcon,
            "log_level" => Self::LogLevel,
            "tun_stack" => Self::TunStack,
            "latency_url" => Self::LatencyUrl,
            _ => Self::SettingsFile,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub text: String,
    pub error: bool,
    /// Unique per message, so a repeated text still counts as new.
    pub id: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub phase: Phase,
    pub core_version: Option<String>,
    pub previous_version: Option<String>,
    /// Every version that resolves: the downloads and the built-in one.
    pub installed: Vec<String>,
    /// The app bundle's own core version, which cannot be deleted.
    pub builtin: Option<String>,
    pub profiles: Vec<Profile>,
    pub active: Option<String>,
    /// The active profile or overrides changed on disk since activation; a reload is on offer.
    pub edited: bool,
    pub mode: String,
    pub groups: Vec<Group>,
    /// The system proxy is on, whoever set it.
    pub proxy_on: bool,
    /// The backend is handling a user command. Others are refused meanwhile, except Stop; the
    /// specific busy states below only say what is happening.
    pub busy: bool,
    /// A proxy write is running: `Some(true)` turning on, `Some(false)` turning off.
    pub proxy_busy: Option<bool>,
    /// The running core's TUN interface, as `/configs` reports it.
    pub tun_device: Option<String>,
    pub tun_busy: Option<TunBusy>,
    /// TUN is asked for, but the selected core file is not authorized to run as root.
    pub tun_needs_auth: bool,
    pub tun_stack: TunStack,
    /// With LAN access on: the running profile's DNS port, while another process holds it.
    pub dns_port_taken: Option<u16>,
    /// At most one message per topic; a later user command of that topic clears it.
    pub messages: BTreeMap<Topic, Message>,
    pub icon_set: String,
    pub theme: Theme,
    pub keep_dock_icon: bool,
    pub log_level: LogLevel,
    pub mixed_port: u16,
    pub controller_port: u16,
    pub allow_lan: bool,
    pub secret: String,
    /// The primary interface's IPv4 address, for sharing the proxy.
    pub lan_ip: Option<Ipv4Addr>,
    /// A PROXY or API change is being applied to the running core.
    pub applying: Option<Area>,
    pub latest: Option<Release>,
    pub update_busy: Option<String>,
    /// The running profile's providers are being updated.
    pub providers_busy: bool,
    pub config_dir: PathBuf,
    pub latency_url: String,
    /// Every entry of `/proxies` and the providers' nodes, with their latency for `latency_url`.
    pub nodes: BTreeMap<String, Node>,
    /// Nodes being tested.
    pub testing: BTreeSet<String>,
    /// The group whose test runs; only one at a time.
    pub testing_group: Option<String>,
}

impl Snapshot {
    pub fn message(&self, topic: Topic) -> Option<&Message> {
        self.messages.get(&topic)
    }

    /// The messages shown beside `area`, newest first.
    pub fn messages_in(&self, area: Area) -> Vec<&Message> {
        let mut list: Vec<&Message> = self
            .messages
            .iter()
            .filter(|(topic, _)| topic.area() == area)
            .map(|(_, m)| m)
            .collect();
        list.sort_by_key(|m| std::cmp::Reverse(m.id));
        list
    }

    /// The node a card for `member` shows and tests; `None` when it is never tested.
    pub fn target(&self, member: &str) -> Option<String> {
        let node = resolve(member, &self.groups, &self.nodes);
        let kind = &self.nodes.get(&node)?.kind;
        (!UNTESTED.contains(&kind.as_str())).then_some(node)
    }

    /// The members a group shows (GLOBAL without REJECT), as their cards' nodes, each once.
    pub fn group_targets(&self, group: &str) -> Vec<String> {
        let Some(group) = self.groups.iter().find(|g| g.name == group) else {
            return Vec::new();
        };
        let mut targets = Vec::new();
        for member in group.shown() {
            if let Some(node) = self.target(member)
                && !targets.contains(&node)
            {
                targets.push(node);
            }
        }
        targets
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
    SetTun(bool),
    SetTunStack(TunStack),
    WatchConnections(bool),
    CheckUpdate,
    Download,
    ActivateVersion(String),
    DeleteVersion(String),
    SetIconSet(String),
    SetTheme(Theme),
    SetKeepDockIcon(bool),
    SetLogLevel(LogLevel),
    /// As typed; empty means the default.
    SetLatencyUrl(String),
    /// Tests the node a member's card stands for.
    TestNode(String),
    TestGroup(String),
    Tested {
        epoch: u64,
        node: String,
        latency: Latency,
    },
    /// Ports arrive as typed, up to five digits; the backend validates them.
    SetProxyPort(u32),
    SetAllowLan(bool),
    SetApiPort(u32),
    RegenerateSecret,
    LinkProfile(PathBuf),
    ImportProfile(PathBuf),
    RemoveProfile(String),
    /// Checks the running profile for edits and updates its providers.
    UpdateProviders,
    ProvidersUpdated(Result<(usize, Vec<String>), String>),
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
    fn topic(&self) -> Topic {
        match self {
            Cmd::SetProxy(_) | Cmd::GuardProxy => Topic::SystemProxy,
            Cmd::SetTun(_) => Topic::Tun,
            Cmd::SetTunStack(_) => Topic::TunStack,
            Cmd::Select { .. } | Cmd::TestNode(_) | Cmd::TestGroup(_) | Cmd::Tested { .. } => {
                Topic::Groups
            }
            Cmd::Activate(_)
            | Cmd::Reload
            | Cmd::Refresh
            | Cmd::LinkProfile(_)
            | Cmd::ImportProfile(_)
            | Cmd::RemoveProfile(_) => Topic::Profile,
            Cmd::UpdateProviders | Cmd::ProvidersUpdated(_) => Topic::Providers,
            // The checks it runs clear their own topics; the listing is the one it repeats.
            Cmd::CheckNow => Topic::ProfileList,
            Cmd::CheckUpdate | Cmd::Download | Cmd::Checked(_) | Cmd::Staged(_) => {
                Topic::CoreUpdate
            }
            Cmd::ActivateVersion(_) | Cmd::DeleteVersion(_) => Topic::CoreVersion,
            Cmd::SetLogLevel(_) => Topic::LogLevel,
            Cmd::SetLatencyUrl(_) => Topic::LatencyUrl,
            Cmd::SetIconSet(_) => Topic::Icon,
            Cmd::SetTheme(_) => Topic::Theme,
            Cmd::SetKeepDockIcon(_) => Topic::DockIcon,
            Cmd::SetProxyPort(_) => Topic::ProxyPort,
            Cmd::SetAllowLan(_) => Topic::Lan,
            Cmd::SetApiPort(_) => Topic::ApiPort,
            Cmd::RegenerateSecret => Topic::Secret,
            _ => Topic::Core,
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
                | Cmd::ProvidersUpdated(_)
                | Cmd::Tested { .. }
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
    /// Raised here when a user command is sent, lowered by the backend when it is done, so a
    /// second click is refused even before the snapshot says busy.
    busy: Arc<AtomicBool>,
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
            busy: Arc::new(AtomicBool::new(false)),
            snapshot: watch::channel(Arc::new(snapshot)).1,
            connections: watch::channel(Arc::new(Vec::new())).1,
            log_generation: watch::channel(0).1,
            logs: Arc::new(Mutex::new(LogBuffer::new(LOG_LINES, LOG_BYTES))),
            quit_requested: watch::channel(false).1,
        };
        (handle, rx)
    }

    /// One user command at a time: another one sent meanwhile is dropped, except Stop, which
    /// also interrupts a start. Clicks then never act on a state they did not see.
    /// What the backend does when a command is done; for tests without one.
    #[cfg(test)]
    pub fn settle(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }

    pub fn send(&self, cmd: Cmd) {
        if cmd.is_user_action() {
            let was_busy = self.busy.swap(true, Ordering::SeqCst);
            if matches!(cmd, Cmd::Stop) {
                self.cancel.store(true, Ordering::SeqCst);
            } else if was_busy {
                return;
            }
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
    let busy = Arc::new(AtomicBool::new(false));
    let (mut actor, snapshot, connections, log_generation, logs) =
        Actor::new(paths, tx.clone(), cancel.clone())?;
    actor.busy = busy.clone();
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
        busy,
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
    /// Shared with `Handle`; lowered when a user command is done.
    busy: Arc<AtomicBool>,
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
    read_proxy: fn(&[u16]) -> proxy::State,
    /// Where the running core's DNS server listens, and whether it found that port taken. Mihomo
    /// binds again only when `listen` changes, so the result holds until then.
    dns_listen: Option<String>,
    dns_taken: bool,
    /// Whether the next start or reload composes with TUN. `app.tun_on` is what the user asked.
    tun: bool,
    /// Asks for administrator rights; blocking. Tests replace it.
    authorize: fn(&Path, Option<&Path>) -> Result<bool>,
    /// The app bundle's core. Tests supply a fake one.
    builtin: Option<update::Builtin>,
    /// An app update brought a newer built-in core, but the running TUN needs its setuid copy
    /// first: asked once, after the launch restore.
    builtin_waits: bool,
    /// The previous proxy port while the system proxy moves off it; it still counts as ours.
    switching_from: Option<u16>,
    next_message: u64,
    /// Changes when the core stops, reloads or the Latency URL changes; older results are dropped.
    epoch: u64,
    /// The running group test's nodes, including ones a single test had already started.
    group_nodes: Vec<String>,
    /// Shared by every test, so at most `TEST_LIMIT` run at once.
    test_slots: Arc<tokio::sync::Semaphore>,
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
        let (settings, settings_notice, invalid) = Settings::load(&paths)?;
        let (app, app_notice) = AppData::load(&paths)?;
        let set_aside: Vec<String> = settings_notice.into_iter().chain(app_notice).collect();
        // A first run has no dashboards to update.
        let first_run = !paths.app_data().exists();
        let (secret, regenerated) = paths.secret()?;
        let api = Api::new(settings.controller_port, &secret)?;
        // Tests must not depend on how the test binary is bundled.
        let builtin = if cfg!(test) { None } else { update::builtin() };
        // Arranged but not recorded: a failed listing here must not forget the order.
        let mut listed = profiles::list(&paths).unwrap_or_default();
        profiles::arrange(&mut listed, &app.profile_order);
        let snap = Snapshot {
            phase: Phase::Stopped,
            installed: update::versions(&paths, builtin.as_ref()),
            builtin: builtin.as_ref().map(|b| b.version.clone()),
            profiles: listed,
            config_dir: paths.config.clone(),
            // Read at launch, before the first publish: the theme, menu bar icon and Dock.
            icon_set: settings.icon_set.clone(),
            theme: settings.theme,
            keep_dock_icon: settings.keep_dock_icon,
            ..Snapshot::default()
        };
        let (snap_tx, snap_rx) = watch::channel(Arc::new(snap.clone()));
        let (conn_tx, conn_rx) = watch::channel(Arc::new(Vec::new()));
        let (log_tx, log_rx) = watch::channel(0);
        let logs = Arc::new(Mutex::new(LogBuffer::new(LOG_LINES, LOG_BYTES)));
        let mut actor = Actor {
            paths,
            settings,
            app,
            secret,
            api,
            tx,
            cancel,
            busy: Arc::new(AtomicBool::new(false)),
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
            dns_listen: None,
            dns_taken: false,
            tun: false,
            authorize: core::authorize,
            builtin,
            builtin_waits: false,
            switching_from: None,
            next_message: 0,
            epoch: 0,
            group_nodes: Vec::new(),
            test_slots: Arc::new(tokio::sync::Semaphore::new(TEST_LIMIT)),
        };
        // Information: its time starts once it is on screen, so it is still seen once.
        if !set_aside.is_empty() {
            actor.say(Topic::SettingsFile, set_aside.join(". "), false);
        }
        for (key, text) in invalid {
            actor.say(Topic::of_setting(key), text, true);
        }
        if regenerated && !first_run {
            let text = "Secret was regenerated. Update your dashboards".to_owned();
            actor.say(Topic::Secret, text, false);
        }
        actor.publish();
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
        self.snap.keep_dock_icon = self.settings.keep_dock_icon;
        self.snap.log_level = self.settings.log_level;
        self.snap.tun_stack = self.settings.tun_stack;
        self.snap.dns_port_taken = self
            .dns_listen
            .as_deref()
            .and_then(profiles::listen_port)
            .filter(|_| self.dns_taken && self.settings.allow_lan && self.core.is_some());
        self.snap.tun_needs_auth = self.app.tun_on
            && self.snap.tun_device.is_none()
            && self
                .exe()
                .is_ok_and(|(exe, _)| exe.is_file() && !core::authorized(&exe));
        self.snap.mixed_port = self.settings.mixed_port;
        self.snap.controller_port = self.settings.controller_port;
        self.snap.allow_lan = self.settings.allow_lan;
        self.snap.secret.clone_from(&self.secret);
        self.snap.latency_url.clone_from(&self.settings.latency_url);
        self.snap.installed = update::versions(&self.paths, self.builtin.as_ref());
        self.snap.builtin = self.builtin.as_ref().map(|b| b.version.clone());
        if self.core.is_none() {
            self.snap.mode = self.app.mode.clone().unwrap_or_default();
        }
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
            Err(e) => self.say(Topic::Core, format!("{e:#}"), true),
        }
        if let Err(e) = profiles::ensure_default(&self.paths) {
            self.say(Topic::Profile, format!("{e:#}"), true);
        }
        // A core placed in bin/ by hand is used, before the cleanup below would delete it.
        if self.app.core_version.is_none()
            && let Some(version) = update::newest_installed(&self.paths)
        {
            self.app.core_version = Some(version.clone());
            match self.app.save(&self.paths) {
                Ok(()) => self.log(false, &format!("Using installed core {version}")),
                Err(e) => self.say(Topic::Core, format!("{e:#}"), true),
            }
        }
        self.settle_builtin();
        self.refresh_profiles();
        self.read_proxy_state();
        // Queued, so the window shows the launch state before a core starts.
        let _ = self.tx.send(Cmd::Restore);
    }

    fn resolve(&self, version: &str) -> Option<PathBuf> {
        update::resolve(&self.paths, self.builtin.as_ref(), version)
    }

    /// At launch, before the core starts: versions that left with an old app, the move to a new
    /// app's core, and a download the built-in core duplicates.
    fn settle_builtin(&mut self) {
        let Some(builtin) = self.builtin.clone() else {
            return;
        };
        let before = self.app.clone();
        let resolves =
            |a: &Self, v: &Option<String>| v.as_deref().is_some_and(|v| a.resolve(v).is_some());
        if self.app.core_version.is_some() && !resolves(self, &self.app.core_version) {
            self.app.core_version = Some(builtin.version.clone());
        }
        if !resolves(self, &self.app.previous_core_version) {
            self.app.previous_core_version = None;
        }
        // Once per built-in version, and never to an older core.
        if self.app.builtin_seen.as_ref() != Some(&builtin.version) {
            self.app.builtin_seen = Some(builtin.version.clone());
            let newer = self
                .app
                .core_version
                .as_deref()
                .is_none_or(|current| update::newer(&builtin.version, current));
            // A running TUN keeps its setuid core until the new one has its own copy.
            let tun_waits = self.app.tun_on
                && self.exe().is_ok_and(|(exe, _)| core::authorized(&exe))
                && !self
                    .resolve(&builtin.version)
                    .is_some_and(|exe| core::authorized(&exe));
            if newer && tun_waits {
                self.builtin_waits = true;
            } else if newer {
                self.app.previous_core_version =
                    self.app.core_version.replace(builtin.version.clone());
                self.log(
                    false,
                    &format!("Using the built-in core {}", builtin.version),
                );
            }
        }
        if self.app != before
            && let Err(e) = self.app.save(&self.paths)
        {
            self.say(Topic::Core, format!("{e:#}"), true);
        }
        match update::remove_duplicate(&self.paths, &builtin) {
            Ok(true) => self.log(
                false,
                &format!(
                    "Removed the download of {}: the built-in core is the same file",
                    builtin.version
                ),
            ),
            Ok(false) => {}
            Err(e) => self.log(true, &format!("{e:#}")),
        }
    }

    /// After an app update moved a TUN user: the previous setuid core already runs, so the
    /// network never waits for this prompt. Cancelled, the previous core keeps running, and
    /// `builtin_seen` keeps the next launch from asking again.
    async fn adopt_builtin(&mut self) {
        let Some(builtin) = self.builtin.clone() else {
            return;
        };
        let version = builtin.version;
        let result = match self.authorize_core(&builtin.exe, &version).await {
            Ok(true) => self.activate_version(&version).await,
            Ok(false) => {
                let text = format!(
                    "The new core {version} is not in use yet, because authorization was cancelled. Choose it in Settings to use it"
                );
                self.say(Topic::Core, text, false);
                return;
            }
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            let text = format!("The new core {version} is not in use yet: {e:#}");
            self.say(Topic::Core, text, true);
        }
    }

    fn log(&mut self, error: bool, text: &str) {
        let mut logs = self.logs.lock().unwrap();
        logs.push(LogLine {
            level: if error { "error" } else { "info" }.into(),
            text: format!("Xihomo: {text}").into(),
        });
        self.log_tx.send_replace(logs.generation);
    }

    /// Shown next to its topic's section, replacing that topic's message, and kept in Logs.
    fn say(&mut self, topic: Topic, text: String, error: bool) {
        self.log(error, &text);
        self.next_message += 1;
        let id = self.next_message;
        self.snap
            .messages
            .insert(topic, Message { text, error, id });
    }

    /// For a condition seen on every check: said again only once its message was replaced.
    fn say_once(&mut self, topic: Topic, text: String, error: bool) {
        if self.snap.message(topic).is_none_or(|m| m.text != text) {
            self.say(topic, text, error);
        }
    }

    fn clear(&mut self, topic: Topic) {
        self.snap.messages.remove(&topic);
    }

    /// Only user choices are remembered; crashes, failed starts and Quit leave them alone.
    fn remember(&mut self, core_on: bool, proxy_on: bool, tun_on: bool) -> Result<()> {
        self.app.core_on = core_on;
        self.app.proxy_on = proxy_on;
        self.app.tun_on = tun_on;
        self.app.save(&self.paths)
    }

    async fn handle(&mut self, cmd: Cmd) {
        let user = cmd.is_user_action();
        if user {
            self.snap.busy = true;
            self.publish();
        }
        self.handle_inner(cmd).await;
        if user {
            self.snap.busy = false;
            self.busy.store(false, Ordering::SeqCst);
        }
    }

    async fn handle_inner(&mut self, cmd: Cmd) {
        let topic = cmd.topic();
        if cmd.is_user_action() {
            self.clear(topic);
        }
        let applies = matches!(
            cmd,
            Cmd::SetProxyPort(_) | Cmd::SetAllowLan(_) | Cmd::SetApiPort(_) | Cmd::RegenerateSecret
        );
        if applies && self.core.is_some() {
            self.snap.applying = Some(topic.area());
            self.publish();
        }
        let result = match cmd {
            Cmd::Start => match self.remember(true, self.app.proxy_on, self.app.tun_on) {
                Ok(()) if self.core.is_none() => {
                    // A click: the one moment a start may ask for authorization.
                    self.plan_tun(true).await;
                    self.start().await
                }
                other => other,
            },
            // Stop turns everything off, so the TUN reason no longer applies.
            Cmd::Stop => match self.remember(false, false, false) {
                Ok(()) => {
                    self.clear(Topic::Tun);
                    self.stop().await
                }
                Err(e) => Err(e),
            },
            Cmd::Activate(name) => self.activate(&name).await,
            // A download in flight would report on providers the reload removed, and its result
            // would replace a reload error before it was read. The buttons wait the same way.
            Cmd::Reload if self.snap.providers_busy => {
                Err(anyhow!("Reload waits for the providers update to finish"))
            }
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
                self.snap.lan_ip = proxy::lan_ip();
                self.follow_core().await
            }
            Cmd::CloseConnection(id) => self.api.close_connection(&id).await,
            Cmd::CloseAll => self.api.close_all().await,
            Cmd::SetProxy(on) => match self.remember(self.app.core_on, on, self.app.tun_on) {
                Ok(()) => self.apply_proxy(on).await,
                Err(e) => Err(e),
            },
            Cmd::SetTun(on) => {
                let result = self.set_tun(on).await;
                self.snap.tun_busy = None;
                result
            }
            Cmd::SetTunStack(stack) => self.set_tun_stack(stack).await,
            Cmd::GuardProxy => self.guard_proxy().await,
            Cmd::Restore => self.restore().await,
            Cmd::WatchConnections(on) => {
                self.watching_connections = on;
                self.reconcile_connection_task();
                Ok(())
            }
            Cmd::CheckUpdate => self.check_update(),
            Cmd::Download => match self.snap.latest.clone() {
                Some(r) if self.resolve(&r.version).is_some() => {
                    self.activate_version(&r.version).await
                }
                _ => self.download(),
            },
            Cmd::ActivateVersion(version) => self.activate_version(&version).await,
            Cmd::DeleteVersion(version) => self.delete_version(&version),
            Cmd::SetIconSet(set) => Settings::write_key(&self.paths, "icon_set", &set)
                .map(|()| self.settings.icon_set = set),
            Cmd::SetTheme(theme) => Settings::write_key(&self.paths, "theme", theme)
                .map(|()| self.settings.theme = theme),
            Cmd::SetLogLevel(level) => self.set_log_level(level).await,
            Cmd::SetLatencyUrl(url) => self.set_latency_url(&url).await,
            Cmd::TestNode(member) => self.test_node(&member),
            Cmd::TestGroup(group) => self.test_group(&group),
            Cmd::Tested {
                epoch,
                node,
                latency,
            } => {
                if epoch == self.epoch {
                    self.snap.testing.remove(&node);
                    if let Some(n) = self.snap.nodes.get_mut(&node) {
                        n.latency = Some(latency);
                    }
                    // Done once none of its nodes is under test, whichever test started them.
                    if self
                        .group_nodes
                        .iter()
                        .all(|n| !self.snap.testing.contains(n))
                    {
                        self.snap.testing_group = None;
                        self.group_nodes.clear();
                    }
                }
                Ok(())
            }
            Cmd::SetKeepDockIcon(on) => Settings::write_key(&self.paths, "keep_dock_icon", on)
                .map(|()| self.settings.keep_dock_icon = on),
            Cmd::SetProxyPort(port) => self.set_proxy_port(port).await,
            Cmd::SetAllowLan(on) => self.set_allow_lan(on).await,
            Cmd::SetApiPort(port) => self.set_api_port(port).await,
            Cmd::RegenerateSecret => self.regenerate_secret().await,
            Cmd::LinkProfile(path) => profiles::link(&self.paths, &path).map(|name| {
                self.log(false, &format!("Linked profile {name}"));
                self.refresh_profiles();
            }),
            Cmd::ImportProfile(path) => profiles::import(&self.paths, &path).map(|name| {
                self.log(false, &format!("Imported profile {name}"));
                self.refresh_profiles();
            }),
            Cmd::RemoveProfile(name) => self.remove_profile(&name),
            Cmd::UpdateProviders => self.update_providers(),
            Cmd::ProvidersUpdated(result) => self.on_providers_updated(result).await,
            Cmd::CheckNow => {
                self.reconciled = None;
                self.periodic().await;
                Ok(())
            }
            Cmd::Checked(result) => self.on_checked(result),
            Cmd::Staged(result) => self.on_staged(result).await,
            Cmd::Shutdown(_) => unreachable!("handled by the run loop"),
        };
        self.snap.applying = None;
        if let Err(e) = result {
            let text = format!("{e:#}");
            // A failed start already shows its reason as the phase.
            if self.snap.phase == Phase::Failed(text.clone()) {
                self.log(true, &text);
            } else {
                self.say(topic, text, true);
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
            // Never a dialog nobody clicked for: xihomo may start at login. The one exception
            // follows below, once this core runs: an app update's new core for a TUN user.
            self.plan_tun(false).await;
            self.start().await
        } else {
            Ok(())
        };
        if std::mem::take(&mut self.builtin_waits) {
            self.adopt_builtin().await;
        }
        let guarded = self.guard_proxy().await;
        started.and(guarded)
    }

    fn exe(&self) -> Result<(PathBuf, String)> {
        let version = self
            .app
            .core_version
            .clone()
            .context("No Mihomo core installed yet: use Settings → Check for Update")?;
        let exe = self
            .resolve(&version)
            .unwrap_or_else(|| self.paths.core_exe(&version));
        Ok((exe, version))
    }

    fn runtime(&self) -> profiles::Runtime<'_> {
        profiles::Runtime::new(
            &self.settings,
            &self.secret,
            self.app.mode.as_deref(),
            self.tun,
        )
    }

    /// Compose and test a candidate with the selected core. Nothing is activated or persisted.
    async fn prepare(&mut self, exe: &Path, name: &str) -> Result<profiles::Candidate> {
        let candidate = profiles::write_candidate(&self.paths, name, &self.runtime())?;
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

    /// The tested candidate becomes the `-f` file of a core about to launch; nothing runs from it
    /// yet, so a dashboard's Restart or Reload never loads a rejected candidate.
    async fn launch_candidate(&mut self, exe: &Path, home: &Path) -> Result<()> {
        let file = self.paths.config();
        write_private(&file, &fs::read(self.paths.candidate())?)?;
        self.launch(exe, home, &file).await
    }

    /// The only place selection is persisted, and it runs after activation has succeeded.
    fn commit(&mut self, name: &str, candidate: profiles::Candidate) -> Result<()> {
        let bytes = fs::read(self.paths.candidate())?;
        write_private(&self.paths.last_good(), &bytes)?;
        write_private(&self.paths.config(), &bytes)?;
        self.app.active_profile = Some(name.to_owned());
        self.app.last_good_home = Some(candidate.home);
        self.app.save(&self.paths)?;
        self.fingerprint = Some(candidate.fingerprint);
        self.snap.edited = false;
        // The chosen profile runs, not last-good.
        self.clear(Topic::LastGood);
        if !candidate.ignored.is_empty() {
            let keys = candidate.ignored.join(", ");
            self.log(
                false,
                &format!("Profile {name}: {keys} ignored (set by Xihomo)"),
            );
        }
        if !candidate.notices.is_empty() {
            self.say(Topic::Profile, candidate.notices.join(". "), false);
        }
        Ok(())
    }

    async fn launch(&mut self, exe: &Path, home: &Path, file: &Path) -> Result<()> {
        self.ports_free(exe).await?;
        // A port or secret changed since it was written must not make recovery fail.
        if file == self.paths.last_good() {
            profiles::reown(file, &self.runtime())?;
        }
        match core::discard_unwritable_cache(home) {
            Ok(true) => self.log(
                false,
                &format!(
                    "Deleted {}/cache.db: it could not be written",
                    home.display()
                ),
            ),
            Ok(false) => {}
            Err(e) => self.log(true, &format!("{e:#}")),
        }
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
                // The live stream starts only now, and Mihomo does not replay what it said while
                // starting: a port in use, say, would never reach Logs.
                let issues = core.take_issues();
                if !issues.is_empty() {
                    let mut logs = self.logs.lock().unwrap();
                    for line in issues {
                        logs.push(line);
                    }
                    self.log_tx.send_replace(logs.generation);
                }
                self.core = Some(core);
                self.spawn_log_task();
                self.reconcile_connection_task();
                self.snap.phase = Phase::Running;
                // "The core is not running" no longer holds.
                self.clear(Topic::Groups);
                self.check_tun().await;
                self.dns_listen = profiles::dns_listen(file);
                if let Some(core) = &self.core {
                    if self.dns_listen.is_some() {
                        // It binds while starting; give that line a moment to be read.
                        core.settle_output().await;
                    }
                    self.dns_taken = core.dns_taken() > 0;
                }
                Ok(())
            }
            Err(e) => {
                core.stop().await?;
                Err(e)
            }
        }
    }

    async fn confirm_ports(&self) -> Result<()> {
        let mixed = self.api.runtime().await?.mixed_port;
        if mixed != self.settings.mixed_port {
            bail!(
                "Mihomo is not serving the mixed port {} (reports {mixed})",
                self.settings.mixed_port
            );
        }
        Ok(())
    }

    /// A port in use stops a launch; the error names the setting, also shown in its section
    /// until the port is free again.
    async fn ports_free(&mut self, exe: &Path) -> Result<()> {
        let as_root = core::authorized(exe);
        for (port, topic, section) in [
            (self.settings.mixed_port, Topic::ProxyPort, "PROXY"),
            (self.settings.controller_port, Topic::ApiPort, "API"),
        ] {
            if core::ensure_ports_free(&[port], as_root).await.is_err() {
                self.say(
                    topic,
                    format!("Port {port} is in use. Change it here"),
                    true,
                );
                bail!("Port {port} is in use. Change it in Settings → {section}");
            }
            self.clear(topic);
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
        self.refresh_with_global_default().await?;
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
        self.ports_free(&exe).await?;
        let primary = async {
            let candidate = self.prepare(&exe, &name).await?;
            self.launch_candidate(&exe, &candidate.home).await?;
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
                anyhow!("{error:#}. Last-good recovery also failed: {recovery:#}")
            })?;
        self.fingerprint = None;
        self.snap.edited = true;
        // An error, so it stays: the profile you picked is not the one running.
        self.say(
            Topic::LastGood,
            format!("Running the last-good configuration because: {error:#}"),
            true,
        );
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if self.core.is_none() {
            self.snap.phase = Phase::Stopped;
            return Ok(());
        }
        self.snap.phase = Phase::Stopping;
        self.clear(Topic::LastGood);
        self.publish();
        let proxy_result = self.release_proxy().await;
        let stop_result = self.stop_child().await;
        self.snap.phase = Phase::Stopped;
        proxy_result.and(stop_result)
    }

    async fn stop_child(&mut self) -> Result<()> {
        self.abort_tasks();
        self.clear_runtime();
        let Some(core) = self.core.take() else {
            return Ok(());
        };
        // Lets the aborted streams close their connections before the core closes them, so the
        // TIME_WAIT is on xihomo's side (see `Api::new`).
        tokio::time::sleep(Duration::from_millis(50)).await;
        core.stop().await
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
        self.new_epoch();
        self.snap.mode.clear();
        self.snap.groups.clear();
        self.snap.nodes.clear();
        self.snap.tun_device = None;
        self.dns_listen = None;
        self.dns_taken = false;
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
        let ports: Vec<u16> = [self.settings.mixed_port]
            .into_iter()
            .chain(self.switching_from)
            .collect();
        self.proxy = (self.read_proxy)(&ports);
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
        if result.is_ok() {
            self.settle_proxy(on).await;
        } else {
            self.read_proxy_state();
        }
        self.snap.proxy_busy = None;
        result
    }

    /// configd publishes the new state a moment after networksetup returns; reading it at once
    /// can still show the old one until the next periodic check. Gives up after `PROXY_SETTLE`.
    async fn settle_proxy(&mut self, on: bool) {
        let deadline = Instant::now() + PROXY_SETTLE;
        loop {
            self.read_proxy_state();
            if self.proxy.on == on || Instant::now() > deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
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

    /// Before a start: whether it composes with TUN. Only a click may ask for authorization.
    async fn plan_tun(&mut self, prompt: bool) {
        self.tun = false;
        self.clear(Topic::Tun);
        if let (true, Ok((exe, version))) = (self.app.tun_on, self.exe()) {
            self.tun = self.tun_allowed(&exe, &version, prompt).await;
        }
    }

    /// Whether `exe` may run with TUN, asking for authorization if `prompt`. When not, says why
    /// in `Topic::Tun`. A cancelled dialog is the user's choice, so it turns `tun_on` off.
    async fn tun_allowed(&mut self, exe: &Path, version: &str, prompt: bool) -> bool {
        if !update::supports_tun(version) {
            let text = format!(
                "TUN is off: it needs core {} or newer. Update the core in Settings",
                update::TUN_MIN
            );
            self.say(Topic::Tun, text, true);
            return false;
        }
        if core::authorized(exe) {
            return true;
        }
        if !prompt {
            let text =
                "TUN is off: this core needs administrator authorization. Turn TUN on to authorize";
            self.say(Topic::Tun, text.to_owned(), true);
            return false;
        }
        match self.authorize_core(exe, version).await {
            Ok(true) => true,
            Ok(false) => {
                self.app.tun_on = false;
                if let Err(e) = self.app.save(&self.paths) {
                    self.log(true, &format!("{e:#}"));
                }
                let text = "TUN stays off: authorization was cancelled".to_owned();
                self.say(Topic::Tun, text, false);
                false
            }
            Err(e) => {
                self.say(Topic::Tun, format!("TUN is off: {e:#}"), true);
                false
            }
        }
    }

    /// Off the actor thread, so the core's output keeps draining while the dialog is open.
    /// The built-in core cannot be setuid inside the app, so it is copied to `bin/<version>/`,
    /// where the version resolves from then on. That directory is made as the user first, so
    /// pruning can remove it without administrator rights.
    async fn authorize_core(&mut self, exe: &Path, version: &str) -> Result<bool> {
        let dest = self.paths.core_exe(version);
        let from = (exe != dest).then(|| exe.to_owned());
        let dir = self.paths.bin().join(version);
        let made = from.is_some() && !dir.exists();
        if made {
            fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        self.snap.tun_busy = Some(TunBusy::Authorizing);
        self.publish();
        let (authorize, file) = (self.authorize, dest.clone());
        let result = tokio::task::spawn_blocking(move || authorize(&file, from.as_deref())).await;
        self.snap.tun_busy = None;
        let granted = result
            .context("authorization stopped")
            .and_then(|granted| granted);
        if made && !matches!(granted, Ok(true)) {
            let _ = fs::remove_dir_all(&dir);
        }
        let granted = granted?;
        if granted {
            self.log(
                false,
                &format!("Authorized {} to run as root", dest.display()),
            );
            // Best effort: the cache.db fallback covers a home it could not reach.
            if let Err(e) = core::share_homes(&self.paths.homes()) {
                self.log(true, &format!("{e:#}"));
            }
        }
        Ok(granted)
    }

    /// Mihomo keeps running when TUN fails and only logs it, so `/configs` is the only way to
    /// know. The TUN can come up a moment after the API does.
    async fn check_tun(&mut self) {
        let deadline = Instant::now() + TUN_SETTLE;
        loop {
            self.snap.tun_device = self.api.runtime().await.ok().and_then(|c| c.tun_device);
            if self.snap.tun_device.is_some() == self.tun || Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        match (self.tun, self.snap.tun_device.is_some()) {
            (true, false) => {
                let tail = self.core.as_ref().map(|c| c.tail(40)).unwrap_or_default();
                let lines: Vec<&str> = tail.lines().filter(|l| l.contains("TUN")).collect();
                let reason = core::rejection_reason(&lines.join("\n"));
                let text = if reason.is_empty() {
                    "TUN did not start. See Logs".to_owned()
                } else {
                    format!("TUN did not start: {reason}")
                };
                self.say(Topic::Tun, text, true);
            }
            (false, true) => self.say(Topic::Tun, "TUN did not turn off".to_owned(), true),
            (true, true) => self.clear(Topic::Tun),
            // Off as asked; a reason it is off stays.
            (false, false) => {}
        }
    }

    /// A reload switches TUN in the running core. Only a new authorization restarts it, since the
    /// running process is still the user's.
    async fn set_tun(&mut self, on: bool) -> Result<()> {
        self.running_or_err()?;
        let (exe, version) = self.exe()?;
        if !on {
            self.app.tun_on = false;
            self.app.save(&self.paths)?;
            self.snap.tun_busy = Some(TunBusy::TurningOff);
            self.publish();
            return self.reload_tun(false).await;
        }
        let fresh = !core::authorized(&exe);
        if !self.tun_allowed(&exe, &version, true).await {
            return Ok(());
        }
        self.app.tun_on = true;
        self.app.save(&self.paths)?;
        self.snap.tun_busy = Some(TunBusy::TurningOn);
        self.publish();
        if fresh {
            self.tun = true;
            return self
                .restart_with(self.settings.clone(), self.secret.clone())
                .await;
        }
        self.reload_tun(true).await
    }

    async fn reload_tun(&mut self, on: bool) -> Result<()> {
        let previous = std::mem::replace(&mut self.tun, on);
        match self.reload_with(self.settings.clone()).await {
            Ok((name, candidate)) => {
                self.commit(&name, candidate)?;
                self.refresh_with_global_default().await
            }
            Err(e) => {
                self.tun = previous;
                Err(e)
            }
        }
    }

    /// Applied by reload while TUN runs, which recreates it with the new stack.
    async fn set_tun_stack(&mut self, stack: TunStack) -> Result<()> {
        // Chosen again: writes it, so a bad value read at launch is fixed in the file too.
        if stack == self.settings.tun_stack {
            return Settings::write_key(&self.paths, "tun_stack", stack);
        }
        let mut next = self.settings.clone();
        next.tun_stack = stack;
        if !(self.tun && self.core.is_some()) {
            Settings::write_key(&self.paths, "tun_stack", stack)?;
            self.settings = next;
            return Ok(());
        }
        let (name, candidate) = self.reload_with(next).await?;
        let saved = Settings::write_key(&self.paths, "tun_stack", stack);
        self.commit(&name, candidate)?;
        self.refresh_with_global_default().await?;
        saved
    }

    async fn activate(&mut self, name: &str) -> Result<()> {
        let (exe, _) = self.exe()?;
        let candidate = self.prepare(&exe, name).await?;
        let Some(running_home) = self.core.as_ref().map(|c| c.home.clone()) else {
            return self.commit(name, candidate);
        };
        if running_home == candidate.home {
            if let Err(e) = self.reload_running().await {
                return Err(self.reload_last_good(e).await);
            }
            self.commit(name, candidate)?;
            self.refresh_with_global_default().await?;
            // Open connections keep the old profile's rules and nodes until closed.
            return self.api.close_all().await;
        }
        // A different home is a process argument, so this needs a managed restart.
        self.read_proxy_state();
        let proxy_was_on = self.proxy.ours;
        self.release_proxy().await?;
        self.stop_child().await?;
        self.cancel.store(false, Ordering::SeqCst);
        let result = match self.launch_candidate(&exe, &candidate.home).await {
            Ok(()) => self.commit(name, candidate),
            Err(e) => self.relaunch_last_good(&exe, e).await,
        };
        self.after_restart(proxy_was_on).await;
        result
    }

    /// A reload replaces the proxies, and with them Mihomo's results.
    fn new_epoch(&mut self) {
        self.epoch += 1;
        self.snap.testing.clear();
        self.snap.testing_group = None;
        self.group_nodes.clear();
    }

    async fn reload_running(&mut self) -> Result<()> {
        self.new_epoch();
        let listen = profiles::dns_listen(&self.paths.candidate());
        let taken_before = self.core.as_ref().map_or(0, Core::dns_taken);
        self.api.reload(&self.paths.candidate()).await?;
        self.confirm_ports().await?;
        self.check_tun().await;
        if listen != self.dns_listen
            && let Some(core) = &self.core
        {
            if listen.is_some() {
                core.settle_output().await;
            }
            self.dns_taken = core.dns_taken() > taken_before;
            self.dns_listen = listen;
        }
        Ok(())
    }

    /// After a failed reload: back to the last committed configuration.
    async fn reload_last_good(&mut self, e: anyhow::Error) -> anyhow::Error {
        self.new_epoch();
        let recovery = self.api.reload(&self.paths.last_good()).await;
        let _ = self.refresh_with_global_default().await;
        match recovery {
            Ok(()) => e.context("reload failed, so the last-good configuration was reloaded"),
            Err(r) => e.context(format!(
                "reload failed and last-good did not reload ({r:#})"
            )),
        }
    }

    fn rebuild_api(&mut self) -> Result<()> {
        self.api = Api::new(self.settings.controller_port, &self.secret)?;
        Ok(())
    }

    /// A port typed in Settings: unprivileged, not the other port, and free right now. The
    /// running core holds the current ports, so only a new one is probed.
    async fn check_port(port: u32, other: u16, other_name: &str, as_root: bool) -> Result<u16> {
        let port = u16::try_from(port)
            .ok()
            .filter(|&p| settings::valid_port(p))
            .context("Use a port from 1024 to 65535")?;
        if port == other {
            bail!("Port {port} is the {other_name} port");
        }
        core::ensure_ports_free(&[port], as_root)
            .await
            .map_err(|_| anyhow!("Port {port} is in use"))?;
        Ok(port)
    }

    /// Reloads the active profile composed with `next`, which is kept only once the core serves
    /// it. On failure the previous settings stay and nothing is committed.
    async fn reload_with(&mut self, next: Settings) -> Result<(String, profiles::Candidate)> {
        let (exe, _) = self.exe()?;
        let name = self
            .app
            .active_profile
            .clone()
            .context("Choose a profile first")?;
        let previous = std::mem::replace(&mut self.settings, next);
        let candidate = match self.prepare(&exe, &name).await {
            Ok(c) if self.core.as_ref().is_some_and(|core| core.home == c.home) => c,
            Ok(_) => {
                self.settings = previous;
                bail!("The core runs the last-good configuration. Reload the profile first");
            }
            Err(e) => {
                self.settings = previous;
                return Err(e);
            }
        };
        if let Err(e) = self.reload_running().await {
            self.settings = previous;
            return Err(self.reload_last_good(e).await);
        }
        Ok((name, candidate))
    }

    /// Points a system proxy of ours at the current port. A failed move turns it off while
    /// `from` still counts as ours, so a half-done change never leaves a dead proxy.
    async fn move_proxy(&mut self, from: u16) -> Result<bool> {
        self.switching_from = Some(from);
        self.read_proxy_state();
        let result = if !self.proxy.ours {
            Ok(false)
        } else if let Err(e) = self.apply_proxy(true).await {
            let off = self.apply_proxy(false).await;
            Err(e.context(match off {
                Ok(()) => "moving the system proxy failed, so it was turned off",
                Err(_) => "moving the system proxy failed, and so did turning it off",
            }))
        } else {
            Ok(true)
        };
        self.switching_from = None;
        self.read_proxy_state();
        result
    }

    async fn set_proxy_port(&mut self, port: u32) -> Result<()> {
        if port == u32::from(self.settings.mixed_port) {
            return Ok(());
        }
        let port = Self::check_port(
            port,
            self.settings.controller_port,
            "API",
            self.runs_as_root(),
        )
        .await?;
        let mut next = self.settings.clone();
        next.mixed_port = port;
        if self.core.is_none() {
            Settings::write_key(&self.paths, "mixed_port", port)?;
            self.settings = next;
            self.say(
                Topic::ProxyPort,
                "Saved. Applies at next start".into(),
                false,
            );
            return Ok(());
        }
        let old = self.settings.mixed_port;
        let (name, candidate) = self.reload_with(next).await?;
        let moved = self.move_proxy(old).await;
        let saved = Settings::write_key(&self.paths, "mixed_port", port);
        self.commit(&name, candidate)?;
        self.refresh_with_global_default().await?;
        // The listener was rebuilt; like Reload, connections close so apps reconnect.
        self.api.close_all().await?;
        saved?;
        let text = if moved? {
            format!("Applied. System proxy moved to {port}")
        } else {
            "Applied".to_owned()
        };
        self.say(Topic::ProxyPort, text, false);
        Ok(())
    }

    fn lan_text(&self) -> String {
        match self.snap.lan_ip {
            Some(ip) => format!("Other devices can use {ip}:{}", self.settings.mixed_port),
            None => "No network".to_owned(),
        }
    }

    /// Reload applies allow-lan and rebuilds the listeners; open connections stay.
    /// Live, without a reload: the core's level and the Logs tab's stream both follow.
    async fn set_log_level(&mut self, level: LogLevel) -> Result<()> {
        // Chosen again: writes it, so a bad value read at launch is fixed in the file too.
        if level == self.settings.log_level {
            return Settings::write_key(&self.paths, "log_level", level);
        }
        Settings::write_key(&self.paths, "log_level", level)?;
        self.settings.log_level = level;
        if self.core.is_some() {
            self.spawn_log_task();
            self.api
                .patch(serde_json::json!({ "log-level": level.as_str() }))
                .await?;
        }
        Ok(())
    }

    /// No reload: the next test uses it, and the cards show what Mihomo has for it already.
    async fn set_latency_url(&mut self, text: &str) -> Result<()> {
        let url = match text.trim() {
            "" => settings::DEFAULT_LATENCY_URL.to_owned(),
            text => settings::latency_url(text).context("Use an http or https URL")?,
        };
        if url == self.settings.latency_url {
            return Ok(());
        }
        Settings::write_key(&self.paths, "latency_url", &url)?;
        self.settings.latency_url = url;
        self.new_epoch();
        if self.core.is_some() {
            self.refresh_runtime().await?;
        }
        Ok(())
    }

    fn test_node(&mut self, member: &str) -> Result<()> {
        self.running_or_err()?;
        // Its card is disabled meanwhile; a group test covers it or leaves it alone.
        if self.snap.testing_group.is_some() {
            return Ok(());
        }
        if let Some(node) = self.snap.target(member)
            && !self.snap.testing.contains(&node)
        {
            self.spawn_tests(vec![node]);
        }
        Ok(())
    }

    fn test_group(&mut self, group: &str) -> Result<()> {
        self.running_or_err()?;
        if self.snap.testing_group.is_some() {
            return Ok(());
        }
        let targets = self.snap.group_targets(group);
        if !targets.is_empty() {
            // A node a single test already runs is adopted, not tested twice.
            let new = targets
                .iter()
                .filter(|n| !self.snap.testing.contains(*n))
                .cloned()
                .collect();
            self.snap.testing_group = Some(group.to_owned());
            self.group_nodes = targets;
            self.spawn_tests(new);
        }
        Ok(())
    }

    /// Beside the actor, like a providers update; each result is sent back as it arrives.
    fn spawn_tests(&mut self, nodes: Vec<String>) {
        let epoch = self.epoch;
        let url: Arc<str> = self.settings.latency_url.as_str().into();
        for node in nodes {
            let provider = self.snap.nodes.get(&node).and_then(|n| n.provider.clone());
            self.snap.testing.insert(node.clone());
            let (api, tx, url, limit) = (
                self.api.clone(),
                self.tx.clone(),
                url.clone(),
                self.test_slots.clone(),
            );
            tokio::task::spawn_local(async move {
                let _permit = limit.acquire_owned().await;
                let latency = api.test(&node, provider.as_deref(), &url).await;
                let _ = tx.send(Cmd::Tested {
                    epoch,
                    node,
                    latency,
                });
            });
        }
    }

    async fn set_allow_lan(&mut self, on: bool) -> Result<()> {
        // Chosen again: writes it, so a bad value read at launch is fixed in the file too.
        if on == self.settings.allow_lan {
            return Settings::write_key(&self.paths, "allow_lan", on);
        }
        let mut next = self.settings.clone();
        next.allow_lan = on;
        if self.core.is_none() {
            Settings::write_key(&self.paths, "allow_lan", on)?;
            self.settings = next;
            self.say(Topic::Lan, "Saved. Applies at next start".into(), false);
            return Ok(());
        }
        let (name, candidate) = self.reload_with(next).await?;
        let saved = Settings::write_key(&self.paths, "allow_lan", on);
        self.commit(&name, candidate)?;
        self.refresh_with_global_default().await?;
        saved?;
        self.snap.lan_ip = proxy::lan_ip();
        let text = if on {
            self.lan_text()
        } else {
            "Only this device can use the proxy".to_owned()
        };
        self.say(Topic::Lan, text, false);
        Ok(())
    }

    async fn set_api_port(&mut self, port: u32) -> Result<()> {
        if port == u32::from(self.settings.controller_port) {
            return Ok(());
        }
        let port =
            Self::check_port(port, self.settings.mixed_port, "proxy", self.runs_as_root()).await?;
        let mut next = self.settings.clone();
        next.controller_port = port;
        if self.core.is_none() {
            Settings::write_key(&self.paths, "controller_port", port)?;
            self.settings = next;
            self.rebuild_api()?;
            self.say(Topic::ApiPort, "Saved. Applies at next start".into(), false);
            return Ok(());
        }
        self.restart_with(next, self.secret.clone()).await?;
        Settings::write_key(&self.paths, "controller_port", port)?;
        self.say(Topic::ApiPort, "Applied".into(), false);
        Ok(())
    }

    async fn regenerate_secret(&mut self) -> Result<()> {
        let secret = settings::new_secret()?;
        if self.core.is_none() {
            self.paths.save_secret(&secret)?;
            self.secret = secret;
            self.rebuild_api()?;
        } else {
            self.restart_with(self.settings.clone(), secret).await?;
            self.paths.save_secret(&self.secret)?;
        }
        let text = "Regenerated. Update your dashboards".to_owned();
        self.say(Topic::Secret, text, false);
        Ok(())
    }

    /// The controller's address and secret change only with a new process. The proxy port stays,
    /// so the system proxy is left in place for the moment without a listener. If the new core
    /// fails, the previous values come back through last-good; if that fails too, the proxy is
    /// turned off.
    async fn restart_with(&mut self, settings: Settings, secret: String) -> Result<()> {
        let (exe, _) = self.exe()?;
        let name = self
            .app
            .active_profile
            .clone()
            .context("Choose a profile first")?;
        let previous_settings = std::mem::replace(&mut self.settings, settings);
        let previous_secret = std::mem::replace(&mut self.secret, secret);
        let prepared = self.prepare(&exe, &name).await;
        let candidate = match prepared {
            Ok(candidate) => candidate,
            Err(e) => {
                self.settings = previous_settings;
                self.secret = previous_secret;
                return Err(e);
            }
        };
        let stopped = self.stop_child().await;
        self.cancel.store(false, Ordering::SeqCst);
        let launched = match (stopped, self.rebuild_api()) {
            (Ok(()), Ok(())) => self.launch_candidate(&exe, &candidate.home).await,
            (Err(e), _) | (_, Err(e)) => Err(e),
        };
        if let Err(e) = launched {
            self.settings = previous_settings;
            self.secret = previous_secret;
            let result = match self.rebuild_api() {
                Ok(()) => self.relaunch_last_good(&exe, e).await,
                Err(rebuild) => Err(e.context(rebuild)),
            };
            if self.core.is_none() {
                let _ = self.release_proxy().await;
            } else {
                let _ = self.refresh_with_global_default().await;
            }
            return result;
        }
        self.commit(&name, candidate)?;
        self.refresh_with_global_default().await
    }

    async fn relaunch_last_good(&mut self, exe: &Path, error: anyhow::Error) -> Result<()> {
        let home = self
            .app
            .last_good_home
            .clone()
            .context("no last-good home")?;
        let last_good = self.paths.last_good();
        match self.launch(exe, &home, &last_good).await {
            Ok(()) => Err(error.context("activation failed, so the last-good configuration runs")),
            Err(recovery) => {
                let message = format!("{error:#}. Last-good recovery also failed: {recovery:#}");
                self.snap.phase = Phase::Failed(message.clone());
                Err(anyhow!(message))
            }
        }
    }

    async fn after_restart(&mut self, proxy_was_on: bool) {
        if self.core.is_none() {
            return;
        }
        let _ = self.refresh_with_global_default().await;
        if proxy_was_on && let Err(e) = self.apply_proxy(true).await {
            self.say(Topic::SystemProxy, format!("{e:#}"), true);
        }
    }

    /// The selected core file starts as root.
    fn runs_as_root(&self) -> bool {
        self.exe().is_ok_and(|(exe, _)| core::authorized(&exe))
    }

    fn running_or_err(&self) -> Result<()> {
        if self.core.is_none() {
            bail!("The core is not running");
        }
        Ok(())
    }

    /// Always remembered; applied now if the core runs, otherwise at the next start. A change
    /// closes open connections: Mihomo routes by mode only when a connection opens.
    async fn set_mode(&mut self, mode: &str) -> Result<()> {
        if !MODES.contains(&mode) {
            bail!("unknown mode {mode}");
        }
        self.app.mode = Some(mode.to_owned());
        self.app.save(&self.paths)?;
        if self.core.is_none() {
            return Ok(());
        }
        // Asked of the core, not the snapshot, so a stale view cannot skip a real change.
        if self.api.runtime().await?.mode == mode {
            return self.refresh_runtime().await;
        }
        self.api.set_mode(mode).await?;
        // After GLOBAL moves off DIRECT, so reconnecting apps cannot land on DIRECT meanwhile.
        self.refresh_with_global_default().await?;
        self.api.close_all().await
    }

    /// Closes the connections that went through `group` by another member, which keep that
    /// route until closed. Picking the current member closes nothing.
    async fn select(&mut self, group: &str, proxy: &str) -> Result<()> {
        self.running_or_err()?;
        self.api.select(group, proxy).await?;
        for conn in self.api.connections().await? {
            if routed_elsewhere(&conn.chains, group, proxy) {
                self.api.close_connection(&conn.id).await?;
            }
        }
        self.refresh_runtime().await
    }

    async fn refresh_runtime(&mut self) -> Result<()> {
        let core = self.api.runtime().await?;
        self.snap.mode = core.mode;
        self.snap.tun_device = core.tun_device;
        (self.snap.groups, self.snap.nodes) = self.api.proxies(&self.settings.latency_url).await?;
        Ok(())
    }

    /// After a (re)load: applies the remembered mode, which `last-good.yaml` may predate, and
    /// moves GLOBAL off DIRECT, where Mihomo starts it and where global acts like direct.
    /// Only run after a start, a profile switch, a Reload or a mode switch, so a later manual
    /// GLOBAL choice stays (Mihomo's cache.db remembers it per profile).
    async fn refresh_with_global_default(&mut self) -> Result<()> {
        self.refresh_runtime().await?;
        if let Some(mode) = self.app.mode.clone()
            && mode != self.snap.mode
        {
            self.api.set_mode(&mode).await?;
            self.refresh_runtime().await?;
        }
        if self.snap.mode == "global"
            && let Some(pick) = global_default(&self.snap.groups)
        {
            self.api.select("GLOBAL", &pick).await?;
            self.refresh_runtime().await?;
        }
        Ok(())
    }

    /// False when `profiles/` could not be listed; the previous list stays on screen.
    fn refresh_profiles(&mut self) -> bool {
        match profiles::list(&self.paths) {
            Ok(mut list) => {
                self.clear(Topic::ProfileList);
                let order = profiles::arrange(&mut list, &self.app.profile_order);
                self.snap.profiles = list;
                if order != self.app.profile_order {
                    self.app.profile_order = order;
                    if let Err(e) = self.app.save(&self.paths) {
                        self.say(Topic::ProfileList, format!("{e:#}"), true);
                    }
                }
                true
            }
            Err(e) => {
                self.say(Topic::ProfileList, format!("{e:#}"), true);
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
            bail!("{name} is the active profile. Switch to another one first");
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
        match proxy {
            Ok(()) => self.clear(Topic::ProxyCheck),
            Err(e) => self.say(Topic::ProxyCheck, format!("{e:#}"), true),
        }
        self.snap.lan_ip = proxy::lan_ip();
        // Logged only: a slow controller must not leave a message on every tick.
        if let Err(e) = self.follow_core().await {
            self.log(true, &format!("{e:#}"));
        }
        // Without a trustworthy list every home would look orphaned.
        if listed {
            self.reconcile();
        }
    }

    /// Follows what another tool changed in the running core (mode, a proxy port that serves,
    /// LAN access) and reports what it cannot follow, then refreshes mode and groups. Commands
    /// run one at a time, so this never interleaves with an apply.
    async fn follow_core(&mut self) -> Result<()> {
        if self.core.is_none() {
            return Ok(());
        }
        let core = self.api.runtime().await?;
        if !self.snap.mode.is_empty()
            && core.mode != self.snap.mode
            && MODES.contains(&core.mode.as_str())
        {
            self.app.mode = Some(core.mode.clone());
            self.app.save(&self.paths)?;
            self.log(
                false,
                &format!("Mode changed to {} by another tool", core.mode),
            );
        }
        let (port, saved) = (core.mixed_port, self.settings.mixed_port);
        if port == saved {
            self.clear(Topic::ExternalPort);
        } else {
            if port != 0 && core::listening(port) {
                Settings::write_key(&self.paths, "mixed_port", port)?;
                self.settings.mixed_port = port;
                let moved = self.move_proxy(saved).await;
                self.say(
                    Topic::ExternalPort,
                    format!("Changed to {port} by another tool"),
                    false,
                );
                moved?;
            } else {
                let text = format!("Another tool set port {port}. Reload restores {saved}");
                self.say_once(Topic::ExternalPort, text, true);
            }
        }
        if core.allow_lan != self.settings.allow_lan {
            Settings::write_key(&self.paths, "allow_lan", core.allow_lan)?;
            self.settings.allow_lan = core.allow_lan;
            let state = if core.allow_lan { "on" } else { "off" };
            let text = format!("LAN access turned {state} by another tool");
            self.say(Topic::Lan, text, false);
        }
        // `/upgrade` swaps the executable in place, bypassing xihomo's checks.
        if self.api.version().await.ok() != self.app.core_version {
            let text = "The core was replaced by another tool".to_owned();
            self.say_once(Topic::CoreReplaced, text, false);
        }
        self.refresh_runtime().await
    }

    /// Replaces a running stream, so a new level takes effect at once.
    fn spawn_log_task(&mut self) {
        if let Some(task) = self.log_task.take() {
            task.abort();
        }
        let (api, logs, log_tx) = (self.api.clone(), self.logs.clone(), self.log_tx.clone());
        let level = self.settings.log_level.as_str();
        self.log_task = Some(tokio::task::spawn_local(async move {
            loop {
                if let Ok(mut resp) = api.logs(level).await {
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

    /// Downloads can be slow, so they run beside the actor and report back.
    fn update_providers(&mut self) -> Result<()> {
        self.running_or_err()?;
        self.check_edits();
        self.snap.providers_busy = true;
        let (api, tx) = (self.api.clone(), self.tx.clone());
        tokio::task::spawn_local(async move {
            let result = api.update_providers().await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Cmd::ProvidersUpdated(result));
        });
        Ok(())
    }

    async fn on_providers_updated(
        &mut self,
        result: Result<(usize, Vec<String>), String>,
    ) -> Result<()> {
        self.snap.providers_busy = false;
        let (updated, failed) = result.map_err(|e| anyhow!("Updating providers failed: {e}"))?;
        if self.core.is_some() {
            self.refresh_runtime().await?;
        }
        if !failed.is_empty() {
            bail!("Updated {updated} providers. Failed: {}", failed.join(", "));
        }
        let text = match updated {
            0 => "This profile has no providers".to_owned(),
            1 => "Updated 1 provider".to_owned(),
            n => format!("Updated {n} providers"),
        };
        self.say(Topic::Providers, text, false);
        Ok(())
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
        // Against every version, the built-in one included.
        let have = update::versions(&self.paths, self.builtin.as_ref());
        if have.iter().any(|v| !update::newer(&release.version, v)) {
            self.say(Topic::CoreUpdate, "Up to date".to_owned(), false);
            self.snap.latest = None;
        } else {
            self.snap.latest = Some(release);
        }
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
        if self.builtin.as_ref().is_some_and(|b| b.version == version) {
            bail!("{version} is built in");
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
        let Some(mut exe) = update::valid_version(target)
            .then(|| self.resolve(target))
            .flatten()
        else {
            bail!("{target} is not installed");
        };
        let last_good = self.paths.last_good();
        if let Some(home) = self
            .app
            .last_good_home
            .clone()
            .filter(|_| last_good.exists())
        {
            profiles::reown(&last_good, &self.runtime())?;
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
            // The new file is not authorized yet; asked while the old core still runs.
            if self.app.tun_on {
                self.tun = self.tun_allowed(&exe, target, true).await;
                // The built-in core's authorization made a copy, which it resolves to now.
                exe = self
                    .resolve(target)
                    .with_context(|| format!("{target} is not installed"))?;
            }
            let (old_exe, old_version) = self.exe()?;
            self.read_proxy_state();
            let proxy_was_on = self.proxy.ours;
            self.release_proxy().await?;
            self.stop_child().await?;
            self.cancel.store(false, Ordering::SeqCst);
            if let Err(e) = self.launch(&exe, &home, &last_good).await {
                let result = match self.launch(&old_exe, &home, &last_good).await {
                    Ok(()) => Err(e.context(format!(
                        "{target} failed to start, so {old_version} runs again"
                    ))),
                    Err(recovery) => {
                        let message = format!("{e:#}. {old_version} also failed: {recovery:#}");
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
