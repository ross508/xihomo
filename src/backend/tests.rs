//! Lifecycle tests against the real core at dev/bin/mihomo with synthetic profiles.
//! They skip when that ignored binary is absent, and never touch system proxy settings.
use super::*;
use crate::settings::temp_paths;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicU16;

const VERSION: &str = "v1.19.33";
static NEXT_PORT: AtomicU16 = AtomicU16::new(0);

fn real_core() -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("dev/bin/mihomo");
    path.exists().then_some(path)
}

const GOOD: &str = "mode: rule\nmixed-port: 7890\nsecret: from-source\nproxy-groups:\n  - {name: PICK, type: select, proxies: [DIRECT, REJECT]}\nrules:\n  - MATCH,PICK\n";

fn actor(name: &str) -> Option<(Actor, mpsc::UnboundedReceiver<Cmd>)> {
    let core = real_core()?;
    let paths = temp_paths(name);
    fs::create_dir_all(paths.bin().join(VERSION)).unwrap();
    std::os::unix::fs::symlink(core, paths.core_exe(VERSION)).unwrap();
    fs::write(paths.profiles().join("good.yaml"), GOOD).unwrap();
    // Distinct, per-process ports so parallel tests and any real instance cannot collide.
    // Four per test: mixed, controller and two spares from `spare`.
    let base =
        21000 + (std::process::id() % 500) as u16 * 80 + NEXT_PORT.fetch_add(4, Ordering::SeqCst);
    Settings {
        mixed_port: base,
        controller_port: base + 1,
        ..Settings::default()
    }
    .save(&paths)
    .unwrap();
    AppData {
        core_version: Some(VERSION.into()),
        active_profile: Some("good".into()),
        ..AppData::default()
    }
    .save(&paths)
    .unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let (actor, ..) = Actor::new(paths, tx, Arc::new(AtomicBool::new(false))).unwrap();
    Some((actor, rx))
}

macro_rules! local_test {
    ($name:ident, $actor:ident, $rx:ident, $body:block) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $name() {
            let Some((mut $actor, mut $rx)) = actor(stringify!($name)) else {
                eprintln!("skipped: dev/bin/mihomo not present");
                return;
            };
            let _ = &mut $rx;
            tokio::task::LocalSet::new()
                .run_until(async { $body })
                .await;
        }
    };
}

/// A free port next to the actor's own two.
fn spare(a: &Actor, n: u16) -> u16 {
    a.settings.mixed_port + 1 + n
}

fn saved_settings(a: &Actor) -> Settings {
    Settings::load(&a.paths).unwrap().0
}

fn message(a: &Actor, area: Area) -> String {
    a.snap
        .message(area)
        .map(|m| m.text.clone())
        .unwrap_or_default()
}

fn saved(actor: &Actor) -> AppData {
    AppData::load(&actor.paths).unwrap().0
}

local_test!(start_control_stop, a, _rx, {
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.phase, Phase::Running, "{:?}", a.snap.error());
    assert_eq!(a.api.version().await.unwrap(), VERSION);

    // The source's secret and port are gone: the generated secret is the one that authenticates.
    assert!(a.paths.last_good().exists());
    let wrong = Api::new(a.settings.controller_port, "from-source").unwrap();
    assert!(wrong.version().await.is_err());

    let group = a.snap.groups.iter().find(|g| g.name == "PICK").unwrap();
    assert_eq!(group.now, "DIRECT");
    a.handle(Cmd::Select {
        group: "PICK".into(),
        proxy: "REJECT".into(),
    })
    .await;
    assert_eq!(
        a.snap.groups.iter().find(|g| g.name == "PICK").unwrap().now,
        "REJECT"
    );
    a.handle(Cmd::SetMode("global".into())).await;
    assert_eq!(a.snap.mode, "global");

    // A second Start is a no-op, not a second process.
    a.handle(Cmd::Start).await;
    assert!(a.snap.error().is_none());

    a.handle(Cmd::Stop).await;
    assert_eq!(a.snap.phase, Phase::Stopped);
    assert!(a.core.is_none());
    core::ensure_ports_free(&[a.settings.mixed_port, a.settings.controller_port])
        .await
        .unwrap();
});

local_test!(
    log_level_changes_the_running_core_without_a_restart,
    a,
    _rx,
    {
        a.handle(Cmd::Start).await;
        assert_eq!(a.snap.phase, Phase::Running, "{:?}", a.snap.error());
        let pid = a.core.as_ref().unwrap().pid();
        let level = async |a: &Actor| {
            let body: serde_json::Value = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(format!(
                    "http://127.0.0.1:{}/configs",
                    a.settings.controller_port
                ))
                .bearer_auth(&a.secret)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            body["log-level"].as_str().unwrap().to_owned()
        };
        assert_eq!(level(&a).await, "info");
        a.handle(Cmd::SetLogLevel(LogLevel::Debug)).await;
        assert!(
            a.snap.message(Area::Update).is_none(),
            "{}",
            message(&a, Area::Update)
        );
        assert_eq!(level(&a).await, "debug");
        assert_eq!(a.core.as_ref().unwrap().pid(), pid, "the same process");
        a.publish();
        assert_eq!(a.snap.log_level, LogLevel::Debug);
        assert_eq!(saved_settings(&a).log_level, LogLevel::Debug);
        a.handle(Cmd::Stop).await;
    }
);

/// A tunnel through the core's mixed port to a loopback listener of the test's own, returned
/// with the listener's end so neither closes early.
async fn tunnel(a: &Actor) -> (std::net::TcpStream, std::net::TcpStream) {
    use std::io::{Read, Write};
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = target.local_addr().unwrap().port();
    let mut tunnel = std::net::TcpStream::connect(("127.0.0.1", a.settings.mixed_port)).unwrap();
    tunnel
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        tunnel,
        "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
    )
    .unwrap();
    let (accepted, _) = target.accept().unwrap();
    let mut reply = [0u8; 128];
    let n = tunnel.read(&mut reply).unwrap();
    assert!(
        reply[..n].starts_with(b"HTTP/1.1 200"),
        "{:?}",
        String::from_utf8_lossy(&reply[..n])
    );
    (tunnel, accepted)
}

