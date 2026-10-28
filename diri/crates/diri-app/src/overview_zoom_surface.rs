//! Painting and input for the Safari-style zoom between the workbench page
//! and its card in the calm Session Overview. The state machine lives in
//! `crate::overview_zoom`; this file only maps it onto the store and onto
//! elements.
//!
//! The flight is a picture, not a layout: when it starts, the page (or, on
//! the way back, the resting card) is captured as a bitmap from the last
//! drawn frame, and only that bitmap flies, scaled by one factor about the
//! terminal grid, so no text re-shapes and no row appears or disappears on
//! the way. The card's own rendering cross-fades in over the last part of
//! the flight, so at progress 1 the screen is the resting card itself.
use super::overview_calm::miniature_box;
use super::*;
use crate::overview_zoom::{
    GridAnchor, Grip, Landing, OverviewZoom, ZoomRect, card_fade, card_snap, chrome_fade,
    chrome_pose, grid_alpha, live_fade, mip_blend, page_frame, snap_frame, snapshot_rect,
};
use crate::terminal_pane::TerminalViewport;
use diri_term::metrics::CellMetrics;
use std::cell::RefCell;
use std::rc::Rc;

/// The pane's grid inset below its title bar: `render_grid_and_overlays`
/// pads the grid 12 pt sideways, 2 pt on top and 10 pt below.
const PAGE_GRID_LEFT: f32 = 12.0;
const PAGE_GRID_TOP: f32 = 2.0;
const PAGE_GRID_BOTTOM: f32 = 10.0;
/// Mip levels captured with a snapshot: enough for the page to shrink 8x
/// with every painted level at most 2x minified.
const SNAPSHOT_LEVELS: usize = 4;

/// What the page's picture was taken of, so a later flight home can tell
/// whether it still shows the page as it is.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PageKey {
    page: ZoomRect,
    header: f32,
    scale: f32,
    /// The mirrored screen's buffer and its generation.
    screen: Option<(usize, u64)>,
}

/// A bitmap in flight: the page as it looked when the pinch began, or the
/// resting card when a card is pinched open without a fresh page picture.
pub(super) struct ZoomSnapshot {
    pub(super) session: SessionId,
    /// Mip levels, largest first, ready for `paint_image`.
    levels: Vec<Arc<gpui::RenderImage>>,
    /// The captured region's size in points.
    size: (f32, f32),
    /// The terminal grid inside the captured region.
    anchor: GridAnchor,
    /// `Page`: a picture of the page; `Overview`: of the resting card.
    pub(super) source: Landing,
    key: PageKey,
    /// How long the capture took, for the frame budget (read by the macOS-only pixel test).
    #[cfg_attr(not(all(test, target_os = "macos")), allow(dead_code))]
    pub(super) cost: std::time::Duration,
}

/// Everything the zoom needs besides the pure state machine.
#[derive(Default)]
pub(super) struct ZoomPresentation {
    pub(super) zoom: OverviewZoom,
    /// The session playing the page: the one you pinched away from, or the
    /// card you pinched (or clicked) open.
    pub(super) session: Option<SessionId>,
    pub(super) grip: Option<Grip>,
    /// The primary pane in window coordinates, supplied by RootView.
    pub(super) page: ZoomRect,
    /// The pane's title bar height (0 under the horizontal tab strip).
    pub(super) page_header: f32,
    /// Card bounds painted last frame, keyed by session. Written during
    /// prepaint, read by the next render.
    pub(super) cards: Rc<RefCell<HashMap<SessionId, ZoomRect>>>,
    /// This transition cross-fades instead of flying: Reduce Motion, a layout
    /// other than the mini windows, or a destination with no card.
    pub(super) crossfade: bool,
    /// The bitmap the current flight paints.
    pub(super) snapshot: Option<Rc<ZoomSnapshot>>,
    /// The last picture of the page, kept while the overview is open so a
    /// card pinched back open grows from sharp pixels instead of an
    /// upscaled card. Only valid while the page and its screen are unchanged.
    pub(super) page_cache: Option<Rc<ZoomSnapshot>>,
    /// Images no longer painted, to leave the sprite atlas on the next render.
    retired: Vec<Arc<gpui::RenderImage>>,
}

