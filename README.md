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

xihomo starts with only its menu bar icon and no window. First run: click the icon, choose Settings…, then Check for Update → Download, Link YAML File…, select the profile, and Start.

GPUI's `runtime_shaders` feature compiles Metal shaders at launch, so the separate Metal Toolchain component is not needed.

## Files

```text
~/.config/xihomo/
  settings.json        active profile, icon set, core versions, ports, per-profile homes
  overrides.yaml       optional; top-level keys replace the profile's
  profiles/            YAML files or symlinks; links are only ever read
  bin/<version>/       core versions (current and rollback target)
  runtime/             candidate.yaml, last-good.yaml (private)
  state/               secret, proxy-on marker, core journal, instance lock
```

Ports default to `mixed-port: 10999` and `external-controller: 127.0.0.1:11000`, and always replace the profile's values. To change them, edit `settings.json` while xihomo is not running.

Effective configuration is: source profile, then `overrides.yaml`, then fields xihomo owns. Each step replaces whole top-level values; nothing is merged. `overrides.yaml` may not set owned keys (`mixed-port`, `secret`, `tun`, `external-controller*`, `external-ui*`, `external-doh-server`). `tun` is forced off and TUN listeners are removed.

Mihomo runs with its home set to the profile's resolved parent directory, or to `homes.<profile>` in `settings.json`. Relative provider paths resolve there, and Mihomo may write caches and geodata there.

## Behavior worth knowing

- **Start is explicit.** Closing the window keeps the menu bar app running. Quit turns the system proxy off and stops the child.
- **External edits** are detected on window activation and every 30 seconds, and only ever offered as Reload.
- **A profile is selected only after activation succeeds.** If it fails, the previous configuration stays; a failed Start retries once with `last-good.yaml`, never in a loop.
- **System proxy** is write-only, like Bettbox: on sets HTTP, HTTPS and SOCKS to `127.0.0.1:<mixed-port>` on every enabled network service through `/usr/sbin/networksetup`; off turns all three off on every enabled service. Earlier proxy values are not read, checked or restored, and PAC, auto-discovery and bypass domains are left untouched.
- **After a crash or power loss**, the next launch turns the proxy off (marked by `state/proxy-on.json`) and stops a leftover core. To recover by hand: `networksetup -setwebproxystate "Wi-Fi" off`, `networksetup -setsecurewebproxystate "Wi-Fi" off` and `networksetup -setsocksfirewallproxystate "Wi-Fi" off`.
- **Core updates** verify the release's SHA-256 digest, the Mach-O architecture and `-v` output, and test the current configuration before switching. The selection changes only after the new core has started. The digest comes from the same release channel, so it detects corruption rather than proving authorship.

## Test

```sh
mise exec -- cargo test
mise exec -- cargo clippy --all-targets
```

- **Layer 1, Rust tests:** configuration, profiles, updater, proxy commands and service listing, menu model and command mapping, and the backend actor. The actor tests use a real core at `dev/bin/mihomo` when it exists and skip otherwise. They use synthetic profiles and loopback ports only, and never change system proxy settings: under `cargo test` any call to `networksetup` panics.
- **Layer 2, GPUI tests:** run in-process with `TestAppContext`, with no real window, mouse, keyboard or focus change. GPUI 0.2.2 keeps stale `debug_bounds` between frames, so the tests check that an element is gone by clicking where it was and expecting no command.
- **Layer 3, real-device tests:** the menu bar, the native menu, window focus and the system proxy on the developer's Mac. They operate the desktop and change network settings, so `cargo test` never runs them; they are done by hand, with the proxy settings saved first and compared afterwards.