/// The core closed its side, so the client sees the end of the tunnel.
fn closed(mut tunnel: std::net::TcpStream) -> bool {
    std::io::Read::read(&mut tunnel, &mut [0u8; 16]).unwrap() == 0
}

async fn open(a: &Actor) -> usize {
    a.api.connections().await.unwrap().len()
}

local_test!(mode_changes_close_open_connections, a, _rx, {
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.phase, Phase::Running, "{:?}", a.snap.error());
    let (first, _far) = tunnel(&a).await;
    assert_eq!(open(&a).await, 1);

    a.handle(Cmd::SetMode(a.snap.mode.clone())).await;
    assert!(a.snap.error().is_none(), "{:?}", a.snap.error());
    assert_eq!(open(&a).await, 1, "the current mode again changes nothing");

    a.handle(Cmd::SetMode("global".into())).await;
    assert!(a.snap.error().is_none(), "{:?}", a.snap.error());
    assert_eq!(open(&a).await, 0);
    assert!(closed(first));
    a.handle(Cmd::Stop).await;
});

local_test!(a_reload_closes_open_connections, a, _rx, {
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.phase, Phase::Running, "{:?}", a.snap.error());
    let (first, _far) = tunnel(&a).await;
    a.handle(Cmd::Reload).await;
    assert!(a.snap.error().is_none(), "{:?}", a.snap.error());
    assert_eq!(open(&a).await, 0);
    assert!(closed(first));
    a.handle(Cmd::Stop).await;
});

local_test!(a_new_pick_closes_connections_through_the_old_one, a, _rx, {
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.phase, Phase::Running, "{:?}", a.snap.error());
    let (first, _far) = tunnel(&a).await;
    let pick = |proxy: &str| Cmd::Select {
        group: "PICK".into(),
        proxy: proxy.into(),
    };
    a.handle(pick("DIRECT")).await;
    assert!(a.snap.error().is_none(), "{:?}", a.snap.error());
    assert_eq!(open(&a).await, 1, "the current pick again changes nothing");

    a.handle(pick("REJECT")).await;
    assert!(a.snap.error().is_none(), "{:?}", a.snap.error());
    assert_eq!(open(&a).await, 0);
    assert!(closed(first));
    a.handle(Cmd::Stop).await;
});

#[test]
fn only_connections_through_the_group_by_another_member_are_stale() {
    let chain = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    let nested = chain(&["node-a", "Inner", "Outer"]);
    assert!(routed_elsewhere(&nested, "Inner", "node-b"));
    assert!(!routed_elsewhere(&nested, "Inner", "node-a"));
    assert!(routed_elsewhere(&nested, "Outer", "Other"));
    assert!(!routed_elsewhere(&nested, "Outer", "Inner"));
    assert!(!routed_elsewhere(&nested, "Elsewhere", "node-a"));
    assert!(!routed_elsewhere(&chain(&["Inner"]), "Inner", "node-a"));
}

local_test!(failed_activation_keeps_selection_and_core, a, _rx, {
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.phase, Phase::Running);
    let last_good = fs::read(a.paths.last_good()).unwrap();

    // Parses as YAML, so only Mihomo's own validation can reject it.
    fs::write(
        a.paths.profiles().join("bad.yaml"),
        "proxy-groups:\n  - {name: X, type: nonsense}\n",
    )
    .unwrap();
    a.handle(Cmd::Activate("bad".into())).await;
    assert!(
        a.snap.error().as_deref().unwrap().contains("rejected"),
        "{:?}",
        a.snap.error()
    );
    assert_eq!(saved(&a).active_profile.as_deref(), Some("good"));
    assert_eq!(fs::read(a.paths.last_good()).unwrap(), last_good);
    assert!(a.api.version().await.is_ok(), "core untouched");

    fs::write(
        a.paths.profiles().join("other.yaml"),
        GOOD.replace("mode: rule", "mode: direct"),
    )
    .unwrap();
    a.handle(Cmd::Activate("other".into())).await;
    assert_eq!(a.snap.error(), None);
    assert_eq!(saved(&a).active_profile.as_deref(), Some("other"));
    assert_eq!(a.snap.mode, "direct");
    assert_eq!(a.core.as_ref().unwrap().home, a.paths.home("other"));

    // The running core's home survives reconciliation even after its profile file is gone.
    fs::remove_file(a.paths.profiles().join("other.yaml")).unwrap();
    a.handle(Cmd::CheckNow).await;
    assert!(a.paths.home("other").is_dir());
    a.handle(Cmd::Stop).await;
});

local_test!(external_edit_is_only_offered_never_applied, a, _rx, {
    a.handle(Cmd::Start).await;
    a.handle(Cmd::Refresh).await;
    assert!(!a.snap.edited);
    let path = a.paths.profiles().join("good.yaml");
    fs::write(&path, GOOD.replace("mode: rule", "mode: direct")).unwrap();
    a.periodic().await;
    assert!(a.snap.edited);
    a.handle(Cmd::Refresh).await;
    assert_eq!(a.snap.mode, "rule", "no automatic network-affecting reload");

    // A broken save: reload fails, the running config stays, the offer stays.
    fs::write(&path, "a: [").unwrap();
    a.handle(Cmd::Reload).await;
    assert!(a.snap.error().is_some());
    a.check_edits();
    assert!(a.snap.edited);
    assert!(a.api.version().await.is_ok());

    fs::write(&path, GOOD.replace("mode: rule", "mode: direct")).unwrap();
    a.handle(Cmd::Reload).await;
    assert_eq!((a.snap.mode.as_str(), a.snap.edited), ("direct", false));
    a.handle(Cmd::Stop).await;
});

