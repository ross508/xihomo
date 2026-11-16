#!/bin/sh
# Builds a local, ad-hoc signed xihomo.app for personal use.
# --dev builds the dev channel instead: target/debug/xihomo-dev.app, a separate app with its own
# bundle identifier, directories and default ports.
# Both carry the Mihomo core pinned in mihomo-version (its version, then the SHA-256 of its release
# asset) at Contents/MacOS/mihomo, the official release binary, unmodified, and its version in
# Info.plist (XihomoCoreVersion). Downloaded core versions still live in the data directory, so the
# signed bundle never changes after the build. The release build leaves no local path inside.
set -eu
cd "$(dirname "$0")/.."
fail() {
  echo "bundle.sh: $*" >&2
  exit 1
}
# Pinned in the repository, so a release asset replaced upstream fails the build. Taken from the
# digest GitHub publishes for the asset when the version is bumped.
{ read -r core_version && read -r digest; } < mihomo-version || fail "mihomo-version needs a version and a SHA-256"
case $core_version in
  *[!A-Za-z0-9.-]* | "") fail "mihomo-version holds no version: $core_version" ;;
  v[0-9]*) ;;
  *) fail "mihomo-version holds no version: $core_version" ;;
esac
case $digest in
  *[!0-9a-f]* | "") fail "mihomo-version holds no SHA-256: $digest" ;;
esac
[ ${#digest} -eq 64 ] || fail "mihomo-version holds no SHA-256: $digest"
# Downloads $1 to $2 unless $2 already has the SHA-256 $3, and fails unless it does after.
fetch() {
  if [ ! -s "$2" ] || [ "$(shasum -a 256 "$2" | cut -d ' ' -f 1)" != "$3" ]; then
    curl -fsSL -o "$2.tmp" "$1"
    [ "$(shasum -a 256 "$2.tmp" | cut -d ' ' -f 1)" = "$3" ] || fail "$1 failed its SHA-256 check"
    mv "$2.tmp" "$2"
  fi
}
mkdir -p target
asset=mihomo-darwin-arm64-$core_version.gz
gz=target/$asset
fetch "https://github.com/MetaCubeX/mihomo/releases/download/$core_version/$asset" "$gz" "$digest"
# GPL-3.0 asks for its text to travel with the core: Mihomo's own LICENSE at that tag, checked
# against the GPL-3.0 text's digest, which does not change between versions.
gpl=target/mihomo-LICENSE
fetch "https://raw.githubusercontent.com/MetaCubeX/mihomo/$core_version/LICENSE" "$gpl" \
  3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986
id=io.github.ross508.xihomo
if [ "${1:-}" = "--dev" ]; then
  mise exec -- cargo build
  name=xihomo-dev profile=debug id=$id.dev
else
  # The last matching prefix wins, so the most specific comes last.
  cargo_home=${CARGO_HOME:-$HOME/.cargo}
  RUSTFLAGS="--remap-path-prefix=$HOME=~ --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$PWD=/xihomo" \
    mise exec -- cargo build --release
  name=xihomo profile=release
fi
app=target/$profile/$name.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
# xihomo is MPL-2.0. The bundled Mihomo core is a separate program under GPL-3.0.
cp LICENSE "$app/Contents/Resources/LICENSE"
cp "$gpl" "$app/Contents/Resources/mihomo-LICENSE"
cp "target/$profile/xihomo" "$app/Contents/MacOS/xihomo"
core=$app/Contents/MacOS/mihomo
gunzip -c "$gz" > "$core"
chmod 755 "$core"
core_sha=$(shasum -a 256 "$core" | cut -d ' ' -f 1)
# The app trusts XihomoCoreVersion without running the core, so the core must say the same.
"$core" -v | grep -qF "Mihomo Meta $core_version " || fail "$core does not report $core_version"
iconset=target/$profile/AppIcon.iconset
rm -rf "$iconset"
"target/$profile/xihomo" --write-iconset "$iconset"
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$iconset"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundleIdentifier</key><string>$id</string>
  <key>CFBundleExecutable</key><string>xihomo</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>XihomoCoreVersion</key><string>$core_version</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
# Without --deep, so mihomo keeps its own signature and stays byte for byte the release binary.
codesign --force --sign - "$app"
codesign --verify --strict "$app" || fail "codesign --verify --strict failed"
[ "$(shasum -a 256 "$core" | cut -d ' ' -f 1)" = "$core_sha" ] || fail "signing changed $core"
if [ "$profile" = release ]; then
  ! strings "$app/Contents/MacOS/xihomo" | grep -qF "$HOME" || fail "xihomo contains $HOME"
  codesign -dv "$app" 2>&1 | grep -qx "Signature=adhoc" || fail "not ad-hoc signed"
  ! codesign -dv "$app" 2>&1 | grep -q "^TeamIdentifier=[^n]" || fail "signed with a team identity"
fi
echo "$app"
