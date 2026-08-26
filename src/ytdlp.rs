//! Minimal `yt-dlp` JSON wrapper for search and playlist-URL expansion.

use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

use crate::Error;

/// Upper bound on a single `yt-dlp` metadata lookup, so a network stall
/// can't block a command handler (or leak its task) forever.
const YTDLP_TIMEOUT: Duration = Duration::from_secs(20);

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

/// Resolves a URL (or, via yt-dlp's default search, a plain search term) to
/// one or more tracks: a single video yields one track, a playlist URL
/// yields all of its (non-Mix) entries.
pub async fn resolve(query_or_url: &str) -> Result<Vec<TrackInfo>, Error> {
    if query_or_url.contains("list=RD") {
        return Err("❌ YouTube's auto-generated 'Mix' playlists cannot be added.".into());
    }

    let entries = run_yt_dlp(&["--default-search", "ytsearch", query_or_url]).await?;
    let tracks: Vec<TrackInfo> = entries
        .iter()
        .filter(|e| !is_mix(e))
        .filter_map(entry_to_track_info)
        .collect();

    if tracks.is_empty() {
        return Err("❌ No addable tracks were found for that URL.".into());
    }

    Ok(tracks)
}
