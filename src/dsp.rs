//! Honest gain normalisation and Dynamic Boost presets.
//!
//! This is deliberately not EBU R128 loudness normalisation and not
//! ReplayGain. It is a fixed, predictable chain:
//!
//! 1. an optional slow RMS automatic gain control that brings a station toward
//!    −18 dBFS, with a gate so silence is not amplified;
//! 2. a per-station dB trim set by the user;
//! 3. a peak limiter at −1 dBFS with a hard clamp.
//!
//! The chain runs in the decoder worker, after decoding and before the sample
//! ring, so the audio callback only ever pops samples. The recording tap is on
//! the encoded bytes upstream, so recordings are not affected by gain or boost.

/// Peak ceiling of the limiter.
pub const CEILING_DB: f32 = -1.0;
/// RMS target of the automatic gain control.
pub const TARGET_RMS_DB: f32 = -18.0;
/// Below this RMS the AGC freezes, so silence and noise are not pumped up.
pub const GATE_DB: f32 = -60.0;
/// Maximum attenuation the AGC may apply.
pub const MAX_CUT_DB: f32 = -18.0;
/// The per-station trim is clamped to this range.
pub const MAX_TRIM_DB: f32 = 12.0;

/// Fixed Dynamic Boost presets. The UI exposes only these four choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DspPreset {
    Off,
    Light,
    Medium,
    Strong,
}

impl DspPreset {
    pub const ALL: [DspPreset; 4] = [
        DspPreset::Off,
        DspPreset::Light,
        DspPreset::Medium,
        DspPreset::Strong,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DspPreset::Off => "off",
            DspPreset::Light => "light",
            DspPreset::Medium => "medium",
            DspPreset::Strong => "strong",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(DspPreset::Off),
            "light" => Some(DspPreset::Light),
            "medium" => Some(DspPreset::Medium),
            "strong" => Some(DspPreset::Strong),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DspPreset::Off => crate::i18n::t("dsp.off"),
            DspPreset::Light => crate::i18n::t("dsp.light"),
            DspPreset::Medium => crate::i18n::t("dsp.medium"),
            DspPreset::Strong => crate::i18n::t("dsp.strong"),
        }
    }

    /// Maximum AGC boost. `Off` disables the AGC entirely.
    pub fn max_boost_db(self) -> f32 {
        match self {
            DspPreset::Off => 0.0,
            DspPreset::Light => 6.0,
            DspPreset::Medium => 12.0,
            DspPreset::Strong => 18.0,
        }
    }

    pub fn enabled(self) -> bool {
        self != DspPreset::Off
    }
}

impl Default for DspPreset {
    fn default() -> Self {
        DspPreset::Off
    }
}

/// The per-stream DSP chain. One instance per decoder attempt.
pub struct Dsp {
    preset: DspPreset,
    trim_db: f32,
    /// Smoothed linear trim, ramped over ~20 ms so a slider step cannot click.
    trim_gain: f32,
    trim_coeff: f32,
    agc: Agc,
    limiter: Limiter,
}

impl Dsp {
    pub fn new(
        preset: DspPreset,
        trim_db: f32,
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        // `process` runs once per interleaved sample, so the update rate is the
        // per-channel rate times the channel count. Using the per-channel rate
        // would halve every time constant on stereo.
        let update_rate = sample_rate
            .max(1)
            .saturating_mul(u32::from(channels.max(1)));
        let mut dsp = Self {
            preset,
            trim_db: trim_db.clamp(-MAX_TRIM_DB, MAX_TRIM_DB),
            trim_gain: db_to_linear(trim_db.clamp(-MAX_TRIM_DB, MAX_TRIM_DB)),
            trim_coeff: 1.0 - (-1.0 / (0.02 * update_rate as f32)).exp(),
            agc: Agc::new(update_rate),
            limiter: Limiter::new(update_rate),
        };
        dsp.reset();
        dsp
    }