impl ZoomPresentation {
    fn release(&mut self, snapshot: Option<Rc<ZoomSnapshot>>) {
        if let Some(snapshot) = snapshot
            && let Ok(snapshot) = Rc::try_unwrap(snapshot)
        {
            self.retired.extend(snapshot.levels);
        }
    }

    pub(super) fn set_snapshot(&mut self, snapshot: Option<Rc<ZoomSnapshot>>) {
        let old = std::mem::replace(&mut self.snapshot, snapshot);
        self.release(old);
    }

    pub(super) fn set_page_cache(&mut self, snapshot: Option<Rc<ZoomSnapshot>>) {
        let old = std::mem::replace(&mut self.page_cache, snapshot);
        self.release(old);
    }

    /// Drop measured cards; the next painted grid measures them again.
    pub(super) fn forget_cards(&self) {
        self.cards.borrow_mut().clear();
    }

    /// The grid scrolled by `dy` since the cards were measured.
    pub(super) fn shift_cards(&self, dy: f32) {
        if dy != 0.0 {
            for card in self.cards.borrow_mut().values_mut() {
                card.y += dy;
            }
        }
    }
}

impl SessionSurfaces {
    /// The user's terminal font, as the panes paint it.
    pub(super) fn terminal_font(&self) -> gpui::Font {
        let store = self.store.read().expect("session store lock poisoned");
        let prefs = store.preferences();
        crate::fonts::terminal_font(&prefs.terminal_font_family, prefs.terminal_ligatures)
    }

    pub(crate) fn set_page_region(&mut self, viewport: TerminalViewport, header: f32) {
        self.zoom.page = ZoomRect {
            x: viewport.x,
            y: viewport.y,
            width: viewport.width,
            height: viewport.height,
        };
        self.zoom.page_header = header;
    }

    /// Whether the transition layer, rather than the resting overview, paints.
    pub(super) fn zoom_painting(&self) -> bool {
        let zoom = &self.zoom.zoom;
        zoom.is_flying() || (zoom.is_tracking() && zoom.progress() > 0.0)
    }

    /// Stop any transition where it is, without animating. Used when another
    /// surface takes over (switcher, peek, window deactivation).
    pub(crate) fn reset_overview_zoom(&mut self, cx: &mut Context<Self>) {
        let visible = self
            .store
            .read()
            .expect("session store lock poisoned")
            .overview_state()
            .is_visible();
        let was_active = self.zoom.zoom.is_active();
        self.zoom.zoom.reset(if visible {
            Landing::Overview
        } else {
            Landing::Page
        });
        self.zoom.grip = None;
        self.zoom.set_snapshot(None);
        if !visible {
            self.zoom.session = None;
            self.zoom.set_page_cache(None);
        }
        if was_active {
            cx.notify();
        }
    }

    /// Only the mini-window cards share the pane's shape, so only they can be
    /// flown into; everything else (and Reduce Motion) cross-fades.
    fn zoom_can_fly(&self, cx: &Context<Self>) -> bool {
        !cx.reduce_motion() && self.overview_variant == OverviewVariant::Windows
    }

    /// The card's on-screen rect: as painted last frame, else as the layout
    /// will paint it at the grid's current scroll.
    fn zoom_slot(&self, id: &SessionId, window: &Window) -> Option<ZoomRect> {
        if let Some(card) = self.zoom.cards.borrow().get(id) {
            return Some(*card);
        }
        self.calm_card_estimate(id, window)
    }

    fn sync_slot_scale(&mut self, slot: Option<ZoomRect>) {
        if let Some(slot) = slot
            && self.zoom.page.width > 1.0
        {
            self.zoom
                .zoom
                .set_slot_scale(slot.width / self.zoom.page.width);
        }
    }

    fn zoom_frame(&self, slot: ZoomRect) -> ZoomRect {
        let zoom = &self.zoom.zoom;
        page_frame(
            self.zoom.page,
            slot,
            self.zoom.grip,
            zoom.finger_weight(),
            zoom.progress(),
            zoom.page_scale(),
        )
    }

