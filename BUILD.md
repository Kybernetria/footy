# Build Instructions

This guide covers building Footy from source.

## Prerequisites

- [Rust](https://rustup.rs/) (latest stable)
- [Bun](https://bun.sh/)
- [Tauri prerequisites](https://tauri.app/start/prerequisites/)

### macOS

```bash
xcode-select --install
```

### Windows

Install Microsoft C++ Build Tools or Visual Studio 2019/2022 with C++ development tools.

### Linux

```bash
# Ubuntu/Debian
sudo apt update
sudo apt install build-essential libasound2-dev pkg-config libssl-dev libvulkan-dev vulkan-tools glslc libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libgtk-layer-shell0 libgtk-layer-shell-dev patchelf cmake

# Fedora/RHEL
sudo dnf groupinstall "Development Tools"
sudo dnf install alsa-lib-devel pkgconf openssl-devel vulkan-devel gtk3-devel webkit2gtk4.1-devel libappindicator-gtk3-devel librsvg2-devel gtk-layer-shell gtk-layer-shell-devel cmake

# Arch Linux
sudo pacman -S base-devel alsa-lib pkgconf openssl vulkan-devel gtk3 webkit2gtk-4.1 libappindicator-gtk3 librsvg gtk-layer-shell cmake
```

## Setup

```bash
git clone git@github.com:cjpais/Footy.git
cd Footy
bun install
bun run tauri dev
```

## Production Build

```bash
bun run tauri build
```

Footy is Parakeet-only. No manual custom model setup is required for development beyond the bundled/downloaded Parakeet resources managed by the app. ONNX Runtime acceleration must remain enabled because Parakeet runs through ONNX Runtime.

## Linux Install From Source Bundle

The raw binary (`src-tauri/target/release/footy`) cannot run standalone because it needs Tauri resources next to the app. Prefer installing from the generated deb/rpm/AppImage bundle.

To extract a deb bundle manually:

```bash
cd /tmp
ar x /path/to/Footy/src-tauri/target/release/bundle/deb/Footy_*_amd64.deb data.tar.gz
tar xzf data.tar.gz
sudo cp usr/bin/footy /usr/bin/
sudo cp -r usr/lib/Footy /usr/lib/
sudo cp -r usr/share/icons/hicolor/* /usr/share/icons/hicolor/
sudo cp usr/share/applications/Footy.desktop /usr/share/applications/
```

After rebuilds, only the binary usually needs re-copying:

```bash
sudo cp src-tauri/target/release/footy /usr/bin/
```

## Troubleshooting

### AppImage build fails on Arch / rolling-release distros

`linuxdeploy` may fail on rolling-release distros. The binary, deb, and rpm bundles usually build fine. To skip AppImage generation:

```bash
bun run tauri build -- --bundles deb
```