local_test!(start_recovers_with_last_good_once, a, _rx, {
    a.handle(Cmd::Start).await;
    a.handle(Cmd::Stop).await;
    fs::remove_file(a.paths.profiles().join("good.yaml")).unwrap();
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.phase, Phase::Running);
    assert!(
        a.snap
            .message(Area::Core)
            .unwrap()
            .text
            .contains("last-good")
    );
    assert!(a.snap.edited);
    a.handle(Cmd::Stop).await;

    // Nothing to fall back on: fail once and stay failed.
    fs::remove_file(a.paths.last_good()).unwrap();
    a.handle(Cmd::Start).await;
    assert!(matches!(a.snap.phase, Phase::Failed(_)));
    assert!(a.core.is_none());
});

local_test!(occupied_port_refuses_start, a, _rx, {
    let squatter = std::net::TcpListener::bind(("127.0.0.1", a.settings.controller_port)).unwrap();
    a.handle(Cmd::Start).await;
    assert!(a.core.is_none());
    match &a.snap.phase {
        Phase::Failed(message) => assert!(message.contains("Settings → API"), "{message}"),
        other => panic!("{other:?}"),
    }
    let port = a.settings.controller_port;
    assert_eq!(
        message(&a, Area::Api),
        format!("Port {port} is in use; change it here")
    );
    assert!(squatter.local_addr().is_ok());
    drop(squatter);
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.phase, Phase::Running);
    assert_eq!(a.snap.message(Area::Api), None, "resolved");
    a.handle(Cmd::Stop).await;
});

local_test!(a_proxy_port_change_applies_by_reload, a, _rx, {
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.phase, Phase::Running, "{:?}", a.snap.error());
    let pid = a.core.as_ref().unwrap().pid();
    let (first, _far) = tunnel(&a).await;
    let (old, new) = (a.settings.mixed_port, spare(&a, 1));

    // Refused before anything changes: privileged, the API's own, or in use.
    let squatter = std::net::TcpListener::bind(("127.0.0.1", spare(&a, 2))).unwrap();
    for (port, why) in [
        (80, "1024 to 65535".to_owned()),
        (70000, "1024 to 65535".to_owned()),
        (u32::from(a.settings.controller_port), "API port".to_owned()),
        (u32::from(spare(&a, 2)), "in use".to_owned()),
    ] {
        a.handle(Cmd::SetProxyPort(port)).await;
        assert!(message(&a, Area::Listen).contains(&why), "{port}");
        assert_eq!(a.settings.mixed_port, old);
    }
    drop(squatter);

    a.handle(Cmd::SetProxyPort(u32::from(new))).await;
    assert_eq!(message(&a, Area::Listen), "Applied");
    assert_eq!(a.snap.applying, None);
    assert_eq!(a.core.as_ref().unwrap().pid(), pid, "no restart");
    assert_eq!(a.api.runtime().await.unwrap().mixed_port, new);
    assert_eq!(saved_settings(&a).mixed_port, new);
    for file in [a.paths.last_good(), a.paths.config()] {
        assert!(
            fs::read_to_string(file)
                .unwrap()
                .contains(&format!("mixed-port: {new}"))
        );
    }
    assert!(closed(first), "like Reload");
    let (_second, _far) = tunnel(&a).await;
    core::ensure_ports_free(&[old]).await.unwrap();
    a.handle(Cmd::Stop).await;
});

local_test!(
    sharing_with_the_network_applies_without_closing_connections,
    a,
    _rx,
    {
        a.handle(Cmd::Start).await;
        let (_open, _far) = tunnel(&a).await;
        a.handle(Cmd::SetAllowLan(true)).await;
        assert!(a.snap.error().is_none(), "{:?}", a.snap.error());
        assert!(a.api.runtime().await.unwrap().allow_lan);
        assert!(saved_settings(&a).allow_lan);
        assert_eq!(open(&a).await, 1, "connections stay");
        a.handle(Cmd::SetAllowLan(false)).await;
        assert!(!a.api.runtime().await.unwrap().allow_lan);
        assert_eq!(
            message(&a, Area::Listen),
            "Only this device can use the proxy"
        );
        a.handle(Cmd::Stop).await;
    }
);

local_test!(api_changes_restart_the_core_with_the_new_values, a, _rx, {
    a.handle(Cmd::Start).await;
    let pid = a.core.as_ref().unwrap().pid();
    let (old_port, new_port) = (a.settings.controller_port, spare(&a, 1));
    a.handle(Cmd::SetApiPort(u32::from(a.settings.mixed_port)))
        .await;
    assert!(message(&a, Area::Api).contains("proxy port"));

    a.handle(Cmd::SetApiPort(u32::from(new_port))).await;
    assert_eq!(message(&a, Area::Api), "Applied", "{:?}", a.snap.error());
    assert_eq!(a.snap.phase, Phase::Running);
    assert_ne!(a.core.as_ref().unwrap().pid(), pid, "a new process");
    assert_eq!(saved_settings(&a).controller_port, new_port);
    assert!(
        Api::new(old_port, &a.secret)
            .unwrap()
            .version()
            .await
            .is_err()
    );
    assert_eq!(a.api.version().await.unwrap(), VERSION);

    let old_secret = a.secret.clone();
    a.handle(Cmd::RegenerateSecret).await;
    assert!(message(&a, Area::Api).starts_with("Regenerated"));
    assert_ne!(a.secret, old_secret);
    assert_eq!(a.paths.secret().unwrap(), (a.secret.clone(), false));
    assert!(
        Api::new(new_port, &old_secret)
            .unwrap()
            .version()
            .await
            .is_err()
    );
    assert!(a.api.version().await.is_ok());

    // The log task follows the new client: a reload's lines still arrive.
    a.logs.lock().unwrap().clear();
    a.handle(Cmd::Reload).await;
    let mut seen = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        seen = a
            .logs
            .lock()
            .unwrap()
            .iter()
            .any(|l| !l.text.starts_with("xihomo"));
        if seen {
            break;
        }
    }
    assert!(seen, "no core log lines after the restart");
    a.handle(Cmd::Stop).await;
});

