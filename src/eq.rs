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
/// input/output dB transfer curve.
#[derive(Clone, Debug)]
pub struct Compand {
    pub attack_s: f64,
    pub decay_s: f64,
    pub points: Vec<(f64, f64)>,
}

/// `loudnorm`: EBU R128 loudness normalization target.
#[derive(Clone, Copy, Debug)]
pub struct Loudnorm {
    pub integrated_lufs: f64,
    pub range_lu: f64,
    pub true_peak_dbtp: f64,
}

/// A full playback EQ chain: an upstream pre-gain for headroom, then
/// resample/dither precision, optional equalizer bands and spatial
/// effects, an optional lowpass to shed inaudible high-frequency energy,
/// then compand + loudnorm.
#[derive(Clone, Debug)]
pub struct EqProfile {
    /// Applied first, via ffmpeg's `volume` filter (e.g. `-6.0` =
    /// `volume=-6dB`). Trims headroom *before* any EQ boost, so a band lift
    /// downstream can't push a sample over 0dBFS and clip.
    pub pre_gain_db: f64,
    pub resample_precision: u32,
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
    /// signal-flow order: pre-gain -> resample -> EQ bands -> lowpass ->
    /// compand -> loudnorm. Pre-gain leads so downstream boosts have
    /// headroom to work with instead of clipping; lowpass follows the EQ
    /// bands so it trims whatever high-frequency energy they didn't
    /// already remove, right before the final loudness/dynamics stage.
    pub fn render(&self) -> String {
        let mut parts = vec![format!("volume={}dB", self.pre_gain_db)];

        parts.push(format!(
            "aresample=48000:resampler=swr:precision={}:dither_method=shibata",
            self.resample_precision
        ));

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
            "compand=attacks={}:decays={}:points={points}",
            self.compand.attack_s, self.compand.decay_s
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
        pre_gain_db: -6.0,
        resample_precision: 24,
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
/// A 6-band subtractive parametric EQ: rather than boosting everything
/// (the previous 3-band Bass/Mid/Treble default), this cuts the bands that
/// cause muddiness (Mid-Bass) and listening fatigue (Upper-Mid) and only
/// lifts the bands that add clarity/air, on the theory that "no coloration"
/// reads as cleaner over a lossy Opus link than "boosted everywhere."
/// `sub_boost`/`stereo_width`/`echo` are all dropped -- these spatial
/// effects added CPU cost and clipping risk without a clearly audible
/// benefit once re-encoded through Discord's Opus pipeline. `compand` and
/// `loudnorm` are aligned with Balanced's headroom-conscious targets
/// (same `-16 LUFS`/attack curve), with a slightly wider loudness range
/// and less peak limiting than Balanced to preserve more dynamics.
///
/// `pre_gain_db: -6.0` matches Balanced, trimming headroom before the EQ
/// bands so their boosts (Sub-Bass/Presence/Air above) can't push a sample
/// over 0dBFS and clip. `lowpass_hz: Some(16_000.0)` is Hi-Fi-specific:
/// high-BPM/high-entropy material (Nightcore and similar) packs far more
/// energy above 16kHz than Balanced's source material typically does, and
/// that inaudible-to-nearly-everyone content was inflating the Opus
/// encoder's bitrate demand enough to contribute to packet loss --
/// stutter -- under Discord's bandwidth cap (see
/// `commands::playback::resolve_target_bitrate`). Cutting it above the
/// EQ bands sheds that load without touching anything a listener would
/// notice.
pub fn default_hifi_profile() -> EqProfile {
    EqProfile {
        pre_gain_db: -6.0,
        resample_precision: 33,
        bands: vec![
            // Sub-Bass: foundation.
            EqBand {
                freq_hz: 60.0,
                width_hz: 40.0,
                gain_db: 1.5,
            },
            // Mid-Bass: cut to remove muddiness.
            EqBand {
                freq_hz: 250.0,
                width_hz: 150.0,
                gain_db: -1.0,
            },
            // Mid: reference point, left flat.
            EqBand {
                freq_hz: 1_000.0,
                width_hz: 500.0,
                gain_db: 0.0,
            },
            // Upper-Mid: cut to reduce listening fatigue.
            EqBand {
                freq_hz: 3_000.0,
                width_hz: 1_000.0,
                gain_db: -0.5,
            },
            // Presence: clarity.
            EqBand {
                freq_hz: 8_000.0,
                width_hz: 2_000.0,
                gain_db: 1.5,
            },
            // Air: sense of openness.
            EqBand {
                freq_hz: 14_000.0,
                width_hz: 3_000.0,
                gain_db: 1.0,
            },
        ],
        sub_boost: None,
        stereo_width: None,
        echo: None,
        // Cuts inaudible (to nearly everyone) high-frequency content that
        // otherwise inflates the Opus encoder's bitrate demand on
        // high-entropy/high-BPM material (e.g. Nightcore).
        lowpass_hz: Some(16_000.0),
        compand: Compand {
            attack_s: 0.02,
            decay_s: 0.1,
            points: vec![(-80.0, -80.0), (-35.0, -35.0), (0.0, -5.0)],
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
            "volume=-6dB,\
aresample=48000:resampler=swr:precision=24:dither_method=shibata,\
anequalizer=c0 f=60 w=15 g=1.5|c1 f=60 w=15 g=1.5,\
compand=attacks=0.02:decays=0.1:points=-80/-80|-35/-35|0/-5,\
loudnorm=I=-16:LRA=10:TP=-2"
        );
    }

    #[test]
    fn hifi_default_is_a_six_band_subtractive_eq_without_spatial_effects() {
        let filter = default_hifi_profile().render();
        assert!(filter.contains("f=60 w=40 g=1.5"), "sub-bass foundation");
        assert!(filter.contains("f=250 w=150 g=-1"), "mid-bass cut");
        assert!(filter.contains("f=1000 w=500 g=0"), "flat mid reference");
        assert!(filter.contains("f=3000 w=1000 g=-0.5"), "upper-mid cut");
        assert!(filter.contains("f=8000 w=2000 g=1.5"), "presence lift");
        assert!(filter.contains("f=14000 w=3000 g=1"), "air lift");
        assert!(!filter.contains("asubboost"));
        assert!(!filter.contains("extrastereo"));
        assert!(!filter.contains("aecho"));
        assert!(filter.contains("loudnorm=I=-16"), "loudness target");
    }

    #[test]
    fn pre_gain_and_lowpass_are_placed_in_signal_flow_order() {
        // volume (pre-gain) leads the whole chain; lowpass sits right after
        // the EQ bands, ahead of compand/loudnorm -- see `EqProfile::render`.
        let filter = default_hifi_profile().render();
        assert!(
            filter.starts_with("volume=-6dB,"),
            "pre-gain leads: {filter}"
        );

        let anequalizer_idx = filter.find("anequalizer=").expect("has EQ bands");
        let lowpass_idx = filter.find("lowpass=f=16000").expect("has lowpass");
        let compand_idx = filter.find("compand=").expect("has compand");
        assert!(
            anequalizer_idx < lowpass_idx && lowpass_idx < compand_idx,
            "expected anequalizer -> lowpass -> compand order, got: {filter}"
        );

        // Balanced has no lowpass configured, so it must not appear at all.
        assert!(!balanced_profile().render().contains("lowpass"));
    }
}
