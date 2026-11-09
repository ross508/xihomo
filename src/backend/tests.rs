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
    let base =
        21000 + (std::process::id() % 500) as u16 * 40 + NEXT_PORT.fetch_add(2, Ordering::SeqCst);
    let settings = Settings {
        mixed_port: base,
        controller_port: base + 1,
        core_version: Some(VERSION.into()),
        active_profile: Some("good".into()),
        ..Settings::default()
    };
    settings.save(&paths).unwrap();
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

fn saved(actor: &Actor) -> Settings {
    Settings::load(&actor.paths).unwrap().0
}

local_test!(start_control_stop, a, _rx, {
    a.handle(Cmd::Start).await;
    assert_eq!(a.snap.phase, Phase::Running, "{:?}", a.snap.error);
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
    assert!(a.snap.error.is_none());

    a.handle(Cmd::Stop).await;
    assert_eq!(a.snap.phase, Phase::Stopped);
    assert!(a.core.is_none());
    core::ensure_ports_free(&[a.settings.mixed_port, a.settings.controller_port])
        .await
        .unwrap();
});

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
        a.snap.error.as_deref().unwrap().contains("rejected"),
        "{:?}",
        a.snap.error
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
    assert_eq!(a.snap.error, None);
    assert_eq!(saved(&a).active_profile.as_deref(), Some("other"));
    assert_eq!(a.snap.mode, "direct");
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
    assert!(a.snap.error.is_some());
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
    assert!(a.snap.notice.as_deref().unwrap().contains("last-good"));
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
        Phase::Failed(message) => assert!(message.contains("in use"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert!(squatter.local_addr().is_ok());
});

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
    a.snap.proxy_on = true;
    let pid = a.core.as_ref().unwrap().pid();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let status = wait_core(&mut a.core).await;
    a.on_core_exit(status).await;
    assert!(matches!(rx.try_recv(), Ok(Cmd::SetProxy(false))));
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
    let error = a.snap.error.clone().unwrap();
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
    assert_eq!(a.snap.error, None);
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
    assert!(a.snap.error.as_deref().unwrap().contains("not installed"));
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
        let (yaml, _) = profiles::compose(
            &fs::read(&path).unwrap(),
            b"",
            &profiles::Runtime {
                mixed_port: 10999,
                controller_port: 11000,
                secret: "x",
            },
        )
        .unwrap_or_else(|_| panic!("composition failed"));
        write_private(&paths.candidate(), yaml.as_bytes()).unwrap();
        let home = paths.root.join("home");
        fs::create_dir_all(&home).unwrap();
        let ok = core::validate(&exe, &paths, &home, &paths.candidate(), "x")
            .await
            .is_ok();
        assert!(ok, "a dev profile failed validation after overrides");
    }
}

/// No core needed: an unread error survives background refreshes, and the next user action clears it.
#[tokio::test(flavor = "current_thread")]
async fn background_refresh_keeps_unread_messages() {
    let paths = temp_paths("messages");
    let (tx, _rx) = mpsc::unbounded_channel();
    let (mut a, ..) = Actor::new(paths, tx, Arc::new(AtomicBool::new(false))).unwrap();
    a.snap.error = Some("unread".into());
    for cmd in [
        Cmd::Refresh,
        Cmd::WatchConnections(true),
        Cmd::WatchConnections(false),
    ] {
        a.handle(cmd).await;
        assert_eq!(a.snap.error.as_deref(), Some("unread"));
    }
    a.handle(Cmd::SetIconSet("bars".into())).await;
    assert_eq!(a.snap.error, None);
}