    /// Change the preset. The AGC and limiter restart from unity so a preset
    /// change cannot inherit a stale envelope.
    pub fn set_preset(&mut self, preset: DspPreset) {
        self.preset = preset;
        self.reset();
    }

    pub fn preset(&self) -> DspPreset {
        self.preset
    }

    /// Set the trim target; `process` ramps the applied gain toward it.
    pub fn set_trim_db(&mut self, trim_db: f32) {
        self.trim_db = trim_db.clamp(-MAX_TRIM_DB, MAX_TRIM_DB);
    }

    pub fn trim_db(&self) -> f32 {
        self.trim_db
    }

    /// Process interleaved samples in place. `Off` with a 0 dB trim is a
    /// bit-exact passthrough; NaN/Inf samples become silence.
    pub fn process(&mut self, samples: &mut [f32]) {
        if self.preset == DspPreset::Off && self.trim_db == 0.0 {
            // Finite samples are bit-exact; non-finite samples are still made
            // safe, because the sink must never receive NaN or Inf.
            for sample in samples.iter_mut() {
                if !sample.is_finite() {
                    *sample = 0.0;
                }
            }
            return;
        }
        let target_trim = db_to_linear(self.trim_db);
        let max_boost = db_to_linear(self.preset.max_boost_db());
        for sample in samples.iter_mut() {
            if !sample.is_finite() {
                *sample = 0.0;
                continue;
            }
            self.trim_gain += self.trim_coeff * (target_trim - self.trim_gain);
            let mut value = *sample;
            if self.preset.enabled() {
                value = self.agc.process(value, max_boost);
            }
            value *= self.trim_gain;
            value = self.limiter.process(value);
            *sample = value.clamp(-self.limiter.ceiling, self.limiter.ceiling);
        }
    }

    /// Current AGC gain in dB, for tests.
    #[cfg(test)]
    pub fn agc_gain_db(&self) -> f32 {
        self.agc.gain_db()
    }

    fn reset(&mut self) {
        self.trim_gain = db_to_linear(self.trim_db);
        self.agc.reset();
        self.limiter.reset();
    }
}

/// Slow RMS AGC with attack/release smoothing and a noise gate.
struct Agc {
    rms: f32,
    gain: f32,
    rms_coeff: f32,
    attack_coeff: f32,
    release_coeff: f32,
    gate: f32,
    target: f32,
    max_cut: f32,
}

impl Agc {
    fn new(update_rate: u32) -> Self {
        let rate = update_rate.max(1) as f32;
        // One-pole RMS over roughly 400 ms.
        let rms_coeff = 1.0 - (-1.0 / (0.4 * rate)).exp();
        let attack_coeff = 1.0 - (-1.0 / (1.0 * rate)).exp();
        let release_coeff = 1.0 - (-1.0 / (4.0 * rate)).exp();
        Self {
            rms: 0.0,
            gain: 1.0,
            rms_coeff,
            attack_coeff,
            release_coeff,
            gate: db_to_linear(GATE_DB),
            target: db_to_linear(TARGET_RMS_DB),
            max_cut: db_to_linear(MAX_CUT_DB),
        }
    }

    fn process(&mut self, sample: f32, max_boost: f32) -> f32 {
        let square = sample * sample;
        self.rms += self.rms_coeff * (square - self.rms);
        let rms = self.rms.sqrt();
        if rms < self.gate {
            // Relax toward unity while gated, so a boost earned during a quiet
            // passage cannot blast the next programme after a silence.
            self.gain += self.release_coeff * (1.0 - self.gain);
            return sample * self.gain;
        }
        let desired = (self.target / rms).clamp(self.max_cut, max_boost.max(self.max_cut));
        let coefficient = if desired < self.gain {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.gain += coefficient * (desired - self.gain);
        sample * self.gain
    }

    fn reset(&mut self) {
        self.rms = 0.0;
        self.gain = 1.0;
    }

    #[cfg(test)]
    fn gain_db(&self) -> f32 {
        20.0 * self.gain.max(1e-9).log10()
    }
}

/// Instant-attack, slow-release peak limiter.
struct Limiter {
    ceiling: f32,
    release_coeff: f32,
    gain: f32,
}

impl Limiter {
    fn new(update_rate: u32) -> Self {
        let rate = update_rate.max(1) as f32;
        Self {
            ceiling: db_to_linear(CEILING_DB),
            release_coeff: 1.0 - (-1.0 / (0.1 * rate)).exp(),
            gain: 1.0,
        }
    }

