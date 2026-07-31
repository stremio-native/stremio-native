# Changelog

This file records notable changes to Stremio Native relative to the initial source snapshot.

## 1.0.5 - 2026-07-31

### Native player tools and recovery

- Added picture-in-picture using the existing Slint/Winit window, including compact always-on-top presentation, player controls, close interception, and restoration of the previous window state.
- Added A/B repeat markers and validation, episode-aware sleep timers, subtitle-inclusive PNG frame capture with reveal-in-folder support, and Auto, Passthrough, Tone Map, and Disabled HDR modes.
- Added independent primary and secondary subtitle selection with conflict prevention and secondary position/scale controls.
- Added embedded chapter projection and deterministic merging with TheIntroDB intro, recap, credits, and outro segments.
- Added generation-guarded playback recovery that retries the same failed source once, resumes VOD near the last stable position, returns live streams to the live edge, and never selects another stream automatically.
- Added an in-player stream picker that preserves the current media context, playback position, pause state, and subtitle preferences when changing source.
- Replaced fixed player-key dispatch with typed, remappable Winit hotkeys supporting modifiers, hold/release actions, reserved-combination checks, conflict feedback, and input-focus suppression.

### Profiles, credentials, and localization

- Added transactional schema migrations for local profiles, profile-scoped settings and Core storage, integrations, downloads, and local-media indexes, with a timestamped database backup before the first migration and legacy tables retained for rollback.
- Added Owner, Standard, and Kids profiles, Argon2id PIN hashing, failed-attempt throttling, parental decisions, startup profile selection, profile management, and live profile switching without restarting the process.
- Added a platform credential-store abstraction backed by Windows Credential Manager and Linux Secret Service, with no plaintext SQLite fallback.
- Moved Stremio authentication keys behind a vault sentinel during Core storage reads and writes, and added vault-backed storage for integration secrets and signed download sources.
- Added Fluent-based native localization resources for English, Portuguese, and Arabic, including English fallback, locale-aware formatting helpers, pluralization, and RTL layout signaling.

### Streams, debrid, and metadata

- Added deterministic offline stream parsing and Smart, Quality, Smallest, Seeders, and Original ranking modes with positive/negative score explanations and recoverable filtering for suspicious or fake results.
- Added typed Real-Debrid, AllDebrid, Premiumize, Debrid-Link, and TorBox integrations with vault-only credentials, connection/account checks, bounded availability requests, timeouts, and short-lived caching.
- Added incremental metadata-provider interfaces for TMDB, OMDb, Fanart.tv, RPDB, Kitsu, AniZip, and Trakt, with provider attribution and region-aware watch-provider enrichment that cannot block canonical Stremio metadata.
- Added a compatible community-addon adapter with trending/rating ordering, language and type filters, pagination, manifest URL validation, caching, and Owner-PIN gating for adult addons.
- Added Movies, Shows, Anime, and Kids navigation entries as native discovery views while retaining Stremio metadata as the canonical source.

### Downloads and local media

- Replaced browser-only download actions with a native, profile-scoped manager supporting queued, resolving, downloading, paused, completed, failed, and cancelled jobs.
- Added HTTP Range resume, `.part` recovery, atomic completion, two-job concurrency, bandwidth limits, vault-backed source data, restart recovery, collision-safe filenames, and subtitle sidecars.
- Added a Downloads room with pause, resume, retry, cancel, play, refresh, and reveal actions; completed media is never deleted implicitly.
- Added device-global local-library roots with recursive scanning and filesystem watching, filename/NFO movie and episode parsing, lightweight fingerprints, move/duplicate detection, external subtitle discovery, repair tools, and direct libmpv playback.

### Customization, backup, and operations

- Added versioned native theme and player-layout manifests with Side Rail, Top Bar, Minimal, and Classic presets, validated fonts/images, managed profile assets, mandatory player controls, safe import/export, and reset paths.
- Added logical backup and restore with manifest validation, restore preview, a safety backup before apply, default secret exclusion, and optional Argon2id/XChaCha20-Poly1305 encrypted secret export.
- Added vault-backed webhook and Telegram notification settings, regional and bandwidth controls, an opt-in connection speed test, and a redacted local diagnostic ZIP with a prefilled issue link.
- Added native profile, integrations, operations, hotkey-editor, Downloads, and local-library Slint surfaces while preserving the existing Stremio visual system.