local_test!(
    changes_while_stopped_are_saved_for_the_next_start,
    a,
    _rx,
    {
        let (proxy, api) = (spare(&a, 1), spare(&a, 2));
        a.handle(Cmd::SetProxyPort(u32::from(proxy))).await;
        assert_eq!(message(&a, Area::Listen), "Saved; applies at next start");
        a.handle(Cmd::SetApiPort(u32::from(api))).await;
        assert_eq!(message(&a, Area::Api), "Saved; applies at next start");
        a.handle(Cmd::RegenerateSecret).await;
        let s = saved_settings(&a);
        assert_eq!((s.mixed_port, s.controller_port), (proxy, api));
        a.handle(Cmd::Start).await;
        assert_eq!(a.snap.phase, Phase::Running, "{:?}", a.snap.error());
        assert_eq!(a.api.runtime().await.unwrap().mixed_port, proxy);
        a.handle(Cmd::Stop).await;
    }
);

local_test!(
    last_good_recovery_uses_the_current_ports_and_secret,
    a,
    _rx,
    {
        a.handle(Cmd::Start).await;
        a.handle(Cmd::Stop).await;
        let (proxy, api) = (spare(&a, 1), spare(&a, 2));
        a.handle(Cmd::SetProxyPort(u32::from(proxy))).await;
        a.handle(Cmd::SetApiPort(u32::from(api))).await;
        a.handle(Cmd::RegenerateSecret).await;
        fs::remove_file(a.paths.profiles().join("good.yaml")).unwrap();
        a.handle(Cmd::Start).await;
        assert_eq!(a.snap.phase, Phase::Running, "{:?}", a.snap.phase);
        assert!(message(&a, Area::Core).contains("last-good"));
        assert!(
            a.snap.message(Area::Core).unwrap().error,
            "stays until resolved"
        );
        assert_eq!(a.api.runtime().await.unwrap().mixed_port, proxy);
        a.handle(Cmd::Stop).await;
    }
);

local_test!(
    a_rejected_candidate_never_replaces_the_running_file,
    a,
    _rx,
    {
        a.handle(Cmd::Start).await;
        let running = fs::read(a.paths.config()).unwrap();
        fs::write(
            a.paths.profiles().join("good.yaml"),
            "proxy-groups:\n  - {name: X, type: nonsense}\n",
        )
        .unwrap();
        a.handle(Cmd::Reload).await;
        assert!(a.snap.error().unwrap().contains("rejected"));
        assert_eq!(fs::read(a.paths.config()).unwrap(), running);
        assert_ne!(fs::read(a.paths.candidate()).unwrap(), running);
        a.handle(Cmd::Stop).await;
    }
);

local_test!(
    changes_made_by_another_tool_are_followed_or_reported,
    a,
    _rx,
    {
        a.handle(Cmd::Start).await;
        let dashboard = Api::new(a.settings.controller_port, &a.secret).unwrap();
        let (kept, moved) = (a.settings.mixed_port, spare(&a, 1));

        dashboard
            .patch(serde_json::json!({"mode": "global"}))
            .await
            .unwrap();
        a.handle(Cmd::Refresh).await;
        assert_eq!(
            (a.snap.mode.as_str(), saved(&a).mode.as_deref()),
            ("global", Some("global"))
        );

        // A port that serves nothing is only reported, and Reload restores the saved one.
        dashboard
            .patch(serde_json::json!({"mixed-port": 0}))
            .await
            .unwrap();
        a.periodic().await;
        assert_eq!(
            message(&a, Area::Listen),
            format!("Another tool set port 0; Reload restores {kept}")
        );
        assert_eq!(saved_settings(&a).mixed_port, kept);
        a.handle(Cmd::Reload).await;
        assert_eq!(a.api.runtime().await.unwrap().mixed_port, kept);
        a.periodic().await;
        assert_eq!(a.snap.message(Area::Listen), None, "resolved");

        dashboard
            .patch(serde_json::json!({"mixed-port": moved, "allow-lan": true}))
            .await
            .unwrap();
        a.periodic().await;
        let s = saved_settings(&a);
        assert_eq!(
            (a.settings.mixed_port, s.mixed_port, s.allow_lan),
            (moved, moved, true)
        );
        assert_eq!(
            message(&a, Area::Listen),
            "LAN access turned on by another tool"
        );
        a.publish();
        assert!(a.snap.allow_lan);
        a.handle(Cmd::Reload).await;
        assert_eq!(
            a.snap.error(),
            None,
            "the followed port confirms after a reload"
        );

        a.app.core_version = Some("v0.0.1".into());
        a.periodic().await;
        assert_eq!(
            message(&a, Area::Update),
            "The core was replaced by another tool"
        );
        a.app.core_version = Some(VERSION.into());
        a.handle(Cmd::Stop).await;
    }
);

local_test!(update_refreshes_the_running_profiles_providers, a, rx, {
    a.handle(Cmd::UpdateProviders).await;
    assert!(a.snap.error().unwrap().contains("not running"));
    a.handle(Cmd::Start).await;
    a.handle(Cmd::UpdateProviders).await;
    assert!(a.snap.providers_busy);
    // A reload in between would race the downloads.
    a.handle(Cmd::Reload).await;
    assert!(message(&a, Area::Profiles).starts_with("Reload waits"));
    let done = rx.recv().await.unwrap();
    a.handle(done).await;
    assert_eq!(message(&a, Area::Profiles), "This profile has no providers");

    // File providers are re-read from the profile's home, all of them at once.
    fs::write(
        a.paths.home("good").join("nodes.yaml"),
        "proxies:\n  - {name: n1, type: http, server: 127.0.0.1, port: 1}\n",
    )
    .unwrap();
    fs::write(
        a.paths.home("good").join("local.yaml"),
        "payload:\n  - example.com\n",
    )
    .unwrap();
    fs::write(
        a.paths.profiles().join("good.yaml"),
        GOOD.replace(
            "proxy-groups:",
            "proxy-providers:\n  nodes: {type: file, path: ./nodes.yaml}\nrule-providers:\n  local: {type: file, behavior: domain, path: ./local.yaml}\nproxy-groups:",
        ),
    )
    .unwrap();
    a.handle(Cmd::Reload).await;
    assert_eq!(a.snap.error(), None);
    a.handle(Cmd::UpdateProviders).await;
    let done = rx.recv().await.unwrap();
    a.handle(done).await;
    assert!(!a.snap.providers_busy);
    assert_eq!(message(&a, Area::Profiles), "Updated 2 providers");
    a.handle(Cmd::Stop).await;
});

