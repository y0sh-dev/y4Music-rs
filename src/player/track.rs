//! Layer 1: derived track metadata (YouTube thumbnails), the
//! `LoopMode::Track` pre-emptive-clone helpers, and seek. Depends only on
//! `state` (for `TrackMeta`) and `crate::audio_source`.

use std::sync::Arc;
use std::time::Duration;

use songbird::Call;
use songbird::tracks::Track;
use tokio::sync::Mutex;

use crate::audio_source::FfmpegEqSource;

use super::state::TrackMeta;

/// Best-effort thumbnail URL derived from a YouTube video ID. Returns `None`
/// for non-YouTube URLs or unrecognised URL shapes.
pub fn derive_youtube_thumbnail(url: &str) -> Option<String> {
    let id = youtube_video_id(url)?;
    Some(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"))
}

pub fn youtube_video_id(url: &str) -> Option<String> {
    fn take_id(s: &str) -> Option<String> {
        let id = s.split(['?', '&', '#', '/']).next()?;
        (!id.is_empty()).then(|| id.to_string())
    }

    let lower = url.to_ascii_lowercase();
    if !lower.contains("youtube.com") && !lower.contains("youtu.be") {
        return None;
    }

    if let Some(after) = url.split("youtu.be/").nth(1) {
        return take_id(after);
    }
    if let Some(idx) = url.find("v=") {
        return take_id(&url[idx + 2..]);
    }
    if let Some(after) = url.split("/shorts/").nth(1) {
        return take_id(after);
    }
    if let Some(after) = url.split("/embed/").nth(1) {
        return take_id(after);
    }

    None
}

/// Ensures the currently playing track has a pre-emptive duplicate
/// (`is_loop_clone: true`) queued immediately after it, at index 1.
/// Idempotent: a no-op if index 1 already holds a loop-clone, or if
/// nothing is currently playing.
///
/// This is the core of `LoopMode::Track`'s "pre-emptive duplication"
/// design (see `LoopHandler`'s doc comment for why the old post-hoc
/// stop-and-splice approach was replaced). By keeping the *next* repeat of
/// the current track already sitting in the queue ahead of time, an
/// ordinary `TrackEvent::End` needs no special-casing at all: songbird's
/// own forward-only queue advancement naturally plays the clone next, and
/// this function is simply called again afterward to top up the following
/// lap's clone.
pub async fn ensure_track_loop_clone(call: &Arc<Mutex<Call>>, extra_args: &[String]) {
    let mut call = call.lock().await;

    let Some(current) = call.queue().current() else {
        return;
    };
    let already_topped_up = call
        .queue()
        .current_queue()
        .get(1)
        .is_some_and(|handle| handle.data::<TrackMeta>().is_loop_clone);
    if already_topped_up {
        return;
    }

    let meta = current.data::<TrackMeta>();
    let volume = current.get_info().await.map(|s| s.volume).unwrap_or(1.0);

    let source = FfmpegEqSource {
        url: meta.url.clone(),
        ytdlp_extra_args: extra_args.to_vec(),
        eq_filter: meta.eq_filter.clone(),
        seek_time: None,
    };
    let clone_meta = Arc::new(TrackMeta {
        title: meta.title.clone(),
        url: meta.url.clone(),
        requested_by: meta.requested_by,
        duration: meta.duration,
        eq_filter: meta.eq_filter.clone(),
        thumbnail: meta.thumbnail.clone(),
        uploader: meta.uploader.clone(),
        requester_name: meta.requester_name.clone(),
        start_offset: Duration::ZERO,
        is_seek: false,
        is_loop_clone: true,
    });
    let clone_track = Track::new_with_data(source.into(), clone_meta).volume(volume);
    let clone_handle = call.enqueue(clone_track).await;

    call.queue().modify_queue(|tracks| {
        if let Some(pos) = tracks.iter().position(|q| q.uuid() == clone_handle.uuid())
            && let Some(clone) = tracks.remove(pos)
        {
            let insert_at = 1.min(tracks.len());
            tracks.insert(insert_at, clone);
        }
    });
}

/// Removes and stops the loop-clone at index 1, if present. A no-op
/// otherwise. Used whenever the *real* next track needs to be back at
/// index 1 -- turning `LoopMode::Track` off, or skipping past the clone so
/// `/skip`/the SKIP button advance to a genuinely different track instead
/// of just replaying the current one again.
pub async fn remove_track_loop_clone(call: &Arc<Mutex<Call>>) {
    let call = call.lock().await;
    call.queue().modify_queue(|tracks| {
        if tracks
            .get(1)
            .is_some_and(|t| t.data::<TrackMeta>().is_loop_clone)
            && let Some(removed) = tracks.remove(1)
        {
            let _ = removed.stop();
        }
    });
}

/// Seeks the currently playing track to an absolute position, by building a
/// new `Track` (via `FfmpegEqSource::seek_time`) and swapping it in for the
/// current one -- songbird's native seek doesn't support custom `Compose`
/// sources. Used by `/seek` and, via `seek_by`, the panel's ⏪/⏩ buttons.
///
/// Clamped to `[0, duration]` when duration is known. Returns the actual
/// (clamped) position on success, or a user-facing error string otherwise.
pub async fn seek_to(
    call: &Arc<Mutex<Call>>,
    extra_args: &[String],
    target: Duration,
) -> Result<Duration, String> {
    let mut call = call.lock().await;

    let Some(current) = call.queue().current() else {
        return Err("Nothing is playing.".to_string());
    };
    let meta = current.data::<TrackMeta>();
    let target = match meta.duration {
        Some(duration) => target.min(duration),
        None => target,
    };
    let volume = current.get_info().await.map(|s| s.volume).unwrap_or(1.0);

    let source = FfmpegEqSource {
        url: meta.url.clone(),
        ytdlp_extra_args: extra_args.to_vec(),
        eq_filter: meta.eq_filter.clone(),
        seek_time: Some(target),
    };
    let new_meta = Arc::new(TrackMeta {
        title: meta.title.clone(),
        url: meta.url.clone(),
        requested_by: meta.requested_by,
        duration: meta.duration,
        eq_filter: meta.eq_filter.clone(),
        thumbnail: meta.thumbnail.clone(),
        uploader: meta.uploader.clone(),
        requester_name: meta.requester_name.clone(),
        start_offset: target,
        is_seek: true,
        is_loop_clone: meta.is_loop_clone,
    });
    let new_track = Track::new_with_data(source.into(), new_meta).volume(volume);
    let new_handle = call.enqueue(new_track).await;

    // Move the new track to the front and pop the old one off, in a single
    // `modify_queue` call so no concurrent op sees two fronts (or none).
    let old_queued = call.queue().modify_queue(|tracks| {
        let new_pos = tracks.iter().position(|q| q.uuid() == new_handle.uuid());
        let new_queued = new_pos.and_then(|i| tracks.remove(i));
        let old_queued = tracks.pop_front();
        if let Some(new_queued) = new_queued {
            tracks.push_front(new_queued);
        }
        old_queued
    });

    // Stop the old track before starting the new one to avoid overlap; new
    // tracks always start paused, so the new one needs an explicit `play()`.
    if let Some(old) = old_queued {
        let _ = old.stop();
    }
    let _ = new_handle.play();

    Ok(target)
}

/// Seeks the currently playing track by `delta_secs` seconds (negative
/// rewinds), relative to its current position (`state.position +
/// meta.start_offset`, to account for a track already seeked once).
pub async fn seek_by(
    call: &Arc<Mutex<Call>>,
    extra_args: &[String],
    delta_secs: i64,
) -> Result<Duration, String> {
    let current_pos = {
        let call = call.lock().await;
        let Some(current) = call.queue().current() else {
            return Err("Nothing is playing.".to_string());
        };
        let meta = current.data::<TrackMeta>();
        let position = current
            .get_info()
            .await
            .map(|s| s.position)
            .map_err(|e| format!("⚠️ {e}"))?;
        position + meta.start_offset
    };
    let target = if delta_secs.is_negative() {
        current_pos.saturating_sub(Duration::from_secs(delta_secs.unsigned_abs()))
    } else {
        current_pos.saturating_add(Duration::from_secs(delta_secs as u64))
    };
    seek_to(call, extra_args, target).await
}