## 1.0.4 - 2026-07-25

### Downloads
- **Windows**: [Installer](https://github.com/perpetus/stremio-native/releases/download/v1.0.4/StremioSetup-v1.0.4-x64.exe) | [Updater package](https://github.com/perpetus/stremio-native/releases/download/v1.0.4/stremio-native-v1.0.4-x86_64-pc-windows-msvc.zip)
- **Linux**: [Binary](https://github.com/perpetus/stremio-native/releases/download/v1.0.4/stremio-native-v1.0.4-x86_64-unknown-linux-gnu)
- **Debian / Ubuntu**: [DEB Package](https://github.com/perpetus/stremio-native/releases/download/v1.0.4/stremio-native_1.0.4-1_amd64.deb)
- **Arch Linux**: [Package](https://github.com/perpetus/stremio-native/releases/download/v1.0.4/stremio-native-1.0.4-1-x86_64.pkg.tar.zst)
- **Fedora**: [RPM Package](https://github.com/perpetus/stremio-native/releases/download/v1.0.4/stremio-native-1.0.4-1.fc44.x86_64.rpm)

### Reliable Windows self-updates
- Stages the downloaded installer first, hands it to the outer application lifecycle, and launches it only after the UI, playback engine, tray, and stream server have shut down.
- Enables Inno Setup logging and a force-close fallback for older clients that launch Setup before their shutdown completes.
- Moves fresh native installations to `%LOCALAPPDATA%\Programs\stremio-native`, avoiding collisions with the official Stremio installation while preserving the registered directory for upgrades.

### Native operating-system media controls
- Adds SMTC on Windows, MPRIS on Linux, and MediaRemote integration on macOS for metadata, playback status, play/pause, seeking, stop, and next-episode commands.
- Adds a Windows taskbar thumbnail play/pause control and taskbar playback progress, with a stable AppUserModelID for correct application naming and icon resolution.
- Holds an operating-system sleep inhibitor only while video is playing and releases it on pause, stop, player close, or process shutdown.

## 1.0.3 - 2026-07-24

### Browser-style smooth scrolling
- Retuned Slint's scroll physics to browser values across every scroll surface: a 100px wheel notch (was 60px), a 250ms cubic ease-out per notch (was a 180ms quadratic step), and a 1200px/s² fling deceleration (was 2000px/s²).
- Added a cubic ease-out simulation matching the Chromium impulse response, integrated incrementally so it composes with `ListView`'s mid-animation viewport corrections instead of fighting them.
- Kept touchpad gestures tracking the fingers 1:1 by applying the notch scale only to phaseless wheel steps.
- These live in `vendor/i-slint-core`; see `vendor/i-slint-core/PATCHES.md`.

### Runtime performance and responsiveness
- Reused Turso connections and in-process Axum routers, applied connection PRAGMAs once, and moved log retention work off the write path.
- Removed the fixed 4ms state-projection delay, reused Slint stream models, and reduced image decode allocations and disk-cache syscalls.
- Shared MPV track state across high-frequency updates, parsed track metadata in one pass, rendered video before Slint composition, and moved shader filesystem preparation off the UI thread.
- Shortened the stream-row highlight transition and stopped the player buffering timer when playback is idle.

### Maintenance and playback polish
- Updated the Rust dependency set, including Turso, Tokio, mlua, zip, and arboard.
- Restored player shortcut focus after automatic file transitions and improved native pointer visibility recovery.

## 1.0.2 - 2026-07-22

### Downloads
- **Windows**: [Installer](https://github.com/perpetus/stremio-native/releases/download/v1.0.2/StremioSetup-v1.0.2-x64.exe) | [Portable](https://github.com/perpetus/stremio-native/releases/download/v1.0.2/stremio-native-v1.0.2-x86_64-pc-windows-msvc.zip)
- **Linux**: [Binary](https://github.com/perpetus/stremio-native/releases/download/v1.0.2/stremio-native-v1.0.2-x86_64-unknown-linux-gnu)
- **Debian / Ubuntu**: [DEB Package](https://github.com/perpetus/stremio-native/releases/download/v1.0.2/stremio-native_1.0.2_amd64.deb)
- **Arch Linux**: [Package](https://github.com/perpetus/stremio-native/releases/download/v1.0.2/stremio-native-1.0.2-1-x86_64.pkg.tar.zst)
- **Fedora**: [RPM Package](https://github.com/perpetus/stremio-native/releases/download/v1.0.2/stremio-native-1.0.2-1.x86_64.rpm)

### Skia rendering and video upscalers
- Replaced the forced FemtoVG/GLES path with Slint's Skia OpenGL renderer and explicit native OpenGL context selection.
- Added typed OpenGL diagnostics and capability gating so Anime4K and FSR are enabled only on desktop OpenGL 3.3 or newer while ordinary playback remains available on older or embedded contexts.
- Made MPV shader configuration atomic, preserved Skia's OpenGL state across MPV callbacks, and exposed acknowledged shader availability and status throughout the player and onboarding UI.
- Added all six Anime4K modes plus AMD FSR with persistent preferences, download/readiness coordination, rejection handling, and safe context recreation.

### Native timeline thumbnail previews
- Replaced the process-per-hover thumbnail draft with a persistent secondary libmpv worker that requires no standalone `mpv.exe`, IPC socket, temporary image, or PNG decoder.
- Added prewarming, coalesced keyframe seeks, delayed exact refinement, stale-request protection, rotation handling, tightly packed RGBA readback, and a bounded 16 MiB exact-frame cache.
- Added aspect-preserving seekbar presentation, loading and timestamp states, immediate settings control, legacy preference migration, and graceful fallback for live, non-seekable, and audio-only sources.
- Added ThumbFast attribution and the referenced MPL 2.0 source notice.

### Onboarding and interface fixes
- Added the native onboarding flow and playback setup page, including upscaler capability status, hardware acceleration, seek duration, subtitle preferences, release highlights, and onboarding audio.
- Corrected renderer-dependent layout and hit testing across onboarding, media carousels, overlays, sliders, board cards, player controls, and settings.
- Reduced redundant image projections and strengthened stale UI update rejection during asynchronous navigation and playback changes.

### Windows build and packaging
- Kept release binaries at the x86-64-v3 baseline while linking native vcpkg libraries statically against the dynamic MSVC CRT expected by rust-skia.
- Added app-local VC143 runtime staging, dependency verification with `dumpbin`, and a SHA-256 runtime manifest so installer users do not need a separate redistributable installation.
- Increased MSVC PDB page size for the large stream-server debug DLL, preventing nondeterministic `LNK1318: LIMIT (12)` failures without discarding debug symbols.
- Updated GitHub Actions triplets, caches, artifact staging, and installer inputs for the static-library/dynamic-CRT configuration.

## 1.0.1 - 2026-07-19

### Downloads
- **Windows**: [Installer](https://github.com/perpetus/stremio-native/releases/download/v1.0.1/StremioSetup-v1.0.1-x64.exe) | [Portable](https://github.com/perpetus/stremio-native/releases/download/v1.0.1/stremio-native-v1.0.1-x86_64-pc-windows-msvc.zip)
- **Linux**: [Binary](https://github.com/perpetus/stremio-native/releases/download/v1.0.1/stremio-native-v1.0.1-x86_64-unknown-linux-gnu)
- **Debian / Ubuntu**: [DEB Package](https://github.com/perpetus/stremio-native/releases/download/v1.0.1/stremio-native_1.0.1_amd64.deb)
- **Arch Linux**: [Package](https://github.com/perpetus/stremio-native/releases/download/v1.0.1/stremio-native-1.0.1-1-x86_64.pkg.tar.zst)
- **Fedora**: [RPM Package](https://github.com/perpetus/stremio-native/releases/download/v1.0.1/stremio-native-1.0.1-1.x86_64.rpm)

### Packaging and Distribution
- Added **Flatpak** infrastructure including desktop entry, appstream metainfo, and a sandboxed Boost compilation module.
- Added **Debian (`.deb`)** package generation.
- Added **Arch Linux (`.pkg.tar.zst`)** package generation using `makepkg` (optimized to reuse the precompiled Linux binary).
- Added **Fedora (`.rpm`)** package generation using `rpmbuild`.

### Lazy Loading and UI Polish
- Integrated a lazy-loaded plugin system.
- Optimized catalog updates in `board.rs` and `search.rs` to use in-place updates, preventing scroll-snapping in ListView during lazy loading.
- Bumped application version to `1.0.1`.

## 1.0.0 - 2026-07-18

### Desktop lifecycle and startup

- Projects the persisted model for the active tab synchronously before starting network loads and performs tab-entry projection on Slint's UI thread, preventing an older queued snapshot from replacing newer Continue Watching, Calendar, Library, Discover, Addons, or Settings state.
- Runs Core sequential effects through a single FIFO executor instead of spawning them concurrently, so an older library/profile storage snapshot cannot commit after a newer snapshot and reappear after restart.
- Reloads only the bounded initial Board/Search catalog range when first-login profile hydration introduces addon catalogs that Core intentionally leaves unloaded, removing the former full-restart requirement.
- Invalidates Calendar's cached metadata requests only when its relevant library items or addon catalogs change; unchanged revisits reuse the ready schedule without a network reload.
- Uses Core's canonical storage-key constants at startup, retains a legacy `server_urls` read fallback, and maps the legacy JSON filename to the canonical `streaming_server_urls` database key.
- Shows the Slint client and starts its event loop before icon lookup, tray/update setup, database initialization, stream-server startup, storage hydration, Core construction, or MPV initialization; the responsive loading UI is now the first startup milestone and reports `shell_ready_ms` for cold-start profiling.
- Runs stream-server startup and all independent Core storage reads concurrently after configuration is available.
- Configures Turso WAL through its row-returning query path, batches the remaining pragmas and schema work, migrates legacy storage in one transaction, and defers log/image-table maintenance beyond the first-frame window.
- Initializes libmpv's shared OpenGL context from the first available render callback after deferred engine startup and requests that callback explicitly, fixing audio-only playback when Slint's one-time graphics-setup event occurred before MPV was ready.
- Queues network-backed external subtitles through libmpv's asynchronous command API and cancels outstanding subtitle requests before Stop, keeping the MPV actor free to process Player Back immediately while preserving ordered `loadfile`/`stop` semantics.
- Reprojects a matching ready Core details model immediately on every details entry path, preventing repeat visits to cached titles from waiting forever for a state event Core correctly omits.
- Keeps details-page Back navigation available during genuine metadata loading so a failed or slow request cannot trap the client on its skeleton state.
- Removes the tray component by ownership during post-event-loop shutdown instead of changing its finalized visibility property, preventing Slint's `Constant property being changed` panic on quit.
- Adds a native system tray with GUI-relevant actions for opening Stremio, Settings, logs, update checks, installation, and quit; closing the window now respects the quit-on-close setting and otherwise hides to the tray.
- Queues tray-driven show/navigation operations onto Slint's event loop to avoid re-entrant Winit window borrows.
- Adds single-instance activation plus official `stremio:` and `magnet:` deep-link forwarding, with commands queued until Core and playback are ready.
- Keeps the latest Discord activity pending while IPC is unavailable, retries connection with a bounded 2-to-30-second backoff, and treats media/pause/resume activity changes as reconnect opportunities without blocking the UI.

### Native shell and UI polish

- Uses the official desktop card interaction split: one Discover click selects and loads the metadata preview, a double-click opens full details, and Library retains its one-click details route through the same shared card primitive.
- Uses the stream-server's exact `icon_48.png` and `app.ico` assets for the tray, Slint window, taskbar, executable resources, and Windows installer.
- Applies the official shell's `#15122b` Windows caption color with white caption text while keeping the operating system's native title-bar controls.
- Centers the Stremio navigation mark against the same fixed rail and header tokens used by the sidebar icons at every responsive UI scale.
- Vertically centers the details stream-row play button in a full-height action slot, including rows whose descriptions wrap.
- Adds localized tray/update strings, a web-style language selector, application/build/shell versions in their official Settings positions, and shell version `1.0.0`.
- Adds an official-style update notification and installer flow backed by GitHub releases through `self_update`.

### Playback dependency and release system

- Replaces the tracked static MPV SDK with the pinned optimized x86-64-v3 `libmpv-2.dll` and COFF import library from the trusted shinchiro GitHub release.
- Compiles Windows and Linux x64 Rust release code for the reproducible `x86-64-v3` CPU baseline, while the local Windows vcpkg graph uses a distinct `x64-windows-v4-static-release` triplet with `/arch:AVX512`. The separation prevents v3/v4 cache reuse and avoids runner-specific `target-cpu=native` output.
- Downloads, extracts, SHA-256 verifies, caches, links, and deploys the DLL and pinned licenses directly from the Rust build script; Cargo builds no longer require PowerShell, 7-Zip, or repository-stored media binaries.
- Resolves dynamic libmpv through `pkg-config` on Linux, with `STREMIO_MPV_DIR` retained as an explicit local SDK override.
- Pins the current Core head plus its `flate2` compatibility correction from `perpetus/stremio-core`, and pins the lifecycle-fixed stream-server revision through remote Git dependencies, so clean CI checkouts do not rely on sibling repositories.
- Disables stream-server's standalone Windows EXE resource table only when it is embedded, preventing duplicate `VERSIONINFO`/icon resources while preserving the GUI executable's own `1.0.0` metadata.
- Preserves only OpenGL state supported by the active context and uses an ES2-compatible RGBA render target, preventing ES3-only libmpv sharing operations from leaking `GL_INVALID_ENUM` into Slint/FemtoVG on Windows.
- Adds clean Windows and Linux release jobs. The Windows job also produces the Inno Setup installer and GitHub updater archive.
- Provisions the optimized static libtorrent 2.0.13 dependency on clean Windows runners through stream-server's pinned vcpkg baseline, overlay, triplet, and GitHub Actions cache.
- Publishes tagged `v*` builds automatically after both platforms pass, with updater-compatible assets, the Linux binary, SHA-256 checksums, direct download links, the matching detailed changelog section, categorized commit links, and a full comparison link.

### Resource baseline

- The settled `1.0.0` process measured 358.6 MB private working set and 0.19% five-second CPU, 455.8 MB (56.0%) below the retained 814.4 MB official Stremio WebView2 baseline.

## Earlier implementation baseline - 2026-07-16

### Highlights

- Reworked the desktop shell and all primary pages around a reusable Slint component system aligned with the official Stremio interaction model.
- Restored end-to-end libmpv playback with direct OpenGL texture composition, full player controls, deterministic first-frame handling, and event coalescing.
- Added typed navigation, global search, Discord Rich Presence, TheIntroDB skip segments, centralized Turso storage, and Windows media-key handling.
- Reduced retained image memory and redundant model/UI work; the latest settled native build measured 406.7 MB in Task Manager versus the retained 814.4 MB official Stremio baseline.

### Application shell and navigation

- Added a typed `NavigationController` with explicit routes for tabs, search, metadata details, addon details, and the player.
- Added back and forward history, route revisions, discover-preview selection, and stale-request rejection so late asynchronous responses cannot overwrite newer navigation.
- Centralized projection of active tab, details, addon dialog, and player visibility into `MainWindow`.
- Reworked the sidebar and top navigation with official Stremio assets, expandable labels, search, profile controls, and window actions.
- Added global keyboard shortcuts plus Windows media-key handling for play, pause, and next episode.
- Added pause-on-occlusion behavior when the corresponding player preference is enabled.

### Slint UI and design system

- Expanded the theme from a small palette into semantic tokens for backgrounds, modal and drawer surfaces, controls, overlays, dividers, scrims, status colors, focus, title bar, muted text, and skeleton states.
- Added reusable action groups, buttons, checkboxes, radio buttons, number and color inputs, text/search inputs, selects, sliders, overlays, horizontal navigation and scrolling, shortcuts, transitions, fallbacks, feedback states, loading placeholders, media carousels, metadata rows, metadata previews, and share prompts.
- Rebuilt Board, Discover, Library, Calendar, Addons, Details, Search, Auth, Settings, and Player pages to use the shared component system.
- Added loading, empty, error, placeholder, context-menu, modal, bottom-sheet, and drawer states across the main routes.
- Added Continue Watching projection alongside board catalogs without hiding it during catalog refreshes.
- Added Discover split-preview navigation, metadata actions, genre/catalog filters, and grid presentation.
- Added global search suggestions and a dedicated results route.
- Added addon source/type filters, installed/community grouping, add-addon flow, details, configuration state, and install/uninstall actions.
- Added calendar item projection and isolated calendar navigation into metadata details.

### Playback and libmpv

- Reconnected Stremio Core stream selection to the statically linked libmpv runtime and Slint's shared OpenGL render path.
- Passes resume position directly through MPV's per-file `start=` option, removing the delayed second exact seek after `file-loaded`.
- Distinguishes render-context initialization from a real decoded frame and reveals video only after the first actual MPV render update.
- Keeps cache buffering separate from initial loading so decoded video is not covered by artwork during later buffering.
- Corrected Windows borrowed-texture orientation with `MPV_RENDER_PARAM_FLIP_Y = 0` and retained aspect-preserving presentation.
- Added a coalescing playback-event inbox that replaces adjacent high-frequency state snapshots without reordering control events.
- Added a UI projection cache and scheduler so only changed playback properties are sent to Slint and at most one state update is queued at a time.
- Added playback, pause, seek, short-seek, volume, mute, fullscreen, speed, audio-track, subtitle-track, subtitle-language, episode, scale, and stream callbacks.
- Added buffered progress, playback statistics, track metadata, stream metadata, player error state, episode drawer, and auto-hiding controls.
- Added first-frame and load timing diagnostics for future playback profiling.
- Added tests for playback event coalescing and orderly inbox shutdown.

### Discord and skip segments

- Added an isolated Discord Rich Presence worker with connect, disconnect, set activity, clear activity, artwork, and playback timestamp support.
- Added configurable Discord activity projection from the current media and playback state.
- Added TheIntroDB v3 segment retrieval for intros, recaps, credits, and previews, with request timeouts and optional bearer authentication.
- Added configurable segment types and context-sensitive skip buttons in the player.
- Added boundary tests for active skip-segment selection.

### Storage and configuration

- Centralized application and Stremio Core storage on a shared Turso database installed through `core-env`.
- Added WAL, normal synchronous mode, memory temp storage, and a bounded SQLite page cache for the local database.
- Added `core_storage`, settings, and logs schemas plus batch setting reads/writes and log pruning.
- Added migration of legacy JSON storage buckets into Turso and migration of `config.json` into the database with a `.bak` handoff.
- Removed the obsolete database-backed image BLOB cache in favor of the bounded memory/filesystem image pipeline.
- Added versioned application configuration and migration of the generated legacy palette while preserving user-customized themes.
- Added configuration for TheIntroDB credentials and per-segment visibility.
- Merges both `library_recent` and `library` storage buckets during startup to preserve recent and long-term library state.

### Performance and resource use

- Registered MiMalloc as the global allocator.
- Reduced the base decoded-image cache from 256 MiB to 32 MiB and added a separate required-image working set with a 60-second idle expiry.
- Added bounded image-fetch workers with independent network, disk-read, and decode concurrency limits.
- Batches image refreshes onto the Slint event loop and safely rearms refresh delivery when new URLs arrive during a pending update.
- Added stable fingerprints for catalogs, profiles, details, calendar, search, addons, and stream lists to skip unchanged model projections.
- Patches cached images into existing Slint models rather than rebuilding entire page models for every image completion.
- Coalesces high-frequency MPV state and redraw work before it reaches the UI thread.
- Avoids rewriting generated icon fonts and Slint font imports when their contents have not changed.
- Loads independent storage buckets concurrently and keeps blocking stream-server startup off the asynchronous executor.
- Added scoped profiling modes for UI, I/O, playback, and full traces in debug builds.
- Compiles tracing out of release builds to minimize production profiling overhead.

### Reliability and diagnostics

- Added a synchronous panic-log fallback with captured backtraces for failures that occur before the non-blocking logger drains.
- Handles poisoned synchronization primitives through explicit recovery instead of panicking in the UI and playback bridges.
- Added navigation tests for invalid tabs, player/details back behavior, rapid metadata navigation, search history, forward history, and addon-route scoping.
- Added configuration migration tests and image-cache budget tests.
- Preserves the Windows GUI subsystem in release builds so launching the application does not open a console window.

### Dependencies and build

- Added `discord-rich-presence`, MiMalloc, and the Slint Winit 0.30 integration required for native media-key events.
- Uses the workspace Turso dependency with default features disabled, reducing the dependency graph and avoiding unwanted default integrations.
- Updated `Cargo.lock` to the dependency closure used by the current successful release build.
- Release build command: `cargo build --release --package stremio-native`.

### Known limitations

- The bundled static libmpv SDK and `playback-mpv/build.rs` currently support only `x86_64-pc-windows-msvc`.
- UI parity work still benefits from manual visual validation at multiple window sizes and DPI scales.
- Playback, subtitle/audio menu behavior, and real streaming should be smoke-tested with live media after each renderer or player change.
- The player buffering pulse timer is not yet gated by player-page visibility; the measured minimized CPU use is low, but a dedicated redraw trace is still recommended.
- Standalone Slint preview files, runtime preview JSON, and captured QA screenshots are development artifacts and are intentionally not part of the release-build commit.