#[tokio::test(flavor = "current_thread")]
async fn a_lost_secret_is_reported_except_on_a_first_run() {
    let paths = temp_paths("secret-report");
    let (tx, _rx) = mpsc::unbounded_channel();
    let a = Actor::new(paths.clone(), tx.clone(), Arc::new(AtomicBool::new(false)))
        .unwrap()
        .0;
    assert_eq!(a.snap.message(Area::Api), None, "first run");
    AppData::default().save(&paths).unwrap();
    fs::write(paths.state().join("secret"), "short").unwrap();
    let a = Actor::new(paths, tx, Arc::new(AtomicBool::new(false)))
        .unwrap()
        .0;
    assert!(message(&a, Area::Api).contains("update your dashboards"));
}

local_test!(unexpected_exit_fails_without_restart, a, rx, {
    a.handle(Cmd::Start).await;
    let pid = a.core.as_ref().unwrap().pid();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let status = wait_core(&mut a.core).await;
    a.on_core_exit(status).await;
    assert!(matches!(&a.snap.phase, Phase::Failed(m) if m.contains("unexpectedly")));
    assert!(a.core.is_none());
    assert!(
        rx.try_recv().is_err(),
        "nothing queued: no restart, and proxy was off"
    );
});

local_test!(core_crash_queues_proxy_restore, a, rx, {
    a.handle(Cmd::Start).await;
    // Marks the state only; this test never changes real proxy settings.
    a.read_proxy = |_| proxy::State {
        on: true,
        ours: true,
    };
    let pid = a.core.as_ref().unwrap().pid();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let status = wait_core(&mut a.core).await;
    a.on_core_exit(status).await;
    assert!(matches!(rx.try_recv(), Ok(Cmd::GuardProxy)));
});

local_test!(broken_core_update_rolls_back_to_running_version, a, _rx, {
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.phase, Phase::Running);

    // Passes `-t` but cannot serve: the failure appears only at startup, the hard recovery case.
    let bad = a.paths.core_exe("v9.9.9");
    fs::create_dir_all(bad.parent().unwrap()).unwrap();
    fs::write(
        &bad,
        "#!/bin/sh\n[ \"$1\" = -t ] && exit 0\necho boom\nexit 3\n",
    )
    .unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o755)).unwrap();

    a.handle(Cmd::ActivateVersion("v9.9.9".into())).await;
    let error = a.snap.error().clone().unwrap();
    assert!(
        error.contains("back on") && error.contains("boom"),
        "{error}"
    );
    assert_eq!(a.snap.phase, Phase::Running);
    assert_eq!(a.api.version().await.unwrap(), VERSION);
    assert_eq!(
        saved(&a).core_version.as_deref(),
        Some(VERSION),
        "selection never moved"
    );
    a.handle(Cmd::Stop).await;
});

local_test!(version_switch_while_stopped_and_rollback, a, _rx, {
    // Another real version, by name. Validation runs against it like any other.
    let other = a.paths.core_exe("v1.0.0");
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(real_core().unwrap(), &other).unwrap();
    a.handle(Cmd::ActivateVersion("v1.0.0".into())).await;
    assert_eq!(a.snap.error(), None);
    let s = saved(&a);
    assert_eq!(
        (
            s.core_version.as_deref(),
            s.previous_core_version.as_deref()
        ),
        (Some("v1.0.0"), Some(VERSION))
    );
    a.handle(Cmd::ActivateVersion(VERSION.into())).await;
    let s = saved(&a);
    assert_eq!(
        (
            s.core_version.as_deref(),
            s.previous_core_version.as_deref()
        ),
        (Some(VERSION), Some("v1.0.0"))
    );
    a.handle(Cmd::ActivateVersion("../../evil".into())).await;
    assert!(a.snap.error().as_deref().unwrap().contains("not installed"));
});

local_test!(logs_stream_into_the_bounded_buffer, a, _rx, {
    a.handle(Cmd::Start).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // A reload logs at info level, proving the stream is live end to end.
    a.handle(Cmd::Reload).await;
    let mut seen = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        seen = a.logs.lock().unwrap().iter().count() > 0;
        if seen {
            break;
        }
    }
    assert!(seen, "no log lines arrived");
    a.handle(Cmd::Stop).await;
    assert!(a.log_task.is_none());
});

local_test!(connection_polling_stops_when_hidden, a, _rx, {
    a.handle(Cmd::Start).await;
    a.handle(Cmd::WatchConnections(true)).await;
    assert!(a.conn_task.is_some());
    a.handle(Cmd::WatchConnections(false)).await;
    assert!(a.conn_task.is_none());
    a.handle(Cmd::Stop).await;
});

