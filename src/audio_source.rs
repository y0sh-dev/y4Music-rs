//! Custom `songbird::input::Compose` source: resolves a track's direct CDN
//! URL via `yt-dlp -j`, then has `ffmpeg` fetch that URL and apply the
//! active EQ filter (see `crate::eq`) while decoding to raw PCM for
//! songbird.

use std::borrow::Cow;
use std::io::{BufRead, BufReader};
use std::process::{ChildStderr, Stdio};
use std::time::Duration;

use serde_json::Value;
use songbird::input::core::io::{MediaSource, ReadOnlySource};
use songbird::input::{AudioStream, AudioStreamError, ChildContainer, Compose, Input, RawAdapter};

/// Raw PCM format ffmpeg emits and `RawAdapter` expects: 48 kHz stereo,
/// matching Discord's own audio requirement so songbird never resamples.
const RAW_SAMPLE_RATE: u32 = 48_000;
const RAW_CHANNELS: u32 = 2;

/// Lazily resolves `url` to a direct CDN stream URL, then spawns `ffmpeg` to
/// fetch it and apply the EQ filter, handing songbird the resulting raw PCM.
#[derive(Clone, Debug)]
pub struct FfmpegEqSource {
    pub url: String,
    /// Forwarded to the `yt-dlp -j` metadata lookup verbatim.
    pub ytdlp_extra_args: Vec<String>,
    /// `None` = Balanced (`crate::eq::balanced_profile`); `Some(filter)` =
    /// Hi-Fi, applying this ffmpeg `-af` filtergraph verbatim.
    pub eq_filter: Option<String>,
    /// When set, ffmpeg seeks to this position before decoding (`-ss` before
    /// `-i`). Used by `/seek` and the panel's seek buttons, which rebuild the
    /// source rather than seeking in place -- see `player::seek_to`.
    pub seek_time: Option<Duration>,
}

