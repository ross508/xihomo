#!/bin/sh
# Prints the Homebrew cask for ross508/homebrew-tap. The release workflow tests this exact cask
# against the built DMG (third argument, a file:// URL) before publishing it with the release URL.
# Usage: scripts/cask.sh <version> <dmg sha256> [url]
set -eu
version=$1 sha256=$2
url=${3:-"https://github.com/ross508/xihomo/releases/download/v#{version}/xihomo-#{version}.dmg"}
cat <<CASK
cask "xihomo" do
  version "$version"
  sha256 "$sha256"

  url "$url"
  name "xihomo"
  desc "Native menu bar GUI for Mihomo"
  homepage "https://github.com/ross508/xihomo"

  depends_on arch: :arm64
  depends_on macos: ">= :ventura"

  app "xihomo.app"

  # Ad-hoc signed and not notarized: without this, Gatekeeper blocks the first launch.
  postflight_steps do
    run "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "{{appdir}}/xihomo.app"]
  end

  # Quitting releases the system proxy and stops the core before the app is replaced.
  uninstall quit: "io.github.ross508.xihomo"

  zap trash: [
    "~/.config/xihomo",
    "~/Library/Application Support/xihomo",
  ]
end
CASK
