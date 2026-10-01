use super::*;
use crate::store::TabOrientation;
use crate::tab_navigation::{TAB_STRIP_HEIGHT, selected_project_tabs};
use diri_ui::title_fade;

pub(super) const TAB_WIDTH: f32 = 164.0;
pub(super) const TAB_HEIGHT: f32 = 30.0;
const TAB_GAP: f32 = 4.0;
/// What a tab leaves its title: the width less the border, padding, mark
/// slot, close button and the two gaps between them.
const TAB_TITLE_WIDTH: f32 = TAB_WIDTH - 2.0 - 20.0 - 18.0 - 7.0 - 18.0 - 7.0;

/// A session tab picked up in the horizontal strip. It carries no ghost:
/// the tab itself is lifted by the strip, locked to the strip's axis.
#[derive(Clone)]
pub(super) struct DraggedTab(pub(super) SessionId);

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// The static face of a session tab: leading mark and title. The mark is
/// the agent's brand while it has nothing to say, and the same activity
/// mark as the sidebar rows (working, needs input, done, sleeping) when it
/// does; both sit in one fixed slot so the title never shifts between them.
pub(super) fn session_tab_face(
    mark: AnyElement,
    title: impl IntoElement,
    active: bool,
    colors: SemanticColors,
) -> gpui::Div {
    div()
        .px(px(10.0))
        .rounded(px(SIDEBAR_ROW_RADIUS))
        .flex()
        .items_center()
        .gap(px(7.0))
        .child(
            div()
                .size(px(18.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(mark),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_size(px(Typo::ROW.size))
                .text_color(if active {
                    colors.primary
                } else {
                    colors.secondary
                })
                .child(title),
        )
}

/// The selected tab's glass pill, drawn as one layer behind the strip's tabs
/// so a selection change glides it from the old tab to the new one instead
/// of the fill blinking off one tab and on at another.
///
/// Positions are in the strip's content space (tab `i` rests at
/// `i * (TAB_WIDTH + TAB_GAP)`), so the pill scrolls with the tabs: when the
/// new selection scrolls the strip, the pill rides the scroll with its old
/// tab and then travels to the new one, never into a slot that is not there.
#[derive(Default)]
pub(super) struct TabPill {
    selected: Option<SessionId>,
    /// Where the pill rests, or is heading.
    to: f32,
    slide: Option<PillSlide>,
}

#[derive(Clone, Copy)]
struct PillSlide {
    from: f32,
    start: Instant,
}

/// Where to draw the pill this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PillFrame {
    pub(super) x: f32,
    pub(super) animating: bool,
}

impl TabPill {
    /// Forgets the resting place, so whatever the strip shows next is drawn
    /// in place rather than slid to (first render, the strip reappearing).
    pub(super) fn forget(&mut self) {
        *self = Self::default();
    }

    /// Settles the pill on `selected` at content x `to`. It glides only when
    /// `may_slide` and the selection actually changed from a tab the strip
    /// still shows; anything else (a reorder moving the selected tab, a
    /// close, a project switch) puts it straight in place, because the tabs
    /// themselves move without it in those cases. A change mid-glide starts
    /// the next glide from where the pill is drawn now, so a held ⌘] chases
    /// rather than queues. `visible` is the viewport's content range: a
    /// start outside it is pulled to its edge, so a long jump spends the
    /// curve's fast opening on screen instead of off it.
    pub(super) fn update(
        &mut self,
        selected: Option<&SessionId>,
        to: f32,
        may_slide: bool,
        visible: Option<(f32, f32)>,
        now: Instant,
    ) -> PillFrame {
        let changed = self.selected.as_ref() != selected;
        if changed && may_slide && self.selected.is_some() && selected.is_some() {
            let current = self.sample(now).x;
            let from = match visible {
                Some((left, right)) if right > left => current.clamp(left - TAB_WIDTH, right),
                _ => current,
            };
            self.slide = ((from - to).abs() >= 0.5).then_some(PillSlide { from, start: now });
        } else if changed || !may_slide || (to - self.to).abs() >= 0.5 {
            self.slide = None;
        }
        self.selected = selected.cloned();
        self.to = to;
        let frame = self.sample(now);
        if !frame.animating {
            self.slide = None;
        }
        frame
    }

    fn sample(&self, now: Instant) -> PillFrame {
        let Some(slide) = self.slide else {
            return PillFrame {
                x: self.to,
                animating: false,
            };
        };
        let progress = now.saturating_duration_since(slide.start).as_secs_f32()
            / Motion::ROW_SELECT_TIME.as_secs_f32();
        if progress >= 1.0 {
            return PillFrame {
                x: self.to,
                animating: false,
            };
        }
        PillFrame {
            x: slide.from + (self.to - slide.from) * Motion::SETTLE.settle(progress),
            animating: true,
        }
    }
}

/// Focus bookkeeping for context menus opened from the horizontal strip.
///
/// The sidebar's own menus live inside its render tree, which never paints
/// while horizontal tabs hide the panel. The strip therefore hosts the same
/// menus in a window-level overlay, and this handle gives them keyboard
/// dismissal without leaving focus on a hidden sidebar afterwards.
pub(super) struct StripMenu {
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
}

impl StripMenu {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            focus: cx.focus_handle(),
            previous_focus: None,
        }
    }
}

impl Sidebar {
    /// Open a session or project context menu from the horizontal strip.
    pub(super) fn open_strip_menu(
        &mut self,
        popover: Popover,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_rename();
        self.dismiss_hover_card(cx);
        if self.project_picker_is_open() {
            self.dismiss_project_picker(window, cx);
        }
        if self.strip_menu.previous_focus.is_none() {
            self.strip_menu.previous_focus = window.focused(cx);
        }
        self.ui.popover = Some(popover);
        self.strip_menu.focus.focus(window, cx);
        cx.notify();
    }