/// Dev profiles are private: only pass/fail is observable, never their contents or Mihomo's output.
#[tokio::test(flavor = "current_thread")]
async fn dev_profiles_validate_after_overrides() {
    let Some(exe) = real_core() else { return };
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("dev/mihomo-valid-configs-for-dev");
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let paths = temp_paths("devprofile");
        let yaml = profiles::compose(
            &fs::read(&path).unwrap(),
            b"",
            &profiles::Runtime::new(&Settings::default(), "x", None),
        )
        .unwrap_or_else(|_| panic!("composition failed"))
        .yaml;
        write_private(&paths.candidate(), yaml.as_bytes()).unwrap();
        let home = paths.data.join("home");
        fs::create_dir_all(&home).unwrap();
        let ok = core::validate(&exe, &paths, &home, &paths.candidate(), "x")
            .await
            .is_ok();
        assert!(ok, "a dev profile failed validation after overrides");
    }
}

/// No core needed: an unread message survives background work, a user action in its area
/// replaces it, and actions in other areas leave it alone.
#[tokio::test(flavor = "current_thread")]
async fn messages_stay_next_to_their_area() {
    let paths = temp_paths("messages");
    let (tx, _rx) = mpsc::unbounded_channel();
    let (mut a, ..) = Actor::new(paths, tx, Arc::new(AtomicBool::new(false))).unwrap();
    a.handle(Cmd::Activate("missing".into())).await;
    let error = a.snap.message(Area::Profiles).unwrap().clone();
    assert!(
        error.error && error.text.contains("No Mihomo core"),
        "{error:?}"
    );
    assert!(
        a.logs
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.text.contains("No Mihomo core")),
        "every message is also in Logs"
    );
    for cmd in [
        Cmd::Refresh,
        Cmd::WatchConnections(true),
        Cmd::SetIconSet("bars".into()),
    ] {
        a.handle(cmd).await;
        assert_eq!(a.snap.message(Area::Profiles), Some(&error));
    }
    a.handle(Cmd::CheckNow).await;
    assert_eq!(a.snap.message(Area::Profiles), None);
}

/// An actor with no core, for the file-handling paths.
fn idle(name: &str) -> Actor {
    let paths = temp_paths(name);
    let (tx, _rx) = mpsc::unbounded_channel();
    Actor::new(paths, tx, Arc::new(AtomicBool::new(false)))
        .unwrap()
        .0
}

