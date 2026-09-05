//! App-owned split intent. Leaves are session identities, never processes or attachments.
use std::collections::HashSet;

use diri_proto::SessionId;
use serde::{Deserialize, Serialize};

pub const MAX_PANES: usize = 8;
const MAX_LAYOUTS: usize = 64;
pub const DIVIDER: f32 = 5.0;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SplitAxis {
    Right,
    Below,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SplitNode {
    Session {
        id: SessionId,
    },
    Split {
        axis: SplitAxis,
        fraction: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    pub path: Vec<bool>,
    pub axis: SplitAxis,
    pub rect: Rect,
    pub parent: Rect,
}

impl SplitNode {
    pub fn session(id: SessionId) -> Self {
        Self::Session { id }
    }

    pub fn ids(&self) -> Vec<SessionId> {
        let mut ids = Vec::new();
        self.collect_ids(&mut ids);
        ids
    }

    fn collect_ids(&self, ids: &mut Vec<SessionId>) {
        match self {
            Self::Session { id } => ids.push(id.clone()),
            Self::Split { first, second, .. } => {
                first.collect_ids(ids);
                second.collect_ids(ids);
            }
        }
    }

    pub fn contains(&self, candidate: &SessionId) -> bool {
        match self {
            Self::Session { id } => id == candidate,
            Self::Split { first, second, .. } => {
                first.contains(candidate) || second.contains(candidate)
            }
        }
    }

    fn split(&mut self, target: &SessionId, new_id: SessionId, axis: SplitAxis) -> bool {
        match self {
            Self::Session { id } if id == target => {
                *self = Self::Split {
                    axis,
                    fraction: 0.5,
                    first: Box::new(self.clone()),
                    second: Box::new(Self::session(new_id)),
                };
                true
            }
            Self::Session { .. } => false,
            Self::Split { first, second, .. } => {
                first.split(target, new_id.clone(), axis) || second.split(target, new_id, axis)
            }
        }
    }

    fn retain(self, keep: &impl Fn(&SessionId) -> bool) -> Option<Self> {
        match self {
            Self::Session { ref id } => keep(id).then_some(self),
            Self::Split {
                axis,
                fraction,
                first,
                second,
            } => match (first.retain(keep), second.retain(keep)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    axis,
                    fraction: normalized_fraction(fraction),
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (first, second) => first.or(second),
            },
        }
    }

    pub fn geometry(&self, rect: Rect) -> (Vec<(SessionId, Rect)>, Vec<Divider>) {
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        self.place(rect, &mut Vec::new(), &mut panes, &mut dividers);
        (panes, dividers)
    }

    fn place(
        &self,
        rect: Rect,
        path: &mut Vec<bool>,
        panes: &mut Vec<(SessionId, Rect)>,
        dividers: &mut Vec<Divider>,
    ) {
        match self {
            Self::Session { id } => panes.push((id.clone(), rect)),
            Self::Split {
                axis,
                fraction,
                first,
                second,
            } => {
                let total = match axis {
                    SplitAxis::Right => rect.width,
                    SplitAxis::Below => rect.height,
                };
                let seam = DIVIDER.min(total.max(0.0));
                let available = (total - seam).max(0.0);
                // At small sizes both children shrink proportionally instead of overflowing.
                let minimum = match axis {
                    SplitAxis::Right => 160.0_f32,
                    SplitAxis::Below => 100.0,
                }
                .min(available / 2.0);
                let a = (available * normalized_fraction(*fraction))
                    .clamp(minimum, available - minimum);
                let (first_rect, second_rect, divider_rect) = match axis {
                    SplitAxis::Right => (
                        Rect { width: a, ..rect },
                        Rect {
                            x: rect.x + a + seam,
                            width: available - a,
                            ..rect
                        },
                        Rect {
                            x: rect.x + a,
                            width: seam,
                            ..rect
                        },
                    ),
                    SplitAxis::Below => (
                        Rect { height: a, ..rect },
                        Rect {
                            y: rect.y + a + seam,
                            height: available - a,
                            ..rect
                        },
                        Rect {
                            y: rect.y + a,
                            height: seam,
                            ..rect
                        },
                    ),
                };
                dividers.push(Divider {
                    path: path.clone(),
                    axis: *axis,
                    rect: divider_rect,
                    parent: rect,
                });
                path.push(false);
                first.place(first_rect, path, panes, dividers);
                path.pop();
                path.push(true);
                second.place(second_rect, path, panes, dividers);
                path.pop();
            }
        }
    }

    pub fn resize(&mut self, path: &[bool], fraction: f32) -> bool {
        let Self::Split {
            first,
            second,
            fraction: saved,
            ..
        } = self
        else {
            return false;
        };
        match path.split_first() {
            None => {
                *saved = normalized_fraction(fraction);
                true
            }
            Some((false, rest)) => first.resize(rest, fraction),
            Some((true, rest)) => second.resize(rest, fraction),
        }
    }

    pub fn neighbor(&self, id: &SessionId, direction: Direction, rect: Rect) -> Option<SessionId> {
        let (panes, _) = self.geometry(rect);
        let (_, source) = panes.iter().find(|(candidate, _)| candidate == id)?;
        let sx = source.x + source.width / 2.0;
        let sy = source.y + source.height / 2.0;
        panes
            .iter()
            .filter(|(candidate, _)| candidate != id)
            .filter_map(|(candidate, r)| {
                let dx = r.x + r.width / 2.0 - sx;
                let dy = r.y + r.height / 2.0 - sy;
                let (forward, cross) = match direction {
                    Direction::Left => (-dx, dy.abs()),
                    Direction::Right => (dx, dy.abs()),
                    Direction::Up => (-dy, dx.abs()),
                    Direction::Down => (dy, dx.abs()),
                };
                (forward > 0.5).then_some((candidate, forward + cross * 2.0))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id.clone())
    }
}

fn normalized_fraction(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.1, 0.9)
    } else {
        0.5
    }
}

#[derive(Clone, Copy)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitLayouts {
    pub version: u32,
    pub layouts: Vec<SplitNode>,
    #[serde(default)]
    pub hidden_auxiliary_parents: Vec<SessionId>,
}

