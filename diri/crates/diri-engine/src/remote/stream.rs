//! Pure reconciliation for offset-addressed remote output frames.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutputFrameAction {
    Feed,
    FeedSuffix { drop_leading: usize },
    Skip,
    Gap { from: u64, up_to: u64 },
}

pub(crate) fn reconcile_output_frame(
    held: u64,
    frame_offset: u64,
    frame_length: usize,
) -> OutputFrameAction {
    if frame_offset == held {
        return OutputFrameAction::Feed;
    }
    if frame_offset > held {
        return OutputFrameAction::Gap {
            from: held,
            up_to: frame_offset,
        };
    }
    let frame_end = frame_offset.saturating_add(frame_length as u64);
    if frame_end <= held {
        return OutputFrameAction::Skip;
    }
    OutputFrameAction::FeedSuffix {
        drop_leading: held.saturating_sub(frame_offset) as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_duplicate_overlap_and_gap_are_distinct() {
        assert_eq!(reconcile_output_frame(10, 10, 5), OutputFrameAction::Feed);
        assert_eq!(reconcile_output_frame(10, 0, 10), OutputFrameAction::Skip);
        assert_eq!(
            reconcile_output_frame(10, 8, 5),
            OutputFrameAction::FeedSuffix { drop_leading: 2 }
        );
        assert_eq!(
            reconcile_output_frame(10, 14, 5),
            OutputFrameAction::Gap {
                from: 10,
                up_to: 14
            }
        );
    }
}
