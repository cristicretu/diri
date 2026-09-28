//! Session rows that grow into the list and collapse out of it. A row that
//! arrives opens a gap by its height and only then fades its content in; a
//! row that leaves fades its content first and then closes the gap. Rows
//! around it are moved by layout, never painted over one another.
//!
//! Enter and exit are membership changes: a session that was not in the
//! sidebar and now is, or the reverse. Folding a project or a parent, a
//! filter, regrouping or reordering hide and show rows the model already
//! knows, so they never animate here.
//!
//! The model is pure: memberships, the rows laid out per container and a
//! clock go in, per-row height factors and content opacities come out. It
//! schedules nothing; a surface asks [`RowMotion::is_animating`] whether
//! another frame is owed. A row that leaves is already gone from the store,
//! so the model keeps the last row it laid out for it and hands that back as
//! a ghost until the collapse ends.
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

use diri_ui::Motion;
use gpui::prelude::FluentBuilder;
use gpui::{AnyElement, InteractiveElement, IntoElement, ParentElement, Styled, div, px};

pub(super) const ENTER: Duration = Duration::from_millis(220);
pub(super) const EXIT: Duration = Duration::from_millis(200);
/// More rows than this in motion at once is a bulk change (a project closed,
/// an import, a resync): everything lands at once instead.
pub(super) const BULK: usize = 4;
/// Share of the arrival spent opening the gap before content starts to show,
/// so text never appears squashed into a sliver.
const ENTER_CONTENT_DELAY: f32 = 0.35;
/// Share of the departure spent fading content out, and where the gap starts
/// to close. They overlap a little so the exit reads as one gesture.
const EXIT_CONTENT: f32 = 0.4;
const EXIT_COLLAPSE_DELAY: f32 = 0.2;

/// What one row paints at one instant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Presence {
    /// Share of the row's natural height its slot takes.
    pub height: f32,
    /// Opacity of the row's content inside that slot.
    pub opacity: f32,
}

impl Presence {
    pub const FULL: Self = Self {
        height: 1.0,
        opacity: 1.0,
    };
    const GONE: Self = Self {
        height: 0.0,
        opacity: 0.0,
    };
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Direction {
    Enter,
    Exit,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    direction: Direction,
    started: Instant,
    /// What was on screen when this motion started, so a reversal turns
    /// around where the row stands instead of jumping.
    from: Presence,
}

impl Entry {
    fn duration(&self) -> Duration {
        match self.direction {
            Direction::Enter => ENTER,
            Direction::Exit => EXIT,
        }
    }

    fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.duration()
    }

    fn sample(&self, now: Instant) -> Presence {
        let progress = (now.saturating_duration_since(self.started).as_secs_f32()
            / self.duration().as_secs_f32())
        .clamp(0.0, 1.0);
        let phase = |start: f32, span: f32| ((progress - start) / span).clamp(0.0, 1.0);
        // The shared curve: it never overshoots, so a row pushing its
        // neighbours never drags them past their resting place.
        let curve = |t: f32| Motion::SETTLE.settle(t);
        let Presence { height, opacity } = self.from;
        match self.direction {
            Direction::Enter => Presence {
                height: height + (1.0 - height) * curve(progress),
                opacity: opacity
                    + (1.0 - opacity)
                        * curve(phase(ENTER_CONTENT_DELAY, 1.0 - ENTER_CONTENT_DELAY)),
            },
            Direction::Exit => Presence {
                height: height
                    * (1.0 - curve(phase(EXIT_COLLAPSE_DELAY, 1.0 - EXIT_COLLAPSE_DELAY))),
                opacity: opacity * (1.0 - curve(phase(0.0, EXIT_CONTENT))),
            },
        }
    }
}

/// One slot of a container while something in it moves.
#[derive(Debug, PartialEq)]
pub(super) enum Slot<R> {
    /// `rows[index]` of the rows the caller passed in.
    Row(usize, Presence),
    /// A row that has left, drawn from what was last laid out for it. It is
    /// presentation only and must not take the pointer.
    Ghost(R, Presence),
}

struct LaidOut<K, R> {
    rows: Vec<(K, R)>,
    pass: u64,
}

pub(super) struct RowMotion<K, R> {
    /// Every row the sidebar holds, as of the last observation. `None` until
    /// the first one, which is a baseline and never animates.
    known: Option<HashSet<K>>,
    entries: HashMap<K, Entry>,
    /// What each container laid out in the last pass, ghosts included, so a
    /// row that leaves has content to paint and a place to paint it.
    laid_out: HashMap<u64, LaidOut<K, R>>,
    pass: u64,
}

