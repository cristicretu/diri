//! Which hue each project wears, and the mark that shows it.
//!
//! The hue comes from the project's id and from nothing stored. A
//! `ProjectId` is the Engine's hash of host and root path, so the hue
//! survives renaming a project, follows the directory, and starts from the
//! same place on every launch and every machine.
//!
//! Hashing straight to a hue collides often (four projects on six hues
//! collide more often than not), so the hash picks a *home* slot on
//! `identity_hue::WHEEL` and a collision steps aside: projects are seated in
//! the order the Engine first saw them, and each walks the wheel from its
//! home, in its own direction, to the first free slot. With more projects
//! than slots, a project shares the least crowded slot on its walk. Titles
//! and headers still tell those apart; hue never carries identity alone.
//!
//! Seating in that order is what keeps colors still. The Engine's project
//! list is append-only, so the order never changes under a project. A pure
//! function of the *set* of ids cannot do this: when two projects share a
//! home, a rule that cannot see who came first must sometimes move the one
//! already on screen (simulated: a fourth project recolored one of three 29%
//! of the time, a sixth one of five 60%). Seated in order, a project that
//! opens takes what is free and moves no one, and a project that leaves can
//! only hand its slot back to a newer one that had stepped aside for it.

use std::collections::HashMap;

use diri_proto::ProjectId;
use diri_term::identity_hue::{self, WHEEL};
use diri_ui::SemanticColors;
use gpui::{Div, Rgba, Styled, div, px};

use crate::store::{SessionStore, SidebarProjection};

const SLOTS: usize = WHEEL.len();

/// An index into `identity_hue::WHEEL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct HueSlot(u8);

impl HueSlot {
    pub(crate) fn degrees(self) -> f32 {
        WHEEL[usize::from(self.0)]
    }

