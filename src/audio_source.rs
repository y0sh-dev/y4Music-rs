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

/// Upper bound on the `yt-dlp -j` stream-URL resolution call, so a network
/// stall can't block a track from ever starting (or ending) forever.
const YTDLP_RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);

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

#[poise::serenity_prelude::async_trait]
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
    let mut cmd = tokio::process::Command::new("yt-dlp");
    cmd.args(extra_args)
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
        // Ensure a timed-out process is actually killed, not just abandoned
        // to keep running in the background after we stop waiting on it.
        .kill_on_drop(true);

    let output = tokio::time::timeout(YTDLP_RESOLVE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            AudioStreamError::Fail(
                format!(
                    "yt-dlp timed out after {}s resolving a stream URL",
                    YTDLP_RESOLVE_TIMEOUT.as_secs()
                )
                .into(),
            )
        })?
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

/// Builds the `ffmpeg` argument vector for the HTTP input -> raw PCM output
/// pipeline. Split out from [`spawn_pipeline`] so the exact production
/// arguments can be exercised by tests (in particular the input/output option
/// ordering, which FFmpeg validates strictly).
///
/// `-thread_queue_size` is deliberately absent: it is an output-only (muxer)
/// option in FFmpeg 9 (`ffmpeg -h full` lists it under "Advanced per-file
/// options (output-only)"), and passing it before `-i` aborts the process with
/// "cannot be applied to input url ... Move this option before the file it
/// belongs to." A single audio input has no muxer thread contention to tune,
/// so no output-side override is needed either.
fn build_ffmpeg_args(stream_url: &str, filter: &str, seek_time: Option<Duration>) -> Vec<String> {
    // One transport/probing profile for both EQ modes -- the previous
    // Hi-Fi-only settings (10-16M analyze/probe, 15s rw_timeout) were
    // tuned for robustness but delayed playback start; this profile still
    // reconnects on TCP/TLS drops and 4xx/5xx HTTP errors, just without
    // over-provisioning the startup probe.
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-reconnect",
        "1",
        "-reconnect_streamed",
        "1",
        "-reconnect_on_network_error",
        "1",
        "-reconnect_on_http_error",
        "4xx,5xx",
        "-reconnect_delay_max",
        "5",
        "-rw_timeout",
        "10000000",
        "-analyzeduration",
        "5M",
        "-probesize",
        "5M",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();

    // Input (demuxer-level) seek, placed before `-i`.
    if let Some(d) = seek_time {
        args.push("-ss".to_string());
        args.push(format!("{:.3}", d.as_secs_f64()));
    }

    args.extend([
        "-i".to_string(),
        stream_url.to_string(),
        "-vn".to_string(),
        "-af".to_string(),
        filter.to_string(),
        "-f".to_string(),
        "f32le".to_string(),
        "-ar".to_string(),
        RAW_SAMPLE_RATE.to_string(),
        "-ac".to_string(),
        RAW_CHANNELS.to_string(),
        "pipe:1".to_string(),
    ]);
    args
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

    let filter: Cow<str> = match eq_filter {
        Some(f) => Cow::Borrowed(f),
        None => Cow::Owned(crate::eq::balanced_profile().render()),
    };

    let mut ffmpeg = std::process::Command::new("ffmpeg");
    ffmpeg
        .args(build_ffmpeg_args(&stream_url, &filter, seek_time))
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
/// line at a time. Runs on tokio's blocking-task pool (bounded, reused)
/// rather than a raw `std::thread::spawn`, so repeated track starts/seeks
/// can't exhaust the OS thread limit -- `ffmpeg`'s stdout still has to go
/// through a `std::process::Child` for `ChildContainer` (see this module's
/// doc comment), so its stderr stays a blocking `std::process::ChildStderr`
/// too; there is no plain `Child`-compatible async read for it.
fn log_stderr_lines(label: &'static str, stderr: ChildStderr) {
    tokio::task::spawn_blocking(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !line.trim().is_empty() {
                tracing::warn!("[{label}] {line}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{build_ffmpeg_args, extract_stream_url};
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// Returns a 16-bit stereo PCM WAV of the requested length, encoded in
    /// memory so the test needs no fixture file.
    fn silent_wav(duration_ms: u32) -> Vec<u8> {
        const SAMPLE_RATE: u32 = 48_000;
        const CHANNELS: u16 = 2;
        const BITS: u16 = 16;
        let frames = SAMPLE_RATE * duration_ms / 1000;
        let data_len = frames * u32::from(CHANNELS) * u32::from(BITS / 8);
        let byte_rate = SAMPLE_RATE * u32::from(CHANNELS) * u32::from(BITS / 8);
        let block_align = CHANNELS * BITS / 8;

        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&CHANNELS.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&BITS.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(44 + data_len as usize, 0);
        wav
    }

    /// Serves `body` over a loopback HTTP server until `stop` is set. The
    /// returned URL is unique per test (the OS assigns a free port).
    fn serve_wav(body: Vec<u8>) -> (String, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback listener");
        let addr = listener.local_addr().expect("read listener address");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);

        let worker = std::thread::spawn(move || {
            while !stop_worker.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = [0u8; 1024];
                        // The request headers are irrelevant: always reply with
                        // the complete body, which FFmpeg accepts even when it
                        // asked for a byte range.
                        let _ = stream.read(&mut request);
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(header.as_bytes());
                        let _ = stream.write_all(&body);
                        let _ = stream.flush();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        (format!("http://{addr}/tone.wav"), stop, worker)
    }

    #[test]
    fn keeps_input_options_before_the_input_url() {
        let args = build_ffmpeg_args(
            "https://cdn.example.com/track.webm",
            "anull",
            Some(Duration::from_millis(1234)),
        );

        let input_at = args.iter().position(|a| a == "-i").expect("-i present");
        assert_eq!(args[input_at + 1], "https://cdn.example.com/track.webm");

        for opt in [
            "-reconnect",
            "-reconnect_streamed",
            "-reconnect_on_network_error",
            "-reconnect_on_http_error",
            "-reconnect_delay_max",
            "-rw_timeout",
            "-analyzeduration",
            "-probesize",
            "-ss",
        ] {
            let at = args
                .iter()
                .position(|a| a == opt)
                .unwrap_or_else(|| panic!("{opt} present"));
            assert!(at < input_at, "{opt} must be an input option (before -i)");
        }
        let seek_at = args.iter().position(|a| a == "-ss").expect("-ss present");
        assert_eq!(args[seek_at + 1], "1.234");

        for opt in ["-vn", "-af", "-f", "-ar", "-ac"] {
            let at = args
                .iter()
                .position(|a| a == opt)
                .unwrap_or_else(|| panic!("{opt} present"));
            assert!(at > input_at, "{opt} must be an output option (after -i)");
        }
        assert!(!args.iter().any(|a| a == "-thread_queue_size"));
    }

    #[test]
    fn ffmpeg_decodes_an_http_input_to_raw_pcm() {
        let (url, stop, worker) = serve_wav(silent_wav(200));
        let filter = crate::eq::balanced_profile().render();

        let output = std::process::Command::new("ffmpeg")
            .args(build_ffmpeg_args(&url, &filter, None))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn ffmpeg")
            .wait_with_output()
            .expect("wait for ffmpeg");

        stop.store(true, Ordering::Relaxed);
        worker.join().expect("join HTTP server");

        assert!(
            output.status.success(),
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty(), "ffmpeg produced no PCM");
        assert_eq!(
            output.stdout.len() % 8,
            0,
            "f32le stereo frames are 8 bytes"
        );
    }

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
