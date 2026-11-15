#!/bin/sh
# Builds a local, ad-hoc signed xihomo.app for personal use.
# --dev builds the dev channel instead: target/debug/xihomo-dev.app, a separate app with its own
# bundle identifier, directories and default ports.
# Both carry the Mihomo core pinned in mihomo-version at Contents/MacOS/mihomo, the official release
# binary, unmodified, and its version in Info.plist (XihomoCoreVersion). Downloaded core versions
# still live in the data directory, so the signed bundle never changes after the build.
set -eu
cd "$(dirname "$0")/.."
fail() {
  echo "bundle.sh: $*" >&2
  exit 1
}
core_version=$(tr -d '[:space:]' < mihomo-version)
case $core_version in
  *[!A-Za-z0-9.-]*) fail "mihomo-version holds no version: $core_version" ;;
  v[0-9]*) ;;
  *) fail "mihomo-version holds no version: $core_version" ;;
esac
asset=mihomo-darwin-arm64-$core_version.gz
gz=target/$asset
# The digest GitHub publishes for the asset, cached beside it so a rebuild needs no network.
if [ ! -s "$gz.sha256" ]; then
  mkdir -p target
  curl -fsSL -H "Accept: application/vnd.github+json" \
    "https://api.github.com/repos/MetaCubeX/mihomo/releases/tags/$core_version" |
    jq -er --arg name "$asset" '.assets[] | select(.name == $name) | .digest | select(startswith("sha256:")) | ltrimstr("sha256:")' \
    > "$gz.sha256.tmp" || fail "no SHA-256 digest published for $asset"
  mv "$gz.sha256.tmp" "$gz.sha256"
fi
digest=$(cat "$gz.sha256")
if [ ! -s "$gz" ] || [ "$(shasum -a 256 "$gz" | cut -d ' ' -f 1)" != "$digest" ]; then
  curl -fsSL -o "$gz.tmp" "https://github.com/MetaCubeX/mihomo/releases/download/$core_version/$asset"
  [ "$(shasum -a 256 "$gz.tmp" | cut -d ' ' -f 1)" = "$digest" ] || fail "$asset failed its SHA-256 check"
  mv "$gz.tmp" "$gz"
fi
id=io.github.ross508.xihomo
if [ "${1:-}" = "--dev" ]; then
  mise exec -- cargo build
  name=xihomo-dev profile=debug id=$id.dev
else
  mise exec -- cargo build --release
  name=xihomo profile=release
fi
app=target/$profile/$name.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
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
echo "$app"
