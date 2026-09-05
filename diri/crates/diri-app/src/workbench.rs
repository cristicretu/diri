//! Pure workbench layout state.
//!
//! GPUI owns pixels and pointer routing; this module owns only the durable
//! split intent. Keeping that seam free of views makes it straightforward to
//! add more pane kinds and a trailing inspector without teaching terminal
//! rendering about workbench policy.

pub const DEFAULT_PRIMARY_FRACTION: f32 = 0.62;
const MIN_PRIMARY_HEIGHT: f32 = 220.0;
const MIN_AUXILIARY_HEIGHT: f32 = 140.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneHeights {
    pub primary: f32,
    pub auxiliary: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorkbenchLayout {
    primary_fraction: f32,
}

impl Default for WorkbenchLayout {
    fn default() -> Self {
        Self {
            primary_fraction: DEFAULT_PRIMARY_FRACTION,
        }
    }
}

impl WorkbenchLayout {
    pub fn from_fraction(primary_fraction: f32) -> Self {
        let mut layout = Self::default();
        if primary_fraction.is_finite() {
            layout.primary_fraction = primary_fraction.clamp(0.0, 1.0);
        }
        layout
    }

    pub fn primary_fraction(&self) -> f32 {
        self.primary_fraction
    }

    pub fn pane_heights(&self, available_height: f32) -> PaneHeights {
        let available_height = available_height.max(0.0);
        if available_height <= MIN_PRIMARY_HEIGHT + MIN_AUXILIARY_HEIGHT {
            // Compress both minimums proportionally. At the threshold this
            // meets the normal clamp exactly, so a continuous window resize
            // cannot jump the divider (or trigger an extra terminal reflow).
            let primary =
                available_height * MIN_PRIMARY_HEIGHT / (MIN_PRIMARY_HEIGHT + MIN_AUXILIARY_HEIGHT);
            return PaneHeights {
                primary,
                auxiliary: available_height - primary,
            };
        }

        let primary = (available_height * self.primary_fraction)
            .clamp(MIN_PRIMARY_HEIGHT, available_height - MIN_AUXILIARY_HEIGHT);
        PaneHeights {
            primary,
            auxiliary: available_height - primary,
        }
    }

    pub fn resize_primary(&mut self, primary_height: f32, available_height: f32) {
        // Below the combined minimum, pane_heights compresses both panes and
        // the divider cannot move. Preserve the ratio restored on expansion.
        if available_height <= MIN_PRIMARY_HEIGHT + MIN_AUXILIARY_HEIGHT {
            return;
        }
        let clamped =
            primary_height.clamp(MIN_PRIMARY_HEIGHT, available_height - MIN_AUXILIARY_HEIGHT);
        self.primary_fraction = clamped / available_height;
    }

    pub fn reset(&mut self) {
        self.primary_fraction = DEFAULT_PRIMARY_FRACTION;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_split_favors_the_primary_pane() {
        let heights = WorkbenchLayout::default().pane_heights(600.0);
        assert_eq!(
            heights,
            PaneHeights {
                primary: 372.0,
                auxiliary: 228.0
            }
        );
    }

    #[test]
    fn drag_respects_both_minimums() {
        let mut layout = WorkbenchLayout::default();
        layout.resize_primary(590.0, 600.0);
        assert_eq!(layout.pane_heights(600.0).primary, 460.0);
        layout.resize_primary(10.0, 600.0);
        assert_eq!(layout.pane_heights(600.0).primary, 220.0);
    }

    #[test]
    fn reset_restores_the_default_ratio() {
        let mut layout = WorkbenchLayout::default();
        layout.resize_primary(400.0, 600.0);
        layout.reset();
        assert_eq!(layout.pane_heights(600.0).primary, 372.0);
    }

    #[test]
    fn resizing_window_has_no_jump_at_minimum_pane_heights() {
        for fraction in [0.0, 0.3, DEFAULT_PRIMARY_FRACTION, 0.9, 1.0] {
            let layout = WorkbenchLayout::from_fraction(fraction);
            let mut previous = layout.pane_heights(359.0);
            for step in 1..=200 {
                let height = 359.0 + step as f32 * 0.01;
                let next = layout.pane_heights(height);
                assert!(
                    (next.primary - previous.primary).abs() <= 0.011,
                    "split jumped from {previous:?} to {next:?} at height {height}"
                );
                assert!((next.primary + next.auxiliary - height).abs() < 0.001);
                previous = next;
            }
        }
    }

    #[test]
    fn dragging_a_fully_compressed_split_preserves_the_restored_ratio() {
        let mut layout = WorkbenchLayout::from_fraction(0.8);
        let previous = layout;
        for pointer in [0.0, 100.0, 250.0, 500.0] {
            layout.resize_primary(pointer, 320.0);
            assert_eq!(
                layout, previous,
                "an immovable divider must not change hidden layout intent"
            );
        }
    }
}