    fn close_strip_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui.popover = None;
        if let Some(previous) = self.strip_menu.previous_focus.take() {
            previous.focus(window, cx);
        }
        cx.notify();
    }

    /// The strip's context menus, painted above the workbench when the
    /// sidebar itself is not on screen. Called on every root render so a
    /// menu dismissed by a row action or the outside-click scrim hands focus
    /// back as it leaves the tree, the same way its Escape path does.
    pub(crate) fn render_strip_menu_overlay(
        &mut self,
        sidebar_painted: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.ui.popover.is_none() {
            if let Some(previous) = self.strip_menu.previous_focus.take() {
                previous.focus(window, cx);
            }
            return None;
        }
        if sidebar_painted || self.project_picker.new_agent {
            return None;
        }
        let colors = self.colors();
        let spec = self.popover(colors, cx)?;
        let popover = self.host_popover(spec, window, cx);
        Some(
            div()
                .id("strip-menu-overlay")
                .absolute()
                .inset_0()
                .track_focus(&self.strip_menu.focus)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        this.close_strip_menu(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(popover)
                .into_any_element(),
        )
    }

    pub(super) fn agent_tab_icon(kind: &ProtoAgentKind, colors: SemanticColors) -> AnyElement {
        Self::agent_tab_icon_sized(kind, colors, 16.0)
    }

    fn agent_tab_icon_sized(
        kind: &ProtoAgentKind,
        colors: SemanticColors,
        size: f32,
    ) -> AnyElement {
        let icon = match ui_agent_kind(kind).brand_mark() {
            Some(mark) => diri_ui::BrandMark::solid(mark, size, colors.secondary)
                .inset(0.08)
                .into_any_element(),
            // Exactly `size`: `sf_symbol` rounds up to its 12 pt tier, which
            // would crowd a progress ring.
            None => diri_ui::Icon::new(diri_ui::IconName::Terminal, size, colors.secondary)
                .into_any_element(),
        };
        div()
            .size(px(size))
            .flex_none()
            .child(icon)
            .into_any_element()
    }

    pub(super) fn navigation_sessions(
        &self,
        store: &mut crate::store::WindowWrite<'_>,
    ) -> Vec<Arc<SessionRecord>> {
        if store.preferences().tab_orientation == TabOrientation::Horizontal {
            selected_project_tabs(store).sessions
        } else if !self.filter_query.text().trim().is_empty() {
            // Numeric shortcuts follow the displayed filtered rows, including
            // disclosed archives and the current project/recency grouping.
            self.focus_rows_for_store(store)
                .iter()
                .filter_map(|row| store.sessions().get(&row.id).cloned())
                .collect()
        } else {
            super::super::filter::filter_projection(
                store.sidebar_projection(),
                self.filter_query.text(),
            )
            .ordered_sessions
            .clone()
        }
    }

    pub fn tab_orientation(&self) -> TabOrientation {
        self.store
            .read()
            .expect("store")
            .preferences()
            .tab_orientation
    }

    pub fn horizontal_tabs_visible(&self) -> bool {
        let store = self.store.read().expect("store");
        store.preferences().tab_orientation == TabOrientation::Horizontal
            && store.preferences().horizontal_tabs_visible
    }

    pub fn toggle_horizontal_tabs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> std::io::Result<()> {
        self.store
            .write()
            .expect("store")
            .update_preferences(|prefs| {
                prefs.horizontal_tabs_visible = !prefs.horizontal_tabs_visible;
            })?;
        if !self.horizontal_tabs_visible() && self.project_picker_active() {
            if self.project_picker.new_agent {
                self.ui.popover = None;
                self.project_picker.new_agent = false;
            }
            self.dismiss_project_picker(window, cx);
        }
        cx.notify();
        Ok(())
    }

    /// Commit the presentation preference before changing the visible chrome.
    /// Selection and all terminal entities stay owned by their existing views.
    pub fn set_tab_orientation(
        &mut self,
        orientation: TabOrientation,
        cx: &mut Context<Self>,
    ) -> std::io::Result<()> {
        let visible = orientation == TabOrientation::Vertical;
        self.store
            .write()
            .expect("store")
            .update_preferences(|prefs| {
                prefs.tab_orientation = orientation;
                prefs.sidebar_visible = visible;
            })?;
        self.ui.visible = visible;
        self.last_tab_selection = None;
        self.tab_pill.forget();
        self.peek_open = false;
        self.peek_close = None;
        self.dismiss_hover_card(cx);
        cx.emit(SidebarEvent::TabOrientationChanged);
        cx.notify();
        Ok(())
    }

    pub(super) fn render_project_tab_rows(
        &mut self,
        available_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors();
        let (tabs, selected, custom_ordering, marks) = {
            let mut store = self.store.write().expect("store");
            let selected = store.selected_session_id().cloned();
            let custom = store.preferences().sidebar_ordering == SidebarOrdering::Custom;
            let tabs = selected_project_tabs(&mut store);
            // The same reduction the sidebar rows use, so a tab and its row
            // never disagree about what a session is doing.
            let marks: Vec<StatusState> = tabs
                .sessions
                .iter()
                .map(|session| {
                    sidebar_activity_state(
                        status_state(session, store.migrating().contains(&session.id)),
                        store.notifications().session_unread(&session.id),
                    )
                })
                .collect();
            (tabs, selected, custom, marks)
        };
        let reduce_motion = cx.reduce_motion();
        if self.tab_shift.settled.get() {
            self.tab_shift.deltas.clear();
        }
        let mut rows = div()
            .id(SharedString::from(format!(
                "horizontal-tab-list-{}",
                tabs.project.as_ref().map_or("empty", |id| id.0.as_str())
            )))
            .flex()
            .items_center()
            .gap(px(TAB_GAP))
            .flex_1()
            .min_w(px(0.0))
            .h(px(30.0))
            .overflow_x_scroll()
            .track_scroll(&self.tab_scroll)
            // The scroller's hitbox covers the gaps between tabs; those move
            // the window like the rest of the strip.
            .titlebar_drag_area();
        if self.last_tab_selection != selected || self.last_tab_available_width != available_width {
            if let Some(index) = tabs
                .sessions
                .iter()
                .position(|session| Some(&session.id) == selected.as_ref())
            {
                // Fixed-width tabs have a known content position before the
                // first layout. GPUI clamps this offset to the final viewport.
                self.tab_scroll
                    .set_offset(point(px(-(index as f32) * (TAB_WIDTH + TAB_GAP)), px(0.0)));
            }
            self.last_tab_selection = selected.clone();
            self.last_tab_available_width = available_width;
        }
        let selected_index = tabs
            .sessions
            .iter()
            .position(|session| Some(&session.id) == selected.as_ref());
        // A tab that is lifted or stepping aside in a drag reorder carries
        // its own fill and motion; the shared pill stands down until it rests.
        let selected_moving = selected.as_ref().is_some_and(|id| {
            self.lift_offset(&LiftKey::SessionTab(id.clone())).is_some()
                || (!reduce_motion && self.tab_shift.deltas.contains_key(id))
        });
        let pill = match selected_index {
            Some(index) if !selected_moving => {
                let previous_shown = self
                    .tab_pill
                    .selected
                    .as_ref()
                    .is_some_and(|previous| tabs.sessions.iter().any(|s| &s.id == previous));
                // The viewport's content range once the scroll above lands,
                // estimated from the last layout (GPUI clamps the offset).
                let viewport = f32::from(self.tab_scroll.bounds().size.width);
                let visible = (viewport > 0.0).then(|| {
                    let content = tab_count_width(tabs.sessions.len());
                    let max = (content - viewport).max(0.0);
                    let left = (-f32::from(self.tab_scroll.offset().x)).clamp(0.0, max);
                    (left, left + viewport)
                });
                Some(self.tab_pill.update(
                    selected.as_ref(),
                    index as f32 * (TAB_WIDTH + TAB_GAP),
                    !reduce_motion && previous_shown && self.lift.is_none(),
                    visible,
                    Instant::now(),
                ))
            }
            _ => {
                self.tab_pill.forget();
                None
            }
        };
        if let Some(frame) = pill {
            let weak = self.weak_self.clone();
            rows = rows.child(
                div()
                    .debug_selector(|| "horizontal-tab-pill".into())
                    .absolute()
                    .top(px(0.0))
                    .left(px(frame.x))
                    .w(px(TAB_WIDTH))
                    .h(px(30.0))
                    .rounded(px(SIDEBAR_ROW_RADIUS))
                    .border_1()
                    .border_color(colors.primary.alpha(0.0))
                    .glass_pill(colors, true)
                    // Frames only while the pill is travelling.
                    .when(frame.animating, |pill| {
                        pill.child(
                            gpui::canvas(
                                move |_, window, _| Self::refresh_on_next_frame(&weak, window),
                                |_, _, _, _| (),
                            )
                            .absolute()
                            .inset_0(),
                        )
                    }),
            );
        }
        let tab_count = tabs.sessions.len();
        let mut mounted = HashSet::with_capacity(tab_count);
        for (index, (session, state)) in tabs.sessions.into_iter().zip(marks).enumerate() {
            mounted.insert(session.id.clone());
            let active = selected.as_ref() == Some(&session.id);
            let props = self.strip_tab_props(
                &session,
                index,
                tab_count,
                active,
                state,
                colors,
                custom_ordering,
                reduce_motion,
                pill.is_some(),
            );
            rows = rows.child(self.mount_strip_tab(props, cx));
        }
        self.tabs_stale = false;
        // Tabs mounted this pass keep their views.
        self.strip_tab_views.retain(|id, _| mounted.contains(id));
        rows.into_any_element()
    }

    /// Builds one strip tab from `props` alone, so a tab whose props are
    /// unchanged renders identically and its cached view can be reused (see
    /// `strip_tabs.rs`). The only other read is the title settle, which
    /// forces a render while it runs.
    pub(super) fn strip_tab(
        &mut self,
        props: &strip_tabs::StripTabProps,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        #[cfg(test)]
        render_probe::tab_built();
        let strip_tabs::StripTabProps {
            ref id,
            ref title,
            ref kind,
            active,
            rank,
            state,
            colors,
            custom_ordering,
            held_hint,
            pill_drawn,
            ..
        } = *props;
        let id = id.clone();
        let title = title.clone();
        let entity = cx.entity();
        // Progress rings the logo while the session has nothing more urgent
        // to say.
        let progress = props.progress.filter(|_| {
            !matches!(
                state,
                StatusState::NeedsInput { .. } | StatusState::Hibernated
            )
        });
        let progress_mark = progress.map(|face| {
            let logo = Self::agent_tab_icon_sized(kind, colors, 10.0);
            crate::progress_mark::progress_mark(face, colors, Some(logo))
        });
        let mark = match state {
            _ if progress_mark.is_some() => div()
                .id(SharedString::from(format!(
                    "horizontal-tab-progress-{}",
                    id.0
                )))
                .role(Role::Image)
                .aria_label("Progress")
                .children(progress_mark)
                .into_any_element(),
            StatusState::IdleSeen | StatusState::None => div()
                .id(SharedString::from(format!("horizontal-tab-logo-{}", id.0)))
                .debug_selector({
                    let id = id.0.clone();
                    move || format!("horizontal-tab-logo-{id}")
                })
                .child(Self::agent_tab_icon(kind, colors))
                .into_any_element(),
            state => div()
                .id(SharedString::from(format!(
                    "horizontal-tab-status-{}",
                    id.0
                )))
                .debug_selector({
                    let id = id.0.clone();
                    move || format!("horizontal-tab-status-{id}")
                })
                .role(Role::Image)
                .aria_label(state.label())
                .child(activity_mark(state, props.activity_frame, colors))
                .into_any_element(),
        };
        let mark = crate::held_hints::in_leading_slot(
            mark,
            16.0,
            format!("held-hint:tab:{}", id.0),
            rank.and_then(crate::held_hints::session_label),
            held_hint,
            colors,
        );
        let debug_id = id.0.clone();
        let close_id = id.clone();
        let probe_key = SharedString::from(format!("tab:{}", id.0));
        // At rest the title fades out where it overflows; while it settles
        // the crossfading label owns the box.
        let face = match self.settling_title(&id, TAB_TITLE_WIDTH) {
            Some(settling) => settling.into_any_element(),
            None => title_fade(title.clone()).into_any_element(),
        };
        let location = props.location.clone();
        let tab = session_tab_face(mark, face, active, colors)
            .id(SharedString::from(format!("horizontal-tab-{}", id.0)))
            .when_some(location, |tab, place| {
                tab.warm_tooltip(move |_, cx| {
                    cx.new(|_| crate::palette_chrome::PaletteTooltip(place.clone(), colors))
                        .into()
                })
            })
            .debug_selector(move || format!("horizontal-tab-{}", debug_id))
            .role(Role::Tab)
            .aria_label(title.clone())
            .aria_selected(active)
            .relative()
            .child(self.fade_probe(probe_key.clone()))
            .flex_none()
            .w(px(TAB_WIDTH))
            .h(px(TAB_HEIGHT))
            .cursor_pointer()
            .border_1()
            .border_color(colors.primary.alpha(0.0))
            // The shared pill layer draws the selection; a tab fills itself
            // only while that layer stands down.
            .glass_pill(colors, active && !pill_drawn)
            .hover(move |row| {
                if active {
                    row
                } else {
                    row.bg(colors.primary.alpha(0.06))
                }
            })
            .when(custom_ordering, |row| {
                let drag_id = id.clone();
                let drag_entity = entity.clone();
                row.on_drag(DraggedTab(id.clone()), move |dragged, grab, window, cx| {
                    // The tab itself lifts from where the pointer grabbed it
                    // and travels only along the strip.
                    let origin = window.mouse_position() - grab;
                    drag_entity.update(cx, |this, cx| {
                        let order = this.store.write().expect("store").sidebar_session_order();
                        this.ui.session_order_at_drag_start = Some(order);
                        this.lift = Some(Lift::new(
                            LiftKey::SessionTab(drag_id.clone()),
                            origin,
                            grab,
                            LiftAxis::Horizontal,
                        ));
                        cx.notify();
                    });
                    cx.new(|_| dragged.clone())
                })
                // Tabs trade places once the pointer crosses a tab's
                // midline in its direction of travel, and the displaced
                // tabs slide into their new slots.
                .drag_over::<DraggedTab>({
                    let id = id.clone();
                    let entity = entity.clone();
                    move |row, dragged, _, cx| {
                        entity.update(cx, |this, cx| {
                            if this.pointer_crossed_tab(&dragged.0, &id)
                                && this.reorder_tab(&dragged.0, &id, cx.reduce_motion())
                            {
                                // The held tab traded places with this one.
                                haptics::perform(Haptic::Snap, haptics::key("tab-slot", &id));
                                cx.notify();
                            }
                        });
                        row
                    }
                })
            })
            .child(crate::held_hints::below(
                div()
                    .id(SharedString::from(format!(
                        "close-horizontal-tab-{}",
                        close_id.0
                    )))
                    .role(Role::Button)
                    .aria_label("Close session")
                    .size(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .hover(move |button| button.bg(colors.primary.alpha(0.10)))
                    .child(sf_symbol("xmark", 8.0, colors.tertiary))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_sessions(vec![close_id.clone()], cx);
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .into_any_element(),
                "close-tab",
                // ⌘W closes the selected session, so only its ✕ says so.
                active
                    .then(|| crate::held_hints::label(crate::commands::CommandId::CloseSession))
                    .flatten(),
                held_hint,
                colors,
            ))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let id = id.clone();
                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.ui.focus_cursor = Some(id.clone());
                        this.open_strip_menu(
                            Popover::SessionActions {
                                id: id.clone(),
                                position: event.position,
                            },
                            window,
                            cx,
                        );
                    }
                }),
            )
            .on_click(cx.listener({
                let id = id.clone();
                move |this, _, _, cx| {
                    this.commit_rename();
                    this.store.write().expect("store").select(id.clone());
                    cx.emit(SidebarEvent::SessionActivated);
                    cx.notify();
                }
            }));
        match (props.lift, props.shift) {
            (Some(offset), _) => lift_in_place(tab, LiftAxis::Horizontal, offset, colors),
            (None, None) => tab.into_any_element(),
            (None, Some((delta, generation))) => {
                let applied = Rc::clone(&self.tab_shift.applied);
                let settled = Rc::clone(&self.tab_shift.settled);
                let id = id.clone();
                tab.with_animation(
                    SharedString::from(format!("tab-shift:{}:{}", id.0, generation)),
                    Animation::new(SECTION_SHIFT_TIME)
                        .with_easing(|delta| Motion::SETTLE.settle(delta)),
                    move |tab, progress| {
                        let offset = delta * (1.0 - progress);
                        applied.borrow_mut().insert(id.clone(), offset);
                        if progress >= 1.0 {
                            settled.set(true);
                        }
                        tab.left(px(offset))
                    },
                )
                .into_any_element()
            }
        }
    }

    /// Session tabs as the strip currently shows them, in order.
    pub(super) fn visible_tab_order(&self) -> Vec<SessionId> {
        let mut store = self.store.write().expect("store");
        selected_project_tabs(&mut store)
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect()
    }

    /// How many session tabs the strip shows, for render-cost benches.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn strip_tab_count_for_test(&self) -> usize {
        self.visible_tab_order().len()
    }

    /// Whether the pointer has passed `target`'s midline in the direction
    /// `moved` is travelling along the strip. Nothing crosses while a
    /// previous reorder's slide is still in flight.
    fn pointer_crossed_tab(&mut self, moved: &SessionId, target: &SessionId) -> bool {
        if moved == target || self.tab_shift.in_flight() {
            return false;
        }
        let Some(pointer) = self.lift.as_ref().map(|lift| lift.pointer) else {
            return false;
        };
        let Some(tab) = self
            .fade_bounds
            .borrow()
            .get(&SharedString::from(format!("tab:{}", target.0)))
            .copied()
        else {
            return false;
        };
        let order = self.visible_tab_order();
        let position = |id: &SessionId| order.iter().position(|candidate| candidate == id);
        let (Some(from), Some(to)) = (position(moved), position(target)) else {
            return false;
        };
        let midline = tab.origin.x + tab.size.width / 2.0;
        if from < to {
            pointer.x >= midline
        } else {
            pointer.x <= midline
        }
    }

    /// Live tab reorder; returns whether the strip's order changed. Tabs only
    /// trade places within their sibling run (the strip flattens a session
    /// tree, and a child cannot be ordered past its parent's peers) and never
    /// across the pin boundary, since pinned rows always sort first.
    fn reorder_tab(&mut self, moved: &SessionId, target: &SessionId, reduce_motion: bool) -> bool {
        let before = self.visible_tab_order();
        let staged = {
            let mut store = self.store.write().expect("store");
            if store.preferences().sidebar_ordering != SidebarOrdering::Custom {
                return false;
            }
            let projection = store.sidebar_projection();
            if !sibling_run(&projection, moved).contains(target) {
                return false;
            }
            let pinned = |id: &SessionId| {
                projection
                    .projects
                    .iter()
                    .flat_map(|group| group.sessions.iter())
                    .find(|row| row.id() == id)
                    .map(|row| row.pinned)
            };
            if pinned(moved) != pinned(target) {
                return false;
            }
            let mut order = store.sidebar_session_order();
            move_past(&mut order, moved, target);
            store.stage_session_order(order)
        };
        self.ui.order_dirty |= staged;
        let after = self.visible_tab_order();
        let changed = staged && before != after;
        if changed {
            self.shift_tabs(&before, &after, reduce_motion);
        }
        changed
    }

    /// Starts the slide from the tabs' current positions to their new slots.
    /// Every tab is the same width, so a slot is an index.
    pub(super) fn shift_tabs(
        &mut self,
        before: &[SessionId],
        after: &[SessionId],
        reduce_motion: bool,
    ) {
        let applied = self.tab_shift.applied.borrow().clone();
        let mut deltas = tab_shift_deltas(before, after, &applied, TAB_WIDTH + TAB_GAP);
        // The lifted tab does not slide; its slot moves under it.
        if let Some(lift) = self.lift.as_mut()
            && let LiftKey::SessionTab(session) = &lift.key
            && let Some(delta) = deltas.remove(session)
        {
            lift.slot.x -= px(delta);
        }
        self.tab_shift.start(deltas, reduce_motion);
    }

    /// `trailing` is the workbench's title-bar action cluster (links,
    /// inspector, notifications) hosted here beside the new-tab control, so
    /// the terminal pane below can drop its own title bar.
    pub fn render_horizontal_tabs(
        &mut self,
        available_width: f32,
        trailing: Option<AnyElement>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        #[cfg(test)]
        let started = std::time::Instant::now();
        // While horizontal tabs hide the panel this strip is the sidebar's
        // only painted surface, so the per-frame work `Sidebar::render`
        // does has to happen here: settle workspace navigation (a pending
        // project-agent open, a created or removed workspace) and keep the
        // working marks' 8 Hz tick alive only while one is on screen.
        self.reconcile_workspace_navigation(cx);
        self.working_row_rendered = false;
        // The strip stands in for the panel; its marks advance through a
        // sidebar notify, not through session rows.
        self.rows_mounted = false;
        self.observe_titles(cx);
        if cx.reduce_motion() {
            self.activity_frame = 0;
        }
        let strip = self.horizontal_strip(available_width, trailing, cx);
        self.schedule_activity_tick(cx);
        self.schedule_title_tick();
        if self.title_tick {
            // Notifies the sidebar, not the caller: `RootView` read it to
            // paint the strip, so the strip repaints on the display link.
            self.request_motion_frame(window, cx);
        }
        #[cfg(test)]
        render_probe::strip_finished(started.elapsed());
        strip
    }

    fn horizontal_strip(
        &mut self,
        available_width: f32,
        trailing: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors();
        self.end_lift_if_released(cx);
        if self.workspace_nav.active.is_some() {
            // The tabs are not on screen, so the pill must not glide from a
            // selection made while they were away.
            self.tab_pill.forget();
            self.workspace_nav.available_width = available_width;
            return self.workspace_strip(colors, cx);
        }
        let rows = self.render_project_tab_rows(available_width, cx);
        let held_hint = self.strip_held_hint;
        div()
            .id("horizontal-tabs")
            .debug_selector(|| "horizontal-tabs".into())
            // The strip is rendered outside the sidebar's own root, so it
            // tracks the pointer for its lifted tab itself.
            .on_drag_move::<DraggedTab>(cx.listener(
                |this, event: &gpui::DragMoveEvent<DraggedTab>, _, cx| {
                    this.track_lift_pointer(event.event.position, cx);
                },
            ))
            .role(Role::TabList)
            .aria_label("Project sessions")
            .titlebar_drag_area()
            .flex_none()
            .h(px(TAB_STRIP_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .relative()
            .py(px(6.0))
            .gap(px(0.0))
            .pl(px(if crate::window_chrome::traffic_lights_visible() && !self.ui.visible {
                92.0
            } else {
                10.0
            }))
            .pr(px(10.0 + self.strip_caption_inset))
            .child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .h(px(1.0))
                    .bg(colors.primary.alpha(0.07)),
            )
            .bg(colors.sidebar_surface())
            .text_color(colors.primary)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .flex()
                    .items_center()
                    // Content only: the strip's own fill keeps its color.
                    .opacity(self.title_opacity)
                    .child(self.project_control(colors, cx))
            .child(rows)
            .child(crate::held_hints::below(
                div()
                    .id("horizontal-peek-tabs")
                    .debug_selector(|| "horizontal-peek-tabs".into())
                    .role(Role::Button)
                    .aria_label("Peek tabs")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .hover(move |button| button.bg(colors.primary.alpha(0.06)))
                    .child(sf_symbol("square.grid.2x2", 12.0, colors.secondary))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(crate::commands::ToggleTabPeek), cx)
                    })
                    .into_any_element(),
                "peek-tabs",
                crate::held_hints::label(crate::commands::CommandId::ToggleTabPeek),
                held_hint,
                colors,
            ))
            .child(crate::held_hints::below(
                div()
                    .id("horizontal-new-tab")
                    .debug_selector(|| "horizontal-new-tab".into())
                    .role(Role::Button)
                    .aria_label("New session")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .hover(move |button| button.bg(colors.primary.alpha(0.06)))
                    .child(sf_symbol("plus", 12.0, colors.secondary))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(crate::commands::NewDefaultSession), cx)
                    })
                    .into_any_element(),
                "new-tab",
                crate::held_hints::label(crate::commands::CommandId::NewDefaultSession),
                held_hint,
                colors,
            ))
            .when_some(trailing, |strip, trailing| {
                strip.child(
                    div()
                        .flex_none()
                        .ml(px(6.0))
                        .flex()
                        .items_center()
                        .child(trailing),
                )
            }))
            .into_any_element()
    }
}

