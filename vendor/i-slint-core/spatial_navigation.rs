// STREMIO-NATIVE PATCH: keyboard spatial navigation (new file).
//
// Arrow-key navigation for a UI that was built for the pointer. Instead of
// teaching every `.slint` component to take keyboard focus, this treats every
// reachable, enabled `TouchArea` (and editable `TextInput`) as a navigation
// target and drives it the way a mouse would:
//
// * moving the focus sends a synthetic pointer move to the target, so its
//   existing `has-hover` styling lights up;
// * activating sends a synthetic press/release, so its existing `clicked`
//   handler runs;
// * a target scrolled out of view inside a `Flickable` is revealed first.
//
// Candidates are collected in the same front-to-back order the mouse
// dispatcher uses, and each one is hit-tested so that anything covered by a
// modal scrim or another overlay is not reachable — exactly like a click.
//
// The application owns policy (whether navigation is enabled, which keys
// navigate, drawing a focus ring) through `WindowInner::set_unhandled_key_hook`
// and the `spatial_*` methods below.

use crate::input::{InternalKeyEvent, KeyEvent, MouseEvent, PointerEventButton};
use crate::item_tree::{
    ItemRc, ItemTreeRc, ItemVisitor, ItemVisitorVTable, ParentItemTraversalMode, TraversalOrder,
    VisitChildrenResult,
};
use crate::items::{Flickable, ItemRef, Opacity, TextInput, TouchArea};
use crate::lengths::{LogicalLength, LogicalPoint, LogicalRect};
use crate::window::WindowInner;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::pin::Pin;
use vtable::VRc;

/// Called with every key press nothing in the focus chain consumed. Return
/// `true` to mark the key as handled.
pub type UnhandledKeyHook = dyn Fn(&KeyEvent) -> bool;

/// A navigation direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpatialDirection {
    Left,
    Right,
    Up,
    Down,
}

/// What [`WindowInner::spatial_navigate`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpatialNavResult {
    /// The focus moved to another target.
    Moved,
    /// No target lay in that direction, so the enclosing scroll area was
    /// scrolled instead. Virtualized lists only instantiate rows once they
    /// approach the viewport, so trying again after a frame may now succeed.
    Scrolled,
    /// Nothing happened.
    Unchanged,
}

/// Gap kept between a revealed target and the edge of its scroll area.
const REVEAL_MARGIN: f32 = 24.;
/// Smallest side, in logical pixels, a target must have.
const MIN_TARGET_SIDE: f32 = 10.;

pub(crate) fn is_single_line_text_input(item: &ItemRc) -> bool {
    item.downcast::<TextInput>().is_some_and(|t| {
        let t = t.as_pin_ref();
        t.single_line() && t.enabled()
    })
}

struct Entry {
    item: ItemRc,
    /// Visible part of the item in window coordinates (empty when off screen).
    visible: LogicalRect,
    /// Full item geometry in window coordinates.
    full: LogicalRect,
    /// Fully scrolled out of a `Flickable`; reachable by scrolling only.
    offscreen: bool,
    /// Passes the size/opacity rules to be a navigation target.
    target: bool,
    /// The innermost `Flickable` that clips part or all of the item away.
    clipper: Option<ItemRc>,
}

#[derive(Clone, Copy)]
struct Reach {
    /// Where to send pointer events so they reach the item.
    point: LogicalPoint,
}

fn rect_is_empty(r: &LogicalRect) -> bool {
    !(r.size.width > 0. && r.size.height > 0.)
}

fn interactive_kind(item: &ItemRc) -> bool {
    if let Some(t) = item.downcast::<TouchArea>() {
        return t.as_pin_ref().enabled();
    }
    if let Some(t) = item.downcast::<TextInput>() {
        let t = t.as_pin_ref();
        return t.enabled() && !t.read_only();
    }
    false
}

