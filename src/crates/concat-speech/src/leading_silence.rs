// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! Optional leading-quiet detection on the decoded 16 kHz mono window.
//! This measures energy, not speech: music and quiet words remain ambiguous.

const FRAME: usize = 320; // 20 ms at 16 kHz
const PREROLL: usize = 4_000; // Keep 250 ms before sustained activity.
const MIN_SKIP: usize = 4_800; // Avoid changing near-immediate speech.
const PEAK_RATIO: f64 = 0.08;

/// Samples to omit from Whisper's input, never from the source media.
/// Ambiguous/invalid input keeps the complete window. No extra model or
/// architecture-specific instructions are needed.
pub(crate) fn leading_samples(samples: &[f32]) -> usize {
    if samples.iter().any(|value| !value.is_finite()) {
        return 0;
    }
    let mut rms = Vec::with_capacity(samples.len() / FRAME);
    for frame in samples.chunks_exact(FRAME) {
        let energy = frame
            .iter()
            .map(|&value| f64::from(value).powi(2))
            .sum::<f64>();
        rms.push((energy / FRAME as f64).sqrt());
    }
    let peak = rms.iter().copied().fold(0.0_f64, f64::max);
    if peak == 0.0 {
        return 0;
    }
    let threshold = PEAK_RATIO * peak;
    let Some(first) = rms.windows(5).position(|frames| {
        frames[0] > threshold && frames.iter().filter(|&&rms| rms > threshold).count() >= 4
    }) else {
        return 0;
    };
    let skip = (first * FRAME).saturating_sub(PREROLL);
    if skip < MIN_SKIP {
        0
    } else {
        skip
    }
}

/// Restore the clock of the complete decoded window, before the caller maps
/// source time onto the timeline (including source_start and playback speed).
pub(crate) fn original_seconds(seconds: f64, skipped: usize) -> f64 {
    seconds + skipped as f64 / 16_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(quiet_frames: usize, active_frames: usize) -> Vec<f32> {
        let mut samples = vec![0.0; quiet_frames * FRAME];
        samples.extend(vec![0.4; active_frames * FRAME]);
        samples
    }

    #[test]
    fn keeps_a_quarter_second_before_sustained_activity() {
        assert_eq!(leading_samples(&audio(213, 100)), 64_160);
    }

    #[test]
    fn keeps_silence_short_audio_and_immediate_speech() {
        assert_eq!(leading_samples(&[]), 0);
        assert_eq!(leading_samples(&vec![0.0; 32_000]), 0);
        assert_eq!(leading_samples(&audio(0, 100)), 0);
        assert_eq!(leading_samples(&audio(10, 100)), 0);
        assert_eq!(leading_samples(&audio(100, 4)), 0);
    }

    #[test]
    fn isolated_click_does_not_start_the_window() {
        let mut samples = audio(100, 100);
        samples[10 * FRAME..11 * FRAME].fill(0.4);
        assert_eq!(leading_samples(&samples), 100 * FRAME - PREROLL);
    }

    #[test]
    fn sustained_quiet_voice_is_relative_to_its_own_peak() {
        let mut samples = audio(100, 100);
        for value in &mut samples {
            *value *= 0.0001;
        }
        assert_eq!(leading_samples(&samples), 100 * FRAME - PREROLL);
    }

    #[test]
    fn invalid_audio_is_not_trimmed() {
        let mut samples = audio(100, 100);
        samples[0] = f32::NAN;
        assert_eq!(leading_samples(&samples), 0);
    }

    #[test]
    fn offset_precedes_timeline_speed_conversion_and_preserves_duration() {
        let start = original_seconds(8.5, 64_160);
        let end = original_seconds(10.5, 64_160);
        assert!((start - 12.51).abs() < 1e-9);
        assert!((end - start - 2.0).abs() < 1e-9);
        // The caller adds timeline position after dividing by clip speed.
        assert!((100.0 + start / 2.0 - 106.255).abs() < 1e-9);
        assert_eq!(original_seconds(8.5, 0), 8.5);
    }
}
