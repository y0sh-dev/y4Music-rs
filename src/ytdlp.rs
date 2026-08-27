//! Minimal `yt-dlp` JSON wrapper for search and playlist streaming.

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

use crate::Error;

/// Upper bound on a single `yt-dlp` metadata lookup, so a network stall
/// can't block a command handler (or leak its task) forever.
const YTDLP_TIMEOUT: Duration = Duration::from_secs(20);

/// Upper bound on a single line read from a `TrackStream`'s stdout, so a
/// stalled network fetch mid-playlist can't block an import forever.
const NEXT_LINE_TIMEOUT: Duration = Duration::from_secs(15);

/// A single track's metadata, as needed by search results and playlists.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub title: String,
    pub uploader: Option<String>,
    pub duration: i64,
    pub webpage_url: String,
    pub thumbnail: Option<String>,
}

async fn run_yt_dlp(args: &[&str]) -> Result<Vec<Value>, Error> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args(["--no-warnings", "-j"])
        .args(args)
        // Ensure a timed-out process is actually killed, not just abandoned
        // to keep running in the background after we stop waiting on it.
        .kill_on_drop(true);

    let output = tokio::time::timeout(YTDLP_TIMEOUT, cmd.output())
        .await
        .map_err(|_| -> Error {
            format!("yt-dlp timed out after {}s.", YTDLP_TIMEOUT.as_secs()).into()
        })?
        .map_err(|e| -> Error {
            if e.kind() == std::io::ErrorKind::NotFound {
                "yt-dlp is not installed or not on PATH.".into()
            } else {
                Box::new(e)
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("yt-dlp failed: {}", stderr.trim()).into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect())
}

fn entry_to_track_info(entry: &Value) -> Option<TrackInfo> {
    let title = entry.get("title")?.as_str()?.to_string();
    let webpage_url = entry
        .get("webpage_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            entry
                .get("id")
                .and_then(Value::as_str)
                .map(|id| format!("https://www.youtube.com/watch?v={id}"))
        })?;

    Some(TrackInfo {
        title,
        uploader: entry
            .get("uploader")
            .and_then(Value::as_str)
            .map(str::to_string),
        duration: entry.get("duration").and_then(Value::as_i64).unwrap_or(0),
        webpage_url,
        thumbnail: entry
            .get("thumbnail")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Filters out YouTube's auto-generated "Mix"/Radio results (`RD`-prefixed
/// IDs).
fn is_mix(entry: &Value) -> bool {
    entry
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| id.starts_with("RD"))
}

/// Searches YouTube for `query`, returning up to `max_results` tracks.
pub async fn search(query: &str, max_results: u32) -> Result<Vec<TrackInfo>, Error> {
    let search_query = format!("ytsearch{max_results}:{query}");
    let entries = run_yt_dlp(&["--flat-playlist", &search_query]).await?;

    Ok(entries
        .iter()
        .filter(|e| !is_mix(e))
        .filter_map(entry_to_track_info)
        .collect())
}

/// Streams a URL (or, via yt-dlp's default search, a plain search term) to
/// tracks one at a time, instead of buffering the whole result set in
/// memory before returning anything -- a single video yields one track, a
/// playlist URL yields all of its (non-Mix) entries. Used to import large
/// playlists without waiting for `yt-dlp` to enumerate every entry first.
///
/// Runs in flat-playlist mode (`--flat-playlist`), so entries come back
/// with only the metadata YouTube's playlist listing itself provides --
/// title and, usually, duration/uploader -- rather than a full per-video
/// fetch. Fields that flat mode doesn't have default the same way as
/// elsewhere in this module (`0` duration, `None` uploader).
pub struct TrackStream {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
}

impl TrackStream {
    /// Spawns `yt-dlp` against `query_or_url` and returns a handle ready to
    /// be polled with `next_track`.
    pub async fn spawn(query_or_url: &str) -> Result<Self, Error> {
        if query_or_url.contains("list=RD") {
            return Err("❌ YouTube's auto-generated 'Mix' playlists cannot be added.".into());
        }

        let mut cmd = Command::new("yt-dlp");
        cmd.args([
            "--no-warnings",
            "-j",
            "--flat-playlist",
            "--ignore-errors",
            "--default-search",
            "ytsearch",
            query_or_url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Belt-and-suspenders alongside this struct's own `Drop` impl below
        // -- a stream abandoned mid-import (an early `?` return, a timeout)
        // must not leave `yt-dlp` running.
        .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| -> Error {
            if e.kind() == std::io::ErrorKind::NotFound {
                "yt-dlp is not installed or not on PATH.".into()
            } else {
                Box::new(e)
            }
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or("failed to capture yt-dlp's stdout")?;
        if let Some(stderr) = child.stderr.take() {
            log_stderr_lines(stderr);
        }

        Ok(Self {
            child,
            lines: BufReader::new(stdout).lines(),
        })
    }

    /// Reads and parses the next track off the stream, skipping blank
    /// lines, unparsable JSON, and filtered-out (Mix) entries along the
    /// way. Returns `Ok(None)` once `yt-dlp` closes its stdout (the
    /// playlist -- or single video -- has been fully enumerated).
    pub async fn next_track(&mut self) -> Result<Option<TrackInfo>, Error> {
        loop {
            let line = tokio::time::timeout(NEXT_LINE_TIMEOUT, self.lines.next_line())
                .await
                .map_err(|_| -> Error {
                    format!(
                        "yt-dlp timed out after {}s waiting for the next track.",
                        NEXT_LINE_TIMEOUT.as_secs()
                    )
                    .into()
                })??;

            let Some(line) = line else {
                return Ok(None);
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if is_mix(&entry) {
                continue;
            }
            if let Some(track) = entry_to_track_info(&entry) {
                return Ok(Some(track));
            }
        }
    }

    /// Waits for `yt-dlp` to exit after the stream has been fully drained
    /// (`next_track` returned `None`), surfacing a non-zero exit as an
    /// error. Intended for callers that parsed zero tracks and want to
    /// know whether that's because the source was genuinely empty or
    /// because `yt-dlp` itself failed.
    pub async fn finish(mut self) -> Result<(), Error> {
        let status = self.child.wait().await?;
        if !status.success() {
            return Err("yt-dlp exited with an error; some tracks may be missing.".into());
        }
        Ok(())
    }
}

impl Drop for TrackStream {
    /// A `TrackStream` dropped before the stream is fully drained (an early
    /// `?` return, a cancelled command) must not leave `yt-dlp` running as
    /// an orphan.
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Forwards `yt-dlp`'s stderr to `tracing`, one line at a time. Left
/// undrained, a `--ignore-errors` run over a large playlist can write
/// enough warnings to fill the stderr pipe and block `yt-dlp` -- which
/// would in turn stall the `next_track` stdout read this whole stream
/// depends on.
fn log_stderr_lines(stderr: ChildStderr) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.trim().is_empty() {
                tracing::warn!("[yt-dlp] {line}");
            }
        }
    });
}
