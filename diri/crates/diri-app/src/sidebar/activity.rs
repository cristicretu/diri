//! Opportunistic motion: callers supply one shared phase on an existing paint.
//! No task, clock, entity invalidation, or frame request belongs to this mark.

use diri_ui::{Icon, IconName, Ink, SemanticColors, StatusState};
use gpui::{AnyElement, IntoElement, div, prelude::*, px, svg};

const FRAMES: [&str; 8] = [
    "icons/working-0.svg",
    "icons/working-1.svg",
    "icons/working-2.svg",
    "icons/working-3.svg",
    "icons/working-4.svg",
    "icons/working-5.svg",
    "icons/working-6.svg",
    "icons/working-7.svg",
];

pub(super) fn frame_at(millis: f64, reduce_motion: bool) -> usize {
    if reduce_motion {
        0
    } else {
        (millis.max(0.0) / 125.0) as usize % FRAMES.len()
    }
}

pub(super) fn activity_mark(
    state: StatusState,
    frame: usize,
    colors: SemanticColors,
) -> AnyElement {
    let slot = div()
        .size(px(16.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center();
    match state {
        StatusState::Working => slot
            .child(
                svg()
                    .path(FRAMES[frame % FRAMES.len()])
                    .size(px(14.0))
                    .text_color(colors.secondary),
            )
            .into_any_element(),
        StatusState::NeedsInput { destructive } => slot
            .child(Icon::new(
                IconName::Warning,
                14.0,
                if destructive {
                    Ink::DANGER
                } else {
                    Ink::ATTENTION
                },
            ))
            .into_any_element(),
        StatusState::DoneUnseen => slot
            .child(Icon::new(IconName::Check, 14.0, Ink::FRESH))
            .into_any_element(),
        StatusState::IdleSeen | StatusState::None | StatusState::Hibernated => {
            slot.into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_activity_frame_is_embedded_in_the_app() {
        use gpui::AssetSource;
        for path in FRAMES {
            let bytes = diri_ui::IconAssets
                .load(path)
                .expect("load activity frame")
                .expect(path);
            assert!(bytes.starts_with(b"<svg"), "{path}");
        }
    }

    #[test]
    fn fleet_shares_eight_bounded_frames_and_reduce_motion_is_static() {
        for millis in [0.0, 124.0, 125.0, 999.0, 1000.0, 1750000000000.0] {
            let frames = [frame_at(millis, false); 30];
            assert!(frames.iter().all(|frame| *frame == frames[0] && *frame < 8));
            assert_eq!(frame_at(millis, true), 0);
        }
        assert_eq!(frame_at(124.0, false), 0);
        assert_eq!(frame_at(125.0, false), 1);
        assert_eq!(frame_at(1000.0, false), 0);
    }
}