#[tokio::test(flavor = "current_thread")]
async fn startup_creates_the_default_profile_once() {
    let mut a = idle("default-profile");
    a.startup().await;
    let names: Vec<_> = a.snap.profiles.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["default"]);
    assert_eq!(a.app.active_profile, None, "nothing is selected or started");
    assert!(a.core.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn theme_and_dock_choices_are_stored_in_settings_yaml() {
    let mut a = idle("theme");
    a.handle(Cmd::SetTheme(Theme::Dark)).await;
    a.handle(Cmd::SetKeepDockIcon(true)).await;
    a.publish();
    assert_eq!((a.snap.theme, a.snap.keep_dock_icon), (Theme::Dark, true));
    let (settings, _) = Settings::load(&a.paths).unwrap();
    assert_eq!(
        (settings.theme, settings.keep_dock_icon),
        (Theme::Dark, true)
    );
    assert!(
        !a.paths.app_data().exists(),
        "preferences never go to app.json"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn active_profile_cannot_be_removed() {
    let mut a = idle("remove-active");
    for name in ["a", "b"] {
        fs::write(a.paths.profiles().join(format!("{name}.yaml")), "a: 1\n").unwrap();
        fs::create_dir_all(a.paths.home(name)).unwrap();
    }
    a.app.active_profile = Some("a".into());
    a.handle(Cmd::RemoveProfile("a".into())).await;
    assert!(a.snap.error().as_deref().unwrap().contains("active"));
    assert!(a.paths.home("a").exists());
    a.handle(Cmd::RemoveProfile("b".into())).await;
    assert_eq!(a.snap.error(), None);
    assert!(!a.paths.home("b").exists());
    assert_eq!(a.snap.profiles.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn profiles_stay_in_the_order_they_were_added() {
    let mut a = idle("profile-order");
    let names =
        |a: &Actor| -> Vec<String> { a.snap.profiles.iter().map(|p| p.name.clone()).collect() };
    let sources = a.paths.data.join("sources");
    fs::create_dir_all(&sources).unwrap();
    for name in ["b", "a", "c"] {
        let file = sources.join(format!("{name}.yaml"));
        fs::write(&file, "a: 1\n").unwrap();
        a.handle(if name == "a" {
            Cmd::LinkProfile(file)
        } else {
            Cmd::ImportProfile(file)
        })
        .await;
    }
    assert_eq!(names(&a), ["b", "a", "c"]);
    assert_eq!(saved(&a).profile_order, ["b", "a", "c"]);

    // Replaced rather than edited in place, as atomic saves and `ln -sf` do: same place.
    let b = a.paths.profiles().join("b.yaml");
    fs::remove_file(&b).unwrap();
    fs::write(&b, "a: 2\n").unwrap();
    assert!(a.refresh_profiles());
    assert_eq!(names(&a), ["b", "a", "c"]);

    // Deleted, then added again: a new profile, so last.
    a.handle(Cmd::RemoveProfile("b".into())).await;
    a.handle(Cmd::ImportProfile(sources.join("b.yaml"))).await;
    assert_eq!(names(&a), ["a", "c", "b"]);

    // A failed listing forgets nothing.
    let profiles = a.paths.profiles();
    let moved = a.paths.config.join("profiles-moved");
    fs::rename(&profiles, &moved).unwrap();
    assert!(!a.refresh_profiles());
    fs::rename(&moved, &profiles).unwrap();
    assert_eq!(saved(&a).profile_order, ["a", "c", "b"]);

    // The next launch shows the recorded order from its first frame.
    let (tx, _rx) = mpsc::unbounded_channel();
    let next = Actor::new(a.paths.clone(), tx, Arc::new(AtomicBool::new(false)))
        .unwrap()
        .0;
    assert_eq!(names(&next), ["a", "c", "b"]);
}

#[tokio::test(flavor = "current_thread")]
async fn reconciliation_runs_first_then_only_on_list_changes() {
    let mut a = idle("reconcile-trigger");
    fs::write(a.paths.profiles().join("p.yaml"), "a: 1\n").unwrap();
    let orphan = a.paths.home("orphan");
    let last_good = a.paths.home("renamed");
    for home in [&orphan, &last_good] {
        fs::create_dir_all(home).unwrap();
    }
    a.app.last_good_home = Some(last_good.clone());
    a.periodic().await;
    assert!(!orphan.exists(), "the first tick always reconciles");
    assert!(last_good.exists(), "protected by path");

    // Same list: the next tick does not touch the disk.
    fs::create_dir_all(&orphan).unwrap();
    a.periodic().await;
    assert!(orphan.exists());
    a.handle(Cmd::CheckNow).await;
    assert!(!orphan.exists(), "Check now always reconciles");

    fs::create_dir_all(&orphan).unwrap();
    fs::write(a.paths.profiles().join("q.yaml"), "a: 1\n").unwrap();
    a.periodic().await;
    assert!(!orphan.exists(), "a changed list reconciles");

    // A failed listing skips the run entirely.
    fs::create_dir_all(&orphan).unwrap();
    let profiles = a.paths.profiles();
    let moved = a.paths.config.join("profiles-moved");
    fs::rename(&profiles, &moved).unwrap();
    a.handle(Cmd::CheckNow).await;
    assert!(a.snap.error().is_some());
    assert!(orphan.exists());
    fs::rename(&moved, &profiles).unwrap();
}

/// The generated first-run profile must pass the real core's own test, not just `compose`.
#[tokio::test(flavor = "current_thread")]
async fn default_profile_passes_the_real_core() {
    let Some(exe) = real_core() else { return };
    let paths = temp_paths("default-validates");
    profiles::ensure_default(&paths).unwrap();
    let candidate = profiles::write_candidate(
        &paths,
        "default",
        &profiles::Runtime::new(&Settings::default(), "x", None),
    )
    .unwrap();
    core::validate(&exe, &paths, &candidate.home, &paths.candidate(), "x")
        .await
        .unwrap();
}

fn install(a: &Actor, versions: &[&str]) {
    for v in versions {
        fs::create_dir_all(a.paths.bin().join(v)).unwrap();
        fs::write(a.paths.core_exe(v), "x").unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_downloaded_core_is_used_right_away() {
    let mut a = idle("downloaded");
    install(&a, &["v1.0.0", "v2.0.0", "v3.0.0"]);
    a.app.core_version = Some("v1.0.0".into());
    a.app.previous_core_version = Some("v3.0.0".into());
    a.handle(Cmd::Staged(Ok("v2.0.0".into()))).await;
    assert_eq!(a.snap.error(), None);
    assert_eq!(a.app.core_version.as_deref(), Some("v2.0.0"));
    assert_eq!(a.app.previous_core_version.as_deref(), Some("v1.0.0"));
    assert_eq!(update::installed(&a.paths), ["v1.0.0", "v2.0.0"]);
    assert_eq!(
        a.snap.message(Area::Update),
        None,
        "success is not announced"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn only_unused_versions_can_be_deleted() {
    let mut a = idle("delete-version");
    install(&a, &["v1.0.0", "v2.0.0"]);
    a.app.core_version = Some("v2.0.0".into());
    a.app.previous_core_version = Some("v1.0.0".into());
    a.handle(Cmd::DeleteVersion("v2.0.0".into())).await;
    assert!(a.snap.error().unwrap().contains("in use"));
    a.handle(Cmd::DeleteVersion("../state".into())).await;
    assert!(a.paths.state().exists());
    a.handle(Cmd::DeleteVersion("v1.0.0".into())).await;
    assert_eq!(a.snap.error(), None);
    assert_eq!(update::installed(&a.paths), ["v2.0.0"]);
    assert_eq!(saved(&a).previous_core_version, None);
}

#[tokio::test(flavor = "current_thread")]
async fn startup_uses_a_core_placed_by_hand_instead_of_deleting_it() {
    let mut a = idle("adopt-core");
    install(&a, &["v1.9.0", "v1.19.33"]);
    a.startup().await;
    assert_eq!(a.app.core_version.as_deref(), Some("v1.19.33"));
    assert_eq!(saved(&a).core_version.as_deref(), Some("v1.19.33"));
    assert!(
        a.paths.core_exe("v1.19.33").is_file(),
        "kept by the cleanup"
    );
    assert_eq!(a.snap.error(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn restore_picks_the_default_profile_and_skips_without_a_core() {
    let mut a = idle("restore-default");
    a.startup().await;
    a.handle(Cmd::Restore).await;
    assert_eq!(a.app.active_profile.as_deref(), Some("default"));
    assert_eq!(a.snap.phase, Phase::Stopped, "no core installed");
    assert_eq!(a.snap.error(), None);
}

local_test!(launch_restores_the_remembered_state, a, _rx, {
    a.handle(Cmd::Restore).await;
    assert_eq!(a.snap.phase, Phase::Running, "core_on defaults to true");

    // Stop is remembered, and also clears the proxy choice.
    a.app.proxy_on = true;
    a.handle(Cmd::Stop).await;
    let s = saved(&a);
    assert!(!s.core_on && !s.proxy_on);
    a.handle(Cmd::Restore).await;
    assert_eq!(a.snap.phase, Phase::Stopped);

    a.handle(Cmd::Start).await;
    assert!(saved(&a).core_on);
    a.handle(Cmd::Stop).await;
});

local_test!(failed_start_shows_its_reason_once, a, _rx, {
    fs::write(a.paths.profiles().join("good.yaml"), "a: [").unwrap();
    a.handle(Cmd::Start).await;
    assert!(matches!(a.snap.phase, Phase::Failed(_)));
    assert_eq!(a.snap.message(Area::Core), None, "the phase carries it");
    assert!(saved(&a).core_on, "a failed start keeps the choice");
});

#[tokio::test(flavor = "current_thread")]
async fn mode_is_recorded_while_stopped() {
    let mut a = idle("mode-stopped");
    a.publish();
    assert_eq!(a.snap.mode, "", "nothing recorded, nothing shown");
    a.handle(Cmd::SetMode("global".into())).await;
    a.publish();
    assert_eq!(a.snap.error(), None);
    assert_eq!(saved(&a).mode.as_deref(), Some("global"));
    assert_eq!(a.snap.mode, "global");
    a.handle(Cmd::SetMode("bogus".into())).await;
    assert!(a.snap.error().is_some());
    assert_eq!(saved(&a).mode.as_deref(), Some("global"));
}

#[test]
fn global_default_prefers_a_group_then_a_node() {
    let group = |name: &str, now: &str, all: &[&str]| Group {
        name: name.into(),
        kind: "Selector".into(),
        now: now.into(),
        all: all.iter().map(|s| s.to_string()).collect(),
    };
    let built_in = ["DIRECT", "REJECT", "REJECT-DROP", "PASS", "COMPATIBLE"];
    let groups = [
        group("GLOBAL", "DIRECT", &["DIRECT", "REJECT", "node", "A", "B"]),
        group("A", "node", &["node"]),
        group("B", "node", &["node"]),
    ];
    assert_eq!(global_default(&groups).as_deref(), Some("A"));
    let only_nodes = [group(
        "GLOBAL",
        "DIRECT",
        &[built_in.as_slice(), &["node"]].concat(),
    )];
    assert_eq!(global_default(&only_nodes).as_deref(), Some("node"));
    assert_eq!(
        global_default(&[group("GLOBAL", "DIRECT", &built_in)]),
        None
    );
    let chosen = [
        group("GLOBAL", "REJECT", &["DIRECT", "REJECT", "A"]),
        group("A", "x", &["x"]),
    ];
    assert_eq!(global_default(&chosen), None, "only DIRECT is replaced");
    assert_eq!(global_default(&groups[1..]), None, "no GLOBAL");
    // A profile's own GLOBAL may list itself or groups in any order; the rule is the same.
    let own = [
        group("GLOBAL", "DIRECT", &["GLOBAL", "DIRECT", "B", "A"]),
        group("A", "node", &["node"]),
        group("B", "node", &["node"]),
    ];
    assert_eq!(global_default(&own).as_deref(), Some("B"));
}

fn global_now(a: &Actor) -> String {
    a.snap
        .groups
        .iter()
        .find(|g| g.name == "GLOBAL")
        .unwrap()
        .now
        .clone()
}

local_test!(remembered_mode_survives_restart_and_reload, a, _rx, {
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.mode, "rule", "the profile's own mode");
    a.handle(Cmd::SetMode("direct".into())).await;
    assert_eq!(a.snap.mode, "direct");
    a.handle(Cmd::Stop).await;
    a.publish();
    assert_eq!(a.snap.mode, "direct", "the recorded mode while stopped");
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.mode, "direct");
    a.handle(Cmd::Reload).await;
    assert_eq!((a.snap.mode.as_str(), a.snap.error()), ("direct", None));
    a.handle(Cmd::Stop).await;
});

local_test!(global_mode_moves_off_direct_once, a, _rx, {
    a.handle(Cmd::Start).await;
    assert_eq!(global_now(&a), "DIRECT", "Mihomo's own default");
    a.handle(Cmd::SetMode("global".into())).await;
    assert_eq!(global_now(&a), "PICK", "the first proxy group");

    // A manual choice is kept by Mihomo across a restart and not replaced.
    a.handle(Cmd::Select {
        group: "GLOBAL".into(),
        proxy: "REJECT".into(),
    })
    .await;
    a.handle(Cmd::Stop).await;
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.mode, "global");
    assert_eq!(global_now(&a), "REJECT");
    a.handle(Cmd::Stop).await;
});

local_test!(remembered_mode_beats_an_older_last_good, a, _rx, {
    a.handle(Cmd::Start).await;
    a.handle(Cmd::SetMode("global".into())).await;
    // A version switch relaunches from last-good.yaml, written while the mode was still rule.
    let other = a.paths.core_exe("v1.0.0");
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(real_core().unwrap(), &other).unwrap();
    a.handle(Cmd::ActivateVersion("v1.0.0".into())).await;
    assert_eq!((a.snap.mode.as_str(), a.snap.error()), ("global", None));
    assert_eq!(global_now(&a), "PICK");
    a.handle(Cmd::Stop).await;

    // A broken profile falls back to last-good, still with the remembered mode.
    a.handle(Cmd::SetMode("direct".into())).await;
    fs::remove_file(a.paths.profiles().join("good.yaml")).unwrap();
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.phase, Phase::Running);
    assert_eq!(a.snap.mode, "direct");
    a.handle(Cmd::Stop).await;
});

/// The system proxy state lags behind networksetup; these readers stand in for configd.
static LAGGING_READS: AtomicU16 = AtomicU16::new(0);
fn lagging_off(_: &[u16]) -> proxy::State {
    let on = LAGGING_READS.fetch_add(1, Ordering::SeqCst) < 3;
    proxy::State { on, ours: on }
}
fn stuck_on(_: &[u16]) -> proxy::State {
    proxy::State {
        on: true,
        ours: true,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn proxy_state_is_read_until_it_settles() {
    let mut a = idle("proxy-settle");
    a.read_proxy = lagging_off;
    let started = Instant::now();
    a.settle_proxy(false).await;
    assert!(!a.snap.proxy_on, "the lagging off is waited for");
    assert_eq!(LAGGING_READS.load(Ordering::SeqCst), 4);
    assert!(started.elapsed() < PROXY_SETTLE);

    // A state that never matches is reported as read, after the deadline.
    a.read_proxy = stuck_on;
    let started = Instant::now();
    a.settle_proxy(false).await;
    assert!(a.snap.proxy_on);
    assert!(started.elapsed() >= PROXY_SETTLE);
}
