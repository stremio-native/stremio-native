# <img src="app/assets/app.ico" width="36" align="center" /> Stremio Native

### A faster, lighter desktop client for Stremio — built from scratch in Rust.

[![Desktop release builds](https://github.com/stremio-native/stremio-native/actions/workflows/release.yml/badge.svg)](https://github.com/stremio-native/stremio-native/actions/workflows/release.yml)
[![Clippy](https://github.com/stremio-native/stremio-native/actions/workflows/clippy.yml/badge.svg)](https://github.com/stremio-native/stremio-native/actions/workflows/clippy.yml)
[![Latest Release](https://img.shields.io/github/v/release/stremio-native/stremio-native?color=7c3aed&label=release)](https://github.com/stremio-native/stremio-native/releases)
![Rust](https://img.shields.io/badge/rust-2024_stable-orange.svg?logo=rust)
![Slint UI](https://img.shields.io/badge/UI-Slint_1.17-blue.svg?logo=slint)
![License](https://img.shields.io/github/license/stremio-native/stremio-native?color=informational)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20Linux-lightgrey)

<!-- SEO Meta Tags & Keywords -->
<!-- Keywords: Stremio alternative client, Stremio desktop, fast Stremio player, lightweight Stremio app, Stremio web ui offline, Slint media player Rust, BitTorrent streaming player, local database media center, open source stream server -->
<meta name="description" content="Stremio Native is an ultra-fast, lightweight, and modern desktop client for Stremio. Built with Rust and Slint UI, it features a custom, open-source stream server instead of the proprietary server.js." />

---

### 🤔 Why Use Stremio Native?

The official Stremio desktop app runs on Electron-style WebViews backed by a separate Node.js server (`server.js`). At idle it spawns **10 processes** and holds **800+ MB of RAM**.

**Stremio Native** replaces all of that with a single Rust binary and a native [Slint](https://slint.dev/) UI:

* **🚀 Instant Startup** — launches in under a second with zero UI lag.
* **💧 56% Less RAM** — **358 MB** idle vs. the 814 MB official baseline.
* **⚡ 1 Process Instead of 10** — an open-source stream server runs in-process; no Node.js required.
* **🔋 Battery-Friendly** — GPU hardware video decoding keeps CPU usage near **0%** during playback.
* **🔒 100% Local & Private** — local SQLite database, zero telemetry, no cloud dependencies.

![Stremio Native Interface](app/assets/preview.png)

See the [changelog](CHANGELOG.md) for the current build's implementation notes and known limitations.

> [!IMPORTANT]
> **⚠️ Full UI Rewrite & Feedback Notice**: Stremio Native is a complete ground-up rewrite in Rust & Slint UI. As a result, some features may not be fully implemented or might not work as intended in all scenarios. If you encounter any bugs, unexpected behavior, or missing features, please [raise an issue on GitHub](https://github.com/stremio-native/stremio-native/issues) so the developer can look into it!
>
> **💻 Modern Hardware Requirements**: Current precompiled release binaries require a **modern CPU** (`x86-64-v3` architecture baseline with AVX2/BMI2 instruction support) and a **modern GPU** (OpenGL 3.3+ support for Skia UI rendering & Anime4K/FSR upscaler shaders, with hardware GPU video decoding for H.264, HEVC, AV1, and VP9).

---

## 📥 Download

| Platform | Format | Link |
| :--- | :--- | :--- |
| **Windows** | Installer | [StremioSetup-v1.0.5-x64.exe](https://github.com/stremio-native/stremio-native/releases/download/v1.0.5/StremioSetup-v1.0.5-x64.exe) |
| **Windows** | Updater ZIP | [stremio-native-v1.0.5-x86_64-pc-windows-msvc.zip](https://github.com/stremio-native/stremio-native/releases/download/v1.0.5/stremio-native-v1.0.5-x86_64-pc-windows-msvc.zip) |
| **Arch Linux** | Pacman `.pkg.tar.zst` | [stremio-native-1.0.5-1-x86_64.pkg.tar.zst](https://github.com/stremio-native/stremio-native/releases/download/v1.0.5/stremio-native-1.0.5-1-x86_64.pkg.tar.zst) |
| **Debian / Ubuntu** | `.deb` | [stremio-native_1.0.5-1_amd64.deb](https://github.com/stremio-native/stremio-native/releases/download/v1.0.5/stremio-native_1.0.5-1_amd64.deb) |
| **Fedora / RHEL** | `.rpm` | [stremio-native-1.0.5-1.fc44.x86_64.rpm](https://github.com/stremio-native/stremio-native/releases/download/v1.0.5/stremio-native-1.0.5-1.fc44.x86_64.rpm) |
| **Linux** | Standalone binary | [stremio-native-v1.0.5-x86_64-unknown-linux-gnu](https://github.com/stremio-native/stremio-native/releases/download/v1.0.5/stremio-native-v1.0.5-x86_64-unknown-linux-gnu) |

---

## 📊 Performance Comparison

| Metric | Official Stremio | Stremio Native | Improvement |
| :--- | ---: | ---: | :---: |
| Processes | 10 | **1** | 90% fewer |
| Idle memory (RAM) | 814.4 MB | **358.6 MB** | 56% lower |
| Idle CPU | — | **0.19%** | near-zero |
| Threads | 190 | **72** | 62% fewer |
| Handles | 4,872 | **891** | 82% fewer |

Measured on Windows x64 from the settled, idle v1.0.0 release process. CPU and I/O are five-second samples; other values are point-in-time readings. The official baseline was captured from a corresponding settled Stremio session. This is an observational comparison, not a controlled laboratory benchmark.

---

## ✨ Features

### 🎨 Native Desktop UI & Customization
* **Slint UI & Custom Themes** — native desktop rendering with Side Rail, Top Bar, Minimal, and Classic theme presets.
* **Browser-Style Smooth Scrolling** — tuned scroll physics (100px wheel step, cubic ease-out, 1200px/s² fling) across all list views.
* **Picture-in-Picture (PiP)** — compact always-on-top window mode with integrated player controls.
* **Multi-Profile Management** — Owner, Standard, and Kids profiles with Argon2id PIN protection and live profile switching.
* **Remappable Hotkeys** — typed Winit hotkey editor with modifier support, hold/release actions, and conflict detection.
* **Native Localization** — Fluent localization engine for English, Portuguese, and Arabic with full RTL support.

### ⚡ Embedded Stream Server & Debrid
* **No External Dependencies** — eliminates the separate Node.js `server.js` process. The stream engine runs asynchronously inside the Rust async runtime.
* **Smart Stream Ranking** — deterministic offline stream parsing with Seeders, Quality, Smallest, and Smart ranking modes.
* **Vault-Secured Debrid** — native integrations for Real-Debrid, AllDebrid, Premiumize, Debrid-Link, and TorBox with OS credential protection.
* **Community Addon Adapter** — manifest validation, pagination, and PIN-gated adult addon filtering.

### 🎞️ Advanced MPV Playback & Shaders
* **Hardware-Accelerated Decoding** — powered by `libmpv` with full GPU hardware video decoding (H.264, HEVC, AV1, VP9).
* **Anime4K and AMD FSR Upscalers** — all six Anime4K GLSL shader modes plus AMD FSR upscaling on desktop OpenGL 3.3+.
* **Dual Subtitle Selection** — independent primary and secondary subtitle selection with position and scale adjustments.
* **Playback Tools** — A/B repeat markers, episode-aware sleep timers, HDR modes (Auto, Passthrough, Tone Map, Disabled), and PNG screenshot capture.
* **Timeline Previews** — secondary persistent `libmpv` worker delivering zero-lag seekbar thumbnail previews with a 16 MiB exact-frame cache.

### 📦 Download Manager & Local Media Indexing
* **Native Download Manager** — profile-scoped manager supporting HTTP Range resume, `.part` recovery, atomic completion, and bandwidth limits.
* **Local Library Scanner** — recursive directory watching, movie/episode filename parsing, file fingerprinting, external subtitle discovery, and direct playback.

### 🔒 OS Integrations & Local-First Storage
* **Native OS Media Controls** — SMTC on Windows, MPRIS on Linux, and MediaRemote on macOS with taskbar thumbnail controls and sleep inhibition.
* **OS Credential Encryption** — backed by Windows Credential Manager and Linux Secret Service for auth tokens and API keys.
* **Local-First SQLite Database** — Turso/Limbo engine with zero telemetry, transactional schema migrations, and automated backups.

---

## 🛠️ Building From Source

### Prerequisites
- **Windows**: [Rust toolchain (`msvc`)](https://rustup.rs/), Visual Studio 2022 C++ Build Tools with the x64 MSVC v143 toolset, a Windows SDK, and LLVM/Clang for native dependencies. Release builds use the dynamic MSVC CRT (`/MD`) while vcpkg libraries remain static.
- **Linux**: Rust, `pkg-config`, `libmpv-dev`, and standard X11/Wayland GUI packages (see the CI workflow for the full list).

### Build & Run

```bash
git clone https://github.com/perpetus/stremio-native.git
cd stremio-native
cargo run --release --package stremio-native
```

Before the first Windows build, install the manifest dependencies with the
project triplet so the static native libraries use the same dynamic CRT as
Rust and Skia:

```powershell
& "$env:VCPKG_ROOT\vcpkg.exe" install `
  --x-manifest-root="$PWD" `
  --x-install-root="$PWD\vcpkg_installed" `
  --triplet=x64-windows-v3-static-md-release `
  --overlay-triplets="$PWD\triplets" `
  --overlay-ports="$PWD\vcpkg-overlays"
```

`cargo build` then discovers that exact installation through the repository's
Cargo configuration. Do not substitute `x64-windows-static`: that triplet uses
the static `/MT` CRT and is incompatible with the `/MD` Skia build.

Settings, logs, and image caches are stored in `./storage/` inside the project directory.

On Windows, `setup/create_setup.cmd` runs `scripts/stage_windows_msvc_runtime.ps1` for both fresh and `SKIP_BUILD=1` builds. The script copies the newest x64 `Microsoft.VC143.CRT` DLLs beside the packaged app, validates runtime imports with `dumpbin`, and writes a SHA-256 manifest. This app-local deployment supports the project's per-user, non-admin installer.

Timeline previews reuse the same packaged libmpv library as the main player. Building or running the application does not depend on the implementation-reference checkout under `docs/thumbfast`.

### CI & Releases

Pushing a `v*` tag builds both Windows and Linux and publishes a GitHub release automatically. Windows release builds target the `x64-windows-v3-static-md-release` triplet (`x86-64-v3` / `/arch:AVX2`, static native libraries, dynamic CRT) for broad CPU compatibility. The release includes installers, portable archives, Linux packages, SHA-256 checksums, changelog notes, and a linked commit diff.

---

## Attribution

The timeline preview scheduler is based on the interaction and seek strategy of [ThumbFast](https://github.com/po5/thumbfast), licensed under the Mozilla Public License 2.0. The source notice, referenced Lua snapshot hash, and license are recorded in [`licenses/thumbfast`](licenses/thumbfast/NOTICE.md).

---

## ⚠️ Disclaimer

Stremio Native is an independent, community-developed project. It is not affiliated with, authorized, maintained, sponsored, or endorsed by SmartCode Ltd (the creators of the official Stremio application).
