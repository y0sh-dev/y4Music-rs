//! Structured knobs for the ffmpeg `-af` filtergraphs used by Balanced and
//! Hi-Fi playback (see `crate::audio_source`). Each field controls exactly
//! one filter parameter; `EqProfile::render` assembles them into the final
//! filter string.

/// One `anequalizer` parametric band, applied identically to both channels.
#[derive(Clone, Copy, Debug)]
pub struct EqBand {
    pub freq_hz: f64,
    pub width_hz: f64,
    pub gain_db: f64,
}

impl EqBand {
    fn render(self) -> String {
        format!(
            "c0 f={f} w={w} g={g}|c1 f={f} w={w} g={g}",
            f = self.freq_hz,
            w = self.width_hz,
            g = self.gain_db
        )
    }
}

/// `asubboost`: reinforces low end below `cutoff_hz`, fed back at `feedback`.
#[derive(Clone, Copy, Debug)]
pub struct SubBoost {
    pub cutoff_hz: f64,
    pub feedback: f64,
}

/// `extrastereo`: widens the stereo image by factor `m` (1.0 = unchanged).
#[derive(Clone, Copy, Debug)]
pub struct StereoWidth {
    pub m: f64,
}

/// `aecho`: a single echo/ambience tap.
#[derive(Clone, Copy, Debug)]
pub struct Echo {
    pub in_gain: f64,
    pub out_gain: f64,
    pub delay_ms: f64,
    pub decay: f64,
}

/// `compand`: dynamic range compression. `points` is a piecewise
/// input/output dB transfer curve. `soft_knee` rounds the transition around
/// each breakpoint instead of bending sharply (`compand`'s `soft-knee`
/// parameter); `delay_s` holds the signal briefly so the level detector can
/// look ahead of the audio it's about to gain-adjust, reducing the
/// audible "pumping" a hard, zero-lookahead knee produces on transients.
#[derive(Clone, Debug)]
pub struct Compand {
    pub attack_s: f64,
    pub decay_s: f64,
    pub points: Vec<(f64, f64)>,
    pub soft_knee: f64,
    pub delay_s: f64,
}

/// `loudnorm`: EBU R128 loudness normalization target.
#[derive(Clone, Copy, Debug)]
pub struct Loudnorm {
    pub integrated_lufs: f64,
    pub range_lu: f64,
    pub true_peak_dbtp: f64,
}

/// A full playback EQ chain: resample/dither precision, optional equalizer
/// bands and spatial effects, an optional lowpass to shed inaudible
/// high-frequency energy, then compand + loudnorm.
#[derive(Clone, Debug)]
pub struct EqProfile {
    pub bands: Vec<EqBand>,
    pub sub_boost: Option<SubBoost>,
    pub stereo_width: Option<StereoWidth>,
    pub echo: Option<Echo>,
    /// Cutoff for an `lowpass` filter inserted right after the EQ bands,
    /// e.g. `Some(16000.0)` = `lowpass=f=16000`. `None` skips it entirely.
    /// Sheds high-frequency content most listeners can't hear anyway, which
    /// otherwise inflates the Opus encoder's bitrate demand on
    /// high-entropy/high-BPM material (e.g. Nightcore).
    pub lowpass_hz: Option<f64>,
    pub compand: Compand,
    pub loudnorm: Loudnorm,
}

