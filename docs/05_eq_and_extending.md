# 05. EQ Configuration & Extension Guide

## `eq.rs`: encoding "what changes where" as types

FFmpeg's `-af` filtergraph is, at bottom, just one opaque string -- which parameter maps to which effect is only discoverable by reading ffmpeg's own docs. `src/eq.rs` decomposes it into structs so the field names themselves double as documentation.

| Struct | ffmpeg filter | Meaning |
|---|---|---|
| `EqBand { freq_hz, width_hz, gain_db }` | one `anequalizer` band | Boost/cut a specific frequency band |
| `SubBoost { cutoff_hz, feedback }` | `asubboost` | Reinforce low end below the cutoff |
| `StereoWidth { m }` | `extrastereo` | Stereo image width (`1.0` = unchanged) |
| `Echo { in_gain, out_gain, delay_ms, decay }` | `aecho` | Adds echo / spatial ambience |
| `Compand { attack_s, decay_s, points, soft_knee, delay_s }` | `compand` | Dynamic range compression. `points` is the input/output dB transfer curve; `soft_knee` rounds the bend at each breakpoint instead of a hard corner; `delay_s` is a lookahead -- it holds the signal briefly so the level detector sees a transient *before* the gain stage reacts to it |
| `Loudnorm { integrated_lufs, range_lu, true_peak_dbtp }` | `loudnorm` | EBU R128 loudness normalization |

`EqProfile` assembles these into the final `-af` string:

```rust
pub struct EqProfile {
    pub bands: Vec<EqBand>,
    pub sub_boost: Option<SubBoost>,
    pub stereo_width: Option<StereoWidth>,
    pub echo: Option<Echo>,
    pub lowpass_hz: Option<f64>,
    pub compand: Compand,
    pub loudnorm: Loudnorm,
}

impl EqProfile {
    pub fn render(&self) -> String { /* joins aresample,anequalizer,lowpass,asubboost,...,compand,loudnorm */ }
}
```

There is no `pre_gain_db` and no `resample_precision` field anymore -- both were removed (see "Removal of Pre-Gain" below). `render()` now opens every profile with a single fixed resample stage, `aresample=48000:resampler=swr:precision=28:cutoff=0.97`, regardless of profile; no per-profile precision knob exists.

`render()`'s assembly order is the signal-flow order: `aresample` (fixed) -> `anequalizer` (only when `bands` is non-empty) -> `lowpass` (only if `Some`) -> (`asubboost`/`extrastereo`/`aecho`, all unused by the current profiles) -> `compand` -> `loudnorm`. Lowpass sits right after the EQ stage (or directly after `aresample` when there are no bands) so it trims high-frequency energy before that energy reaches the compressor/loudness stage.

Fields that are `Option`/`Vec` signal "this profile doesn't use it" -- e.g. `balanced_profile()` leaves `sub_boost`/`stereo_width`/`echo` all `None`, keeping it to plain band correction plus normalization; `default_hifi_profile()` additionally leaves `bands` empty and drops the spatial effects, emitting only the protective stages.

## Balanced vs Hi-Fi Settings Comparison

| Parameter | Balanced | Hi-Fi |
|---|---|---|
| Resample | `precision=28:cutoff=0.97` (fixed, both profiles) | same |
| EQ bands (`anequalizer`) | 60Hz / w15 / +1.5dB | none (uncoloured reference path) |
| Lowpass (`lowpass`) | -- | 17500Hz |
| Sub-bass boost (`asubboost`) | -- | dropped |
| Stereo width (`extrastereo`) | -- | dropped |
| Echo (`aecho`) | -- | dropped |
| Compressor attack / decay | 0.02s / 0.1s | 0.005s / 0.1s |
| Compressor points | -80/-80\|-35/-35\|0/-5 | -80/-80\|-6/-6\|0/-1 |
| Compressor soft-knee | 0.01 | 3.0 |
| Compressor lookahead (`delay_s`) | 0s | 0.005s |
| Loudness target | I=-16 LUFS / LRA=10 / TP=-2.0dB | I=-16 LUFS / LRA=14 / TP=-1.5dB |

### Removal of Pre-Gain (Leveraging 32-bit Float)

