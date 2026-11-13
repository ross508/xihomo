#!/bin/sh
# Builds a local, ad-hoc signed xihomo.app for personal use.
# --dev builds the dev channel instead: target/debug/xihomo-dev.app, a separate app with its own
# bundle identifier, directories and default ports.
# Mihomo is not bundled: core versions live in the data directory so the signed bundle never changes.
set -eu
cd "$(dirname "$0")/.."
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
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
codesign --force --sign - "$app"
echo "$app"