impl Default for SplitLayouts {
    fn default() -> Self {
        Self {
            version: 1,
            layouts: Vec::new(),
            hidden_auxiliary_parents: Vec::new(),
        }
    }
}

impl SplitLayouts {
    pub fn containing(&self, id: &SessionId) -> Option<&SplitNode> {
        self.layouts.iter().find(|layout| layout.contains(id))
    }
    pub fn containing_mut(&mut self, id: &SessionId) -> Option<&mut SplitNode> {
        self.layouts.iter_mut().find(|layout| layout.contains(id))
    }

    pub fn split(&mut self, target: SessionId, new_id: SessionId, axis: SplitAxis) -> bool {
        if target == new_id
            || self
                .containing(&target)
                .is_some_and(|tree| tree.ids().len() >= MAX_PANES || tree.contains(&new_id))
        {
            return false;
        }
        if self.containing(&target).is_none() && self.layouts.len() >= MAX_LAYOUTS {
            return false;
        }
        // Moving an existing session between workspaces never creates two controllers.
        self.close(&new_id);
        if let Some(layout) = self.containing_mut(&target) {
            return layout.split(&target, new_id, axis);
        }
        if self.layouts.len() >= MAX_LAYOUTS {
            return false;
        }
        let mut layout = SplitNode::session(target.clone());
        layout.split(&target, new_id, axis);
        self.layouts.push(layout);
        true
    }

    pub fn close(&mut self, id: &SessionId) -> Option<SessionId> {
        let index = self.layouts.iter().position(|layout| layout.contains(id))?;
        let layout = self
            .layouts
            .remove(index)
            .retain(&|candidate| candidate != id)?;
        let ids = layout.ids();
        if ids.len() > 1 {
            self.layouts.insert(index, layout);
        }
        ids.first().cloned()
    }

    /// Call only once the authoritative session list has hydrated. Empty startup
    /// caches must not erase layouts awaiting their saved sessions.
    pub fn reconcile(&mut self, exists: impl Fn(&SessionId) -> bool) {
        let mut seen = HashSet::new();
        if self.version != 1 {
            *self = Self::default();
            return;
        }
        self.hidden_auxiliary_parents.retain(|id| exists(id));
        self.hidden_auxiliary_parents.truncate(MAX_LAYOUTS);
        self.layouts = std::mem::take(&mut self.layouts)
            .into_iter()
            .take(MAX_LAYOUTS)
            .filter_map(|tree| {
                let ids = tree.ids();
                if ids.len() > MAX_PANES
                    || ids.iter().any(|id| seen.contains(id))
                    || ids.iter().collect::<HashSet<_>>().len() != ids.len()
                {
                    return None;
                }
                let tree = tree.retain(&exists)?;
                let ids = tree.ids();
                if ids.len() < 2 {
                    return None;
                }
                seen.extend(ids);
                Some(tree)
            })
            .collect();
    }
}