impl EqProfile {
    /// Renders this profile to an ffmpeg `-af` filtergraph string, in
    /// signal-flow order: resample -> EQ bands -> lowpass -> compand ->
    /// loudnorm. No pre-gain stage: with 32-bit float internal processing
    /// there's no fixed-point headroom to protect, so trimming gain before
    /// the EQ bands bought nothing but a level mismatch for the operator to
    /// account for -- it's gone.
    ///
    /// `aresample` is pinned to a fixed high-precision/anti-alias setting
    /// (`precision=28:cutoff=0.97`) for both profiles, per the DSP audit's
    /// resample recommendation -- no per-profile precision knob remains.
    pub fn render(&self) -> String {
        let mut parts = vec!["aresample=48000:resampler=swr:precision=28:cutoff=0.97".to_string()];

        if !self.bands.is_empty() {
            let bands = self
                .bands
                .iter()
                .map(|b| b.render())
                .collect::<Vec<_>>()
                .join("|");
            parts.push(format!("anequalizer={bands}"));
        }
        // Right after the EQ bands: sheds whatever high-frequency energy
        // they didn't already remove, before any spatial effects below.
        if let Some(hz) = self.lowpass_hz {
            parts.push(format!("lowpass=f={hz}"));
        }
        if let Some(s) = self.sub_boost {
            parts.push(format!(
                "asubboost=cutoff={}:feedback={}",
                s.cutoff_hz, s.feedback
            ));
        }
        if let Some(s) = self.stereo_width {
            parts.push(format!("extrastereo=m={}", s.m));
        }
        if let Some(e) = self.echo {
            parts.push(format!(
                "aecho={}:{}:{}:{}",
                e.in_gain, e.out_gain, e.delay_ms, e.decay
            ));
        }

        let points = self
            .compand
            .points
            .iter()
            .map(|(i, o)| format!("{i}/{o}"))
            .collect::<Vec<_>>()
            .join("|");
        parts.push(format!(
            "compand=attacks={}:decays={}:points={points}:soft-knee={}:delay={}",
            self.compand.attack_s,
            self.compand.decay_s,
            self.compand.soft_knee,
            self.compand.delay_s
        ));

        parts.push(format!(
            "loudnorm=I={}:LRA={}:TP={}",
            self.loudnorm.integrated_lufs, self.loudnorm.range_lu, self.loudnorm.true_peak_dbtp
        ));

        parts.join(",")
    }
}

/// Balanced mode: a mild bass lift plus gentle normalization. Hardcoded,
/// not operator-configurable.
pub fn balanced_profile() -> EqProfile {
    EqProfile {
        bands: vec![EqBand {
            freq_hz: 60.0,
            width_hz: 15.0,
            gain_db: 1.5,
        }],
        sub_boost: None,
        stereo_width: None,
        echo: None,
        lowpass_hz: None,
        compand: Compand {
            attack_s: 0.02,
            decay_s: 0.1,
            points: vec![(-80.0, -80.0), (-35.0, -35.0), (0.0, -5.0)],
            soft_knee: 0.01,
            delay_s: 0.0,
        },
        loudnorm: Loudnorm {
            integrated_lufs: -16.0,
            range_lu: 10.0,
            true_peak_dbtp: -2.0,
        },
    }
}