    fn process(&mut self, sample: f32) -> f32 {
        let peak = sample.abs();
        let desired = if peak > self.ceiling {
            self.ceiling / peak
        } else {
            1.0
        };
        if desired < self.gain {
            self.gain = desired;
        } else {
            self.gain += self.release_coeff * (1.0 - self.gain);
        }
        sample * self.gain
    }

    fn reset(&mut self) {
        self.gain = 1.0;
    }
}

pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn sine(amplitude: f32, seconds: f32) -> Vec<f32> {
        let count = (RATE as f32 * seconds) as usize;
        (0..count)
            .map(|index| {
                amplitude * (2.0 * std::f32::consts::PI * 440.0 * index as f32 / RATE as f32).sin()
            })
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        let sum: f32 = samples.iter().map(|sample| sample * sample).sum();
        (sum / samples.len().max(1) as f32).sqrt()
    }

    #[test]
    fn off_with_zero_trim_is_a_bit_exact_passthrough() {
        let mut samples = sine(0.5, 0.25);
        let original = samples.clone();
        let mut dsp = Dsp::new(DspPreset::Off, 0.0, RATE, 1);
        dsp.process(&mut samples);
        assert_eq!(samples, original);
    }

    #[test]
    fn sine_converges_toward_the_target() {
        let mut samples = sine(db_to_linear(-30.0) * std::f32::consts::SQRT_2, 12.0);
        let mut dsp = Dsp::new(DspPreset::Strong, 0.0, RATE, 1);
        dsp.process(&mut samples);
        let tail = &samples[samples.len() - RATE as usize..];
        let level_db = 20.0 * rms(tail).log10();
        assert!(
            (level_db - TARGET_RMS_DB).abs() <= 1.5,
            "tail was {level_db:.2} dBFS, expected {TARGET_RMS_DB} ±1.5"
        );
    }

    #[test]
    fn a_loud_transient_never_exceeds_the_ceiling() {
        let mut samples = vec![0.0f32; RATE as usize];
        // A full-scale step with a +12 dB trim, before the slow AGC can react.
        for sample in &mut samples {
            *sample = 1.0;
        }
        let mut dsp = Dsp::new(DspPreset::Light, 12.0, RATE, 1);
        dsp.process(&mut samples);
        let peak = samples.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        assert!(
            peak <= db_to_linear(CEILING_DB) + 1e-4,
            "peak {peak} exceeded the ceiling"
        );
    }

    #[test]
    fn silence_stays_silent() {
        let mut samples = vec![0.0f32; 4096];
        let mut dsp = Dsp::new(DspPreset::Strong, 12.0, RATE, 1);
        dsp.process(&mut samples);
        assert!(samples.iter().all(|sample| *sample == 0.0));
        assert_eq!(dsp.agc_gain_db(), 0.0);
    }

    #[test]
    fn stereo_sine_converges_toward_the_target() {
        let mono = sine(db_to_linear(-30.0) * std::f32::consts::SQRT_2, 12.0);
        let mut stereo = Vec::with_capacity(mono.len() * 2);
        for sample in &mono {
            stereo.push(*sample);
            stereo.push(*sample);
        }
        let mut dsp = Dsp::new(DspPreset::Strong, 0.0, RATE, 2);
        dsp.process(&mut stereo);
        let tail = &stereo[stereo.len() - (RATE as usize * 2)..];
        let level_db = 20.0 * rms(tail).log10();
        assert!(
            (level_db - TARGET_RMS_DB).abs() <= 1.5,
            "stereo tail was {level_db:.2} dBFS, expected {TARGET_RMS_DB} ±1.5"
        );
    }