pub fn deserialize_split_layouts<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SplitLayouts, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let mut layouts: SplitLayouts = serde_json::from_value(value).unwrap_or_default();
    layouts.reconcile(|_| true);
    Ok(layouts)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(value: &str) -> SessionId {
        SessionId::new(value)
    }
    fn nested() -> SplitLayouts {
        let mut layouts = SplitLayouts::default();
        assert!(layouts.split(id("a"), id("b"), SplitAxis::Right));
        assert!(layouts.split(id("b"), id("c"), SplitAxis::Below));
        layouts
    }
    #[test]
    fn nested_splits_preserve_identity_and_fill_the_viewport() {
        let layouts = nested();
        let tree = layouts.containing(&id("a")).unwrap();
        let (panes, dividers) = tree.geometry(Rect {
            width: 805.0,
            height: 605.0,
            ..Rect::default()
        });
        assert_eq!(panes[0].1.width, 400.0);
        assert_eq!(panes[1].1.height, 300.0);
        assert_eq!(panes[2].1.y, 305.0);
        assert_eq!(dividers.len(), 2);
        assert_eq!(
            tree.neighbor(
                &id("c"),
                Direction::Up,
                Rect {
                    width: 805.0,
                    height: 605.0,
                    ..Rect::default()
                }
            ),
            Some(id("b"))
        );
    }
    #[test]
    fn closing_collapses_only_the_affected_branch() {
        let mut layouts = nested();
        assert_eq!(layouts.close(&id("b")), Some(id("a")));
        assert_eq!(
            layouts.containing(&id("a")).unwrap().ids(),
            vec![id("a"), id("c")]
        );
        assert_eq!(layouts.close(&id("a")), Some(id("c")));
        assert!(layouts.layouts.is_empty());
    }
    #[test]
    fn persistence_reconciles_deleted_sessions_and_unknown_versions() {
        let layouts = nested();
        let mut restored: SplitLayouts =
            serde_json::from_str(&serde_json::to_string(&layouts).unwrap()).unwrap();
        restored.reconcile(|id| id.0 != "b");
        assert_eq!(restored.layouts[0].ids(), vec![id("a"), id("c")]);
        restored.version = 99;
        restored.reconcile(|_| true);
        assert!(restored.layouts.is_empty());
    }
    #[test]
    fn duplicates_and_limits_do_not_change_layout() {
        let mut layouts = nested();
        let before = layouts.clone();
        assert!(!layouts.split(id("a"), id("c"), SplitAxis::Right));
        assert_eq!(layouts, before);
        for i in 3..MAX_PANES {
            assert!(layouts.split(id("a"), id(&i.to_string()), SplitAxis::Right));
        }
        assert!(!layouts.split(id("a"), id("overflow"), SplitAxis::Below));
        let duplicate = layouts.layouts[0].clone();
        layouts.layouts.push(duplicate);
        layouts.reconcile(|_| true);
        assert_eq!(layouts.layouts.len(), 1);
    }
    #[test]
    fn resizing_nested_branch_does_not_move_its_sibling() {
        let mut layouts = nested();
        let tree = layouts.containing_mut(&id("a")).unwrap();
        assert!(tree.resize(&[true], 0.7));
        let (panes, _) = tree.geometry(Rect {
            width: 805.0,
            height: 605.0,
            ..Rect::default()
        });
        assert_eq!(panes[0].1.width, 400.0);
        assert_eq!(panes[1].1.height, 420.0);
        tree.resize(&[], f32::NAN);
        let (panes, _) = tree.geometry(Rect {
            width: 2.0,
            height: 2.0,
            ..Rect::default()
        });
        assert!(
            panes
                .iter()
                .all(|(_, rect)| rect.width >= 0.0 && rect.height >= 0.0)
        );
    }
}