The previous `-6dB` pre-gain (ffmpeg's `volume` filter, applied before any EQ boost) existed to protect fixed-point headroom: without it, a boosted band could push a sample past 0dBFS and clip. FFmpeg's internal pipeline here runs in `f32le` (32-bit float), where intermediate values aren't clamped to `[-1.0, 1.0]` between filter stages -- a band boost can exceed 0dBFS mid-chain without clipping, and `loudnorm` at the end of the chain still receives the signal's full dynamic range intact. Pre-gain bought no real protection on this pipeline; it only shifted the compander's effective input level, which is exactly the mismatch the DSP audit flagged as skewing `compand`'s behavior. Removing it means one less parameter to keep in sync between profiles and no more implicit level offset for the compressor to compensate for.

### Transparent Reference Path (No EQ Bands)

Hi-Fi is deliberately *not* a second voicing. `bands` is empty, so `render()` emits no `anequalizer` stage at all: whatever the source contains reaches the listener's own high-resolution gear, room correction, or system-wide EQ (EasyEffects and similar) without the bot stacking another tonal curve on top. Balanced remains the opinionated profile for listeners who want a mild bass lift and consistent loudness out of the box. The remaining Hi-Fi stages are protective only (bandwidth guard, peak control, loudness targeting), and `EQ_HIFI_FILTER` still allows a fully custom curve when an operator wants one.

### Transparent Peak Protection (Lookahead Compression & Soft-Knee)

Hi-Fi's `compand` no longer shapes average loudness: the first two breakpoints are 1:1 (`-80/-80`, `-6/-6`), so everything at or below -6 dBFS passes through untouched. Only peaks above -6 dBFS are eased down (`0/-1`), preventing the occasional transient from clipping without compressing the body of the music. `Compand::delay_s` (0.005s on Hi-Fi) holds the signal briefly so the level detector sees a transient slightly ahead of the gain stage acting on it, and `Compand::soft_knee` (3.0 on Hi-Fi, vs. Balanced's near-hard 0.01) rounds the transition around the breakpoint. The faster attack (0.005s) catches peaks while the 0.1s decay releases quickly enough not to ride the programme material. Balanced keeps its gentler, louder average-level compression.

### Lowpass (17500Hz): Reducing Encoder Load (Nightcore Mitigation)

Hi-Fi's `lowpass_hz: Some(17_500.0)` inserts `lowpass=f=17500` right after `aresample` (there is no `anequalizer` in this profile), cutting frequency content above 17.5kHz that's inaudible to nearly everyone. High-BPM/high-entropy material (Nightcore and similar) packs disproportionate energy in that range; left in, it inflates the Opus encoder's bitrate demand enough to contribute to packet loss under Discord's bandwidth cap (see `commands::playback::resolve_target_bitrate`'s token-bucket limiting), audible as playback stutter. Shedding it before the compressor/loudness stage keeps that protection without touching anything a listener would notice. Balanced leaves `lowpass_hz: None` and does not apply this.

## `EQ_HIFI_FILTER` Environment Variable: a Raw-String Escape Hatch

Hi-Fi's default value comes from `eq::default_hifi_profile().render()`, but that's only the **fallback used when the environment variable isn't set**.

```rust
// main.rs
let eq_hifi_filter = std::env::var("EQ_HIFI_FILTER")
    .unwrap_or_else(|_| eq::default_hifi_profile().render());
```

If an operator sets `EQ_HIFI_FILTER` to an arbitrary `-af` string, it bypasses `eq.rs`'s structs entirely. The structs only organize how the in-code default is assembled; an operator's own tuning freedom (any string, unconstrained by the struct's shape) is deliberately preserved at runtime. Balanced mode has no equivalent override path (no `BALANCED_FILTER`) -- it always uses `balanced_profile()` as-is.

## Extension Guide

### Adding a new EQ band / effect

1. Add a new struct to `eq.rs` (e.g. `struct Deesser { .. }`) with fields for the corresponding ffmpeg filter's arguments.
2. Add an `Option<Deesser>`-shaped field to `EqProfile`.
3. In `EqProfile::render()`, add a branch that appends to the filter string only when it's `Some` (follow the existing `sub_boost`/`echo` pattern).
4. Set a value in whichever of `balanced_profile()` / `default_hifi_profile()` should use it.
5. Run `cargo test` (`eq::tests`) and update/add expected-string assertions as needed.

### Adding a new slash command

1. Write an `async fn` tagged `#[poise::command(slash_command, ...)]` in the appropriate file under `src/commands/` (an existing file like `playback.rs`, or a new one).
2. Register it in `src/commands/mod.rs::all()`'s vector (a missed registration fails silently at the Discord UI level, not at runtime, so it's easy to overlook).
3. Reuse `commands::playback::ensure_call`/`songbird_manager` for voice operations, and `player::refresh_panel` for panel updates.

### Supporting a new audio source (e.g. SoundCloud)

`FfmpegEqSource` currently passes the URL string straight through to `yt-dlp -j`, so any site `yt-dlp` already supports likely works with no code changes. For a custom protocol `yt-dlp` doesn't support, add a new type implementing `Compose` in `audio_source.rs` and switch to it at the equivalent of `commands/playback.rs::resolve_and_build_track`. Apply the same seek-related constraints described in `02_audio_pipeline.md` (`TrackMeta::start_offset`/`is_seek`) to the new source as well.

---

## Epilogue: Hitting the Discord Opus Wall

After chasing every last bit of resolution across reference IEMs, we ran straight into an obvious physical truth: Discord voice channels re-encode audio in real time at **64–96 kbps Opus** (128 kbps at higher boost tiers). No amount of parametric EQ, lookahead compression, or resampling precision can reconstruct harmonics that a lossy transport throws away—and the better your listening gear gets, the clearer that ceiling becomes.

Rather than fighting a losing battle with DSP tricks, `y4Music-rs` accepts the transport for what it is: `Balanced` keeps casual playback pleasant, `Hi-Fi` steps out of the way, and the project rests here—finished, honest, and at peace with the Opus limit.
