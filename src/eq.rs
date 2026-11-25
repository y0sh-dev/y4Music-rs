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
/// This is a **transparent / uncoloured reference path**, not a second
/// voicing. `bands` is deliberately empty, so `EqProfile::render` emits no
/// `anequalizer=` at all: listeners with high-resolution gear or their own
/// system-wide EQ (EasyEffects and similar) hear the source, not the bot's
/// taste layered on top of theirs. Balanced remains the opinionated,
/// everyone-friendly profile.
///
/// Only protective stages remain:
///
/// - `lowpass_hz: Some(17_500.0)` sheds content above 17.5 kHz that is
///   inaudible to nearly everyone and otherwise inflates the Opus encoder's
///   bitrate demand on high-entropy/high-BPM material (see
///   `commands::playback::resolve_target_bitrate` and the Discord bandwidth
///   cap that can turn that into stutter).
/// - `compand` is transient protection rather than loudness colouring:
///   everything at or below -6 dBFS passes 1:1 (`-80/-80`, `-6/-6`), only
///   peaks above it are eased down to `0/-1`. A 5 ms `delay_s` lookahead lets
///   the detector see a transient before the gain stage acts on it, with
///   `soft_knee: 3.0` smoothing the transition.
/// - `loudnorm` keeps the -16 LUFS / -1.5 dBTP targets but widens `LRA` to
///   14 so the source's dynamics are not squashed toward a narrow target.
///
/// There is still no pre-gain stage: 32-bit float processing has no
/// fixed-point headroom to protect, and trimming gain before the compressor
/// only skewed its effective input level. `sub_boost`/`stereo_width`/`echo`
/// remain dropped: they add cost and clipping risk without a clearly audible
/// benefit once re-encoded through Discord's Opus pipeline.
pub fn default_hifi_profile() -> EqProfile {
    EqProfile {
        // Empty on purpose: `render()` omits `anequalizer=` entirely.
        bands: Vec::new(),
        sub_boost: None,
        stereo_width: None,
        echo: None,
        // Protects the Opus encoder from inaudible ultra-high-frequency load.
        lowpass_hz: Some(17_500.0),
        compand: Compand {
            attack_s: 0.005,
            decay_s: 0.1,
            points: vec![(-80.0, -80.0), (-6.0, -6.0), (0.0, -1.0)],
            soft_knee: 3.0,
            delay_s: 0.005,
        },
        loudnorm: Loudnorm {
            integrated_lufs: -16.0,
            range_lu: 14.0,
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
    fn hifi_default_is_a_transparent_reference_path() {
        let filter = default_hifi_profile().render();
        assert!(
            !filter.contains("anequalizer="),
            "Hi-Fi must stay uncoloured (no EQ bands): {filter}"
        );
        assert!(!filter.contains("asubboost"));
        assert!(!filter.contains("extrastereo"));
        assert!(!filter.contains("aecho"));
        assert!(filter.contains("lowpass=f=17500"), "bandwidth guard");
        assert!(
            filter.contains(
                "compand=attacks=0.005:decays=0.1:points=-80/-80|-6/-6|0/-1:soft-knee=3:delay=0.005"
            ),
            "transparent peak protection: {filter}"
        );
        assert!(
            filter.contains("loudnorm=I=-16:LRA=14:TP=-1.5"),
            "wide-dynamics loudness target: {filter}"
        );
    }

    #[test]
    fn resample_and_lowpass_are_placed_in_signal_flow_order() {
        // aresample leads the whole chain (no pre-gain stage); with no EQ
        // bands, lowpass sits directly after it, ahead of compand/loudnorm --
        // see `EqProfile::render`.
        let filter = default_hifi_profile().render();
        assert!(
            filter.starts_with("aresample=48000:resampler=swr:precision=28:cutoff=0.97,"),
            "resample leads: {filter}"
        );

        let lowpass_idx = filter.find("lowpass=f=17500").expect("has lowpass");
        let compand_idx = filter.find("compand=").expect("has compand");
        let loudnorm_idx = filter.find("loudnorm=").expect("has loudnorm");
        assert!(
            lowpass_idx < compand_idx && compand_idx < loudnorm_idx,
            "expected lowpass -> compand -> loudnorm order, got: {filter}"
        );
        assert!(!filter.contains("anequalizer="), "no EQ stage: {filter}");

        // Balanced has no lowpass configured, so it must not appear at all.
        assert!(!balanced_profile().render().contains("lowpass"));
    }

    #[test]
    fn default_hifi_filtergraph_is_accepted_by_ffmpeg() {
        let filter = default_hifi_profile().render();
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=1000:duration=0.2",
                "-af",
                &filter,
                "-f",
                "null",
                "-",
            ])
            .output()
            .expect("spawn ffmpeg");
        assert!(
            output.status.success(),
            "ffmpeg rejected the Hi-Fi filtergraph: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
