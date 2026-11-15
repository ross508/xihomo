# xihomo

A minimal, native macOS GUI for [Mihomo](https://github.com/MetaCubeX/mihomo).

The X is a little nod to OS X, the former name of macOS.

xihomo lives in the menu bar and the Dock; the Dock icon can be set to show only while its window is open. It runs Mihomo with your profiles, switches modes and nodes, tests their latency, and turns the system proxy and TUN mode on and off.

The app ships with a Mihomo core, so it works on first launch without a network. Settings → Check for Update downloads newer cores, which are kept outside the app.

Requires macOS 13 or later on Apple Silicon.

## Install

With Homebrew:

```sh
brew install --cask ross508/tap/xihomo
```

Or download the DMG from [Releases](https://github.com/ross508/xihomo/releases) and drag xihomo to Applications. macOS blocks the first launch, because xihomo is not notarized (see below). Allow it once in System Settings → Privacy & Security → Open Anyway, or run:

```sh
xattr -dr com.apple.quarantine /Applications/xihomo.app
```

## Security

- xihomo is ad-hoc signed and not notarized, so macOS cannot tell who built it.
- The Homebrew cask removes the quarantine attribute from `xihomo.app`, which skips Gatekeeper's first-launch check.
- TUN mode needs administrator rights once per core version. After a password prompt, xihomo makes a copy of the Mihomo core owned by root with the setuid bit, in `~/Library/Application Support/xihomo/bin/`. That copy runs as root whenever xihomo starts it. Deleting it removes the authorization.
- The bundled core is the official Mihomo release binary, unmodified.

To verify a download:

- compare its SHA-256 with the `.sha256` file published beside it on the release;
- run `codesign -dv /Applications/xihomo.app`, which should report `Signature=adhoc`;
- or build it yourself: install [mise](https://mise.jdx.dev), then run `mise install` and `scripts/bundle.sh`, which writes `target/release/xihomo.app`.

## License

xihomo is licensed under the [Mozilla Public License 2.0](LICENSE).

Mihomo is licensed under GPL-3.0, and xihomo ships it unmodified as a separate program. This arrangement is an aggregate, so GPL-3.0 does not extend to xihomo itself. That left us free to choose a permissive license such as MIT or Apache-2.0. We chose MPL-2.0 out of respect for the Mihomo team's work and the GPL's principle that downstream recipients should retain their freedoms. Under MPL-2.0, changes to xihomo's own files stay open too.

The bundled Mihomo version is pinned in [`mihomo-version`](mihomo-version), its license is in [`mihomo-LICENSE`](mihomo-LICENSE), and its source is at `https://github.com/MetaCubeX/mihomo/tree/<version>`, for example [v1.19.34](https://github.com/MetaCubeX/mihomo/tree/v1.19.34).