impl<K, R> Default for RowMotion<K, R> {
    fn default() -> Self {
        Self {
            known: None,
            entries: HashMap::new(),
            laid_out: HashMap::new(),
            pass: 0,
        }
    }
}

/// A container key for [`RowMotion::layout`].
pub(super) fn container(key: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

impl<K: Eq + Hash + Clone, R: Clone> RowMotion<K, R> {
    /// Records which rows exist now. `animate` is false when no list is on
    /// screen to show the change, under Reduce Motion, and before the store
    /// holds its first list: the change lands and nothing replays later.
    /// Call it only when membership may have changed; it allocates.
    pub fn observe<'a>(
        &mut self,
        current: impl IntoIterator<Item = &'a K> + Clone,
        animate: bool,
        now: Instant,
    ) where
        K: 'a,
    {
        // Most store updates are status and title changes. Membership that
        // did not change costs a walk, not a set.
        if let Some(known) = &self.known {
            let mut count = 0;
            let unchanged = current.clone().into_iter().all(|id| {
                count += 1;
                known.contains(id)
            }) && count == known.len();
            if unchanged {
                return;
            }
        }
        let current: HashSet<K> = current.into_iter().cloned().collect();
        let Some(known) = self.known.replace(current) else {
            return;
        };
        if !animate {
            self.entries.clear();
            return;
        }
        self.entries.retain(|_, entry| !entry.done(now));
        let current = self.known.as_ref().expect("just stored");
        for id in current.difference(&known) {
            let from = self
                .entries
                .get(id)
                .map_or(Presence::GONE, |entry| entry.sample(now));
            self.entries.insert(
                id.clone(),
                Entry {
                    direction: Direction::Enter,
                    started: now,
                    from,
                },
            );
        }
        for id in known.difference(current) {
            // A row that was not on screen (folded away, filtered out) has
            // nothing to collapse.
            let on_screen = self
                .laid_out
                .values()
                .any(|container| container.rows.iter().any(|(key, _)| key == id));
            if !on_screen {
                self.entries.remove(id);
                continue;
            }
            let from = self
                .entries
                .get(id)
                .map_or(Presence::FULL, |entry| entry.sample(now));
            self.entries.insert(
                id.clone(),
                Entry {
                    direction: Direction::Exit,
                    started: now,
                    from,
                },
            );
        }
        if self.entries.len() > BULK {
            self.entries.clear();
        }
    }

    /// Starts a layout pass: forgets finished motion so a list at rest takes
    /// the allocation-free path.
    pub fn begin_layout(&mut self, now: Instant) {
        self.entries.retain(|_, entry| !entry.done(now));
        self.pass += 1;
    }

    /// Ends a layout pass: a container this pass did not lay out is off
    /// screen, and its rows no longer have anything to collapse.
    pub fn end_layout(&mut self) {
        let pass = self.pass;
        self.laid_out.retain(|_, container| container.pass == pass);
    }

    /// Lays out one container (a project's rows, a recency bucket) at `now`.
    /// `None` means nothing in it moves and `rows` paint exactly as given.
    /// Otherwise the slots to paint, in order, with each leaving row woven
    /// back in after the row it last followed.
    pub fn layout(
        &mut self,
        container: u64,
        rows: &[R],
        id: impl Fn(&R) -> &K,
        now: Instant,
    ) -> Option<Vec<Slot<R>>> {
        let pass = self.pass;
        let previous = self.laid_out.entry(container).or_insert(LaidOut {
            rows: Vec::new(),
            pass,
        });
        previous.pass = pass;
        if self.entries.is_empty() {
            record(&mut previous.rows, rows, &id);
            return None;
        }
        let entries = &self.entries;
        let presence = |key: &K| match entries.get(key) {
            Some(entry) if entry.direction == Direction::Enter => entry.sample(now),
            _ => Presence::FULL,
        };
        let mut keys: Vec<K> = rows.iter().map(|row| id(row).clone()).collect();
        let mut slots: Vec<Slot<R>> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| Slot::Row(index, presence(id(row))))
            .collect();
        for (position, (key, row)) in previous.rows.iter().enumerate() {
            let leaving = entries
                .get(key)
                .is_some_and(|entry| entry.direction == Direction::Exit && !entry.done(now));
            if !leaving || keys.contains(key) {
                continue;
            }
            let at = previous.rows[..position]
                .iter()
                .rev()
                .find_map(|(before, _)| keys.iter().position(|placed| placed == before))
                .map_or(0, |index| index + 1);
            keys.insert(at, key.clone());
            slots.insert(at, Slot::Ghost(row.clone(), entries[key].sample(now)));
        }
        let moving = slots.iter().any(|slot| match slot {
            Slot::Row(_, presence) => *presence != Presence::FULL,
            Slot::Ghost(..) => true,
        });
        if !moving {
            record(&mut previous.rows, rows, &id);
            return None;
        }
        previous.rows.clear();
        previous
            .rows
            .extend(keys.into_iter().zip(slots.iter()).map(|(key, slot)| {
                let row = match slot {
                    Slot::Row(index, _) => rows[*index].clone(),
                    Slot::Ghost(row, _) => row.clone(),
                };
                (key, row)
            }));
        Some(slots)
    }

    #[cfg(all(test, target_os = "macos"))]
    pub fn is_idle_for_test(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether any row still owes a frame at `now`.
    pub fn is_animating(&self, now: Instant) -> bool {
        self.entries.values().any(|entry| !entry.done(now))
    }
}