fn effectively_transparent(item: &ItemRc) -> bool {
    let mut cur = Some(item.clone());
    while let Some(i) = cur {
        if let Some(o) = i.downcast::<Opacity>()
            && o.as_pin_ref().opacity() < 0.05
        {
            return true;
        }
        cur = i.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    false
}

fn is_target_shape(full: &LogicalRect, window: &LogicalRect) -> bool {
    let (w, h) = (full.size.width, full.size.height);
    let (ww, wh) = (window.size.width.max(1.), window.size.height.max(1.));
    let min = w.min(h);
    let max = w.max(h);
    if min < MIN_TARGET_SIDE {
        return false;
    }
    // Scroll bars and resize grips: long and thin.
    if min < 20. && max / min > 10. {
        return false;
    }
    // Scrims, backdrops and page-sized click catchers.
    if w * h > 0.45 * ww * wh || (w >= 0.9 * ww && h >= 0.5 * wh) {
        return false;
    }
    true
}

/// Collects interactive items in mouse hit-test order: front-most first.
fn collect(item: &ItemRc, window: &LogicalRect, out: &mut Vec<Entry>) {
    // `visible: false` lowers to a zero-sized clipping element; skip it whole.
    let geom = item.geometry();
    if item.borrow().as_ref().clips_children() && rect_is_empty(&geom) {
        return;
    }

    let mut visitor = |tree: &ItemTreeRc, index: u32, _: Pin<ItemRef>| -> VisitChildrenResult {
        collect(&ItemRc::new(tree.clone(), index), window, out);
        VisitChildrenResult::CONTINUE
    };
    vtable::new_vref!(let mut visitor : VRefMut<ItemVisitorVTable> for ItemVisitor = &mut visitor);
    VRc::borrow_pin(item.item_tree())
        .as_ref()
        .visit_children_item(item.index() as isize, TraversalOrder::FrontToBack, visitor);

    if !interactive_kind(item) {
        return;
    }
    let (clip, full) = item.absolute_clip_rect_and_geometry();
    let visible = full.intersection(&clip).filter(|r| !rect_is_empty(r));
    let offscreen = visible.is_none();
    if offscreen && !item.is_visible_or_clipped_by_flickable() {
        return;
    }
    let target = is_target_shape(&full, window) && !effectively_transparent(item);
    let clipper = if visible.is_some_and(|v| contains_rect(&v, &full)) {
        None
    } else {
        clipping_flickable(item, &full)
    };
    out.push(Entry {
        item: item.clone(),
        visible: visible.unwrap_or_default(),
        full,
        offscreen,
        target,
        clipper,
    });
}

/// The innermost `Flickable` ancestor whose visible area does not fully
/// contain `full`.
fn clipping_flickable(item: &ItemRc, full: &LogicalRect) -> Option<ItemRc> {
    let mut cur = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    while let Some(anc) = cur {
        if anc.downcast::<Flickable>().is_some() {
            let (clip, geo) = anc.absolute_clip_rect_and_geometry();
            let area = geo.intersection(&clip).unwrap_or_default();
            if !contains_rect(&area, full) {
                return Some(anc);
            }
        }
        cur = anc.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    None
}

fn is_descendant_or_self(item: &ItemRc, ancestor: &ItemRc) -> bool {
    let mut cur = Some(item.clone());
    while let Some(i) = cur {
        if i == *ancestor {
            return true;
        }
        cur = i.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    false
}

fn hit(entries: &[Entry], p: LogicalPoint) -> Option<usize> {
    entries
        .iter()
        .position(|e| !e.offscreen && e.visible.contains(p))
}

fn nearest_flickable(item: &ItemRc) -> Option<ItemRc> {
    let mut cur = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    while let Some(i) = cur {
        if i.downcast::<Flickable>().is_some() {
            return Some(i);
        }
        cur = i.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    None
}

/// For an item clipped by a scroll area: reachable when that scroll area is
/// not itself covered by something else (a modal, for instance). The item is
/// revealed before it is hovered or clicked, so the aim is provisional.
fn reach_through_flickable(entries: &[Entry], e: &Entry) -> Option<Reach> {
    let flick = nearest_flickable(&e.item)?;
    let (clip, geo) = flick.absolute_clip_rect_and_geometry();
    let area = geo.intersection(&clip)?;
    match hit(entries, area.center()) {
        Some(j) if !is_descendant_or_self(&entries[j].item, &flick) => None,
        _ => Some(Reach {
            point: e.full.center(),
        }),
    }
}

/// Whether a click could reach entry `i`, and where to aim it.
fn reach(entries: &[Entry], i: usize) -> Option<Reach> {
    let e = &entries[i];
    if e.offscreen {
        // Can't hit-test what isn't on screen.
        return reach_through_flickable(entries, e);
    }
    let r = e.visible;
    let samples = [
        r.center(),
        LogicalPoint::new(r.min_x() + r.width() * 0.25, r.min_y() + r.height() * 0.25),
        LogicalPoint::new(r.min_x() + r.width() * 0.75, r.min_y() + r.height() * 0.25),
        LogicalPoint::new(r.min_x() + r.width() * 0.25, r.min_y() + r.height() * 0.75),
        LogicalPoint::new(r.min_x() + r.width() * 0.75, r.min_y() + r.height() * 0.75),
    ];
    let mut fallback = None;
    for p in samples {
        match hit(entries, p) {
            Some(j) if j == i => return Some(Reach { point: p }),
            Some(j) if fallback.is_none() && is_descendant_or_self(&entries[j].item, &e.item) => {
                fallback = Some(Reach { point: p })
            }
            _ => {}
        }
    }
    // A sliver at the edge of a scroll area is often under an overlay (a
    // scroll bar, a carousel arrow); it becomes clickable once revealed.
    fallback.or_else(|| {
        let partly_clipped =
            e.visible.width() < e.full.width() - 0.5 || e.visible.height() < e.full.height() - 0.5;
        if partly_clipped {
            reach_through_flickable(entries, e)
        } else {
            None
        }
    })
}

fn same_rect(a: &LogicalRect, b: &LogicalRect) -> bool {
    (a.min_x() - b.min_x()).abs() < 2.
        && (a.min_y() - b.min_y()).abs() < 2.
        && (a.max_x() - b.max_x()).abs() < 2.
        && (a.max_y() - b.max_y()).abs() < 2.
}

fn contains_rect(outer: &LogicalRect, inner: &LogicalRect) -> bool {
    outer.min_x() <= inner.min_x() + 0.5
        && outer.min_y() <= inner.min_y() + 0.5
        && outer.max_x() >= inner.max_x() - 0.5
        && outer.max_y() >= inner.max_y() - 0.5
}

/// Lower is better; `None` when `to` does not lie in `dir` from `from`.
fn score(dir: SpatialDirection, from: &LogicalRect, to: &LogicalRect) -> Option<f32> {
    if contains_rect(to, from) {
        return None;
    }
    let (fc, tc) = (from.center(), to.center());
    // The candidate's centre must lie past (or nearly past) the origin's edge,
    // so a neighbour in the next row that is merely offset by a few pixels is
    // not mistaken for the next item along this row.
    let tol_x = from.width().min(to.width()) * 0.25;
    let tol_y = from.height().min(to.height()) * 0.25;
    let (ahead, gap, orth_gap, main_center, orth_center) = match dir {
        SpatialDirection::Right => (
            tc.x > from.max_x() - tol_x && to.max_x() > from.max_x() + 1.,
            to.min_x() - from.max_x(),
            (to.min_y() - from.max_y()).max(from.min_y() - to.max_y()),
            tc.x - fc.x,
            tc.y - fc.y,
        ),
        SpatialDirection::Left => (
            tc.x < from.min_x() + tol_x && to.min_x() < from.min_x() - 1.,
            from.min_x() - to.max_x(),
            (to.min_y() - from.max_y()).max(from.min_y() - to.max_y()),
            fc.x - tc.x,
            tc.y - fc.y,
        ),
        SpatialDirection::Down => (
            tc.y > from.max_y() - tol_y && to.max_y() > from.max_y() + 1.,
            to.min_y() - from.max_y(),
            (to.min_x() - from.max_x()).max(from.min_x() - to.max_x()),
            tc.y - fc.y,
            tc.x - fc.x,
        ),
        SpatialDirection::Up => (
            tc.y < from.min_y() + tol_y && to.min_y() < from.min_y() - 1.,
            from.min_y() - to.max_y(),
            (to.min_x() - from.max_x()).max(from.min_x() - to.max_x()),
            fc.y - tc.y,
            tc.x - fc.x,
        ),
    };
    if !ahead {
        return None;
    }
    // Items sharing a row (or column) with the origin win by a wide margin;
    // after that the closest edge, then the best-aligned centre.
    Some(gap.max(0.) + orth_gap.max(0.) * 4. + orth_center.abs() * 0.25 + main_center.abs() * 0.05)
}

/// Scrolls every `Flickable` around `item` so the item (plus a margin) shows.
fn reveal(item: &ItemRc) {
    let mut cur = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    while let Some(anc) = cur {
        if let Some(f) = anc.downcast::<Flickable>() {
            let f = f.as_pin_ref();
            let flick_origin = anc.absolute_clip_rect_and_geometry().1.origin;
            let full = item.absolute_clip_rect_and_geometry().1;
            let (vx, vy) = (f.viewport_x().get(), f.viewport_y().get());
            // Window coordinates -> the flickable's content coordinates.
            let to_content = |x: f32, y: f32| {
                LogicalPoint::new(x - flick_origin.x - vx, y - flick_origin.y - vy)
            };
            f.reveal_points(
                &anc,
                &[
                    to_content(full.min_x() - REVEAL_MARGIN, full.min_y() - REVEAL_MARGIN),
                    to_content(full.max_x() + REVEAL_MARGIN, full.max_y() + REVEAL_MARGIN),
                ],
            );
        }
        cur = anc.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
}

/// Scrolls the innermost `Flickable` around `item` that can still move in
/// `dir` by most of a page. Returns whether anything moved.
fn page_scroll(item: &ItemRc, dir: SpatialDirection) -> bool {
    let mut cur = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    while let Some(anc) = cur {
        if let Some(f) = anc.downcast::<Flickable>() {
            let f = f.as_pin_ref();
            let geo = anc.geometry();
            let horizontal = matches!(dir, SpatialDirection::Left | SpatialDirection::Right);
            let (pos, content, page) = if horizontal {
                (f.viewport_x().get(), f.viewport_width().get(), geo.width())
            } else {
                (
                    f.viewport_y().get(),
                    f.viewport_height().get(),
                    geo.height(),
                )
            };
            let min = -(content - page).max(0.);
            let forward = matches!(dir, SpatialDirection::Right | SpatialDirection::Down);
            let target = if forward {
                (pos - page * 0.6).max(min)
            } else {
                (pos + page * 0.6).min(0.)
            };
            if (target - pos).abs() > 0.5 {
                let len = LogicalLength::new(target);
                if horizontal {
                    Flickable::FIELD_OFFSETS.viewport_x().apply_pin(f).set(len);
                } else {
                    Flickable::FIELD_OFFSETS.viewport_y().apply_pin(f).set(len);
                }
                return true;
            }
        }
        cur = anc.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    false
}

impl WindowInner {
    /// Installs (or with `None`, removes) the hook that receives key presses
    /// nothing in the focus chain consumed. A single-line `TextInput` also
    /// offers Up/Down to the hook before handling them itself.
    pub fn set_unhandled_key_hook(&self, hook: Option<Rc<UnhandledKeyHook>>) {
        *self.unhandled_key_hook.borrow_mut() = hook;
    }

    pub(crate) fn call_unhandled_key_hook(&self, event: &InternalKeyEvent) -> bool {
        // Clone out of the cell: the hook may well call back into the window.
        let hook = self.unhandled_key_hook.borrow().clone();
        hook.is_some_and(|hook| hook(&event.key_event))
    }

    /// Whether the keyboard focus is in an editable text field.
    pub fn text_input_has_focus(&self) -> bool {
        self.focus_item.borrow().upgrade().is_some_and(|i| {
            i.downcast::<TextInput>()
                .is_some_and(|t| t.as_pin_ref().enabled())
        })
    }

    fn spatial_entries(&self) -> Option<(Vec<Entry>, LogicalRect)> {
        // Popups handle their own keyboard navigation.
        if !self.active_popups.borrow().is_empty() {
            return None;
        }
        let root = ItemRc::new(self.try_component()?, 0);
        self.ensure_tree_instantiated();
        let window = root.geometry();
        let mut entries = Vec::new();
        collect(&root, &window, &mut entries);
        Some((entries, window))
    }

    /// Reachable navigation targets with their pointer aim, front-most first.
    fn spatial_targets(entries: &[Entry]) -> Vec<(usize, Reach)> {
        let mut out: Vec<(usize, Reach)> = Vec::new();
        for i in 0..entries.len() {
            if !entries[i].target {
                continue;
            }
            // Stacked duplicates (a hover catcher over a click area, say):
            // keep the front-most one.
            if out
                .iter()
                .any(|(j, _)| same_rect(&entries[*j].full, &entries[i].full))
            {
                continue;
            }
            if let Some(r) = reach(entries, i) {
                out.push((i, r));
            }
        }
        out
    }

    fn send_pointer_move(&self, point: LogicalPoint) {
        self.process_mouse_input(MouseEvent::Moved {
            position: point,
            touch_finger_id: 0,
        });
    }

    /// Fresh pointer aim for `item`, if it is still reachable.
    fn spatial_aim(&self, item: &ItemRc) -> Option<LogicalPoint> {
        let (entries, _) = self.spatial_entries()?;
        let i = entries.iter().position(|e| e.item == *item)?;
        reach(&entries, i).map(|r| r.point)
    }

    /// Moves the navigation focus one step in `dir`. With no current focus the
    /// hovered target, or failing that the target nearest the upper-left of the
    /// content, receives it. When nothing lies in `dir` and `allow_scroll` is
    /// set, the enclosing scroll area pages instead.
    pub fn spatial_navigate(&self, dir: SpatialDirection, allow_scroll: bool) -> SpatialNavResult {
        let Some((entries, window)) = self.spatial_entries() else {
            return SpatialNavResult::Unchanged;
        };
        let targets = Self::spatial_targets(&entries);
        let current = self.spatial_focus.borrow().upgrade();
        let from = current
            .as_ref()
            .and_then(|c| targets.iter().find(|(i, _)| entries[*i].item == *c))
            .map(|(i, _)| entries[*i].full);

        let chosen = match (from, current.as_ref()) {
            (Some(from), Some(current)) => targets
                .iter()
                // Something scrolled out of view is only reachable from inside
                // the scroll area that hides it: the end of one carousel row
                // must not jump to a hidden card of the next row.
                .filter(|(i, _)| {
                    entries[*i]
                        .clipper
                        .as_ref()
                        .is_none_or(|f| is_descendant_or_self(current, f))
                })
                .filter_map(|(i, _)| score(dir, &from, &entries[*i].full).map(|s| (*i, s)))
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i),
            _ => {
                let hovered = targets.iter().find(|(i, _)| {
                    entries[*i]
                        .item
                        .downcast::<TouchArea>()
                        .is_some_and(|t| t.as_pin_ref().has_hover())
                });
                hovered.map(|(i, _)| *i).or_else(|| {
                    let anchor = LogicalPoint::new(
                        window.min_x() + window.width() * 0.3,
                        window.min_y() + window.height() * 0.3,
                    );
                    targets
                        .iter()
                        .filter(|(i, _)| !entries[*i].offscreen)
                        .min_by(|a, b| {
                            let da = (entries[a.0].full.center() - anchor).square_length();
                            let db = (entries[b.0].full.center() - anchor).square_length();
                            da.total_cmp(&db)
                        })
                        .map(|(i, _)| *i)
                })
            }
        };

        if let Some(i) = chosen {
            let item = entries[i].item.clone();
            drop(entries);
            self.spatial_focus_item(&item);
            return SpatialNavResult::Moved;
        }
        if allow_scroll && let Some(c) = current {
            if page_scroll(&c, dir) {
                return SpatialNavResult::Scrolled;
            }
        }
        SpatialNavResult::Unchanged
    }

    /// Gives `item` the navigation focus: reveals it and hovers it.
    pub fn spatial_focus_item(&self, item: &ItemRc) {
        *self.spatial_focus.borrow_mut() = item.downgrade();
        reveal(item);
        if let Some(p) = self.spatial_aim(item) {
            self.send_pointer_move(p);
        }
    }

    /// Re-aims the synthetic pointer at the focused target, e.g. after a
    /// scroll animation moved it.
    pub fn spatial_refresh_hover(&self) {
        let item = self.spatial_focus.borrow().upgrade();
        if let Some(item) = item
            && let Some(p) = self.spatial_aim(&item)
        {
            self.send_pointer_move(p);
        }
    }

    /// Clicks the focused target. Returns `false` when there is none.
    pub fn spatial_activate(&self) -> bool {
        let item = self.spatial_focus.borrow().upgrade();
        let Some(item) = item else { return false };
        reveal(&item);
        let Some(position) = self.spatial_aim(&item) else {
            return false;
        };
        self.send_pointer_move(position);
        let button = PointerEventButton::Left;
        self.process_mouse_input(MouseEvent::Pressed {
            position,
            button,
            click_count: 0,
            touch_finger_id: 0,
        });
        self.process_mouse_input(MouseEvent::Released {
            position,
            button,
            click_count: 0,
            touch_finger_id: 0,
        });
        true
    }

    /// Drops the navigation focus. With `exit_pointer`, the synthetic pointer
    /// also leaves the window so no hover state lingers.
    pub fn spatial_clear(&self, exit_pointer: bool) {
        let had = self
            .spatial_focus
            .replace(Default::default())
            .upgrade()
            .is_some();
        if had && exit_pointer {
            self.process_mouse_input(MouseEvent::Exit);
        }
    }

    /// The focused target's visible rectangle in window coordinates
    /// (`x, y, width, height`), or `None` when nothing visible has the focus.
    pub fn spatial_focus_rect(&self) -> Option<(f32, f32, f32, f32)> {
        let item = self.spatial_focus.borrow().upgrade()?;
        if !self.active_popups.borrow().is_empty() || !item.is_visible() {
            return None;
        }
        let (clip, full) = item.absolute_clip_rect_and_geometry();
        let r = full.intersection(&clip).filter(|r| !rect_is_empty(r))?;
        Some((r.min_x(), r.min_y(), r.width(), r.height()))
    }
}
