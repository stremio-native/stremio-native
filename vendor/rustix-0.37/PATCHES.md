# stremio-native patches to `rustix` 0.37

Vendored verbatim from crates.io `rustix 0.37.28` (the final 0.37 release),
then patched so it compiles on the nightly pinned in `rust-toolchain.toml`.
Wired up through `[patch.crates-io]` in the root `Cargo.toml`; only the 0.37
line is replaced, rustix 0.38 and 1.x still come from crates.io. Every change
is marked with a `STREMIO-NATIVE PATCH` comment.

## Why

`souvlaki` (MPRIS media controls) uses zbus 3, which requires `async-io` 1.x,
which requires `rustix` 0.37. Its `build.rs` probes whether
`#![feature(rustc_attrs)]` compiles and, on nightly, sets `cfg(rustc_attrs)`.
Recent nightlies still accept the feature gate but removed
`rustc_layout_scalar_valid_range_start/end`, so the crate fails with
"cannot find attribute `rustc_layout_scalar_valid_range_start`".
The patch skips that probe; the cfg only enabled a niche-layout optimization
for `Errno`.

Drop this directory and the patch entry once souvlaki moves to zbus 4+ or the
toolchain moves back to stable.
