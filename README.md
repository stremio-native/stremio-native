# <img src="app/assets/app.ico" width="36" align="center" /> Stremio Native

### A faster, lighter desktop client for Stremio — built from scratch in Rust.

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

---

## 📥 Download

| Platform | Format | Link |
| :--- | :--- | :--- |
| **Windows** | Installer | [StremioSetup-v1.0.4-x64.exe](https://github.com/perpetus/stremio-native/releases/download/v1.0.4/StremioSetup-v1.0.4-x64.exe) |
| **Windows** | Updater ZIP | [stremio-native-v1.0.4-x86_64-pc-windows-msvc.zip](https://github.com/perpetus/stremio-native/releases/download/v1.0.4/stremio-native-v1.0.4-x86_64-pc-windows-msvc.zip) |
| **Arch Linux** | Pacman `.pkg.tar.zst` | [stremio-native-1.0.4-1-x86_64.pkg.tar.zst](https://github.com/perpetus/stremio-native/releases/download/v1.0.4/stremio-native-1.0.4-1-x86_64.pkg.tar.zst) |
| **Debian / Ubuntu** | `.deb` | [stremio-native_1.0.4-1_amd64.deb](https://github.com/perpetus/stremio-native/releases/download/v1.0.4/stremio-native_1.0.4-1_amd64.deb) |
| **Fedora / RHEL** | `.rpm` | [stremio-native-1.0.4-1.fc44.x86_64.rpm](https://github.com/perpetus/stremio-native/releases/download/v1.0.4/stremio-native-1.0.4-1.fc44.x86_64.rpm) |
| **Linux** | Standalone binary | [stremio-native-v1.0.4-x86_64-unknown-linux-gnu](https://github.com/perpetus/stremio-native/releases/download/v1.0.4/stremio-native-v1.0.4-x86_64-unknown-linux-gnu) |

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

### 🎨 Native Desktop UI
* **Fixed Sidebar Navigation** — vertical sidebar with hover labels for Board, Discover, Library, Calendar, Addons, and Settings.
* **Catalog Split Preview** — clicking a media card opens a side-panel with blurred poster backdrops, cast, genres, and metadata without leaving the catalog.
* **Episode & Stream Picker** — real-time episode search, capsule season switching, thumbnails, release dates, watched indicators, and a stream-provider sheet with back-navigation.

### ⚡ Embedded Stream Server
* **No External Dependencies** — eliminates the separate Node.js `server.js` process. The stream engine runs asynchronously inside the Rust async runtime.
* **Hardware-Accelerated Playback** — powered by `libmpv` with full GPU hardware decoding (H.264, HEVC, AV1, VP9).

### 🎞️ Rendering & Video Shaders
* **Skia OpenGL UI** — Slint 1.17.1 runs explicitly on the `winit` backend with the `skia-opengl` renderer and negotiates the highest desktop OpenGL version exposed by the driver (OpenGL 4.6 on current Intel/NVIDIA drivers). MPV renders into double-buffered RGBA textures only after Skia has flushed the UI frame, then Skia composites the borrowed video texture on the next frame.
* **Anime4K and FSR Hooks** — shader presets remain in MPV's multipass GLSL hook pipeline. Custom shaders require desktop OpenGL 3.3 or newer.
* **Graceful GPU Fallback** — OpenGL ES and older desktop contexts continue plain MPV playback while shader choices are disabled with a detected-version explanation. The saved preference is restored after a capable context is recreated.

### 🖼️ Native Timeline Previews
* **Persistent Secondary libmpv Decoder** — timeline previews use a paused, software-decoding MPV client in the same process on Windows and Linux. No standalone `mpv.exe`, Lua IPC, sockets, temporary images, or PNG decoding are required.
* **Fast Then Exact Seeking** — the worker prewarms after a video loads, coalesces rapid cursor movement, shows a nearby keyframe first, and refines it with an exact frame after the cursor settles.
* **Fixed Resource Budget** — exact RGBA frames use a 16 MiB LRU cache and the decoder remains paused when the timeline is not being hovered. Previews are enabled by default and can be disabled immediately under Player settings.
* **Playback-First Fallback** — live/non-seekable streams and audio-only media continue playing without previews. Thumbnail frames intentionally omit subtitles, Anime4K, FSR, and other main-player overlays.

### 📦 Local-First Storage & Privacy
* **Single Database File** — all settings, history, and metadata live in `./storage/stremio.db` (Turso/Limbo engine).
* **Zero Telemetry** — no tracking, analytics, or phone-home connections.

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