    #[test]
    fn the_gate_relaxes_a_stale_boost() {
        let mut dsp = Dsp::new(DspPreset::Strong, 0.0, RATE, 1);
        let mut quiet = sine(db_to_linear(-50.0) * std::f32::consts::SQRT_2, 10.0);
        dsp.process(&mut quiet);
        assert!(
            dsp.agc_gain_db() > 3.0,
            "the quiet passage should have earned a boost"
        );
        let mut silence = vec![0.0f32; RATE as usize * 20];
        dsp.process(&mut silence);
        assert!(
            dsp.agc_gain_db() < 1.0,
            "the gate should have relaxed the boost, got {} dB",
            dsp.agc_gain_db()
        );
    }

    #[test]
    fn off_passthrough_still_sanitises_non_finite() {
        let mut samples = vec![f32::NAN, 0.5, f32::INFINITY];
        let mut dsp = Dsp::new(DspPreset::Off, 0.0, RATE, 1);
        dsp.process(&mut samples);
        assert_eq!(samples, vec![0.0, 0.5, 0.0]);
    }

    #[test]
    fn boost_is_capped_by_the_preset() {
        let mut samples = sine(db_to_linear(-50.0) * std::f32::consts::SQRT_2, 20.0);
        let mut dsp = Dsp::new(DspPreset::Light, 0.0, RATE, 2);
        dsp.process(&mut samples);
        assert!(
            dsp.agc_gain_db() <= DspPreset::Light.max_boost_db() + 0.1,
            "AGC gain {} exceeded the Light cap",
            dsp.agc_gain_db()
        );
    }

    #[test]
    fn processing_is_chunk_size_invariant() {
        let source = sine(0.2, 0.5);
        let mut whole = source.clone();
        let mut dsp = Dsp::new(DspPreset::Medium, 3.0, RATE, 1);
        dsp.process(&mut whole);

        let mut piecewise = source;
        let mut dsp = Dsp::new(DspPreset::Medium, 3.0, RATE, 1);
        for chunk in piecewise.chunks_mut(1) {
            dsp.process(chunk);
        }
        for (left, right) in whole.iter().zip(piecewise.iter()) {
            assert!(
                (left - right).abs() < 1e-6,
                "chunking changed the output: {left} vs {right}"
            );
        }
    }

    #[test]
    fn non_finite_samples_become_silence() {
        let mut samples = vec![f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.5, -0.5];
        let mut dsp = Dsp::new(DspPreset::Strong, 6.0, RATE, 2);
        dsp.process(&mut samples);
        assert!(samples.iter().all(|sample| sample.is_finite()));
        assert!(samples
            .iter()
            .all(|sample| sample.abs() <= db_to_linear(CEILING_DB) + 1e-4));
        assert_eq!(samples[0], 0.0);
        assert_eq!(samples[1], 0.0);
        assert_eq!(samples[2], 0.0);
    }

    #[test]
    fn preset_round_trips_through_text() {
        for preset in DspPreset::ALL {
            assert_eq!(DspPreset::parse(preset.as_str()), Some(preset));
        }
        assert_eq!(DspPreset::parse("loud"), None);
    }

    #[test]
    fn trim_is_clamped() {
        let dsp = Dsp::new(DspPreset::Off, 99.0, RATE, 1);
        assert_eq!(dsp.trim_db(), MAX_TRIM_DB);
        let dsp = Dsp::new(DspPreset::Off, -99.0, RATE, 1);
        assert_eq!(dsp.trim_db(), -MAX_TRIM_DB);
    }
}