    /// Follow a store change that opened or closed the overview without a
    /// pinch (⇧⌘O, Esc, a click, Return, the close button) with the
    /// non-interactive version of the same flight.
    pub(super) fn follow_overview_visibility(
        &mut self,
        visible: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let now = cx.background_executor().now();
        let reduced = cx.reduce_motion();
        let target = if visible {
            Landing::Overview
        } else {
            Landing::Page
        };
        if self.zoom.zoom.is_tracking() || self.zoom.zoom.heading() == Some(target) {
            // A pinch (or its release) already owns this change.
            return;
        }
        if self.zoom.zoom.is_flying() {
            // ⇧⌘O mid-flight turns the flight around from where it is.
            self.zoom.zoom.animate_to(target, now, reduced);
            return;
        }
        let selected = self
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            .cloned();
        self.zoom.grip = None;
        self.zoom.session = selected.clone();
        match &selected {
            // Every open starts at the top, scrolled only as far as it takes
            // to show the card the page is about to land in.
            Some(id) => self.reveal_calm_card(id, window, visible),
            None if visible => self
                .overview_grid_scroll
                .set_offset(point(px(0.0), px(0.0))),
            None => {}
        }
        let slot = selected.as_ref().and_then(|id| self.zoom_slot(id, window));
        self.zoom.crossfade = !self.zoom_can_fly(cx) || slot.is_none();
        if let (Some(id), Some(slot), false) = (&selected, slot, self.zoom.crossfade) {
            let taken = if visible {
                self.take_page_snapshot(id, window)
            } else {
                self.take_return_snapshot(id, slot, window)
            };
            self.zoom.crossfade = !taken;
        }
        if visible {
            self.zoom.zoom.reset(Landing::Page);
            if let Some(id) = selected {
                self.request_screen(id, cx);
            }
        } else {
            // Leaving lands on whatever is now selected: the card that was
            // activated, or the page you came from after Esc.
            self.zoom.zoom.reset(Landing::Overview);
        }
        self.zoom.zoom.animate_to(target, now, reduced);
        self.sync_slot_scale(slot);
    }

    /// A trackpad pinch anywhere in the window. Returns whether the zoom took
    /// it; RootView plays the threshold haptic from `take_zoom_feedback`.
    pub(crate) fn overview_pinch(
        &mut self,
        event: &gpui::PinchEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let now = cx.background_executor().now();
        let reduced = cx.reduce_motion();
        let fingers = (f32::from(event.position.x), f32::from(event.position.y));
        match event.phase {
            gpui::TouchPhase::Started => self.begin_zoom(fingers, now, window, cx),
            gpui::TouchPhase::Moved => {
                if !self.zoom.zoom.is_tracking() {
                    return false;
                }
                self.zoom.zoom.pinch(event.delta, now);
                if let Some(grip) = self.zoom.grip.as_mut() {
                    grip.fingers = fingers;
                }
                if self.zoom.zoom.progress() > 0.0 {
                    self.open_overview_under_pinch();
                }
                cx.notify();
                true
            }
            gpui::TouchPhase::Ended | gpui::TouchPhase::Cancelled => {
                let landing = if event.phase == gpui::TouchPhase::Ended {
                    self.zoom.zoom.release(now, reduced)
                } else {
                    self.zoom.zoom.cancel(now, reduced)
                };
                let Some(landing) = landing else {
                    return false;
                };
                self.settle_store_on(landing, cx);
                cx.notify();
                true
            }
        }
    }