/// Offsets that carry each tab from where it is drawn to its new slot.
/// `pitch` is one slot: tab width plus gap.
/// Laid-out width of `count` fixed-width tabs and the gaps between them.
fn tab_count_width(count: usize) -> f32 {
    (count as f32 * (TAB_WIDTH + TAB_GAP) - TAB_GAP).max(0.0)
}

fn tab_shift_deltas(
    before: &[SessionId],
    after: &[SessionId],
    applied: &HashMap<SessionId, f32>,
    pitch: f32,
) -> HashMap<SessionId, f32> {
    let mut deltas = HashMap::new();
    for (new_index, id) in after.iter().enumerate() {
        let Some(old_index) = before.iter().position(|candidate| candidate == id) else {
            continue;
        };
        let delta =
            (old_index as f32 - new_index as f32) * pitch + applied.get(id).copied().unwrap_or(0.0);
        if delta.abs() >= 0.5 {
            deltas.insert(id.clone(), delta);
        }
    }
    deltas
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{
        LayoutNode, PaneId, TabId, WorkspaceId, WorkspaceRecord, WorkspaceSnapshot, WorkspaceTab,
    };
    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    /// Only the strip paints, exactly as the app does while horizontal tabs
    /// hide the sidebar panel.
    struct StripOnly {
        sidebar: Entity<Sidebar>,
    }
    impl Render for StripOnly {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.render_horizontal_tabs(900.0, None, window, cx)
                }))
        }
    }
    fn strip_harness(
        cx: &mut TestAppContext,
        reduce_motion: bool,
    ) -> (Entity<Sidebar>, &mut VisualTestContext) {
        cx.update(|cx| cx.set_reduce_motion(reduce_motion));
        let (view, cx) = cx.add_window_view(move |_, cx| {
            let sidebar = cx.new(|cx| {
                let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                sidebar
                    .set_tab_orientation(TabOrientation::Horizontal, cx)
                    .unwrap();
                sidebar
            });
            cx.observe(&sidebar, |_, _, cx| cx.notify()).detach();
            StripOnly { sidebar }
        });
        (view.read_with(cx, |view, _| view.sidebar.clone()), cx)
    }

    #[gpui::test]
    fn a_tab_trading_places_ticks_once_per_slot(cx: &mut TestAppContext) {
        let (sidebar, cx) = strip_harness(cx, true);
        let order = sidebar.update(cx, |sidebar, _| sidebar.visible_tab_order());
        let bounds = |cx: &mut VisualTestContext, id: &SessionId| {
            cx.debug_bounds(Box::leak(
                format!("horizontal-tab-{}", id.0).into_boxed_str(),
            ))
            .expect("tab")
        };
        // Only siblings on the same side of the pin boundary trade places.
        let (moved, target) = order
            .iter()
            .enumerate()
            .flat_map(|(index, moved)| {
                order[index + 1..]
                    .iter()
                    .map(move |target| (moved.clone(), target.clone()))
            })
            .find(|(moved, target)| {
                sidebar.update(cx, |sidebar, _| {
                    let before = sidebar.visible_tab_order();
                    let traded = sidebar.reorder_tab(moved, target, true);
                    if traded {
                        assert!(sidebar.reorder_tab(moved, target, true));
                        assert_eq!(sidebar.visible_tab_order(), before);
                    }
                    traded
                })
            })
            .expect("the preview strip has two tabs that can trade places");
        cx.run_until_parked();
        let from = bounds(cx, &moved);
        let over = bounds(cx, &target);
        let _ = haptics::testing::take();

        cx.simulate_mouse_down(from.center(), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            from.center() + point(px(6.0), px(0.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        // Over the neighbour but short of its midline: hover, not a slot.
        cx.simulate_mouse_move(
            point(over.left() + px(4.0), over.center().y),
            MouseButton::Left,
            Modifiers::default(),
        );
        assert_eq!(haptics::testing::take(), []);

        let past_midline = point(over.center().x + px(4.0), over.center().y);
        cx.simulate_mouse_move(past_midline, MouseButton::Left, Modifiers::default());
        assert_eq!(
            haptics::testing::take(),
            [(Haptic::Snap, haptics::key("tab-slot", &target))]
        );
        cx.simulate_mouse_move(
            past_midline + point(px(1.0), px(0.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(past_midline, MouseButton::Left, Modifiers::default());
        assert_eq!(
            haptics::testing::take(),
            [],
            "holding the new slot and letting go add nothing"
        );
    }

    /// How many times each mounted strip tab has rendered.
    fn tab_renders(
        sidebar: &Entity<Sidebar>,
        cx: &mut VisualTestContext,
    ) -> HashMap<SessionId, usize> {
        sidebar.update(cx, |sidebar, cx| {
            sidebar
                .strip_tab_views
                .iter()
                .map(|(id, view)| (id.clone(), view.read(cx).renders))
                .collect()
        })
    }

    /// Tabs whose render count moved between two samples.
    fn rerendered(
        before: &HashMap<SessionId, usize>,
        after: &HashMap<SessionId, usize>,
    ) -> Vec<SessionId> {
        let mut ids: Vec<SessionId> = after
            .iter()
            .filter(|(id, renders)| before.get(*id) != Some(*renders))
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort_by(|left, right| left.0.cmp(&right.0));
        ids
    }

    #[gpui::test]
    fn strip_tabs_rerender_only_when_they_change(cx: &mut TestAppContext) {
        let (sidebar, cx) = strip_harness(cx, false);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let order = sidebar.update(cx, |sidebar, _| sidebar.visible_tab_order());
        assert!(order.len() >= 3, "the preview strip shows several tabs");
        let working: Vec<SessionId> = sidebar.update(cx, |sidebar, _| {
            let store = sidebar.store.read().unwrap();
            let mut working: Vec<SessionId> = order
                .iter()
                .filter(|id| store.sessions()[*id].status == diri_proto::SessionStatus::Working)
                .cloned()
                .collect();
            working.sort_by(|left, right| left.0.cmp(&right.0));
            working
        });
        assert!(!working.is_empty(), "the preview strip has a working tab");
        assert_eq!(tab_renders(&sidebar, cx).len(), order.len());

        // A working mark's tick re-renders the working tabs alone.
        let before = tab_renders(&sidebar, cx);
        cx.executor().advance_clock(Duration::from_millis(125));
        cx.run_until_parked();
        assert_eq!(rerendered(&before, &tab_renders(&sidebar, cx)), working);

        // A store publication that changes nothing re-renders no tab.
        let before = tab_renders(&sidebar, cx);
        sidebar.update(cx, |sidebar, cx| sidebar.store_changed(cx));
        cx.run_until_parked();
        assert_eq!(rerendered(&before, &tab_renders(&sidebar, cx)), []);

        // A frame the strip's host draws for its own reasons (a terminal
        // repaint) reuses every tab.
        let host = cx.update(|window, _| window.root::<StripOnly>().flatten().unwrap());
        let before = tab_renders(&sidebar, cx);
        host.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(rerendered(&before, &tab_renders(&sidebar, cx)), []);

        // Hovering a tab re-renders that tab for its hover fill.
        let hovered = order
            .iter()
            .find(|id| {
                !working.contains(id)
                    && sidebar.update(cx, |sidebar, _| {
                        sidebar.store.read().unwrap().selected_session_id() != Some(*id)
                    })
            })
            .unwrap()
            .clone();
        let bounds = cx
            .debug_bounds(Box::leak(
                format!("horizontal-tab-{}", hovered.0).into_boxed_str(),
            ))
            .expect("tab");
        let before = tab_renders(&sidebar, cx);
        cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(rerendered(&before, &tab_renders(&sidebar, cx)), [hovered]);

        // Selecting another tab re-renders the old and the new selection
        // (and, if the strip scrolls to it, every tab whose bounds moved).
        let (old, new) = sidebar.update(cx, |sidebar, _| {
            let mut store = sidebar.store.write().unwrap();
            let old = store.selected_session_id().cloned().unwrap();
            let new = order.iter().find(|id| **id != old).unwrap().clone();
            store.select(new.clone());
            (old, new)
        });
        let before = tab_renders(&sidebar, cx);
        sidebar.update(cx, |sidebar, cx| sidebar.store_changed(cx));
        cx.run_until_parked();
        let changed = rerendered(&before, &tab_renders(&sidebar, cx));
        assert!(
            changed.contains(&old) && changed.contains(&new),
            "{changed:?}"
        );
        cx.run_until_parked();
        let before = tab_renders(&sidebar, cx);
        host.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(rerendered(&before, &tab_renders(&sidebar, cx)), []);

        // Any other sidebar notify re-renders every tab once, as a safety net.
        let before = tab_renders(&sidebar, cx);
        sidebar.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(
            rerendered(&before, &tab_renders(&sidebar, cx)).len(),
            order.len()
        );
    }

    /// Tabs stepping aside in a reorder slide are drawn from their animation
    /// every frame, so they render on every frame until they rest; the rest
    /// of the strip is reused.
    #[gpui::test]
    fn sliding_strip_tabs_render_every_frame_until_they_rest(cx: &mut TestAppContext) {
        let (sidebar, cx) = strip_harness(cx, false);
        cx.run_until_parked();
        let host = cx.update(|window, _| window.root::<StripOnly>().flatten().unwrap());
        let order = sidebar.update(cx, |sidebar, _| sidebar.visible_tab_order());
        let sliding: Vec<SessionId> = order
            .iter()
            .enumerate()
            .flat_map(|(index, moved)| {
                order[index + 1..]
                    .iter()
                    .map(move |target| (moved.clone(), target.clone()))
            })
            .find_map(|(moved, target)| {
                sidebar.update(cx, |sidebar, _| {
                    sidebar
                        .reorder_tab(&moved, &target, false)
                        .then(|| sidebar.tab_shift.deltas.keys().cloned().collect())
                })
            })
            .expect("the preview strip has two tabs that can trade places");
        assert!(!sliding.is_empty());
        let mut sliding = sliding;
        sliding.sort_by(|left, right| left.0.cmp(&right.0));
        host.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        for _ in 0..3 {
            let before = tab_renders(&sidebar, cx);
            host.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            assert_eq!(rerendered(&before, &tab_renders(&sidebar, cx)), sliding);
        }
        // At rest they are reused like any other tab.
        sidebar.update(cx, |sidebar, _| sidebar.tab_shift.settled.set(true));
        host.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        let before = tab_renders(&sidebar, cx);
        host.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(rerendered(&before, &tab_renders(&sidebar, cx)), []);
    }

    /// The held-⌘ hints reach cached tabs through their props: they appear
    /// and leave with the opacity the host hands the strip.
    #[gpui::test]
    fn held_hints_show_and_leave_on_cached_strip_tabs(cx: &mut TestAppContext) {
        let (sidebar, cx) = strip_harness(cx, true);
        cx.run_until_parked();
        let host = cx.update(|window, _| window.root::<StripOnly>().flatten().unwrap());
        let first = sidebar.update(cx, |sidebar, _| sidebar.visible_tab_order()[0].clone());
        let mark_hint: &'static str =
            Box::leak(format!("held-hint:tab:{}", first.0).into_boxed_str());
        assert!(cx.debug_bounds(mark_hint).is_none());
        assert!(cx.debug_bounds("held-hint:close-tab").is_none());
        for (opacity, shown) in [(1.0, true), (1.0, true), (0.0, false)] {
            sidebar.update(cx, |sidebar, _| sidebar.strip_held_hint = opacity);
            host.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            assert_eq!(cx.debug_bounds(mark_hint).is_some(), shown);
            assert_eq!(cx.debug_bounds("held-hint:close-tab").is_some(), shown);
        }
    }

    /// The selection pill glides as its own layer: while it travels, the
    /// cached tabs are reused frame after frame, and only the old and new
    /// selection re-render, once, when the selection changes (their fills
    /// hand over to the pill).
    #[gpui::test]
    fn the_pill_glides_over_reused_strip_tabs(cx: &mut TestAppContext) {
        let (sidebar, cx) = strip_harness(cx, false);
        cx.run_until_parked();
        let host = cx.update(|window, _| window.root::<StripOnly>().flatten().unwrap());
        let order = sidebar.update(cx, |sidebar, _| sidebar.visible_tab_order());
        let tab = |cx: &mut VisualTestContext, id: &SessionId| {
            cx.debug_bounds(Box::leak(
                format!("horizontal-tab-{}", id.0).into_boxed_str(),
            ))
            .expect("tab")
        };
        let (old, new) = sidebar.update(cx, |sidebar, _| {
            let mut store = sidebar.store.write().unwrap();
            let old = store.selected_session_id().cloned().unwrap();
            let at = order.iter().position(|id| *id == old).unwrap();
            // A neighbour, so the strip does not need to scroll to it.
            let new = order[if at > 0 { at - 1 } else { at + 1 }].clone();
            store.select(new.clone());
            (old, new)
        });
        let pill = |cx: &mut VisualTestContext| {
            cx.debug_bounds("horizontal-tab-pill")
                .expect("the pill is drawn")
                .origin
                .x
        };
        let before = tab_renders(&sidebar, cx);
        sidebar.update(cx, |sidebar, cx| sidebar.store_changed(cx));
        cx.run_until_parked();
        let changed = rerendered(&before, &tab_renders(&sidebar, cx));
        assert!(
            changed.contains(&old) && changed.contains(&new),
            "{changed:?}"
        );
        let start = pill(cx);
        let old_x = tab(cx, &old).origin.x;
        let new_x = tab(cx, &new).origin.x;
        assert!(
            (start - new_x).abs() > px(1.0),
            "the pill starts its glide away from the new tab"
        );
        let mut last = start;
        let mut moved = false;
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(20));
            let before = tab_renders(&sidebar, cx);
            host.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            assert_eq!(
                rerendered(&before, &tab_renders(&sidebar, cx)),
                [],
                "tabs are reused while the pill travels"
            );
            let x = pill(cx);
            moved |= x != last;
            assert!(
                (x - old_x).abs() <= (new_x - old_x).abs() + px(0.5),
                "the pill travels between the two tabs"
            );
            last = x;
        }
        assert!(moved, "the pill keeps moving over cached tabs");
        std::thread::sleep(Motion::ROW_SELECT_TIME);
        host.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(pill(cx), new_x, "the pill lands on the new tab");
        let before = tab_renders(&sidebar, cx);
        host.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(rerendered(&before, &tab_renders(&sidebar, cx)), []);
    }

    #[gpui::test]
    fn horizontal_tabs_show_the_activity_mark_instead_of_the_logo(cx: &mut TestAppContext) {
        let (_sidebar, cx) = strip_harness(cx, true);
        // Working and needs-input sessions carry their state in the leading slot.
        assert!(
            cx.debug_bounds("horizontal-tab-status-preview-codex")
                .is_some()
        );
        assert!(
            cx.debug_bounds("horizontal-tab-logo-preview-codex")
                .is_none()
        );
        assert!(
            cx.debug_bounds("horizontal-tab-status-preview-claude")
                .is_some()
        );
        // A turn that finished after the session was last seen is unread.
        assert!(
            cx.debug_bounds("horizontal-tab-status-preview-cursor")
                .is_some()
        );
        // A session with nothing to report keeps the agent's brand mark.
        assert!(
            cx.debug_bounds("horizontal-tab-logo-preview-shell")
                .is_some()
        );
        assert!(
            cx.debug_bounds("horizontal-tab-status-preview-shell")
                .is_none()
        );
        // Both marks occupy the same slot, so the title never shifts.
        let status = cx
            .debug_bounds("horizontal-tab-status-preview-codex")
            .unwrap();
        let logo = cx
            .debug_bounds("horizontal-tab-logo-preview-shell")
            .unwrap();
        let status_tab = cx.debug_bounds("horizontal-tab-preview-codex").unwrap();
        let logo_tab = cx.debug_bounds("horizontal-tab-preview-shell").unwrap();
        assert_eq!(
            status.center().x - status_tab.left(),
            logo.center().x - logo_tab.left()
        );
    }

    #[gpui::test]
    fn horizontal_strip_keeps_the_working_mark_ticking_while_the_panel_is_hidden(
        cx: &mut TestAppContext,
    ) {
        let (sidebar, cx) = strip_harness(cx, false);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(!sidebar.is_visible(), "the panel is hidden");
            assert!(sidebar.working_row_rendered);
            assert!(sidebar.activity_tick.is_some());
        });
        for _ in 0..3 {
            let frame = sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame);
            cx.executor().advance_clock(Duration::from_millis(125));
            cx.run_until_parked();
            assert_eq!(
                sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame),
                (frame + 1) % 8,
            );
        }
        // Once nothing works, the strip lets the wake lapse.
        sidebar.update(cx, |sidebar, cx| {
            let mut store = sidebar.store.write().unwrap();
            let sessions: Vec<_> = store.sessions().values().cloned().collect();
            for session in sessions {
                let mut session = (*session).clone();
                session.status = diri_proto::SessionStatus::Idle;
                store.upsert_session(session);
            }
            drop(store);
            cx.notify();
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(125));
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(!sidebar.working_row_rendered);
            assert!(sidebar.activity_tick.is_none());
        });
    }

    /// A floating menu is its own window and reads the sidebar while it
    /// draws, so GPUI comes to regard that window as the sidebar's. Closing
    /// it leaves the sidebar with no window until the main one draws again,
    /// and a tick landing in that gap must not strand the working marks.
    #[gpui::test]
    fn working_mark_keeps_ticking_after_a_floating_window_closes(cx: &mut TestAppContext) {
        struct Panel {
            sidebar: Entity<Sidebar>,
        }
        impl Render for Panel {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let _ = self.sidebar.read(cx).is_visible();
                div()
            }
        }
        let (sidebar, cx) = strip_harness(cx, false);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let panel = cx.update(|_, cx| {
            let sidebar = sidebar.clone();
            cx.open_window(Default::default(), |_, cx| cx.new(|_| Panel { sidebar }))
                .unwrap()
        });
        cx.run_until_parked();
        panel
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        for _ in 0..3 {
            let frame = sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame);
            cx.executor().advance_clock(Duration::from_millis(125));
            cx.run_until_parked();
            assert_eq!(
                sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame),
                (frame + 1) % 8,
            );
        }
    }

    #[gpui::test]
    fn horizontal_strip_settles_a_pending_project_agent_open(cx: &mut TestAppContext) {
        let (sidebar, cx) = strip_harness(cx, true);
        let claude = SessionId::new("preview-claude");
        let workspace = WorkspaceId::new("project-view");
        let snapshot = |revision: u64, project: &diri_proto::ProjectId| WorkspaceSnapshot {
            revision,
            workspaces: vec![WorkspaceRecord {
                id: workspace.clone(),
                project_id: Some(project.clone()),
                name: "Project".into(),
                selected_tab: Some(TabId::new("agent-tab")),
                tabs: vec![WorkspaceTab {
                    id: TabId::new("agent-tab"),
                    title: None,
                    layout: LayoutNode::Pane {
                        id: PaneId::new("pane"),
                        session_id: claude.clone(),
                    },
                    focused_pane: PaneId::new("pane"),
                    zoomed_pane: None,
                }],
            }],
            ..Default::default()
        };
        // A tab click: the session is selected and its project agent opens.
        let project = sidebar.update(cx, |sidebar, cx| {
            sidebar.preview = false;
            let mut store = sidebar.store.write().unwrap();
            let project = store.sessions()[&claude].project_id.clone();
            store.seed_workspace_snapshot_for_test(snapshot(4, &project));
            store.select(claude.clone());
            drop(store);
            assert!(sidebar.open_selected_project_agent(cx));
            assert!(sidebar.project_agent_open_pending());
            project
        });
        // The Engine answers while only the strip is painting.
        sidebar.update(cx, |sidebar, cx| {
            sidebar
                .store
                .write()
                .unwrap()
                .finish_workspace_edit_for_test(snapshot(5, &project));
            cx.notify();
        });
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(
                !sidebar.project_agent_open_pending(),
                "the strip must settle the open the way the panel's render does"
            );
            assert_eq!(sidebar.workspace_nav.active.as_ref(), Some(&workspace));
        });
    }

    fn tab(id: &str) -> SessionId {
        SessionId(id.into())
    }

    const SLOT: f32 = TAB_WIDTH + TAB_GAP;

    #[test]
    fn the_pill_appears_in_place_on_first_render() {
        let now = Instant::now();
        let mut pill = TabPill::default();
        let frame = pill.update(Some(&tab("a")), 2.0 * SLOT, true, None, now);
        assert_eq!(
            frame,
            PillFrame {
                x: 2.0 * SLOT,
                animating: false
            }
        );
    }

    #[test]
    fn a_selection_change_glides_on_the_settle_curve_and_lands_exactly() {
        let now = Instant::now();
        let mut pill = TabPill::default();
        pill.update(Some(&tab("a")), 0.0, true, None, now);
        let start = pill.update(Some(&tab("b")), SLOT, true, None, now);
        assert_eq!(
            start,
            PillFrame {
                x: 0.0,
                animating: true
            }
        );
        let half = pill.update(
            Some(&tab("b")),
            SLOT,
            true,
            None,
            now + Motion::ROW_SELECT_TIME / 2,
        );
        assert!(half.animating);
        assert!(half.x > SLOT * 0.5 && half.x < SLOT, "{}", half.x);
        let end = pill.update(
            Some(&tab("b")),
            SLOT,
            true,
            None,
            now + Motion::ROW_SELECT_TIME,
        );
        assert_eq!(
            end,
            PillFrame {
                x: SLOT,
                animating: false
            }
        );
    }

    #[test]
    fn a_change_mid_glide_chases_from_where_the_pill_is_drawn() {
        let now = Instant::now();
        let mut pill = TabPill::default();
        pill.update(Some(&tab("a")), 0.0, true, None, now);
        pill.update(Some(&tab("b")), SLOT, true, None, now);
        let later = now + Duration::from_millis(40);
        let drawn = pill.sample(later).x;
        let retarget = pill.update(Some(&tab("c")), 2.0 * SLOT, true, None, later);
        assert_eq!(
            retarget,
            PillFrame {
                x: drawn,
                animating: true
            }
        );
    }

    #[test]
    fn reorders_closes_and_reduced_motion_put_the_pill_in_place() {
        let now = Instant::now();
        let mut pill = TabPill::default();
        pill.update(Some(&tab("a")), 0.0, true, None, now);
        // The selected tab moved without the selection changing.
        let reordered = pill.update(Some(&tab("a")), SLOT, true, None, now);
        assert_eq!(
            reordered,
            PillFrame {
                x: SLOT,
                animating: false
            }
        );
        // The caller withholds the glide (closed tab, reduce motion, drag).
        let snapped = pill.update(Some(&tab("b")), 3.0 * SLOT, false, None, now);
        assert_eq!(
            snapped,
            PillFrame {
                x: 3.0 * SLOT,
                animating: false
            }
        );
    }

    #[test]
    fn a_start_off_screen_is_pulled_to_the_viewport_edge() {
        let now = Instant::now();
        let mut pill = TabPill::default();
        pill.update(Some(&tab("a")), 0.0, true, None, now);
        let visible = (20.0 * SLOT, 24.0 * SLOT);
        let frame = pill.update(Some(&tab("z")), 22.0 * SLOT, true, Some(visible), now);
        assert_eq!(
            frame,
            PillFrame {
                x: 20.0 * SLOT - TAB_WIDTH,
                animating: true
            }
        );
    }

    #[test]
    fn a_forgotten_pill_does_not_glide() {
        let now = Instant::now();
        let mut pill = TabPill::default();
        pill.update(Some(&tab("a")), 0.0, true, None, now);
        pill.forget();
        let frame = pill.update(Some(&tab("b")), SLOT, true, None, now);
        assert_eq!(
            frame,
            PillFrame {
                x: SLOT,
                animating: false
            }
        );
    }
}
