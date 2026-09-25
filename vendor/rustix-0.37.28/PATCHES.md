# stremio-native patches to `rustix`

Vendored verbatim from crates.io `rustix 0.37.28`, then patched to disable one
build-script probe. Wired up through `[patch.crates-io]` in the root
`Cargo.toml`. The only change is marked `STREMIO-NATIVE PATCH` in `build.rs`,
so `rg "STREMIO-NATIVE PATCH"` finds it directly.

## Why this exists

Under this workspace's pinned toolchain (`rust-toolchain.toml`,
`nightly-2026-07-31`), `cargo check -p stremio-native` failed with:

```
error: cannot find attribute `rustc_layout_scalar_valid_range_start` in this scope
  --> rustix-0.37.28/src/backend/linux_raw/io/errno.rs:28:25
```

`rustix 0.37.28`'s `build.rs` unconditionally calls
`use_feature_or_nothing("rustc_attrs")`, which probes only whether the
compiler superficially accepts `#![feature(rustc_attrs)]` — not whether the
specific internal attributes rustix then applies
(`rustc_layout_scalar_valid_range_start`/`_end`,
`rustc_nonnull_optimization_guaranteed`) are actually resolvable on that
toolchain. `nightly-2026-07-31` accepts the feature gate but not those
attributes, so the probe passes and the subsequent `cfg_attr`-gated use sites
fail to compile.

Confirmed via authenticated `gh` CLI access to the real upstream
`stremio-native/stremio-native` GitHub Actions history that this is not a
local or memory-pressure artifact: this exact error has failed on *every*
Clippy CI run, on clean unconstrained cloud runners, for over a month — since
the commit that introduced the `nightly-2026-07-31` pin — independent of
whatever unrelated change each PR made.

Confirmed via `cargo update -p rustix@0.37.28 --precise <newer> --dry-run`
that a plain version bump is not available as a fix: `0.37.28` is already the
newest `0.37.x` release, and `async-io 1.13.0` (pulled in via
`souvlaki 0.8.3` → `zbus 3.15.2`, the latest available `souvlaki` release)
hard-requires `rustix = "^0.37.1"`, which categorically excludes `0.38+`.
There is no available upgrade path for that chain today.

Upstream rustix itself already fixed this class of problem: starting at
`0.38.44` (confirmed by diffing `build.rs` across `0.37.28`, `0.38.44`, and
`1.1.4` from the local registry cache), the `rustc_attrs` probe was made
opt-in-only, gated behind the `rustc-dep-of-std` feature or an explicit
`RUSTFLAGS=--cfg=rustix_use_experimental_features`, and defaults to off for a
normal build. This patch backports exactly that behavior to `0.37.28` by
simply never calling the probe. Every call site that consumes the `rustc_attrs`
cfg is already `cfg_attr`-gated, so this is a no-op for correctness — it only
forgoes an internal niche-optimization hint (a scalar-validity-range niche for
some FFI integer wrappers), never soundness.

## The patch

### `build.rs`

One line, commented out: the unconditional `use_feature_or_nothing("rustc_attrs")`
call near the top of `main()`. See the `STREMIO-NATIVE PATCH` comment at the
call site for the full rationale.

## Validation

- `cargo check -p 'rustix@0.37.28' --offline` under the pinned
  `nightly-2026-07-31` toolchain: clean, zero errors (only pre-existing,
  unrelated lint warnings).
- `cargo check -p stremio-native --offline` under the same toolchain: gets
  past the point that previously failed (`rustix`'s `errno.rs`) and compiles
  dozens of further crates (including proc-macro/codegen-heavy ones like
  `aws-lc-sys`, `skia-bindings`, `i-slint-renderer-skia`, `gtk`) before
  reaching an unrelated, pre-existing `libtorrent-rasterbar` pkg-config
  version mismatch (`stream-server`'s `libtorrent-sys` build script; see
  `investigation/HANDOFF.md` history) — not a rustix or toolchain issue.
- A separate LLVM-codegen `SIGSEGV`, previously seen only on two alternate,
  non-pinned nightly dates (see `investigation/HANDOFF.md`), did not recur
  during this full-workspace build under the patched, pinned nightly,
  suggesting it is tied to those specific alternate nightlies rather than
  general host or toolchain instability.

## Re-vendoring a newer rustix release

Only needed if the `souvlaki`/`zbus`/`async-io` chain above ever gains an
upgrade path off `rustix 0.37.x`, at which point this entire vendor directory
and its `[patch.crates-io]` entry should simply be deleted rather than
re-vendored — the fix this patch backports already ships upstream from
`0.38.44` onward.

If rust-toolchain.toml's pinned nightly ever changes and the underlying
`rustc_attrs` incompatibility reappears against a different rustix version in
this same `0.37.x` line:

1. `cp -r ~/.cargo/registry/src/*/rustix-<version>/. vendor/rustix-<version>/`,
   then delete `.cargo-ok` and `.cargo_vcs_info.json`.
2. Comment out the same `use_feature_or_nothing("rustc_attrs")` call in the
   new `build.rs` and restore this file (update the version numbers above).
3. Update the `[patch.crates-io]` path in the root `Cargo.toml` and the
   `vendor/` directory name to match.
4. `cargo check -p 'rustix@<version>' --offline`.
