//! Layer 0: shared per-guild player state, plus the small timer-control
//! helpers everything else in `player` is built on. Depends on nothing else
//! in this module tree.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use poise::serenity_prelude as serenity;
use tokio::sync::Mutex;

/// Metadata attached to every enqueued track via `Track::new_with_data`, so
/// the queue and panel can display it without re-querying yt-dlp.
#[derive(Debug, Clone)]
pub struct TrackMeta {
    pub title: String,
    pub url: String,
    pub requested_by: serenity::UserId,
    pub duration: Option<Duration>,
    /// This track's resolved EQ filter (`None` = Balanced, `Some` = Hi-Fi's
    /// `-af` string), snapshotted at enqueue time.
    pub eq_filter: Option<String>,
    /// Best-effort thumbnail URL, if derivable. See `derive_youtube_thumbnail`.
    pub thumbnail: Option<String>,
    /// Uploader/channel name, if known; shown as "By: {uploader}" on the panel.
    pub uploader: Option<String>,
    /// Display-name snapshot of whoever queued this track, for the panel footer.
    pub requester_name: String,
    /// Playback offset already elapsed before this track's `Input` starts
    /// (non-zero only for a `seek_to` replacement). Real position is always
    /// `state.position + start_offset`.
    pub start_offset: Duration,
    /// Whether this is a `seek_to` replacement track rather than a genuinely
    /// new one. `LoopHandler` uses this to avoid re-enqueueing a duplicate.
    pub is_seek: bool,
    /// Whether this track is `LoopMode::Track`'s pre-emptive duplicate of
    /// the track ahead of it in the queue (see `ensure_track_loop_clone`),
    /// rather than a genuinely distinct queue entry. Keeps queue index 1
    /// reserved/identifiable for `remove_track_loop_clone` and the
    /// `/shuffle`/`/playnext` offset checks, and drives the "(Loop)"
    /// annotation in `/queue`.
    pub is_loop_clone: bool,
}

/// How often the Now Playing panel is re-rendered while a track plays, so
/// its progress bar visibly advances.
pub const PROGRESS_TICK_INTERVAL: Duration = Duration::from_secs(15);

/// How long the bot waits, after everyone leaves its voice channel, before
/// auto-leaving.
pub const EMPTY_CHANNEL_LEAVE_DELAY: Duration = Duration::from_secs(30);

/// How long the bot waits, after its queue drains to empty, before
/// auto-leaving (independent of `EMPTY_CHANNEL_LEAVE_DELAY`).
pub const IDLE_QUEUE_LEAVE_DELAY: Duration = Duration::from_secs(600);

/// Loop mode for a guild's playback. Neither mode uses songbird's native
/// per-track loop (`TrackHandle::enable_loop`) -- see `LoopHandler`'s doc
/// comment for why that doesn't work with this bot's custom audio source.
/// Both modes are implemented by `LoopHandler` re-enqueueing a fresh copy of
/// each track as it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopMode {
    #[default]
    Off,
    Track,
    Queue,
}

impl LoopMode {
    /// `Off -> Track -> Queue -> Off`.
    pub fn cycle(self) -> Self {
        match self {
            LoopMode::Off => LoopMode::Track,
            LoopMode::Track => LoopMode::Queue,
            LoopMode::Queue => LoopMode::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LoopMode::Off => "Loop: Off",
            LoopMode::Track => "Loop: Track",
            LoopMode::Queue => "Loop: Queue",
        }
    }
}

