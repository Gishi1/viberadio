//! Live level windows for finding the end of a song.
//!
//! A duration lookup can be wrong about the station's edit, and an ad break
//! often starts without a new ICY title. The decoder reports a 0.5-second RMS
//! window, so the recording can close on the fade, silence or hard drop that
//! marks the real end.

/// One 0.5-second RMS window, measured from the current title's start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelSample {
    pub rms: f32,
    pub at: f32,
}

const SILENCE_SAMPLES: usize = 4;
const FADE_SAMPLES: usize = 5;
const MEDIAN_SAMPLES: usize = 40;

/// Whether the tail of `samples` looks like the end of the song.
///
/// The detector waits at least 45 seconds, or 70% of a known canonical length,
/// so an intro or a quiet bridge cannot close the file. It then accepts a
/// sustained silence, a gradual fade that ends well below the song's level, or
/// a hard drop held for a second.
pub fn song_end(samples: &[LevelSample], expected_seconds: Option<u32>) -> bool {
    let Some(last) = samples.last() else {
        return false;
    };
    let min_elapsed = expected_seconds
        .filter(|seconds| *seconds > 0)
        .map(|seconds| (seconds as f32 * 0.7).max(45.0))
        .unwrap_or(45.0);
    if last.at < min_elapsed {
        return false;
    }
    let start = samples.len().saturating_sub(MEDIAN_SAMPLES);
    let mut sorted: Vec<f32> = samples[start..].iter().map(|sample| sample.rms).collect();
    sorted.sort_by(|left, right| {
        left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
    });
    let median = sorted[sorted.len() / 2].max(0.005);
    let low = median * 0.2;
    if samples.len() >= SILENCE_SAMPLES {
        let tail = &samples[samples.len() - SILENCE_SAMPLES..];
        if tail.iter().all(|sample| sample.rms < low) {
            return true;
        }
    }
    if samples.len() >= FADE_SAMPLES {
        let recent = &samples[samples.len() - FADE_SAMPLES..];
        let mut declining = true;
        for pair in recent.windows(2) {
            if pair[1].rms > pair[0].rms * 1.05 {
                declining = false;
                break;
            }
        }
        let first = recent[0].rms.max(1e-6);
        let end = recent[recent.len() - 1].rms.max(1e-6);
        let drop_db = 20.0 * (first / end).log10();
        if declining && drop_db >= 8.0 && end < median * 0.35 {
            return true;
        }
    }
    if samples.len() >= 3 {
        let recent = &samples[samples.len() - 3..];
        let before = recent[0].rms.max(1e-6);
        let now = recent[2].rms.max(1e-6);
        if 20.0 * (before / now).log10() >= 10.0
            && recent[1].rms < low
            && now < low
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(values: &[(f32, f32)]) -> Vec<LevelSample> {
        values
            .iter()
            .map(|(at, rms)| LevelSample { rms: *rms, at: *at })
            .collect()
    }

    #[test]
    fn a_sustained_silence_ends_the_song() {
        let mut values = vec![(0.0, 0.2); 120];
        for index in 0..4 {
            values.push((60.0 + index as f32 * 0.5, 0.001));
        }
        assert!(song_end(&samples(&values), Some(80)));
    }

    #[test]
    fn a_gradual_fade_ends_the_song() {
        let mut values = vec![(0.0, 0.2); 120];
        for (index, rms) in [0.18, 0.12, 0.07, 0.03, 0.01].into_iter().enumerate() {
            values.push((60.0 + index as f32 * 0.5, rms));
        }
        assert!(song_end(&samples(&values), Some(80)));
    }

    #[test]
    fn a_hard_drop_held_for_a_second_ends_the_song() {
        let mut values = vec![(0.0, 0.2); 120];
        values.push((60.0, 0.2));
        values.push((60.5, 0.001));
        values.push((61.0, 0.001));
        assert!(song_end(&samples(&values), Some(80)));
    }

    #[test]
    fn a_quiet_bridge_is_not_the_end() {
        let mut values = vec![(0.0, 0.2); 120];
        for index in 0..4 {
            values.push((60.0 + index as f32 * 0.5, 0.08));
        }
        for index in 0..4 {
            values.push((62.0 + index as f32 * 0.5, 0.2));
        }
        assert!(!song_end(&samples(&values), Some(80)));
    }

    #[test]
    fn an_early_drop_is_ignored() {
        let mut values = vec![(0.0, 0.2); 40];
        for index in 0..4 {
            values.push((20.0 + index as f32 * 0.5, 0.001));
        }
        assert!(!song_end(&samples(&values), Some(200)));
    }

    #[test]
    fn a_steady_song_is_not_the_end() {
        let values: Vec<(f32, f32)> = (0..240)
            .map(|index| (index as f32 * 0.5, 0.2))
            .collect();
        assert!(!song_end(&samples(&values), Some(80)));
    }

    #[test]
    fn an_unknown_length_still_waits_for_the_minimum() {
        let mut values = vec![(0.0, 0.2); 40];
        for index in 0..4 {
            values.push((20.0 + index as f32 * 0.5, 0.001));
        }
        assert!(!song_end(&samples(&values), None));
        let mut values = vec![(0.0, 0.2); 120];
        for index in 0..4 {
            values.push((60.0 + index as f32 * 0.5, 0.001));
        }
        assert!(song_end(&samples(&values), None));
    }
}
