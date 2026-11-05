# stremio-native patches to `i-slint-core`

Vendored verbatim from crates.io `i-slint-core 1.17.1`, then patched to give
scrolling browser physics. Wired up through `[patch.crates-io]` in the root
`Cargo.toml`. Every change is marked with a `STREMIO-NATIVE PATCH` comment, so
`rg "STREMIO-NATIVE PATCH"` lists the full divergence.

## Why this lives in the engine

The obvious place for this is the UI layer — a `ScrollView` with
`animate viewport-y { duration: 250ms; easing: cubic-bezier(0.25, 0.1, 0.25, 1.0); }`.
That does not work here. `ListView` rewrites `viewport-y` from its own layout
pass whenever it revises a size estimate for a virtualized row
(`model/repeater.rs`, `update_visible_instances`). A declarative property
animation would animate those corrections too and visibly stutter, and Discover
and Library — the two surfaces that matter most — are both `ListView`s.

Slint solves this internally with *incremental* physics animations, which only
ever apply a per-frame delta and therefore compose with an external writer.
Upstream says as much in `items/flickable.rs`:

> Note that this animation must support the viewport_x/_y and width/height
> changing, as e.g. the ListView might resize the viewport if it gets a new size
> estimate. At the time of writing, in practice this means we must use a physics
> animation.

So the retune has to happen where those physics live. Patching here also means
no `.slint` call site has to opt in: every `ScrollView`, `ListView`, `Flickable`
and drag-pan surface in the app picks it up at once.

## The patches

### `items/flickable.rs`

| Constant | Upstream | Here | Rationale |
|---|---|---|---|
| `DECELERATION` | `2000` px/s² | `1200` px/s² | Upstream stops a 1000px/s fling after 250px. Browsers decelerate at 980–1200 and glide 416px, the difference between stiff and fluid. |
| `WHEEL_SCROLL_DURATION` | `180ms` | `250ms` | Chromium and Firefox both land in the 200–250ms band per notch. |
| `WHEEL_NOTCH_SCALE` | *(new)* | `100/60` | `i-slint-backend-winit` hands us 60px per wheel line; browsers scroll a 100px notch (3 lines × 33.3px). |

Also in this file:

- `is_notched_wheel_step()` — new helper naming the existing test that separates
  a notched wheel (`Moved` only) from a phased touchpad gesture (`Started` →
  `Moved` → `Ended`). The notch scale applies only to the former; touchpad
  deltas must stay 1:1 with the fingers.
- The wheel animation now builds `CubicEaseOutParameters` instead of
  `ConstantDecelerationParameters`, and `running_animation` holds the new type.

Applying the notch scale here rather than in `i-slint-backend-winit` is
deliberate: it keeps the scale next to the only branch that already knows the
device kind, and avoids vendoring a second crate.

### `animations/physics_simulation.rs`

Adds `CubicEaseOutParameters` / `CubicEaseOut`, tracing `1 - (1 - u)^3` — the
Chromium impulse response, and the curve `cubic-bezier(0.25, 0.1, 0.25, 1.0)`
approximates. Upstream's constant deceleration traces `1 - (1 - u)^2`, which
reads as abrupt: it has covered 43.8% of the distance a quarter of the way in,
where the cubic has covered 57.8%.

Like `ConstantDeceleration` it integrates incrementally (see above), and it
carries `remaining_distance()` so the Flickable can fold an in-flight animation
into the next notch — that accumulation is what makes a fast scroll burst track
the input instead of restarting from the animated position each time.

`ConstantDecelerationParameters::{new_with_distance, remaining_distance}` lost
their only caller and are marked `#[expect(dead_code)]` rather than deleted, to
keep the re-vendor diff clean. `ConstantDeceleration` itself is still live: it
drives the fling after a touchpad or drag release.

## Re-vendoring a new Slint release

1. Bump the `slint` / `slint-build` versions in the root `Cargo.toml` and build
   once so cargo unpacks the new crate.
2. `cp -r ~/.cargo/registry/src/*/i-slint-core-<version>/. vendor/i-slint-core/`,
   then delete `.cargo-ok` and `.cargo_vcs_info.json`.
3. Re-apply the changes above (`git diff` against the previous vendored tree is
   the fastest guide) and restore this file.
4. `cargo build -p i-slint-core && cargo clippy --all-targets --locked`.

Note that the packaged crate cannot build its own test target: `#[cfg(test)]`
code in `textlayout/sharedparley.rs` `include_bytes!`s a font that ships only in
the git repo, and `item_tree.rs` has a test-only trait impl that no longer
matches. Both predate this fork. The tests added to
`animations/physics_simulation.rs` therefore document the curve but do not run
in CI; the math they assert was verified independently.

## Keyboard spatial navigation

Arrow-key navigation for a UI built around the pointer. Rather than making
every `.slint` component focusable, the engine treats each reachable, enabled
`TouchArea` (and editable `TextInput`) as a navigation target and drives it
like a mouse: moving the focus sends a synthetic pointer move (so existing
`has-hover` styling shows), activating sends a press/release (so existing
`clicked` handlers run), and targets scrolled out of a `Flickable` are revealed
first. Targets are collected in mouse hit-test order and hit-tested, so items
under a modal scrim are unreachable, exactly as they are to a click. This needs
item geometry, clip rects and the mouse dispatcher, none of which the public API
exposes — hence the engine.

The application decides policy through a hook and a small API; see
`app/src/keyboard_nav.rs`.

### `spatial_navigation.rs` *(new)*

- `SpatialDirection`, `SpatialNavResult`, `UnhandledKeyHook`.
- `WindowInner::set_unhandled_key_hook`, `spatial_navigate`,
  `spatial_activate`, `spatial_focus_item`, `spatial_refresh_hover`,
  `spatial_clear`, `spatial_focus_rect`, `text_input_has_focus`.

### `window.rs`

- `WindowInner` gains `spatial_focus` and `unhandled_key_hook`.
- `process_key_input` calls the hook for key presses the focus chain did not
  accept, before the built-in Tab/Escape handling.
- It also offers Up/Down to the hook *before* dispatch when a single-line
  `TextInput` has focus; upstream consumes them there (cursor to line
  start/end), which would trap navigation in every search field.

### `item_tree.rs`

- `ItemRc::absolute_clip_rect_and_geometry` is `pub(crate)` (was private).

### `lib.rs`

- `pub mod spatial_navigation;`
