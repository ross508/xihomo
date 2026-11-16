<img src="assets/logo.png" align="right" width="112" alt="Xihomo logo">

# Xihomo

[![Release](https://img.shields.io/github/v/release/ross508/xihomo?label=release&logo=github&color=2f6fdb)](https://github.com/ross508/xihomo/releases/latest)
[![Mihomo core](https://img.shields.io/badge/dynamic/regex?url=https%3A%2F%2Fraw.githubusercontent.com%2Fross508%2Fxihomo%2Fmaster%2Fmihomo-version&search=%5Ev%5B0-9.%5D%2B&label=mihomo&logo=go&logoColor=white&color=0e8a7d)](https://github.com/MetaCubeX/mihomo)

A minimal, native macOS GUI for [Mihomo](https://github.com/MetaCubeX/mihomo). The X is a little nod to OS X.

A compact window and a menu bar icon give you the everyday controls. Xihomo fills in the common parts of a Mihomo config with opinionated defaults, and a lean Settings panel covers the few options most people change. A Mihomo core is built in, so it works right after install.

**A macOS app only.** It requires macOS 13 Ventura or later on an Apple Silicon Mac.

## Install

```sh
brew install --cask ross508/tap/xihomo
```

Or download the DMG from [Releases](https://github.com/ross508/xihomo/releases) and drag Xihomo to Applications. Xihomo is not notarized, so allow its first launch in System Settings → Privacy & Security → Open Anyway, or run:

```sh
xattr -dr com.apple.quarantine /Applications/Xihomo.app
```

## Uninstall

```sh
brew uninstall --cask xihomo         # removes the app
brew uninstall --zap --cask xihomo   # also removes cores and app data
```

Neither touches `~/.config/xihomo`, so your settings and profiles stay. Delete it yourself if you no longer need it.

## Security

- Ad-hoc signed, not notarized. The Homebrew cask removes its quarantine, skipping Gatekeeper's first-launch check.
- TUN mode asks for an administrator password once per core version, then keeps a root-owned setuid core in `~/Library/Application Support/xihomo/bin/`. Delete it to revoke.
- The built-in core is Mihomo's official binary, unmodified.

Verify a download against its `.sha256` file, or build it with [mise](https://mise.jdx.dev): `mise install && scripts/bundle.sh`.

## License

Xihomo is licensed under the [Mozilla Public License 2.0](LICENSE).

Mihomo is licensed under GPL-3.0, and Xihomo ships it unmodified as a separate program. This arrangement is an aggregate, so GPL-3.0 does not extend to Xihomo itself. That left us free to choose a permissive license such as MIT or Apache-2.0. We chose MPL-2.0 out of respect for the Mihomo team's work and the GPL's principle that downstream recipients should retain their freedoms. Under MPL-2.0, changes to Xihomo's own files stay open too.