/// UI state tracked per guild.
#[derive(Default)]
pub struct GuildPlayerState {
    /// Where to post/update the Now Playing panel.
    pub text_channel: Option<serenity::ChannelId>,
    /// The currently displayed panel message, if any.
    pub now_playing_message: Option<serenity::MessageId>,
    /// Whether the Play/End global event handlers are already registered on
    /// this guild's `Call`, to avoid stacking duplicates.
    pub panel_events_registered: bool,
    /// Pending empty-channel auto-leave timer, if any.
    pub empty_channel_leave_task: Option<tokio::task::JoinHandle<()>>,
    /// This guild's current loop mode.
    pub loop_mode: LoopMode,
    /// Background task that periodically refreshes the panel while playing,
    /// so the progress bar advances between Play/End/button events.
    pub progress_ticker: Option<tokio::task::JoinHandle<()>>,
    /// Pending queue-idle auto-leave timer, if any.
    pub idle_leave_task: Option<tokio::task::JoinHandle<()>>,
    /// Whether this guild's queue is currently paused because every
    /// listener is deafened, so resume only ever reverts a pause this logic
    /// itself caused (never a user's manual pause).
    pub silence_auto_paused: bool,
    /// When a panel button press was last processed for this guild, for the
    /// 1.5s cooldown in `handle_component_interaction`.
    pub last_button_press: Option<std::time::Instant>,
}

/// Shared, guild-keyed player UI state, stored in `Data`.
///
/// Values are `Arc<Mutex<..>>` (not a bare `Mutex<..>`) so the DashMap
/// shard-lock guard can be dropped before awaiting the inner mutex.
pub type GuildPlayers = DashMap<serenity::GuildId, Arc<Mutex<GuildPlayerState>>>;

/// Aborts `task` unless it is the currently-executing task itself.
///
/// Needed because this module's auto-leave timers call `cleanup_guild`,
/// which aborts every pending timer in `GuildPlayerState` -- including,
/// when a timer's own closure is what's running, itself. A task cannot
/// meaningfully abort itself: Tokio only applies a self-abort at the task's
/// next `.await`, silently dropping it there with no error, which previously
/// made `cleanup_guild` stop partway through (e.g. never reaching
/// `manager.remove`, so the bot looked disconnected but wasn't). See
/// `abort_unless_self_tests` below for a regression test.
pub fn abort_unless_self(task: tokio::task::JoinHandle<()>) {
    if tokio::task::try_id().is_some_and(|id| id == task.id()) {
        drop(task);
    } else {
        task.abort();
    }
}

/// Cancels `GuildPlayerState::idle_leave_task`, if running.
pub fn stop_idle_leave_task(state: &mut GuildPlayerState) {
    if let Some(task) = state.idle_leave_task.take() {
        abort_unless_self(task);
    }
}

/// Cancels `GuildPlayerState::progress_ticker`, if running.
pub fn stop_progress_ticker(state: &mut GuildPlayerState) {
    if let Some(task) = state.progress_ticker.take() {
        abort_unless_self(task);
    }
}

#[cfg(test)]
mod abort_unless_self_tests {
    use super::abort_unless_self;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// A task that finds and aborts its own `JoinHandle` must keep running
    /// to completion instead of silently cancelling itself.
    #[tokio::test]
    async fn self_abort_does_not_cancel_the_calling_task() {
        let ran_to_completion = Arc::new(AtomicBool::new(false));
        let ran_to_completion_in_task = ran_to_completion.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<tokio::task::JoinHandle<()>>();

        let handle = tokio::spawn(async move {
            if let Ok(own_handle) = rx.await {
                abort_unless_self(own_handle);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            ran_to_completion_in_task.store(true, Ordering::SeqCst);
        });
        tx.send(handle)
            .expect("the task hasn't run far enough to drop its receiver yet");

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            ran_to_completion.load(Ordering::SeqCst),
            "task should have run to completion after (not) aborting its own handle"
        );
    }

    /// Sanity check: a plain, unguarded `.abort()` in the same scenario does
    /// genuinely cancel the calling task, confirming the test above exercises
    /// real self-cancellation.
    #[tokio::test]
    async fn plain_self_abort_does_cancel_the_calling_task() {
        let ran_to_completion = Arc::new(AtomicBool::new(false));
        let ran_to_completion_in_task = ran_to_completion.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<tokio::task::JoinHandle<()>>();

        let handle = tokio::spawn(async move {
            if let Ok(own_handle) = rx.await {
                own_handle.abort();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            ran_to_completion_in_task.store(true, Ordering::SeqCst);
        });
        tx.send(handle)
            .expect("the task hasn't run far enough to drop its receiver yet");

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !ran_to_completion.load(Ordering::SeqCst),
            "expected the unguarded self-abort to cancel the task before completion"
        );
    }
}