/// Hi-Fi mode's built-in default, overridable at startup via
/// `EQ_HIFI_FILTER` (an arbitrary raw `-af` string, bypassing this profile
/// entirely).
///
/// A 9-band proportional-Q parametric EQ (Q ≈ 1.2-1.5, i.e. `width_hz` is
/// sized relative to `freq_hz` at every band rather than using a handful of
/// fixed absolute widths). The previous 6-band design used disproportionately
/// wide high-band widths relative to their center frequency, which -- summed
/// with the neighboring bands' skirts in `anequalizer`'s per-band biquads --
/// produced comb-filter-like ripple in the upper register. Proportional-Q
/// sizing keeps each band's skirt in scale with its own center frequency, so
/// adjacent bands overlap consistently across the spectrum instead of
/// clashing more in some registers than others.
///
/// `sub_boost`/`stereo_width`/`echo` are all dropped -- these spatial
/// effects added CPU cost and clipping risk without a clearly audible
/// benefit once re-encoded through Discord's Opus pipeline.
///
/// There is no pre-gain stage (see `EqProfile::render`): 32-bit float
/// processing has no fixed-point headroom to protect, so trimming gain
/// before the EQ bands only shifted the compressor's effective input level
/// without adding any real protection -- exactly the "pre-gain throws off
/// the compander" mismatch the DSP audit flagged. `compand` compensates
/// with a tighter `attack_s`/`decay_s`, a soft knee (`soft_knee`) instead of
/// a hard bend at each breakpoint, and a short `delay_s` lookahead so the
/// level detector sees a transient slightly before the gain stage acts on
/// it, rather than reacting after the fact -- both reduce audible pumping
/// compared to the previous zero-lookahead, hard-knee curve.
///
/// `lowpass_hz: Some(18_000.0)` is Hi-Fi-specific: high-BPM/high-entropy
/// material (Nightcore and similar) packs far more energy above 18kHz than
/// Balanced's source material typically does, and that inaudible-to-nearly-
/// everyone content was inflating the Opus encoder's bitrate demand enough
/// to contribute to packet loss -- stutter -- under Discord's bandwidth cap
/// (see `commands::playback::resolve_target_bitrate`). Cutting it above the
/// EQ bands sheds that load without touching anything a listener would
/// notice.
pub fn default_hifi_profile() -> EqProfile {
    EqProfile {
        bands: vec![
            EqBand {
                freq_hz: 60.0,
                width_hz: 50.0,
                gain_db: 0.8,
            },
            EqBand {
                freq_hz: 120.0,
                width_hz: 100.0,
                gain_db: 0.4,
            },
            EqBand {
                freq_hz: 250.0,
                width_hz: 180.0,
                gain_db: -0.5,
            },
            EqBand {
                freq_hz: 500.0,
                width_hz: 350.0,
                gain_db: 0.3,
            },
            EqBand {
                freq_hz: 2_000.0,
                width_hz: 1_400.0,
                gain_db: 0.2,
            },
            EqBand {
                freq_hz: 5_500.0,
                width_hz: 3_500.0,
                gain_db: -0.4,
            },
            EqBand {
                freq_hz: 8_000.0,
                width_hz: 5_500.0,
                gain_db: 0.4,
            },
            EqBand {
                freq_hz: 11_000.0,
                width_hz: 7_500.0,
                gain_db: 0.3,
            },
            EqBand {
                freq_hz: 14_000.0,
                width_hz: 9_500.0,
                gain_db: 0.2,
            },
        ],
        sub_boost: None,
        stereo_width: None,
        echo: None,
        // Cuts inaudible (to nearly everyone) high-frequency content that
        // otherwise inflates the Opus encoder's bitrate demand on
        // high-entropy/high-BPM material (e.g. Nightcore).
        lowpass_hz: Some(18_000.0),
        compand: Compand {
            attack_s: 0.01,
            decay_s: 0.15,
            points: vec![(-80.0, -80.0), (-18.0, -18.0), (0.0, -2.5)],
            soft_knee: 6.0,
            delay_s: 0.01,
        },
        loudnorm: Loudnorm {
            integrated_lufs: -16.0,
            range_lu: 11.0,
            true_peak_dbtp: -1.5,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_renders_expected_filtergraph() {
        assert_eq!(
            balanced_profile().render(),
            "aresample=48000:resampler=swr:precision=28:cutoff=0.97,\
anequalizer=c0 f=60 w=15 g=1.5|c1 f=60 w=15 g=1.5,\
compand=attacks=0.02:decays=0.1:points=-80/-80|-35/-35|0/-5:soft-knee=0.01:delay=0,\
loudnorm=I=-16:LRA=10:TP=-2"
        );
    }

    #[test]
    fn hifi_default_is_a_nine_band_proportional_q_eq_without_spatial_effects() {
        let filter = default_hifi_profile().render();
        assert!(filter.contains("f=60 w=50 g=0.8"), "sub-bass");
        assert!(filter.contains("f=120 w=100 g=0.4"), "bass");
        assert!(filter.contains("f=250 w=180 g=-0.5"), "mid-bass cut");
        assert!(filter.contains("f=500 w=350 g=0.3"), "low-mid");
        assert!(filter.contains("f=2000 w=1400 g=0.2"), "mid");
        assert!(filter.contains("f=5500 w=3500 g=-0.4"), "upper-mid cut");
        assert!(filter.contains("f=8000 w=5500 g=0.4"), "presence lift");
        assert!(filter.contains("f=11000 w=7500 g=0.3"), "upper-presence");
        assert!(filter.contains("f=14000 w=9500 g=0.2"), "air lift");
        assert!(!filter.contains("asubboost"));
        assert!(!filter.contains("extrastereo"));
        assert!(!filter.contains("aecho"));
        assert!(filter.contains("loudnorm=I=-16"), "loudness target");
    }

    #[test]
    fn resample_and_lowpass_are_placed_in_signal_flow_order() {
        // aresample leads the whole chain (no pre-gain stage); lowpass sits
        // right after the EQ bands, ahead of compand/loudnorm -- see
        // `EqProfile::render`.
        let filter = default_hifi_profile().render();
        assert!(
            filter.starts_with("aresample=48000:resampler=swr:precision=28:cutoff=0.97,"),
            "resample leads: {filter}"
        );

        let anequalizer_idx = filter.find("anequalizer=").expect("has EQ bands");
        let lowpass_idx = filter.find("lowpass=f=18000").expect("has lowpass");
        let compand_idx = filter.find("compand=").expect("has compand");
        assert!(
            anequalizer_idx < lowpass_idx && lowpass_idx < compand_idx,
            "expected anequalizer -> lowpass -> compand order, got: {filter}"
        );

        // Balanced has no lowpass configured, so it must not appear at all.
        assert!(!balanced_profile().render().contains("lowpass"));
    }
}