/// The rows of one container in paint order: `rows` as given when `slots`
/// is `None`, otherwise the slots with their ghosts. The flag marks a ghost.
pub(super) fn paint_order<'a, R>(
    rows: &'a [R],
    slots: &'a Option<Vec<Slot<R>>>,
) -> impl Iterator<Item = (&'a R, Presence, bool)> {
    let resting = slots
        .is_none()
        .then_some(rows.iter())
        .into_iter()
        .flatten()
        .map(|row| (row, Presence::FULL, false));
    let moving = slots.iter().flatten().map(move |slot| match slot {
        Slot::Row(index, presence) => (&rows[*index], *presence, false),
        Slot::Ghost(row, presence) => (row, *presence, true),
    });
    resting.chain(moving)
}

/// A row's slot at `presence`: the slot takes that share of the row's
/// `natural` height and clips to it, and the row, laid out at full size and
/// centred in it, carries the content opacity. Pill, hue tick and text all
/// ride inside, so nothing floats outside the gap. A ghost takes no pointer.
pub(super) fn slot(row: AnyElement, presence: Presence, natural: f32, ghost: bool) -> AnyElement {
    if presence == Presence::FULL && !ghost {
        return row;
    }
    let height = natural * presence.height;
    div()
        .relative()
        .flex_none()
        .w_full()
        .h(px(height))
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px((height - natural) / 2.0))
                .h(px(natural))
                .opacity(presence.opacity)
                .child(row),
        )
        .when(ghost, |slot| {
            slot.child(
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .capture_any_mouse_down(|_, _, cx| cx.stop_propagation())
                    .capture_any_mouse_up(|_, _, cx| cx.stop_propagation()),
            )
        })
        .into_any_element()
}

