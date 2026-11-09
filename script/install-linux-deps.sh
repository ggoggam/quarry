#!/usr/bin/env bash
# System libraries GPUI needs to build and link on Debian/Ubuntu.
set -euo pipefail

packages=(
  build-essential
  pkg-config
  libxkbcommon-dev
  libxkbcommon-x11-dev
  libwayland-dev
  libvulkan-dev
  libx11-dev
  libx11-xcb-dev
  libxcb1-dev
  libxi-dev
  libxrandr-dev
  libfontconfig-dev
  libfreetype-dev
  libssl-dev
  libzstd-dev
)

sudo=""
if [ "$(id -u)" -ne 0 ]; then
  sudo="sudo"
fi
$sudo apt-get update
$sudo apt-get install -y --no-install-recommends "${packages[@]}"
