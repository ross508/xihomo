# xihomo

A small macOS menu bar controller for [Mihomo](https://github.com/MetaCubeX/mihomo), written in Rust and GPUI. Personal use on Apple Silicon. It manages one unprivileged Mihomo child process and the macOS system proxy. TUN is not supported.

## Run

Requires full Xcode and the project's [mise](https://mise.jdx.dev) toolchain.

```sh
mise install
mise exec -- cargo run            # development
scripts/bundle.sh                 # target/release/xihomo.app, ad-hoc signed
open target/release/xihomo.app
```

xihomo starts with only its menu bar icon and no window. First run: click the icon, choose Settings…, then Check for Update → Update to the latest core. If `profiles/` is empty, xihomo creates `default.yaml` (`mode: direct`), selects it and starts the core with it; use Link YAML File… for a real profile and select it.

GPUI's `runtime_shaders` feature compiles Metal shaders at launch, so the separate Metal Toolchain component is not needed.

## Files

Files are split by who maintains them.

```text
~/.config/xihomo/                       yours; safe to keep in dotfiles
  settings.yaml                         preferences: ports, icon set, theme
  overrides.yaml                        optional; top-level keys replace the profile's
  profiles/                             *.yaml files or symlinks; links are only ever read

~/Library/Application Support/xihomo/   xihomo's; no need to touch it
  bin/<version>/mihomo                  current and previous core
  homes/<profile>/                      Mihomo home per profile: cache.db, geodata, providers
  runtime/                              candidate.yaml, last-good.yaml (private)
  state/                                app.json, secret, core journal, instance lock
```

`settings.yaml` holds only what you edit; missing keys take their defaults:

```yaml
mixed_port: 10999
controller_port: 11000
icon_set: circle
theme: auto        # auto | light | dark
```

Ports always replace the profile's `mixed-port` and `external-controller` (bound to `127.0.0.1`), and are read at launch, so edit them while xihomo is not running. xihomo rewrites `settings.yaml` only when you change the icon set or theme in Settings, which drops comments. An unparseable file is moved to `settings.yaml.invalid` and defaults are used. What xihomo maintains itself (active profile, core versions, last-good home, your last Start/Stop and System Proxy choices, and your mode) lives in `state/app.json`.

Effective configuration is: source profile, then `overrides.yaml`, then fields xihomo owns. Each step replaces whole top-level values; nothing is merged. `overrides.yaml` applies to every profile and may not set owned keys (`mixed-port`, `secret`, `tun`, `external-controller*`, `external-ui*`, `external-doh-server`). `tun` is forced off and TUN listeners are removed. Once you pick a mode, it is also written as `mode` for every profile, replacing the profile's and `overrides.yaml`'s; until then each profile keeps its own.

### Profiles

- A profile is a `*.yaml` entry in `profiles/`, a regular file or a symlink, and its name is the file stem. `.yml` files are not listed. Link YAML File… creates a symlink with an absolute target.
- Mihomo always runs with `-d homes/<profile>/` and `-f runtime/candidate.yaml`. Only the profile file itself is read; nothing next to a symlink's target is read or written, and relative paths in a profile resolve inside its home.
- Each home keeps its own `cache.db`, so node selections are remembered per profile by Mihomo itself (`profile.store-selected`, which a profile or `overrides.yaml` may turn off). A home is created when its profile is first activated. Switching to another profile restarts the core.
- Renaming a profile file changes its identity: its selections are lost and the old home is cleaned up.
- **Remove** is in the right-click menu of a profile in Control, disabled for the active profile. It deletes a regular file permanently (not to the Trash), or only the link for a symlink, never its target, and deletes the profile's home in the same step.

### Cleanup

xihomo keeps its data folder from growing. Every 30 seconds it lists `profiles/`; on the first check after launch, and whenever the list has changed, it deletes:

- `homes/` directories with no matching profile, except the running core's home and the last-good home;
- core versions in `bin/` other than the current and the previous;
- `*.tmp<pid>` files in `runtime/` and `state/` left by another process, such as one that crashed mid-write.

If `profiles/` cannot be listed, nothing is deleted. Orphaned homes go immediately, with no grace period.

## Behavior worth knowing

- **Launch restores your last choices.** If a core is installed, it starts at launch unless you stopped it before quitting; the system proxy is off by default and comes back on only if you left it on. Only your own Start, Stop and System Proxy clicks are remembered; crashes, failed starts and Quit are not. Stop also turns the proxy off. Closing the window keeps the menu bar app running; Quit turns xihomo's proxy off and stops the child.
- **Messages appear next to what they are about**, as one small line (red for errors), and are also written to Logs. A failed core shows its reason in the Control status line. There is no banner.
- **External edits** are detected on window activation and every 30 seconds, and only ever offered as Reload.
- **Mode** (rule, global, direct) is in Control and in the menu, also while the core is stopped. Your pick is remembered and survives Stop/Start, profile switches and Reload; while the core runs it applies immediately, without a restart, and open connections stay open. While stopped it applies at the next start.
- **Global mode never routes everything direct by default.** Mihomo's GLOBAL group starts on DIRECT. Whenever the core runs in global mode with GLOBAL on DIRECT (after a start, a profile switch, a Reload or a switch to global), xihomo selects GLOBAL's first proxy group, or else its first node. A later choice of your own is kept: Mihomo remembers it per profile. In global mode Control lists GLOBAL first and leaves REJECT out of its choices.
- **The menu**, top to bottom: Open xihomo and Settings…; the status line and Start/Stop Core; Rule, Global and Direct, checked on the current mode; System Proxy, checked when the system proxy is on; Profile, Reload Edited Profile (only after an edit) and Check Now; Quit xihomo.
- **Check Now** (menu and Settings) runs the 30-second check immediately: refresh the profile list, detect edits, add the proxy to newly appeared network services, and clean up.
- **Theme** in Settings applies immediately: `auto` follows the system appearance; `light` and `dark` fix the appearance of xihomo's window, including its title bar, and of its menus. The menu bar icon is a template image the system tints, whatever the theme.
- **A profile is selected only after activation succeeds.** If it fails, the previous configuration stays; a failed Start retries once with `last-good.yaml`, never in a loop.
- **System proxy** shows the system's real state: on whenever HTTP, HTTPS or SOCKS is enabled, even if another tool or you set it. The button is idempotent: when on, it turns all three off on every enabled network service; when off, it sets them to `127.0.0.1:<mixed-port>`. Both go through `/usr/sbin/networksetup` and take a few seconds, during which the button is disabled. Earlier proxy values are never restored, and PAC, auto-discovery and bypass domains are left untouched. Stop and Quit turn the proxy off only if it points at xihomo.
- **A proxy pointing at xihomo with no core behind it is turned off**: after launch, when the core exits unexpectedly, and every 30 seconds. This also covers a crash or power loss; the next launch also stops a leftover core. To recover by hand: `networksetup -setwebproxystate "Wi-Fi" off`, `networksetup -setsecurewebproxystate "Wi-Fi" off` and `networksetup -setsocksfirewallproxystate "Wi-Fi" off`.
- **Core versions** are listed in Settings; click one to switch to it, right-click it to delete it (not the one in use). Check for Update offers Update to the newer release, which downloads, verifies the SHA-256 digest, the Mach-O architecture and `-v` output, tests the current configuration, and switches; a running core restarts on it, and if it fails to start the previous core takes over again. The digest comes from the same release channel, so it detects corruption rather than proving authorship.

## Test

```sh
mise exec -- cargo test
mise exec -- cargo clippy --all-targets
```

- **Layer 1, Rust tests:** settings and app state, profiles, removal and cleanup, updater, proxy commands and service listing, menu model and command mapping, proxy state parsing, and the backend actor. The actor tests use a real core at `dev/bin/mihomo` when it exists and skip otherwise. They use synthetic profiles and loopback ports only, and never change system proxy settings: under `cargo test` any call to `networksetup` panics.
- **Layer 2, GPUI tests:** run in-process with `TestAppContext`, with no real window, mouse, keyboard or focus change. GPUI 0.2.2 keeps stale `debug_bounds` between frames, so the tests check that an element is gone by clicking where it was and expecting no command.
- **Layer 3, real-device tests:** the menu bar, the native menu, window focus and the system proxy on the developer's Mac. They operate the desktop and change network settings, so `cargo test` never runs them; they are done by hand, with the proxy settings saved first and compared afterwards.