/// Keeps `stored` equal to `rows`. A list that did not change is refreshed in
/// place, so a sidebar at rest allocates nothing here.
fn record<K: Eq + Clone, R: Clone>(stored: &mut Vec<(K, R)>, rows: &[R], id: impl Fn(&R) -> &K) {
    let same = stored.len() == rows.len()
        && stored
            .iter()
            .zip(rows)
            .all(|((key, _), row)| key == id(row));
    if same {
        for ((_, slot), row) in stored.iter_mut().zip(rows) {
            slot.clone_from(row);
        }
    } else {
        stored.clear();
        stored.extend(rows.iter().map(|row| (id(row).clone(), row.clone())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Model = RowMotion<&'static str, &'static str>;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    /// One layout pass over a single container, as the sidebar renders it.
    fn frame(
        model: &mut Model,
        rows: &[&'static str],
        now: Instant,
    ) -> Option<Vec<Slot<&'static str>>> {
        model.begin_layout(now);
        let slots = model.layout(1, rows, |row| row, now);
        model.end_layout();
        slots
    }

    /// Row labels and presences of a pass, ghosts marked with `~`.
    fn describe(rows: &[&'static str], slots: &[Slot<&'static str>]) -> Vec<(String, Presence)> {
        slots
            .iter()
            .map(|slot| match slot {
                Slot::Row(index, presence) => (rows[*index].to_owned(), *presence),
                Slot::Ghost(row, presence) => (format!("~{row}"), *presence),
            })
            .collect()
    }

    /// A model that has seen `rows` once, at rest.
    fn settled(rows: &[&'static str], now: Instant) -> Model {
        let mut model = Model::default();
        model.observe(rows, true, now);
        assert!(frame(&mut model, rows, now).is_none());
        model
    }

    #[test]
    fn the_first_observation_is_a_baseline() {
        let now = Instant::now();
        let mut model = Model::default();
        model.observe(&["a", "b"], true, now);
        assert!(!model.is_animating(now));
        assert!(frame(&mut model, &["a", "b"], now).is_none());
    }

    #[test]
    fn an_arriving_row_opens_its_gap_before_its_content_shows() {
        let now = Instant::now();
        let mut model = settled(&["a", "c"], now);
        let rows = ["a", "b", "c"];
        model.observe(&rows, true, now);
        assert!(model.is_animating(now));

        let start = frame(&mut model, &rows, now).unwrap();
        assert_eq!(start[1], Slot::Row(1, Presence::GONE));
        assert_eq!(start[0], Slot::Row(0, Presence::FULL));

        let early = describe(&rows, &frame(&mut model, &rows, now + ms(60)).unwrap());
        assert!(early[1].1.height > 0.3, "{early:?}");
        assert_eq!(early[1].1.opacity, 0.0, "content waits for the gap");

        let late = describe(&rows, &frame(&mut model, &rows, now + ms(170)).unwrap());
        assert!(late[1].1.height > 0.95);
        assert!(late[1].1.opacity > 0.3 && late[1].1.opacity < 1.0);

        assert!(frame(&mut model, &rows, now + ENTER).is_none());
        assert!(!model.is_animating(now + ENTER));
    }

    #[test]
    fn a_leaving_row_fades_then_collapses_in_its_old_place() {
        let now = Instant::now();
        let mut model = settled(&["a", "b", "c"], now);
        let rows = ["a", "c"];
        model.observe(&rows, true, now);

        let early = describe(&rows, &frame(&mut model, &rows, now + ms(40)).unwrap());
        assert_eq!(early[1].0, "~b", "the ghost stays where it was");
        assert_eq!(early[1].1.height, 1.0, "the gap holds while content fades");
        assert!(early[1].1.opacity < 1.0);

        let late = describe(&rows, &frame(&mut model, &rows, now + ms(150)).unwrap());
        assert_eq!(late[1].1.opacity, 0.0);
        assert!(late[1].1.height < 0.5);

        assert!(frame(&mut model, &rows, now + EXIT).is_none());
        // The ghost is dropped once the collapse ends.
        assert!(model.laid_out[&1].rows.iter().all(|(key, _)| *key != "b"));
    }

    #[test]
    fn a_ghost_follows_the_row_it_last_followed_even_when_that_one_leaves_too() {
        let now = Instant::now();
        let mut model = settled(&["a", "b", "c", "d"], now);
        let rows = ["a", "d"];
        model.observe(&rows, true, now);
        let slots = describe(&rows, &frame(&mut model, &rows, now + ms(16)).unwrap());
        let order: Vec<_> = slots.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(order, ["a", "~b", "~c", "d"]);
        // The first row of a container can leave too.
        let mut model = settled(&["a", "b"], now);
        model.observe(&["b"], true, now);
        let slots = describe(&["b"], &frame(&mut model, &["b"], now + ms(16)).unwrap());
        assert_eq!(slots[0].0, "~a");
    }

    #[test]
    fn a_row_removed_while_arriving_turns_around_where_it_stands() {
        let now = Instant::now();
        let mut model = settled(&["a"], now);
        model.observe(&["a", "b"], true, now);
        let mid = now + ms(150);
        let arriving = describe(&["a", "b"], &frame(&mut model, &["a", "b"], mid).unwrap())[1].1;
        model.observe(&["a"], true, mid);
        let leaving = describe(&["a"], &frame(&mut model, &["a"], mid).unwrap())[1].1;
        assert_eq!(arriving, leaving, "no jump on reversal");
        let later = describe(&["a"], &frame(&mut model, &["a"], mid + ms(100)).unwrap())[1].1;
        assert!(later.height < leaving.height && later.opacity < leaving.opacity);

        // And back again: a close the Engine refused brings the row back.
        let back = mid + ms(100);
        model.observe(&["a", "b"], true, back);
        let returning = describe(&["a", "b"], &frame(&mut model, &["a", "b"], back).unwrap())[1].1;
        assert_eq!(returning, later);
        assert!(frame(&mut model, &["a", "b"], back + ENTER).is_none());
    }

    #[test]
    fn a_bulk_change_cuts_instead_of_animating() {
        let now = Instant::now();
        let before = ["a", "b", "c", "d", "e", "f"];
        let mut model = settled(&before, now);
        model.observe(&["a"], true, now);
        assert!(!model.is_animating(now));
        assert!(frame(&mut model, &["a"], now).is_none());

        // Also when the rows arrive one update at a time.
        let mut model = settled(&["a"], now);
        let mut rows = vec!["a"];
        for (step, id) in ["b", "c", "d", "e"].into_iter().enumerate() {
            rows.push(id);
            model.observe(&rows, true, now + ms(10 * step as u64));
        }
        assert!(model.is_animating(now + ms(40)));
        rows.push("f");
        model.observe(&rows, true, now + ms(50));
        assert!(!model.is_animating(now + ms(50)));
    }

    #[test]
    fn unanimated_changes_land_at_once_and_stop_what_was_moving() {
        let now = Instant::now();
        let mut model = settled(&["a"], now);
        model.observe(&["a", "b"], true, now);
        assert!(model.is_animating(now));
        // Reduce Motion, a hidden sidebar, or the strip standing in for it.
        model.observe(&["a", "b", "c"], false, now + ms(20));
        assert!(!model.is_animating(now + ms(20)));
        assert!(frame(&mut model, &["a", "b", "c"], now + ms(20)).is_none());
    }

    #[test]
    fn a_row_that_left_while_off_screen_has_nothing_to_collapse() {
        let now = Instant::now();
        let mut model = settled(&["a", "b"], now);
        // `b`'s project is folded: known, but not laid out.
        model.observe(&["a", "b"], true, now);
        assert!(frame(&mut model, &["a"], now).is_none());
        model.observe(&["a"], true, now + ms(16));
        assert!(!model.is_animating(now + ms(16)));
    }

    #[test]
    fn showing_and_hiding_known_rows_is_not_an_arrival() {
        let now = Instant::now();
        let mut model = settled(&["a", "b", "c"], now);
        // A filter, a fold or a regrouping changes what is laid out, not
        // what exists.
        assert!(frame(&mut model, &["b"], now).is_none());
        assert!(frame(&mut model, &["c", "a", "b"], now).is_none());
        assert!(!model.is_animating(now));
    }

    #[test]
    fn frames_stop_the_moment_the_motion_ends() {
        let now = Instant::now();
        let mut model = settled(&["a"], now);
        model.observe(&["a", "b"], true, now);
        let mut t = now;
        let mut frames = 0;
        while model.is_animating(t) {
            frames += 1;
            assert!(frames < 100);
            assert!(frame(&mut model, &["a", "b"], t).is_some());
            t += ms(16);
        }
        // 220 ms at 16 ms per frame: 14 frames, and not one more.
        assert_eq!(frames, 14);
        assert!(frame(&mut model, &["a", "b"], t).is_none());
        assert!(model.entries.is_empty(), "nothing kept at rest");
    }

    #[test]
    fn at_rest_the_record_is_refreshed_in_place() {
        let now = Instant::now();
        let mut model = settled(&["a", "b"], now);
        let before = model.laid_out[&1].rows.as_ptr();
        assert!(frame(&mut model, &["a", "b"], now).is_none());
        assert_eq!(model.laid_out[&1].rows.as_ptr(), before);
    }

    #[test]
    fn a_container_not_laid_out_is_forgotten() {
        let now = Instant::now();
        let mut model = settled(&["a"], now);
        model.begin_layout(now);
        model.end_layout();
        assert!(model.laid_out.is_empty());
    }

    #[test]
    fn the_curves_stay_in_range_and_never_back_up() {
        let now = Instant::now();
        for direction in [Direction::Enter, Direction::Exit] {
            let entry = Entry {
                direction,
                started: now,
                from: if direction == Direction::Enter {
                    Presence::GONE
                } else {
                    Presence::FULL
                },
            };
            let mut previous = entry.sample(now);
            for step in 1..=50 {
                let sample = entry.sample(now + ms(step * 5));
                for value in [sample.height, sample.opacity] {
                    assert!((0.0..=1.0).contains(&value));
                }
                if direction == Direction::Enter {
                    assert!(sample.height >= previous.height);
                    assert!(sample.opacity >= previous.opacity);
                } else {
                    assert!(sample.height <= previous.height);
                    assert!(sample.opacity <= previous.opacity);
                }
                previous = sample;
            }
            assert_eq!(
                previous,
                if direction == Direction::Enter {
                    Presence::FULL
                } else {
                    Presence::GONE
                }
            );
        }
    }
}