    fn begin_zoom(
        &mut self,
        fingers: (f32, f32),
        now: std::time::Instant,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (overview_visible, switcher_visible, selected, focused) = {
            let store = self.store.read().expect("session store lock poisoned");
            (
                store.overview_state().is_visible(),
                store.switcher_state().is_visible(),
                store.selected_session_id().cloned(),
                store.overview_state().focused().cloned(),
            )
        };
        if self.peek.paint_visible() || switcher_visible {
            return false;
        }
        self.zoom.zoom.advance(now);
        let grabbing_flight = self.zoom.zoom.is_active() && self.zoom.session.is_some();
        if !grabbing_flight {
            let session = if overview_visible {
                // The card under the fingers, as in Safari; otherwise the
                // keyboard-focused card.
                let under = self
                    .zoom
                    .cards
                    .borrow()
                    .iter()
                    .find(|(_, card)| card.contains(fingers.0, fingers.1))
                    .map(|(id, _)| id.clone());
                under.or(focused)
            } else {
                selected
            };
            let Some(session) = session else {
                return false;
            };
            if !overview_visible {
                // The grid opens at the top, scrolled just far enough that
                // the card the page shrinks into is on screen.
                self.reveal_calm_card(&session, window, true);
            }
            self.zoom.session = Some(session);
            self.zoom.crossfade = !self.zoom_can_fly(cx);
        }
        let Some(session) = self.zoom.session.clone() else {
            return false;
        };
        let slot = self.zoom_slot(&session, window);
        if slot.is_none() {
            self.zoom.crossfade = true;
        }
        self.sync_slot_scale(slot);
        let at_rest = if overview_visible {
            Landing::Overview
        } else {
            Landing::Page
        };
        let has_picture = self
            .zoom
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.session == session);
        if let (Some(slot), false) = (slot, self.zoom.crossfade)
            && !(grabbing_flight && has_picture)
        {
            // Take the picture before anything moves: the last frame is
            // still exactly what is on screen.
            let taken = match at_rest {
                Landing::Page => self.take_page_snapshot(&session, window),
                Landing::Overview => self.take_return_snapshot(&session, slot, window),
            };
            self.zoom.crossfade = !taken;
        }
        if !grabbing_flight {
            // At rest the presentation is wherever the store says it is.
            self.zoom.zoom.reset(at_rest);
        }
        // Take hold of the frame that is on screen right now.
        let on_screen = if !grabbing_flight && at_rest == Landing::Page {
            self.zoom.page
        } else {
            slot.map_or(self.zoom.page, |slot| self.zoom_frame(slot))
        };
        self.zoom.grip = Some(Grip::take(on_screen, fingers));
        self.zoom.zoom.begin(at_rest, now);
        // Snapshot cards for sessions that are not mounted start loading now,
        // while the page still covers the grid.
        self.request_screen(session, cx);
        cx.notify();
        true
    }

    /// The grid is behind the page from the first frame the page shrinks, so
    /// the store opens it then (keys and focus follow). A pinch that never
    /// shrinks the page never touches the store.
    fn open_overview_under_pinch(&mut self) {
        let mut store = self.store.write().expect("session store lock poisoned");
        if !store.overview_state().is_visible() {
            store.toggle_overview();
        }
    }

    /// Make the store agree with where a released pinch is heading, at
    /// release rather than at landing, so keys go to the right place at once.
    fn settle_store_on(&mut self, landing: Landing, cx: &mut Context<Self>) {
        let mut store = self.store.write().expect("session store lock poisoned");
        match landing {
            Landing::Overview => {
                if !store.overview_state().is_visible() {
                    store.toggle_overview();
                }
            }
            Landing::Page => {
                // Pinching a card open selects it at release, so the
                // workbench under the growing page is already that session.
                let selected = store.selected_session_id().cloned();
                let activate = self.zoom.session.clone().filter(|id| {
                    selected.as_ref() != Some(id) && store.sessions().contains_key(id)
                });
                if let Some(id) = activate.clone() {
                    store.select(id);
                }
                store.dismiss_overview();
                drop(store);
                if activate.is_some() {
                    cx.emit(TabPeekActivated);
                }
            }
        }
    }

    /// The window lost the gesture (deactivation, a modal surface): return
    /// to where the pinch started, exactly as a system cancel would.
    pub(crate) fn cancel_overview_pinch(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        if let Some(landing) = self.zoom.zoom.cancel(now, cx.reduce_motion()) {
            self.settle_store_on(landing, cx);
            cx.notify();
        }
    }

    pub(crate) fn take_zoom_feedback(&mut self) -> bool {
        self.zoom.zoom.take_feedback()
    }

    /// Advance a flight, and ask for the next frame only while one is in the
    /// air. A resting or finger-driven zoom schedules nothing.
    pub(super) fn advance_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        match self.zoom.zoom.advance(now) {
            Some(Landing::Page) => {
                self.zoom.session = None;
                self.zoom.grip = None;
                self.zoom.set_snapshot(None);
                // The live pane is back; its picture goes stale from here.
                self.zoom.set_page_cache(None);
            }
            Some(Landing::Overview) => self.zoom.set_snapshot(None),
            None => {}
        }
        if self.zoom.zoom.is_flying() {
            window.request_animation_frame();
        }
    }

    /// Nothing shows the zoom any more: let its pictures go, and take the
    /// ones already let go out of the sprite atlas.
    pub(super) fn settle_zoom_images(&mut self, overview_visible: bool, window: &mut Window) {
        if !self.zoom.zoom.is_active() {
            self.zoom.set_snapshot(None);
            if !overview_visible {
                self.zoom.set_page_cache(None);
            }
        }
        for image in self.zoom.retired.drain(..) {
            let _ = window.drop_image(image);
        }
    }

    fn page_key(&self, id: &SessionId, window: &Window) -> PageKey {
        PageKey {
            page: self.zoom.page,
            header: self.zoom.page_header,
            scale: window.scale_factor(),
            screen: self.card_terminal(id).map(|element| {
                let buffer = element.buffer();
                let generation = buffer.read().map_or(u64::MAX, |buffer| buffer.generation());
                (Arc::as_ptr(&buffer) as *const () as usize, generation)
            }),
        }
    }

    /// The page's grid corner and cell width, relative to the page.
    fn page_anchor(&self, window: &Window) -> GridAnchor {
        let size = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .terminal_font_size;
        let metrics = CellMetrics::measure(window.text_system(), &self.terminal_font(), px(size));
        GridAnchor {
            x: PAGE_GRID_LEFT,
            y: self.zoom.page_header + PAGE_GRID_TOP,
            cell: f32::from(metrics.cell_width),
        }
    }

    /// The card's grid corner and cell width, relative to a card at `slot`:
    /// inside the one-point border, below the strip, anchored to the bottom
    /// of the miniature the way `miniature_box` paints it.
    fn card_anchor(&self, id: &SessionId, slot: ZoomRect, window: &Window) -> GridAnchor {
        let geometry = self.calm_card_geometry(window);
        let pose = geometry.pose;
        let metrics = CellMetrics::measure(window.text_system(), &geometry.font, px(pose.font));
        let rows = self
            .card_terminal(id)
            .map_or_else(|| self.calm_fleet_rows(), |element| element.grid_rows())
            .max(1);
        let mini = slot.height - pose.strip - 2.0;
        GridAnchor {
            x: 1.0 + pose.grid_left,
            y: 1.0 + pose.strip + mini
                - pose.grid_bottom
                - f32::from(metrics.line_height) * f32::from(rows),
            cell: f32::from(metrics.cell_width),
        }
    }

    fn capture(
        &self,
        id: &SessionId,
        region: ZoomRect,
        anchor: GridAnchor,
        source: Landing,
        window: &Window,
    ) -> Option<Rc<ZoomSnapshot>> {
        let started = std::time::Instant::now();
        let bounds = gpui::Bounds::new(
            point(px(region.x), px(region.y)),
            gpui::size(px(region.width), px(region.height)),
        );
        match window.capture_region(bounds, SNAPSHOT_LEVELS) {
            Ok(levels) if !levels.is_empty() => {
                let cost = started.elapsed();
                Some(Rc::new(ZoomSnapshot {
                    session: id.clone(),
                    levels,
                    size: (region.width, region.height),
                    anchor,
                    source,
                    key: self.page_key(id, window),
                    cost,
                }))
            }
            // No picture (no renderer, a window with no frame yet): the
            // transition cross-fades instead of flying.
            Ok(_) | Err(_) => None,
        }
    }

    /// Picture the page as the last frame drew it. The picture stops just
    /// below the grid's last row: the rest of the pane is its background,
    /// which the flying card's own fill already paints.
    fn take_page_snapshot(&mut self, id: &SessionId, window: &Window) -> bool {
        let page = self.zoom.page;
        let anchor = self.page_anchor(window);
        let rows = self
            .card_terminal(id)
            .map_or(0, |element| element.grid_rows());
        let size = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .terminal_font_size;
        let line = f32::from(
            CellMetrics::measure(window.text_system(), &self.terminal_font(), px(size)).line_height,
        );
        let content = anchor.y + line * f32::from(rows) + PAGE_GRID_BOTTOM;
        let region = ZoomRect {
            height: if rows > 0 {
                content.min(page.height)
            } else {
                page.height
            },
            ..page
        };
        let snapshot = self.capture(id, region, anchor, Landing::Page, window);
        let taken = snapshot.is_some();
        self.zoom.set_page_cache(snapshot.clone());
        self.zoom.set_snapshot(snapshot);
        taken
    }

    /// Picture for a card growing back into the page: the page's own picture
    /// if it still shows the page as it is, else the resting card.
    pub(super) fn take_return_snapshot(
        &mut self,
        id: &SessionId,
        slot: ZoomRect,
        window: &Window,
    ) -> bool {
        let key = self.page_key(id, window);
        let cached = self
            .zoom
            .page_cache
            .clone()
            .filter(|cache| cache.session == *id && cache.key == key);
        if let Some(cached) = cached {
            self.zoom.set_snapshot(Some(cached));
            return true;
        }
        // Only the miniature below the card's strip: the flying frame draws
        // its own strip, and a picture of the title would grow to headline
        // size on the way to the page.
        let top = 1.0 + self.calm_card_geometry(window).pose.strip;
        let anchor = self.card_anchor(id, slot, window);
        let anchor = GridAnchor {
            y: anchor.y - top,
            ..anchor
        };
        let region = ZoomRect {
            y: slot.y + top,
            height: (slot.height - top).max(1.0),
            ..slot
        };
        let snapshot = self.capture(id, region, anchor, Landing::Overview, window);
        let taken = snapshot.is_some();
        self.zoom.set_snapshot(snapshot);
        taken
    }

    /// Records where a card painted, for the next frame's flight.
    pub(super) fn card_probe(&self, id: SessionId) -> impl IntoElement {
        let cards = Rc::clone(&self.zoom.cards);
        gpui::canvas(
            move |bounds, _, _| {
                cards.borrow_mut().insert(
                    id,
                    ZoomRect {
                        x: f32::from(bounds.origin.x),
                        y: f32::from(bounds.origin.y),
                        width: f32::from(bounds.size.width),
                        height: f32::from(bounds.size.height),
                    },
                );
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0()
    }

    /// This card is in flight above the grid, so its place paints nothing.
    pub(super) fn zoom_card_in_flight(&self, id: &SessionId) -> bool {
        self.zoom.session.as_ref() == Some(id) && self.zoom_painting() && !self.zoom.crossfade
    }

    pub(super) fn render_overview_zoom(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors();
        let progress = self.zoom.zoom.progress();
        // Read last frame's card before the grid re-measures this frame.
        let session = self.zoom.session.clone().and_then(|id| {
            self.store
                .read()
                .expect("session store lock poisoned")
                .sessions()
                .get(&id)
                .cloned()
        });
        let slot = session
            .as_ref()
            .and_then(|session| self.zoom_slot(&session.id, window));
        self.sync_slot_scale(slot);
        let flying = !self.zoom.crossfade && session.is_some() && slot.is_some();
        let grid = self.render_overview(window, cx);
        let mut layer = div().id("overview-zoom").absolute().inset_0().size_full();
        let shield = div()
            .id("overview-zoom-shield")
            .absolute()
            .inset_0()
            .occlude();
        let (Some(session), Some(slot), true) = (session, slot, flying) else {
            // Reduce Motion (or nowhere to land): the grid simply fades over
            // the workbench, following the fingers.
            return layer
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .size_full()
                        .opacity(progress.clamp(0.0, 1.0))
                        .child(grid),
                )
                .child(shield)
                .into_any_element();
        };
        let (theme, focused) = {
            let store = self.store.read().expect("session store lock poisoned");
            (
                crate::app_theme::terminal_theme_in(&store),
                store.overview_state().focused() == Some(&session.id),
            )
        };
        let page = self.zoom.page;
        let fingers_frame = self.zoom_frame(slot);
        // Near the card the picture settles onto the card's own size, so the
        // card's rendering fades in exactly under it.
        let frame = snap_frame(
            fingers_frame,
            (slot.width, slot.height),
            card_snap(progress),
        );
        let page_scale = self.zoom.zoom.page_scale() * frame.width / fingers_frame.width.max(1.0);
        let geometry = self.calm_card_geometry(window);
        let fade = card_fade(progress);
        let snapshot = self
            .zoom
            .snapshot
            .clone()
            .filter(|snapshot| snapshot.session == session.id);
        // A picture of the page as it still is needs no hand-over at the
        // page end; anything else (a card picture, or a page that has since
        // printed more) fades into the live pane under it on the way home.
        let current_page = snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.source == Landing::Page && snapshot.key == self.page_key(&session.id, window)
        });
        let home = if current_page {
            1.0
        } else {
            live_fade(progress)
        };
        layer = layer
            // The workbench under the shrinking page is revealed as the
            // overview's desk, never as a second copy of the terminal.
            .child(
                div()
                    .absolute()
                    .left(px(page.x))
                    .top(px(page.y))
                    .w(px(page.width))
                    .h(px(page.height))
                    .opacity(home)
                    .bg(self.calm_desk()),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .size_full()
                    .opacity(grid_alpha(progress))
                    .child(grid),
            );
        if fade < 1.0 {
            let (strip, radius) = chrome_pose(self.zoom.page_header, geometry.pose, progress);
            let backdrop = snapshot.as_ref().map(|snapshot| {
                let rect = snapshot_rect(
                    frame,
                    self.page_anchor(window),
                    self.card_anchor(&session.id, slot, window),
                    snapshot.anchor,
                    snapshot.size,
                    progress,
                    page_scale,
                    slot.width / page.width.max(1.0),
                );
                snapshot_layer(snapshot, rect, radius, window.scale_factor())
            });
            let mini = div()
                .flex_none()
                .w(px(frame.width))
                .h(px((frame.height - strip - 2.0).max(0.0)));
            let card = self.calm_window_card(
                &session,
                strip,
                radius,
                chrome_fade(progress),
                false,
                mini.into_any_element(),
                backdrop,
                theme,
                colors,
                window,
                cx,
            );
            layer = layer.child(
                div()
                    .absolute()
                    .left(px(frame.x))
                    .top(px(frame.y))
                    .w(px(frame.width))
                    .h(px(frame.height))
                    .opacity(home)
                    .child(card),
            );
        }
        if fade > 0.0 {
            // The card's own rendering, at its own size, over the picture.
            // At progress 1 this is the only thing painted, so the flight
            // lands as the resting card with no swap.
            let pose = geometry.pose;
            let element = self.card_terminal(&session.id).cloned();
            let mini = miniature_box(
                element.as_ref(),
                slot.width,
                (slot.height - pose.strip - 2.0).max(0.0),
                pose.grid_left,
                pose.grid_bottom,
                pose.font,
                &geometry.font,
                theme,
                session.hibernation.is_some(),
                window,
            );
            let card = self.calm_window_card(
                &session,
                pose.strip,
                pose.radius,
                1.0,
                focused,
                mini.into_any_element(),
                None,
                theme,
                colors,
                window,
                cx,
            );
            let mut real = div()
                .absolute()
                .left(px(frame.x + (frame.width - slot.width) / 2.0))
                .top(px(frame.y + (frame.height - slot.height) / 2.0))
                .w(px(slot.width))
                .h(px(slot.height));
            if fade < 1.0 {
                real = real.opacity(fade);
            }
            layer = layer.child(real.child(card));
        }
        // The grid is a picture until the flight lands; clicks mid-flight
        // would act on cards that are still moving.
        layer.child(shield).into_any_element()
    }
}

