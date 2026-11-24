//! Layer 1: derived track metadata (YouTube thumbnails), the
//! `LoopMode::Track` pre-emptive-clone helpers, and seek. Depends only on
//! `state` (for `TrackMeta`) and `crate::audio_source`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use songbird::Call;
use songbird::tracks::{Track, TrackQueue};
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
    // A direct `modify_queue` peek at index 1, rather than
    // `current_queue()` (which clones the *entire* queue into a fresh
    // `Vec` just to look at one slot), so this stays O(1) regardless of
    // queue length -- worth caring about since this runs on every
    // `LoopMode::Track` lap.
    let already_topped_up = call.queue().modify_queue(|tracks| {
        tracks
            .get(1)
            .is_some_and(|t| t.data::<TrackMeta>().is_loop_clone)
    });
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

/// Shuffles only the genuinely upcoming tracks (the queue tail) in place with
/// a Fisher-Yates pass, using a lightweight non-cryptographic PRNG instead of
/// pulling in `rand`. Returns the number of tracks in the shuffleable tail.
///
/// Index 1 is reserved for `LoopMode::Track`'s pre-emptive clone when one is
/// present (see `ensure_track_loop_clone`); it stays in place, as does the
/// currently playing track at index 0. Shared by `/shuffle` and the panel's
/// SHUFFLE button so their behaviour can't drift apart.
pub fn shuffle_queue_tail(queue: &TrackQueue) -> usize {
    queue.modify_queue(|tracks| {
        let start_idx = shuffle_start_index(
            tracks
                .get(1)
                .is_some_and(|t| t.data::<TrackMeta>().is_loop_clone),
        );
        let tail_len = tracks.len().saturating_sub(start_idx);
        if tail_len > 1 {
            shuffle_slice(&mut tracks.make_contiguous()[start_idx..]);
        }
        tail_len
    })
}

/// First index eligible for shuffling: skip index 0 (the current track) and,
/// when present, index 1 (the `LoopMode::Track` clone).
fn shuffle_start_index(has_loop_clone_at_1: bool) -> usize {
    if has_loop_clone_at_1 { 2 } else { 1 }
}

/// In-place Fisher-Yates over `items`, driven by `Xorshift64` seeded from the
/// wall clock. Randomness quality is irrelevant here -- this is queue
/// ordering, not cryptography -- and `rand` is too heavy a dependency for it.
fn shuffle_slice<T>(items: &mut [T]) {
    let mut rng = Xorshift64::from_system_time();
    for i in (1..items.len()).rev() {
        let j = rng.next_below(i as u64 + 1) as usize;
        items.swap(i, j);
    }
}

/// Fallback seed for `Xorshift64` when the wall clock is unavailable (or
/// lands exactly on the epoch).
const XORSHIFT_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Minimal `Xorshift64` PRNG; the shift triple and non-zero guard are from
/// Marsaglia's original formulation.
struct Xorshift64(u64);

impl Xorshift64 {
    fn from_system_time() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(XORSHIFT_SEED);
        Self(if nanos == 0 { XORSHIFT_SEED } else { nanos })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform-enough value in `[0, upper)` via Lemire's multiply-shift
    /// reduction (no division, negligible bias for small queues).
    fn next_below(&mut self, upper: u64) -> u64 {
        ((u128::from(self.next_u64()) * u128::from(upper)) >> 64) as u64
    }
}

/// Resolves the absolute seek destination for a track of `duration`, clamping
/// to `[0, duration]` when it is known. Unknown durations are rejected rather
/// than passed through: an `-ss` beyond EOF yields no PCM at all, so the
/// track would end immediately with no explanation.
fn resolve_seek_target(duration: Option<Duration>, target: Duration) -> Result<Duration, String> {
    match duration {
        Some(duration) => Ok(target.min(duration)),
        None => {
            Err("Cannot seek: this track's duration is unknown (e.g. a live stream).".to_string())
        }
    }
}

/// Seeks the currently playing track to an absolute position, by building a
/// new `Track` (via `FfmpegEqSource::seek_time`) and swapping it in for the
/// current one -- songbird's native seek doesn't support custom `Compose`
/// sources. Used by `/seek` and, via `seek_by`, the panel's ⏪/⏩ buttons.
///
/// Clamped to `[0, duration]`. Tracks whose duration is unknown (live
/// streams, or sources yt-dlp reported no duration for) are rejected: seeking
/// them could land past EOF and produce an empty PCM stream, which songbird
/// would surface as an immediate `End` rather than a usable position.
///
/// Returns the actual (clamped) position on success, or a user-facing error
/// string otherwise.
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
    let target = resolve_seek_target(meta.duration, target)?;
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

#[cfg(test)]
mod tests {
    use super::{resolve_seek_target, shuffle_slice, shuffle_start_index};
    use std::time::Duration;

    #[test]
    fn resolve_seek_target_clamps_to_a_known_duration() {
        let duration = Duration::from_secs(120);
        assert_eq!(
            resolve_seek_target(Some(duration), Duration::from_secs(30)),
            Ok(Duration::from_secs(30))
        );
        assert_eq!(
            resolve_seek_target(Some(duration), Duration::from_secs(300)),
            Ok(duration)
        );
    }

    #[test]
    fn resolve_seek_target_rejects_an_unknown_duration() {
        assert!(resolve_seek_target(None, Duration::from_secs(10)).is_err());
    }

    #[test]
    fn shuffle_start_index_reserves_current_and_loop_clone() {
        assert_eq!(shuffle_start_index(false), 1);
        assert_eq!(shuffle_start_index(true), 2);
    }

    #[test]
    fn shuffle_slice_leaves_the_guarded_prefix_and_permutes_only_the_tail() {
        let mut tracks = [10_u32, 20, 30, 40, 50, 60];
        let before = tracks;
        // `true` simulates a `LoopMode::Track` clone sitting at index 1.
        let start_idx = shuffle_start_index(true);

        shuffle_slice(&mut tracks[start_idx..]);

        assert_eq!(tracks[..start_idx], before[..start_idx]);
        let mut shuffled_tail = tracks[start_idx..].to_vec();
        shuffled_tail.sort_unstable();
        assert_eq!(shuffled_tail, before[start_idx..].to_vec());
    }

    #[test]
    fn shuffle_slice_is_a_no_op_for_a_zero_or_one_element_tail() {
        let mut empty: [u32; 0] = [];
        shuffle_slice(&mut empty);

        let mut single = [42_u32];
        shuffle_slice(&mut single);
        assert_eq!(single, [42]);
    }
}
