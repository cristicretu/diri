//! Navigation-only label filtering. Filtering never writes preferences or
//! changes selection; clearing restores the exact unfiltered projection.
use crate::store::{SidebarProjection, SidebarRow};
use crate::switcher::display_title;
use std::ops::Range;
use std::sync::Arc;

/// Case-insensitive substring matching, with byte ranges mapped back to the
/// original UTF-8 string even when lowercase expands a character.
pub(super) fn label_match(label: &str, query: &str) -> Option<Range<usize>> {
    if query.trim().is_empty() {
        return None;
    }
    let folded = label.to_lowercase();
    let mut source = Vec::new();
    for (start, ch) in label.char_indices() {
        let end = start + ch.len_utf8();
        for lower in ch.to_lowercase() {
            source.extend(std::iter::repeat_n(start..end, lower.len_utf8()));
        }
    }
    let query = query.trim().to_lowercase();
    let start = folded.find(&query)?;
    Some(source[start].start..source[start + query.len() - 1].end)
}

pub(super) fn filter_projection(
    projection: Arc<SidebarProjection>,
    query: &str,
) -> Arc<SidebarProjection> {
    if query.trim().is_empty() {
        return projection;
    }
    let mut filtered = (*projection).clone();
    for group in &mut filtered.projects {
        group
            .active
            .retain(|session| label_match(&display_title(session), query).is_some());
        group
            .archived
            .retain(|session| label_match(&display_title(session), query).is_some());
        // Search is a flat list within each existing project heading. Folded
        // descendants participate without changing their saved disclosure.
        group.sessions = group
            .active
            .iter()
            .map(|session| SidebarRow {
                session: session.clone(),
                depth: 0,
                has_children: false,
                collapsed: false,
                pinned: false,
                rails: 0,
            })
            .collect();
    }
    filtered
        .projects
        .retain(|group| !group.active.is_empty() || !group.archived.is_empty());
    filtered.ordered_sessions = filtered
        .projects
        .iter()
        .flat_map(|group| group.active.iter().cloned())
        .collect();
    filtered.display_order = filtered
        .projects
        .iter()
        .flat_map(|group| {
            group
                .active
                .iter()
                .chain(&group.archived)
                .map(|session| session.id.clone())
        })
        .collect();
    Arc::new(filtered)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filtering_keeps_selected_work_and_saved_folds_while_clear_restores_projection() {
        use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
        use crate::store::SessionStore;
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let (mut store, _effects) = SessionStore::headless(fixture.prefs);
        store.hydrate(fixture.list);
        store.select(fixture.selected_session_id.unwrap());
        let selected = store.selected_session_id().cloned();
        let records = store.sessions().clone();
        let project = store.selected_session().unwrap().project_id.clone();
        store.toggle_project_collapsed(project.clone()).unwrap();
        let prefs = store.preferences().clone();
        let original = store.sidebar_projection();
        let filtered = filter_projection(original.clone(), "sidebar");
        assert!(!filtered.projects.is_empty());
        assert!(
            filtered
                .projects
                .iter()
                .flat_map(|group| &group.sessions)
                .all(|row| label_match(&display_title(&row.session), "sidebar").is_some())
        );
        assert!(
            filter_projection(original.clone(), "nonexistent-label-123")
                .projects
                .is_empty()
        );
        let restored = filter_projection(original.clone(), "");
        assert!(Arc::ptr_eq(&original, &restored));
        assert_eq!(store.preferences(), &prefs);
        assert_eq!(store.selected_session_id(), selected.as_ref());
        assert_eq!(store.sessions(), &records);
    }

    #[test]
    fn matching_maps_multibyte_and_expanded_lowercase_to_original_glyphs() {
        assert_eq!(label_match("α Open İTEM 🦀", "OPEN"), Some(3..7));
        assert_eq!(label_match("İTEM", "i"), Some(0..2));
        assert_eq!(label_match("Rust 🦀", "🦀"), Some(5..9));
        assert_eq!(label_match("fish", "   "), None);
    }
}