    /// The mark color on the theme `colors` was derived from. `colors` is the
    /// crossfade's current frame while a theme change is fading, so the mark
    /// fades with the chrome around it.
    pub(crate) fn color(self, colors: SemanticColors) -> Rgba {
        identity_hue::mark(colors.background, self.degrees())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProjectHues(HashMap<ProjectId, HueSlot>);

impl ProjectHues {
    /// Hues for the projects the sidebar shows, seated in `seniority` order
    /// (`SessionStore::project_seniority`). A lone project gets none: with
    /// nothing to tell it apart from, a mark would be decoration.
    pub(crate) fn of(projection: &SidebarProjection, seniority: &[ProjectId]) -> Self {
        if projection.projects.len() < 2 {
            return Self::default();
        }
        let mut seated: Vec<(usize, u64, &ProjectId)> = projection
            .projects
            .iter()
            .map(|group| {
                let id = &group.project.id;
                // A project the Engine has not listed yet is the newest.
                let rank = seniority
                    .iter()
                    .position(|known| known == id)
                    .unwrap_or(usize::MAX);
                (rank, stable_hash(&id.0), id)
            })
            .collect();
        seated.sort_unstable_by_key(|(rank, hash, _)| (*rank, *hash));
        Self::assign(seated.into_iter().map(|(_, _, project)| project))
    }

    /// Seats `projects` in the order given; earlier projects choose first.
    pub(crate) fn assign<'a>(projects: impl IntoIterator<Item = &'a ProjectId>) -> Self {
        let mut load = [0_usize; SLOTS];
        let mut hues = HashMap::new();
        for project in projects {
            if hues.contains_key(project) {
                continue;
            }
            let slot = walk(stable_hash(&project.0))
                .min_by_key(|slot| load[*slot])
                .expect("the wheel has slots");
            load[slot] += 1;
            hues.insert(project.clone(), HueSlot(slot as u8));
        }
        Self(hues)
    }

    pub(crate) fn slot(&self, project: &ProjectId) -> Option<HueSlot> {
        self.0.get(project).copied()
    }

    pub(crate) fn color(&self, project: &ProjectId, colors: SemanticColors) -> Option<Rgba> {
        self.slot(project).map(|slot| slot.color(colors))
    }
}

impl SessionStore {
    pub(crate) fn project_hues(&mut self) -> ProjectHues {
        let projection = self.sidebar_projection();
        ProjectHues::of(&projection, self.project_seniority())
    }
}

fn home(hash: u64) -> usize {
    (hash % SLOTS as u64) as usize
}

/// Every slot once, starting at home. `min_by_key` keeps the first of equals,
/// so the walk order is also the tie-break.
fn walk(hash: u64) -> impl Iterator<Item = usize> {
    // One step clockwise or one step counter-clockwise: the strides coprime
    // with six slots.
    let stride = if (hash >> 8) & 1 == 0 { 1 } else { SLOTS - 1 };
    (0..SLOTS).map(move |step| (home(hash) + step * stride) % SLOTS)
}

/// FNV-1a with a final avalanche. `std`'s hashers make no promise across
/// releases, and a project must not change color when the toolchain does.
fn stable_hash(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash
}

const TICK_WIDTH: f32 = 3.0;
const TICK_HEIGHT: f32 = 12.0;

/// The mark a session wears where sessions of several projects mix: a short
/// rounded tick. Where a project itself is shown, its glyph takes the hue
/// instead and nothing is added.
pub(crate) fn tick(color: Rgba) -> Div {
    div()
        .flex_none()
        .w(px(TICK_WIDTH))
        .h(px(TICK_HEIGHT))
        .rounded(px(TICK_WIDTH / 2.0))
        .bg(color)
}

/// The tick inside the leading edge of a sidebar row. It is laid over the
/// row rather than into it, so the row's columns, and the title width
/// computed from them, do not move. The parent must be `relative`.
pub(crate) fn row_tick(color: Rgba, row_height: f32) -> Div {
    tick(color)
        .absolute()
        .left(px(2.0))
        .top(px((row_height - TICK_HEIGHT) / 2.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(range: std::ops::Range<usize>) -> Vec<ProjectId> {
        // Shaped like the Engine's ids: `p_` and twelve hex digits.
        range
            .map(|index| {
                ProjectId::new(format!("p_{:012x}", stable_hash(&index.to_string()) >> 16))
            })
            .collect()
    }

    const PINNED_EMPTY: u64 = 17_058_014_651_485_797_458;
    const PINNED_ID: u64 = 4_986_836_285_482_338_516;
    const PINNED_DEGREES: f32 = 260.0;

    fn hue_distance(left: HueSlot, right: HueSlot) -> f32 {
        let apart = (left.degrees() - right.degrees()).abs();
        apart.min(360.0 - apart)
    }

    #[test]
    fn projects_that_fit_the_wheel_never_share_a_hue() {
        for start in 0..400 {
            for count in 2..=SLOTS {
                let projects = ids(start..start + count);
                let hues = ProjectHues::assign(&projects);
                for (index, left) in projects.iter().enumerate() {
                    for right in &projects[index + 1..] {
                        let apart =
                            hue_distance(hues.slot(left).unwrap(), hues.slot(right).unwrap());
                        assert!(apart >= 44.9, "{left} and {right} are {apart} apart");
                    }
                }
            }
        }
    }

    #[test]
    fn projects_beyond_the_wheel_share_evenly() {
        for start in 0..200 {
            for count in SLOTS + 1..=2 * SLOTS {
                let projects = ids(start..start + count);
                let hues = ProjectHues::assign(&projects);
                let mut load = [0; SLOTS];
                for project in &projects {
                    load[usize::from(hues.slot(project).unwrap().0)] += 1;
                }
                assert!(load.iter().all(|count| (1..=2).contains(count)), "{load:?}");
            }
        }
    }

    #[test]
    fn assignment_is_deterministic_and_ignores_duplicates() {
        let mut projects = ids(0..7);
        let once = ProjectHues::assign(&projects);
        projects.push(projects[2].clone());
        assert_eq!(ProjectHues::assign(&projects), once);
        // Pinned: a project must not change color when the toolchain or the
        // hasher's implementation does.
        assert_eq!(stable_hash(""), PINNED_EMPTY);
        assert_eq!(stable_hash("p_0123456789ab"), PINNED_ID);
        assert_eq!(
            once.slot(&projects[0]).map(HueSlot::degrees),
            Some(PINNED_DEGREES)
        );
    }

    #[test]
    fn a_project_alone_at_its_home_wears_the_same_hue_everywhere() {
        // What survives across machines, whatever else each has open.
        for project in ids(0..200) {
            let alone = ProjectHues::assign([&project]);
            assert_eq!(
                usize::from(alone.slot(&project).unwrap().0),
                home(stable_hash(&project.0))
            );
        }
    }

    #[test]
    fn opening_a_project_never_recolors_the_ones_already_open() {
        for start in 0..500 {
            let projects = ids(start..start + 2 * SLOTS);
            for count in 1..projects.len() {
                let before = ProjectHues::assign(&projects[..count]);
                let after = ProjectHues::assign(&projects[..=count]);
                for project in &projects[..count] {
                    assert_eq!(before.slot(project), after.slot(project));
                }
            }
        }
    }

    #[test]
    fn closing_a_project_moves_only_projects_that_had_stepped_aside() {
        // How often closing one of `open` projects recolors another. Nothing
        // is stored, so the rest are seated as if it had never been open.
        for (open, at_most) in [(3, 0.23), (4, 0.36), (6, 0.63)] {
            let mut closed = 0;
            let mut disturbed = 0;
            for start in 0..500 {
                let projects = ids(start..start + open);
                let before = ProjectHues::assign(&projects);
                for gone in 0..projects.len() {
                    let rest: Vec<_> = projects
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| *index != gone)
                        .map(|(_, project)| project)
                        .collect();
                    let after = ProjectHues::assign(rest.iter().copied());
                    let mut moved = false;
                    for project in rest {
                        if before.slot(project) == after.slot(project) {
                            continue;
                        }
                        moved = true;
                        assert_ne!(
                            usize::from(before.slot(project).unwrap().0),
                            home(stable_hash(&project.0)),
                            "{project} was at home and moved anyway"
                        );
                    }
                    closed += 1;
                    disturbed += usize::from(moved);
                }
            }
            // Measured: 20%, 33% and 59%. Only projects newer than the one
            // closed can move, and only back toward their home.
            let share = disturbed as f32 / closed as f32;
            assert!(share < at_most, "{open} open: {share}");
        }
    }

    #[test]
    fn projects_are_seated_in_the_order_the_engine_lists_them() {
        use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Projects);
        let listed: Vec<_> = fixture
            .list
            .projects
            .iter()
            .map(|project| project.id.clone())
            .collect();
        let mut store = fixture.into_store();
        assert_eq!(store.project_seniority(), listed);
        let hues = store.project_hues();
        assert_eq!(hues, ProjectHues::assign(&listed));
        // Six projects, six hues: none shared.
        let mut slots: Vec<_> = listed
            .iter()
            .map(|project| hues.slot(project).unwrap().0)
            .collect();
        slots.sort_unstable();
        slots.dedup();
        assert_eq!(slots.len(), SLOTS);
    }

    #[test]
    fn the_mark_fades_with_the_theme_and_lands_on_it() {
        use crate::app_theme::{self, live::testing};
        use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
        use diri_term::theme::TermTheme;
        use std::time::Duration;

        let slot = HueSlot(2);
        let settled = |theme: TermTheme| identity_hue::mark(theme.background, slot.degrees());
        let _fades = testing::enable_with_manual_clock();
        let mut store = SidebarPreviewFixture::make(PreviewScenario::Typical).into_store();
        let mut show = |theme: &str| {
            store
                .update_preferences(|prefs| prefs.terminal_theme = theme.into())
                .unwrap();
            slot.color(app_theme::sidebar_colors_in(&store))
        };
        assert_eq!(show("dracula"), settled(TermTheme::DRACULA));
        // The frame a change lands in still shows the old colors.
        assert_eq!(show("github-light"), settled(TermTheme::DRACULA));

        testing::advance(Duration::from_millis(60));
        assert!(testing::tick());
        let midway = show("github-light");
        assert_ne!(midway, settled(TermTheme::DRACULA));
        assert_ne!(midway, settled(TermTheme::GITHUB_LIGHT));

        testing::advance(Duration::from_millis(400));
        assert!(!testing::tick());
        assert_eq!(show("github-light"), settled(TermTheme::GITHUB_LIGHT));
    }

    #[test]
    fn status_inks_stay_louder_than_any_project_mark() {
        use diri_term::theme::{TermTheme, ThemeAppearance};
        use diri_ui::{Ink, Palette};

        for theme in TermTheme::CATALOG {
            let colors = crate::app_theme::sidebar_colors(theme.id);
            let surface = gpui::Rgba {
                a: 1.0,
                ..colors.sidebar_surface()
            };
            for slot in 0..SLOTS as u8 {
                let mark = identity_hue::loudness(HueSlot(slot).color(colors), surface);
                for ink in [Ink::DANGER, Ink::ATTENTION, Ink::FRESH, Palette::CLAY] {
                    let bright = ink == Ink::ATTENTION || ink == Ink::FRESH;
                    let ink = identity_hue::loudness(ink, surface);
                    assert!(ink.chroma > mark.chroma * 1.4, "{}", theme.id);
                    let apart = (ink.hue - mark.hue).abs();
                    assert!(apart.min(360.0 - apart) >= 20.0, "{}", theme.id);
                    // Contrast ranks only the bright inks, and only on dark
                    // chrome. The inks are fixed colors: the danger red is a
                    // dark color that sits near 3:1 on several dark themes,
                    // and on a light theme every one of them is under 3:1.
                    // There they outrank the mark by chroma and size alone.
                    if bright && theme.appearance == ThemeAppearance::Dark {
                        assert!(ink.contrast > mark.contrast * 1.05, "{}", theme.id);
                    }
                }
            }
        }
    }

    #[test]
    fn a_lone_project_carries_no_mark() {
        use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
        let mut fleet = SidebarPreviewFixture::make(PreviewScenario::Fleet).into_store();
        assert_eq!(fleet.project_hues(), ProjectHues::default());
        let mut typical = SidebarPreviewFixture::make(PreviewScenario::Typical).into_store();
        assert_eq!(
            typical.project_hues().0.len(),
            typical.sidebar_projection().projects.len()
        );
    }
}