#[async_trait::async_trait]
impl Compose for FfmpegEqSource {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        spawn_pipeline(
            &self.url,
            &self.ytdlp_extra_args,
            self.eq_filter.as_deref(),
            self.seek_time,
        )
        .await
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

impl From<FfmpegEqSource> for Input {
    fn from(val: FfmpegEqSource) -> Self {
        Input::Lazy(Box::new(val))
    }
}

/// Runs a metadata-only `yt-dlp -j` lookup and extracts a direct,
/// ffmpeg-fetchable stream URL from its JSON output.
async fn resolve_stream_url(url: &str, extra_args: &[String]) -> Result<String, AudioStreamError> {
    let output = tokio::process::Command::new("yt-dlp")
        .args(extra_args)
        .args([
            "-f",
            "bestaudio/best",
            "--no-playlist",
            "--no-warnings",
            "--quiet",
            "-j",
            url,
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| AudioStreamError::Fail(format!("failed to spawn yt-dlp: {e}").into()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AudioStreamError::Fail(
            format!("yt-dlp failed to resolve a stream URL: {}", stderr.trim()).into(),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let entry: Value = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str(line).ok())
        .ok_or_else(|| {
            AudioStreamError::Fail("yt-dlp returned no usable metadata".to_string().into())
        })?;

    extract_stream_url(&entry).ok_or_else(|| {
        AudioStreamError::Fail(
            "yt-dlp did not report a playable stream URL"
                .to_string()
                .into(),
        )
    })
}

/// Prefers the top-level `url` field; falls back to the highest-bitrate
/// audio-only entry in `formats`. Split out from `resolve_stream_url` so it
/// is testable without spawning a real `yt-dlp` process.
fn extract_stream_url(entry: &Value) -> Option<String> {
    if let Some(u) = entry.get("url").and_then(Value::as_str) {
        return Some(u.to_string());
    }

    entry
        .get("formats")?
        .as_array()?
        .iter()
        .filter(|f| f.get("vcodec").and_then(Value::as_str) == Some("none"))
        .filter_map(|f| {
            let u = f.get("url")?.as_str()?.to_string();
            let abr = f.get("abr").and_then(Value::as_f64).unwrap_or(0.0);
            Some((u, abr))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(u, _)| u)
}

/// Resolves the stream URL, spawns `ffmpeg` to fetch and filter it, and
/// wraps its stdout as a raw-PCM `AudioStream`.
async fn spawn_pipeline(
    url: &str,
    extra_args: &[String],
    eq_filter: Option<&str>,
    seek_time: Option<Duration>,
) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
    let stream_url = resolve_stream_url(url, extra_args).await?;

    let is_hifi = eq_filter.is_some();
    let filter: Cow<str> = match eq_filter {
        Some(f) => Cow::Borrowed(f),
        None => Cow::Owned(crate::eq::balanced_profile().render()),
    };

    let mut ffmpeg = std::process::Command::new("ffmpeg");
    ffmpeg.args(["-hide_banner", "-loglevel", "error"]);
    ffmpeg.args(["-reconnect", "1", "-reconnect_streamed", "1"]);
    if is_hifi {
        ffmpeg.args([
            "-reconnect_delay_max",
            "10",
            "-rw_timeout",
            "15000000",
            "-thread_queue_size",
            "16384",
            "-analyzeduration",
            "10M",
            "-probesize",
            "10M",
            "-fflags",
            "+nobuffer+genpts",
        ]);
    } else {
        ffmpeg.args([
            "-reconnect_delay_max",
            "5",
            "-rw_timeout",
            "5000000",
            "-thread_queue_size",
            "4096",
        ]);
    }
    // Input (demuxer-level) seek, placed before `-i`.
    let seek_arg = seek_time.map(|d| format!("{:.3}", d.as_secs_f64()));
    if let Some(seek_arg) = &seek_arg {
        ffmpeg.args(["-ss", seek_arg]);
    }
    ffmpeg
        .args(["-i", &stream_url, "-vn", "-af", &filter])
        .args([
            "-f",
            "f32le",
            "-ar",
            &RAW_SAMPLE_RATE.to_string(),
            "-ac",
            &RAW_CHANNELS.to_string(),
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut ffmpeg_child = ffmpeg
        .spawn()
        .map_err(|e| AudioStreamError::Fail(format!("failed to spawn ffmpeg: {e}").into()))?;
    if let Some(stderr) = ffmpeg_child.stderr.take() {
        log_stderr_lines("ffmpeg", stderr);
    }

    let container = ChildContainer::from(vec![ffmpeg_child]);
    let raw = RawAdapter::new(
        ReadOnlySource::new(container),
        RAW_SAMPLE_RATE,
        RAW_CHANNELS,
    );

    Ok(AudioStream {
        input: Box::new(raw) as Box<dyn MediaSource>,
    })
}

/// Forwards a child process's stderr into `tracing`, tagged by `label`, one
/// line at a time, on a plain OS thread.
fn log_stderr_lines(label: &'static str, stderr: ChildStderr) {
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !line.trim().is_empty() {
                tracing::warn!("[{label}] {line}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::extract_stream_url;
    use serde_json::json;

    #[test]
    fn prefers_the_top_level_url_field() {
        let entry = json!({
            "url": "https://cdn.example.com/direct.webm",
            "formats": [
                {"vcodec": "none", "abr": 160.0, "url": "https://cdn.example.com/other.webm"}
            ],
        });
        assert_eq!(
            extract_stream_url(&entry).as_deref(),
            Some("https://cdn.example.com/direct.webm")
        );
    }

    #[test]
    fn falls_back_to_the_highest_bitrate_audio_only_format() {
        let entry = json!({
            "formats": [
                {"vcodec": "h264", "abr": 0.0, "url": "https://cdn.example.com/video-only.mp4"},
                {"vcodec": "none", "abr": 128.0, "url": "https://cdn.example.com/low.webm"},
                {"vcodec": "none", "abr": 160.0, "url": "https://cdn.example.com/high.webm"},
            ],
        });
        assert_eq!(
            extract_stream_url(&entry).as_deref(),
            Some("https://cdn.example.com/high.webm")
        );
    }

    #[test]
    fn returns_none_when_nothing_playable_is_present() {
        let entry = json!({"formats": [{"vcodec": "h264", "url": "https://cdn.example.com/video-only.mp4"}]});
        assert_eq!(extract_stream_url(&entry), None);
    }
}