/// The snapshot painted at `rect` (window coordinates), blending the two mip
/// levels that bracket its on-screen size so shrinking text neither aliases
/// nor pops between levels.
fn snapshot_layer(
    snapshot: &ZoomSnapshot,
    rect: ZoomRect,
    radius: f32,
    scale_factor: f32,
) -> AnyElement {
    let source_px = snapshot
        .levels
        .first()
        .map_or(0.0, |level| level.size(0).width.0 as f32);
    let (fine, coarse, mix) =
        mip_blend(source_px, rect.width * scale_factor, snapshot.levels.len());
    let bounds = gpui::Bounds::new(
        point(px(rect.x), px(rect.y)),
        gpui::size(px(rect.width), px(rect.height)),
    );
    let paint = move |image: Arc<gpui::RenderImage>| {
        gpui::canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let _ = window.paint_image(
                    bounds,
                    gpui::Corners::all(px(radius)),
                    image.clone(),
                    0,
                    false,
                );
            },
        )
        .absolute()
        .inset_0()
    };
    let mut layer = div().absolute().inset_0();
    if mix > 0.0 {
        layer = layer.child(paint(snapshot.levels[coarse].clone()));
    }
    if mix < 1.0 {
        let finest = paint(snapshot.levels[fine].clone());
        layer = if mix > 0.0 {
            layer.child(div().absolute().inset_0().opacity(1.0 - mix).child(finest))
        } else {
            layer.child(finest)
        };
    }
    layer.into_any_element()
}
