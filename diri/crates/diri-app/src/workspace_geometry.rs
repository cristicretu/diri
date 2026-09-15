//! Shared settled workbench geometry. Preview scaling never recomputes pane minima.
use diri_proto::{
    SessionId,
    workspace::{
        LayoutAxis, LayoutNode, MAX_LAYOUT_DEPTH, MAX_TAB_PANES, PaneId, TabId, WorkspaceTab,
    },
};
use std::collections::HashSet;

pub(crate) const DIVIDER: f32 = 5.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
impl Rect {
    fn valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|value| value.is_finite())
            && self.width > 0.0
            && self.height > 0.0
    }
    pub(crate) fn intersects(self, other: Self) -> bool {
        self.width > 0.0
            && self.height > 0.0
            && other.width > 0.0
            && other.height > 0.0
            && self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaneIdentity {
    pub pane: PaneId,
    pub session: SessionId,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PanePlacement {
    pub identity: PaneIdentity,
    pub bounds: Rect,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WorkspaceGeometry {
    pub tab: TabId,
    pub focused: PaneIdentity,
    pub panes: Vec<PanePlacement>,
    pub dividers: Vec<Rect>,
    pub bounds: Rect,
}

impl WorkspaceGeometry {
    /// Resolve the durable focus target separately from its presentation. No
    /// session is spawned, selected, attached or resized by this projection.
    pub(crate) fn settled(tab: &WorkspaceTab, bounds: Rect) -> Option<Self> {
        if !bounds.valid() {
            return None;
        }
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        let mut identities = HashSet::new();
        place(
            &tab.layout,
            bounds,
            0,
            &mut panes,
            &mut dividers,
            &mut identities,
        )?;
        let focused = panes
            .iter()
            .find(|pane| pane.identity.pane == tab.focused_pane)?
            .identity
            .clone();
        if let Some(zoomed) = &tab.zoomed_pane {
            if *zoomed != focused.pane {
                return None;
            }
            panes.retain(|pane| pane.identity.pane == *zoomed);
            panes.first_mut()?.bounds = bounds;
            dividers.clear();
        }
        Some(Self {
            tab: tab.id.clone(),
            focused,
            panes,
            dividers,
            bounds,
        })
    }

    /// Fit the already-settled geometry with one uniform transform. Calling
    /// `settled` at thumbnail dimensions would clamp uneven splits toward half.
    pub(crate) fn fit(&self, target: Rect) -> Option<Self> {
        if !target.valid() {
            return None;
        }
        let scale = (target.width / self.bounds.width).min(target.height / self.bounds.height);
        let fitted = Rect {
            x: target.x + (target.width - self.bounds.width * scale) / 2.0,
            y: target.y + (target.height - self.bounds.height * scale) / 2.0,
            width: self.bounds.width * scale,
            height: self.bounds.height * scale,
        };
        let transform = |bounds: Rect| Rect {
            x: fitted.x + (bounds.x - self.bounds.x) * scale,
            y: fitted.y + (bounds.y - self.bounds.y) * scale,
            width: bounds.width * scale,
            height: bounds.height * scale,
        };
        Some(Self {
            tab: self.tab.clone(),
            focused: self.focused.clone(),
            bounds: fitted,
            panes: self
                .panes
                .iter()
                .map(|pane| PanePlacement {
                    identity: pane.identity.clone(),
                    bounds: transform(pane.bounds),
                })
                .collect(),
            dividers: self.dividers.iter().copied().map(transform).collect(),
        })
    }

    /// All intersecting leaf sessions, before applying any transport budget.
    /// Deduplication is across cards at the caller, since a grid can paint twice.
    pub(crate) fn visible_sessions(&self, clip: Rect) -> Vec<SessionId> {
        self.panes
            .iter()
            .filter(|pane| pane.bounds.intersects(clip))
            .map(|pane| pane.identity.session.clone())
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PreviewDemand {
    pub visible_panes: usize,
    pub reused_sessions: Vec<SessionId>,
    pub subscriptions: Vec<SessionId>,
    pub deferred_sessions: Vec<SessionId>,
}

/// Report omitted live work explicitly. A connection budget is not a claim that
/// every visible pane is receiving updates. Resident sources consume no slots.
pub(crate) fn preview_demand(
    layouts: &[WorkspaceGeometry],
    clip: Rect,
    resident: &HashSet<SessionId>,
    budget: usize,
) -> PreviewDemand {
    let mut demand = PreviewDemand {
        visible_panes: 0,
        reused_sessions: Vec::new(),
        subscriptions: Vec::new(),
        deferred_sessions: Vec::new(),
    };
    let mut seen = HashSet::new();
    for layout in layouts {
        for session in layout.visible_sessions(clip) {
            demand.visible_panes += 1;
            if !seen.insert(session.clone()) {
                continue;
            }
            if resident.contains(&session) {
                demand.reused_sessions.push(session);
            } else if demand.subscriptions.len() < budget {
                demand.subscriptions.push(session);
            } else {
                demand.deferred_sessions.push(session);
            }
        }
    }
    demand
}

fn place(
    node: &LayoutNode,
    bounds: Rect,
    depth: usize,
    panes: &mut Vec<PanePlacement>,
    dividers: &mut Vec<Rect>,
    identities: &mut HashSet<PaneId>,
) -> Option<()> {
    if depth > MAX_LAYOUT_DEPTH {
        return None;
    }
    match node {
        LayoutNode::Pane { id, session_id } => {
            if panes.len() >= MAX_TAB_PANES || !identities.insert(id.clone()) {
                return None;
            }
            panes.push(PanePlacement {
                identity: PaneIdentity {
                    pane: id.clone(),
                    session: session_id.clone(),
                },
                bounds,
            });
        }
        LayoutNode::Split {
            axis,
            fraction,
            first,
            second,
            ..
        } => {
            if !fraction.is_finite() || !(0.1..=0.9).contains(fraction) {
                return None;
            }
            let horizontal = *axis == LayoutAxis::Horizontal;
            let total = if horizontal {
                bounds.width
            } else {
                bounds.height
            };
            let seam = DIVIDER.min(total.max(0.0));
            let available = (total - seam).max(0.0);
            let minimum = if horizontal { 160.0_f32 } else { 100.0 }.min(available / 2.0);
            let a = (available * fraction).clamp(minimum, available - minimum);
            let (left, right, divider) = if horizontal {
                (
                    Rect { width: a, ..bounds },
                    Rect {
                        x: bounds.x + a + seam,
                        width: available - a,
                        ..bounds
                    },
                    Rect {
                        x: bounds.x + a,
                        width: seam,
                        ..bounds
                    },
                )
            } else {
                (
                    Rect {
                        height: a,
                        ..bounds
                    },
                    Rect {
                        y: bounds.y + a + seam,
                        height: available - a,
                        ..bounds
                    },
                    Rect {
                        y: bounds.y + a,
                        height: seam,
                        ..bounds
                    },
                )
            };
            dividers.push(divider);
            place(first, left, depth + 1, panes, dividers, identities)?;
            place(second, right, depth + 1, panes, dividers, identities)?;
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::SplitId;
    fn pane(id: &str) -> LayoutNode {
        LayoutNode::Pane {
            id: PaneId::new(id),
            session_id: SessionId::new(format!("session-{id}")),
        }
    }
    fn tab() -> WorkspaceTab {
        WorkspaceTab {
            id: TabId::new("tab-a"),
            title: None,
            focused_pane: PaneId::new("b"),
            zoomed_pane: None,
            layout: LayoutNode::Split {
                id: SplitId::new("split-a"),
                axis: LayoutAxis::Horizontal,
                fraction: 0.7,
                first: Box::new(pane("a")),
                second: Box::new(LayoutNode::Split {
                    id: SplitId::new("split-b"),
                    axis: LayoutAxis::Vertical,
                    fraction: 0.3,
                    first: Box::new(pane("b")),
                    second: Box::new(pane("c")),
                }),
            },
        }
    }
    #[test]
    fn thumbnail_retains_uneven_nested_geometry_and_exact_focus_identity() {
        let tab = tab();
        let before = tab.clone();
        let source = WorkspaceGeometry::settled(
            &tab,
            Rect {
                width: 1005.0,
                height: 605.0,
                ..Rect::default()
            },
        )
        .unwrap();
        let preview = source
            .fit(Rect {
                x: 10.0,
                y: 20.0,
                width: 201.0,
                height: 121.0,
            })
            .unwrap();
        assert_eq!(source.panes[0].bounds.width, 700.0);
        assert_eq!(preview.panes[0].bounds.width, 140.0);
        assert!((preview.panes[1].bounds.height - 36.0).abs() < 0.001);
        assert!((preview.panes[2].bounds.height - 84.0).abs() < 0.001);
        assert_eq!(
            preview.focused,
            PaneIdentity {
                pane: PaneId::new("b"),
                session: SessionId::new("session-b")
            }
        );
        assert_eq!(preview.tab, tab.id);
        assert_eq!(tab, before);
        assert_eq!(preview.dividers[0].width, 1.0);
    }
    #[test]
    fn zoomed_pane_changes_only_projection_and_demand() {
        let mut tab = tab();
        tab.zoomed_pane = Some(PaneId::new("b"));
        let geometry = WorkspaceGeometry::settled(
            &tab,
            Rect {
                width: 1000.0,
                height: 600.0,
                ..Rect::default()
            },
        )
        .unwrap();
        assert_eq!(geometry.panes.len(), 1);
        assert!(geometry.dividers.is_empty());
        assert_eq!(
            geometry.visible_sessions(geometry.bounds),
            vec![SessionId::new("session-b")]
        );
        assert_eq!(geometry.panes[0].bounds, geometry.bounds);
        assert!(
            geometry
                .visible_sessions(Rect {
                    x: 2000.0,
                    width: 100.0,
                    height: 100.0,
                    ..Rect::default()
                })
                .is_empty()
        );
        tab.zoomed_pane = Some(PaneId::new("a"));
        assert!(WorkspaceGeometry::settled(&tab, geometry.bounds).is_none());
    }
    #[test]
    fn demand_reports_every_omitted_split_pane_instead_of_claiming_all_live() {
        let bounds = Rect {
            width: 1200.0,
            height: 800.0,
            ..Rect::default()
        };
        let layouts: Vec<_> = (0..6)
            .map(|tab_index| WorkspaceGeometry {
                tab: TabId::new(tab_index.to_string()),
                focused: PaneIdentity {
                    pane: PaneId::new("focused"),
                    session: SessionId::new("session-0"),
                },
                panes: (0..8)
                    .map(|pane_index| PanePlacement {
                        identity: PaneIdentity {
                            pane: PaneId::new(format!("{tab_index}-{pane_index}")),
                            session: SessionId::new(format!(
                                "session-{}",
                                tab_index * 8 + pane_index
                            )),
                        },
                        bounds,
                    })
                    .collect(),
                dividers: vec![],
                bounds,
            })
            .collect();
        let resident = HashSet::from([SessionId::new("session-0")]);
        let ui = preview_demand(&layouts, bounds, &resident, 8);
        assert_eq!(ui.visible_panes, 48);
        assert_eq!(ui.reused_sessions.len(), 1);
        assert_eq!(ui.subscriptions.len(), 8);
        assert_eq!(ui.deferred_sessions.len(), 39);
        let server = preview_demand(&layouts, bounds, &resident, 16);
        assert_eq!(server.deferred_sessions.len(), 31);
    }

    #[test]
    fn invalid_geometry_or_stale_focus_is_rejected_without_fallback_identity() {
        let bounds = Rect {
            width: 800.0,
            height: 600.0,
            ..Rect::default()
        };
        let mut tab = tab();
        tab.focused_pane = PaneId::new("deleted");
        assert!(WorkspaceGeometry::settled(&tab, bounds).is_none());
        assert!(
            WorkspaceGeometry::settled(
                &tab,
                Rect {
                    width: f32::NAN,
                    ..bounds
                }
            )
            .is_none()
        );
    }
}
