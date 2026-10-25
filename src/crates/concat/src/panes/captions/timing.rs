// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! Reading margins for generated speech captions, in playback seconds.
//! Imported SRT cues and script captions retain their existing timing.

const LEAD_SECONDS: f64 = 0.75;
const TRAIL_SECONDS: f64 = 0.75;
const MIN_SECONDS: f64 = 2.0;

/// Give each phrase reading time within the source clip, without overlaps.
/// Input ranges must be finite, nonempty, sorted by start and within `duration`.
/// Neighbours share the midpoint between their speech boundaries. Margins
/// shrink there; two seconds is a minimum only when the available slot fits it.
/// Overlapping recognition ranges are partitioned too, rather than stacking text.
pub(super) fn readable_ranges(ranges: &[(f64, f64)], duration: f64) -> Vec<(f64, f64)> {
    let mut out = Vec::with_capacity(ranges.len());
    let mut left = 0.0;
    for (index, &(speech_start, speech_end)) in ranges.iter().enumerate() {
        let right = ranges.get(index + 1).map_or(duration, |next| {
            ((speech_end + next.0) / 2.0).clamp(left, duration)
        });
        let mut start = (speech_start - LEAD_SECONDS).clamp(left, right);
        let mut end = (speech_end + TRAIL_SECONDS).clamp(start, right);
        let target = MIN_SECONDS.min(right - left);
        let missing = (target - (end - start)).max(0.0);
        start = (start - missing / 2.0).max(left);
        end = (start + target).max(end).min(right);
        start = (end - target).min(start).max(left);
        out.push((start, end));
        left = right;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::readable_ranges;

    fn close(actual: (f64, f64), expected: (f64, f64)) {
        assert!((actual.0 - expected.0).abs() < 1e-9, "{actual:?}");
        assert!((actual.1 - expected.1).abs() < 1e-9, "{actual:?}");
    }

    #[test]
    fn adds_three_quarters_of_a_second_at_each_end() {
        close(readable_ranges(&[(3.0, 5.0)], 10.0)[0], (2.25, 5.75));
    }

    #[test]
    fn holds_short_phrases_for_two_seconds() {
        close(readable_ranges(&[(3.0, 3.2)], 10.0)[0], (2.1, 4.1));
    }

    #[test]
    fn extends_away_from_clip_edges() {
        close(readable_ranges(&[(0.0, 0.1)], 10.0)[0], (0.0, 2.0));
        close(readable_ranges(&[(9.9, 10.0)], 10.0)[0], (8.0, 10.0));
        close(readable_ranges(&[(0.0, 0.1)], 1.0)[0], (0.0, 1.0));
    }

    #[test]
    fn shares_short_gaps_without_overlapping() {
        let ranges = readable_ranges(&[(2.0, 4.0), (4.4, 6.0)], 10.0);
        close(ranges[0], (1.25, 4.2));
        close(ranges[1], (4.2, 6.75));
    }

    #[test]
    fn dense_speech_takes_priority_over_minimum_duration() {
        let ranges = readable_ranges(&[(0.0, 0.2), (0.3, 0.5), (0.6, 0.8)], 1.0);
        assert_eq!(ranges.len(), 3);
        close(ranges[0], (0.0, 0.25));
        close(ranges[1], (0.25, 0.55));
        close(ranges[2], (0.55, 1.0));
    }

    #[test]
    fn keeps_gaps_when_full_margins_fit() {
        let ranges = readable_ranges(&[(2.0, 3.0), (8.0, 9.0)], 12.0);
        close(ranges[0], (1.25, 3.75));
        close(ranges[1], (7.25, 9.75));
    }

    #[test]
    fn overlapping_recognition_ranges_still_do_not_stack_captions() {
        let ranges = readable_ranges(&[(1.0, 8.0), (2.0, 3.0), (4.0, 5.0)], 10.0);
        for pair in ranges.windows(2) {
            assert!(pair[0].1 <= pair[1].0);
        }
        assert!(
            ranges
                .iter()
                .all(|&(start, end)| start >= 0.0 && end <= 10.0)
        );
    }

    #[test]
    fn no_speech_produces_no_captions() {
        assert!(readable_ranges(&[], 10.0).is_empty());
    }
}
