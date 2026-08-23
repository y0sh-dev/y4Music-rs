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

/// A full playback EQ chain: resample/dither precision, optional
/// equalizer bands and spatial effects, then compand + loudnorm.
#[derive(Clone, Debug)]
pub struct EqProfile {
    pub resample_precision: u32,
    pub bands: Vec<EqBand>,
    pub sub_boost: Option<SubBoost>,
    pub stereo_width: Option<StereoWidth>,
    pub echo: Option<Echo>,
    pub compand: Compand,
    pub loudnorm: Loudnorm,
}

impl EqProfile {
    /// Renders this profile to an ffmpeg `-af` filtergraph string.
    pub fn render(&self) -> String {
        let mut parts = vec![format!(
            "aresample=48000:resampler=swr:precision={}:dither_method=shibata",
            self.resample_precision
        )];

        if !self.bands.is_empty() {
            let bands = self
                .bands
                .iter()
                .map(|b| b.render())
                .collect::<Vec<_>>()
                .join("|");
            parts.push(format!("anequalizer={bands}"));
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
        resample_precision: 24,
        bands: vec![EqBand {
            freq_hz: 60.0,
            width_hz: 15.0,
            gain_db: 1.5,
        }],
        sub_boost: None,
        stereo_width: None,
        echo: None,
        compand: Compand {
            attack_s: 0.02,
            decay_s: 0.1,
            points: vec![(-80.0, -80.0), (-35.0, -35.0), (0.0, -5.0)],
        },
        loudnorm: Loudnorm {
            integrated_lufs: -18.0,
            range_lu: 10.0,
            true_peak_dbtp: -2.0,
        },
    }
}

/// Hi-Fi mode's built-in default, overridable at startup via
/// `EQ_HIFI_FILTER` (an arbitrary raw `-af` string, bypassing this profile
/// entirely). Bass/mid/treble bands, sub-bass reinforcement, stereo
/// widening, a touch of echo, and a punchier compand/loudnorm curve than
/// Balanced. The treble band and its gain are this round's audible tuning
/// pass -- the previous default had no presence/air band at all.
pub fn default_hifi_profile() -> EqProfile {
    EqProfile {
        resample_precision: 33,
        bands: vec![
            EqBand {
                freq_hz: 55.0,
                width_hz: 15.0,
                gain_db: 2.0,
            },
            EqBand {
                freq_hz: 1_000.0,
                width_hz: 200.0,
                gain_db: 1.5,
            },
            EqBand {
                freq_hz: 9_000.0,
                width_hz: 2_000.0,
                gain_db: 1.2,
            },
        ],
        sub_boost: Some(SubBoost {
            cutoff_hz: 70.0,
            feedback: 0.2,
        }),
        stereo_width: Some(StereoWidth { m: 1.1 }),
        echo: Some(Echo {
            in_gain: 0.8,
            out_gain: 0.3,
            delay_ms: 20.0,
            decay: 0.02,
        }),
        compand: Compand {
            attack_s: 0.005,
            decay_s: 0.1,
            points: vec![(-80.0, -80.0), (-30.0, -20.0), (-10.0, -8.0), (0.0, -5.0)],
        },
        loudnorm: Loudnorm {
            integrated_lufs: -14.0,
            range_lu: 11.0,
            true_peak_dbtp: -1.0,
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
            "aresample=48000:resampler=swr:precision=24:dither_method=shibata,\
anequalizer=c0 f=60 w=15 g=1.5|c1 f=60 w=15 g=1.5,\
compand=attacks=0.02:decays=0.1:points=-80/-80|-35/-35|0/-5,\
loudnorm=I=-18:LRA=10:TP=-2"
        );
    }

    #[test]
    fn hifi_default_includes_a_treble_band() {
        let filter = default_hifi_profile().render();
        assert!(filter.contains("f=9000 w=2000 g=1.2"));
    }
}
