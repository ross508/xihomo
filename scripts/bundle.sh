#!/bin/sh
# Builds a local, ad-hoc signed target/release/xihomo.app for personal use.
# Mihomo is not bundled: core versions live in ~/.config/xihomo/bin so the signed bundle never changes.
set -eu
cd "$(dirname "$0")/.."
mise exec -- cargo build --release
app=target/release/xihomo.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp target/release/xihomo "$app/Contents/MacOS/xihomo"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>xihomo</string>
  <key>CFBundleIdentifier</key><string>dev.xihomo.app</string>
  <key>CFBundleExecutable</key><string>xihomo</string>
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
