//! Per-guild Now Playing panel and its control buttons.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use poise::serenity_prelude as serenity;
use songbird::tracks::{PlayMode, Track};
use songbird::{Call, Event, EventContext, EventHandler as SongbirdEventHandler};
use tokio::sync::Mutex;

use crate::audio_source::FfmpegEqSource;

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
}

/// Best-effort thumbnail URL derived from a YouTube video ID. Returns `None`
/// for non-YouTube URLs or unrecognised URL shapes.
pub(crate) fn derive_youtube_thumbnail(url: &str) -> Option<String> {
    let id = youtube_video_id(url)?;
    Some(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"))
}

fn youtube_video_id(url: &str) -> Option<String> {
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

/// Width (in characters) of the text progress bar `now_playing_embed` renders.
const PROGRESS_BAR_LENGTH: usize = 20;

/// Renders a `████────` -style text progress bar for `position` out of
/// `total`. A zero (or otherwise degenerate) `total` renders an empty bar.
fn progress_bar(position: Duration, total: Duration) -> String {
    let ratio = if total.as_secs_f64() > 0.0 {
        (position.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = ((PROGRESS_BAR_LENGTH as f64) * ratio) as usize;
    let filled = filled.min(PROGRESS_BAR_LENGTH);
    format!(
        "{}{}",
        "█".repeat(filled),
        "─".repeat(PROGRESS_BAR_LENGTH - filled)
    )
}

/// How often the Now Playing panel is re-rendered while a track plays, so
/// its progress bar visibly advances.
const PROGRESS_TICK_INTERVAL: Duration = Duration::from_secs(10);

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

/// Which songbird event a `PanelUpdater` instance is registered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelUpdaterKind {
    Play,
    End,
}

/// Songbird global event handler that keeps the Now Playing panel in sync
/// with the call's queue. Registered once per guild for `TrackEvent::Play`
/// and once for `TrackEvent::End`.
pub struct PanelUpdater {
    pub http: Arc<serenity::Http>,
    pub guild_id: serenity::GuildId,
    pub guild_players: Arc<GuildPlayers>,
    pub call: Arc<Mutex<Call>>,
    /// Needed to leave the call from `sync_idle_leave_task`'s spawned timer.
    pub manager: Arc<songbird::Songbird>,
    pub kind: PanelUpdaterKind,
}

#[async_trait::async_trait]
impl SongbirdEventHandler for PanelUpdater {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        refresh_panel(&self.http, self.guild_id, &self.guild_players, &self.call).await;
        match self.kind {
            PanelUpdaterKind::Play => {
                ensure_progress_ticker(
                    self.http.clone(),
                    self.guild_id,
                    self.guild_players.clone(),
                    self.call.clone(),
                )
                .await;
            }
            PanelUpdaterKind::End => {
                if let Some(state_arc) = self.guild_players.get(&self.guild_id).map(|e| e.clone()) {
                    stop_progress_ticker(&mut *state_arc.lock().await);
                }
            }
        }
        sync_idle_leave_task(
            self.http.clone(),
            self.guild_id,
            self.guild_players.clone(),
            self.manager.clone(),
            &self.call,
        )
        .await;
        None
    }
}

/// Starts or cancels `GuildPlayerState::idle_leave_task` to match whether
/// the call's queue is currently empty.
pub(crate) async fn sync_idle_leave_task(
    http: Arc<serenity::Http>,
    guild_id: serenity::GuildId,
    guild_players: Arc<GuildPlayers>,
    manager: Arc<songbird::Songbird>,
    call: &Arc<Mutex<Call>>,
) {
    let is_idle = call.lock().await.queue().current().is_none();
    let Some(state_arc) = guild_players.get(&guild_id).map(|e| e.clone()) else {
        return;
    };
    let mut state = state_arc.lock().await;

    if !is_idle {
        stop_idle_leave_task(&mut state);
        return;
    }
    if state
        .idle_leave_task
        .as_ref()
        .is_some_and(|t| !t.is_finished())
    {
        return;
    }
    tracing::info!(
        "Queue idle in guild {guild_id}; will auto-leave in {}s if nothing is queued by then.",
        IDLE_QUEUE_LEAVE_DELAY.as_secs()
    );
    state.idle_leave_task = Some(tokio::spawn(async move {
        tokio::time::sleep(IDLE_QUEUE_LEAVE_DELAY).await;
        let Some(call) = manager.get(guild_id) else {
            tracing::info!(
                "Idle-leave timer fired for guild {guild_id}, but there's no active call any more; nothing to do."
            );
            return;
        };
        if call.lock().await.queue().current().is_none() {
            tracing::info!("Idle-leave timer fired for guild {guild_id}; still idle, leaving now.");
            cleanup_guild(&http, guild_id, &guild_players, &manager).await;
        } else {
            tracing::info!(
                "Idle-leave timer fired for guild {guild_id}, but a track is playing again; staying."
            );
        }
    }));
}

/// Cancels `GuildPlayerState::idle_leave_task`, if running.
pub(crate) fn stop_idle_leave_task(state: &mut GuildPlayerState) {
    if let Some(task) = state.idle_leave_task.take() {
        abort_unless_self(task);
    }
}

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
fn abort_unless_self(task: tokio::task::JoinHandle<()>) {
    if tokio::task::try_id().is_some_and(|id| id == task.id()) {
        drop(task);
    } else {
        task.abort();
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

/// Starts `GuildPlayerState::progress_ticker` if it isn't already running.
async fn ensure_progress_ticker(
    http: Arc<serenity::Http>,
    guild_id: serenity::GuildId,
    guild_players: Arc<GuildPlayers>,
    call: Arc<Mutex<Call>>,
) {
    let Some(state_arc) = guild_players.get(&guild_id).map(|e| e.clone()) else {
        return;
    };
    let mut state = state_arc.lock().await;
    if state
        .progress_ticker
        .as_ref()
        .is_some_and(|t| !t.is_finished())
    {
        return;
    }
    state.progress_ticker = Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(PROGRESS_TICK_INTERVAL);
        // Skip the immediate first tick; `Play` already triggered a refresh.
        interval.tick().await;
        loop {
            interval.tick().await;
            refresh_panel(&http, guild_id, &guild_players, &call).await;
            let still_playing = call.lock().await.queue().current().is_some();
            if !still_playing {
                break;
            }
        }
    }));
}

/// Cancels `GuildPlayerState::progress_ticker`, if running.
pub(crate) fn stop_progress_ticker(state: &mut GuildPlayerState) {
    if let Some(task) = state.progress_ticker.take() {
        abort_unless_self(task);
    }
}

/// Songbird global event handler, registered for `TrackEvent::Error`, that
/// tells the text channel when a track failed to play.
pub struct PlaybackErrorNotifier {
    pub http: Arc<serenity::Http>,
    pub guild_id: serenity::GuildId,
    pub guild_players: Arc<GuildPlayers>,
}

#[async_trait::async_trait]
impl SongbirdEventHandler for PlaybackErrorNotifier {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let EventContext::Track(track_ctx) = ctx else {
            return None;
        };

        let state_arc = self
            .guild_players
            .get(&self.guild_id)
            .map(|entry| entry.clone())?;
        let channel_id = state_arc.lock().await.text_channel?;

        for (state, handle) in *track_ctx {
            let title = handle.data::<TrackMeta>().title.clone();
            let detail = match &state.playing {
                PlayMode::Errored(e) => e.to_string(),
                other => format!("{other:?}"),
            };
            tracing::warn!(
                "Playback error for '{title}' in guild {}: {detail}",
                self.guild_id
            );
            let _ = channel_id
                .say(
                    &self.http,
                    format!("❌ Playback error: Skipping **{title}**."),
                )
                .await;
        }

        None
    }
}

/// Songbird global event handler, registered for `TrackEvent::End`, that
/// implements both `LoopMode::Queue` and `LoopMode::Track` by re-enqueueing
/// a fresh copy of the ended track (a songbird `Track` can't be replayed
/// once finished, and -- see below -- `FfmpegEqSource`'s raw PCM stream
/// can't be native-seeked back to the start either). `Queue` appends the
/// copy to the back; `Track` swaps it back into the front so the same song
/// repeats before the next queued one plays. Skips a track that ended in
/// `PlayMode::Errored`, so a permanently-broken URL doesn't loop forever.
///
/// `LoopMode::Track` deliberately does *not* use songbird's native
/// `TrackHandle::enable_loop()`. That mechanism restarts a track by seeking
/// its `Input` back to position 0, but `FfmpegEqSource`'s output is wrapped
/// in `ReadOnlySource`, which reports `is_seekable() == false`
/// unconditionally -- so the native in-place loop's own seek-to-0 always
/// fails, and songbird marks the track `PlayMode::Errored` instead of
/// looping it. Rebuilding a fresh `Track` (the same trick `seek_to` already
/// uses to work around this source's lack of native seek support) sidesteps
/// the problem entirely: each loop iteration is a brand new `Track`/`Input`,
/// so `TrackState::position` naturally starts back at 0 with no special
/// correction needed anywhere else.
pub struct LoopHandler {
    pub extra_args: Vec<String>,
    pub guild_id: serenity::GuildId,
    pub guild_players: Arc<GuildPlayers>,
    pub call: Arc<Mutex<Call>>,
}

#[async_trait::async_trait]
impl SongbirdEventHandler for LoopHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let EventContext::Track(track_ctx) = ctx else {
            return None;
        };

        let state_arc = self
            .guild_players
            .get(&self.guild_id)
            .map(|entry| entry.clone())?;
        let loop_mode = state_arc.lock().await.loop_mode;
        if loop_mode == LoopMode::Off {
            return None;
        }

        for (state, handle) in *track_ctx {
            if matches!(state.playing, PlayMode::Errored(_)) {
                continue;
            }
            let meta = handle.data::<TrackMeta>();

            // A `seek_to` swap also ends the old track; that's not a real
            // end-of-track and must not produce a duplicate re-enqueue.
            let is_seek_replacement =
                self.call
                    .lock()
                    .await
                    .queue()
                    .current()
                    .is_some_and(|current| {
                        let current_meta = current.data::<TrackMeta>();
                        current_meta.is_seek && current_meta.url == meta.url
                    });
            if is_seek_replacement {
                continue;
            }

            let source = FfmpegEqSource {
                url: meta.url.clone(),
                ytdlp_extra_args: self.extra_args.clone(),
                eq_filter: meta.eq_filter.clone(),
                seek_time: None,
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
                start_offset: Duration::ZERO,
                is_seek: false,
            });
            let new_track = Track::new_with_data(source.into(), new_meta).volume(state.volume);

            match loop_mode {
                LoopMode::Queue => {
                    self.call.lock().await.enqueue(new_track).await;
                }
                LoopMode::Track => {
                    // By the time this handler runs, songbird's own internal
                    // queue advancement has already popped the ended track
                    // and started whatever was next (if anything). Swap our
                    // fresh copy back into the front and stop that
                    // already-started track, so the same song repeats
                    // before the queue moves on.
                    let mut call = self.call.lock().await;
                    let new_handle = call.enqueue(new_track).await;
                    let displaced = call.queue().modify_queue(|tracks| {
                        let pos = tracks.iter().position(|q| q.uuid() == new_handle.uuid());
                        let new_queued = pos.and_then(|i| tracks.remove(i));
                        let displaced = tracks.pop_front();
                        if let Some(new_queued) = new_queued {
                            tracks.push_front(new_queued);
                        }
                        displaced
                    });
                    if let Some(displaced) = displaced
                        && displaced.uuid() != new_handle.uuid()
                    {
                        let _ = displaced.stop();
                        let _ = new_handle.play();
                    }
                }
                LoopMode::Off => unreachable!("filtered out above"),
            }
        }

        None
    }
}

/// Rebuilds and posts/edits the Now Playing panel for a guild from its
/// call's current queue state. Always edits the existing message in place,
/// sending a fresh one only if there is none yet.
pub async fn refresh_panel(
    http: &serenity::Http,
    guild_id: serenity::GuildId,
    guild_players: &GuildPlayers,
    call: &Arc<Mutex<Call>>,
) {
    let Some(state_arc) = guild_players.get(&guild_id).map(|entry| entry.clone()) else {
        return;
    };
    let mut state = state_arc.lock().await;
    let Some(channel_id) = state.text_channel else {
        return;
    };

    let snapshot = call.lock().await.queue().current_queue();
    let current = snapshot.first();
    let next_title = snapshot
        .get(1)
        .map(|handle| handle.data::<TrackMeta>().title.clone());
    // No play history is tracked, so the current track is always shown as
    // position 1 of the queue's remaining length.
    let queue_position = 1usize;
    let queue_total = snapshot.len().max(1);
    let loop_mode = state.loop_mode;

    let (embed, components) = match current {
        Some(handle) => {
            let meta = handle.data::<TrackMeta>();
            let track_state = handle.get_info().await.ok();
            (
                now_playing_embed(
                    &meta,
                    track_state.as_ref(),
                    loop_mode,
                    next_title.as_deref(),
                    queue_position,
                    queue_total,
                ),
                control_panel_components(track_state.as_ref(), loop_mode),
            )
        }
        None => (
            finished_embed(),
            disabled_control_panel_components(loop_mode),
        ),
    };

    match state.now_playing_message {
        Some(message_id) => {
            let edit = serenity::EditMessage::new()
                .embed(embed)
                .components(components);
            if channel_id
                .edit_message(http, message_id, edit)
                .await
                .is_err()
            {
                state.now_playing_message = None;
            }
        }
        None => {
            let msg = serenity::CreateMessage::new()
                .embed(embed)
                .components(components);
            if let Ok(sent) = channel_id.send_message(http, msg).await {
                state.now_playing_message = Some(sent.id);
            }
        }
    }
}

/// Tears down a guild's voice connection and panel state: cancels pending
/// timers, edits the panel to a goodbye message, drops the `Call`, and
/// removes the `GuildPlayerState` entry. Shared by `/leave`, the
/// bot-was-disconnected detector, and the auto-leave timers.
///
/// Returns `true` if there was anything to clean up.
pub async fn cleanup_guild(
    http: &serenity::Http,
    guild_id: serenity::GuildId,
    guild_players: &GuildPlayers,
    manager: &songbird::Songbird,
) -> bool {
    let state_arc = guild_players.remove(&guild_id).map(|(_, v)| v);
    let had_call = manager.get(guild_id).is_some();

    if let Some(state_arc) = &state_arc {
        let mut state = state_arc.lock().await;
        if let Some(task) = state.empty_channel_leave_task.take() {
            abort_unless_self(task);
        }
        stop_progress_ticker(&mut state);
        stop_idle_leave_task(&mut state);
        if let (Some(message_id), Some(channel_id)) =
            (state.now_playing_message.take(), state.text_channel)
        {
            let edit = serenity::EditMessage::new()
                .embed(goodbye_embed())
                .components(Vec::new());
            let _ = channel_id.edit_message(http, message_id, edit).await;
        }
    }

    if had_call {
        // On error, `Songbird::remove` leaves its bookkeeping entry behind
        // even though local audio has already stopped; log it so a stuck
        // "looks disconnected but isn't" state is visible.
        if let Err(e) = manager.remove(guild_id).await {
            tracing::warn!(
                "Failed to fully leave voice in guild {guild_id} (local audio already stopped, \
                 but the Discord-side leave may not have gone through): {e}"
            );
        } else {
            tracing::info!("Left voice in guild {guild_id}.");
        }
    }

    had_call || state_arc.is_some()
}

fn goodbye_embed() -> serenity::CreateEmbed {
    serenity::CreateEmbed::new()
        .title("👋 See you!")
        .description("Thanks for using the bot.")
        .color(serenity::Colour::DARK_GREY)
}

/// Reacts to a human joining/leaving the bot's voice channel (auto-leave
/// when left alone, cancel on rejoin) and to the bot's own voice state
/// changing unexpectedly (kicked, connection dropped).
pub async fn handle_voice_state_update(
    ctx: &serenity::Context,
    guild_players: &Arc<GuildPlayers>,
    old: &Option<serenity::VoiceState>,
    new: &serenity::VoiceState,
) -> Result<(), crate::Error> {
    let Some(guild_id) = new.guild_id else {
        return Ok(());
    };
    let Some(manager) = songbird::get(ctx).await else {
        return Ok(());
    };
    let bot_id = ctx.cache.current_user().id;

    if new.user_id == bot_id {
        // Only treat this as a real disconnect if songbird also agrees the
        // call is gone, so an internal reconnect isn't mistaken for one.
        let was_connected = old.as_ref().is_some_and(|vs| vs.channel_id.is_some());
        if was_connected && new.channel_id.is_none() && manager.get(guild_id).is_none() {
            tracing::info!("Bot was disconnected from voice in guild {guild_id}; cleaning up.");
            cleanup_guild(&ctx.http, guild_id, guild_players, &manager).await;
        }
        return Ok(());
    }

    if manager.get(guild_id).is_none() {
        tracing::trace!(
            "handle_voice_state_update: guild {guild_id} has no active call; ignoring human voice state change."
        );
        return Ok(());
    }
    let Some(bot_channel_id) = ctx
        .cache
        .guild(guild_id)
        .and_then(|guild| guild.voice_states.get(&bot_id)?.channel_id)
    else {
        tracing::warn!(
            "handle_voice_state_update: guild {guild_id}'s cache has no voice state for the bot itself, even though songbird reports an active call -- skipping this event (auto-leave/auto-pause logic won't run)."
        );
        return Ok(());
    };

    // How many non-bot humans are in the bot's channel, and whether every
    // one of them is deafened; feeds both blocks below.
    let (human_count, all_deafened) = ctx
        .cache
        .guild(guild_id)
        .map(|guild| {
            let humans: Vec<bool> = guild
                .voice_states
                .values()
                .filter(|vs| {
                    vs.channel_id == Some(bot_channel_id)
                        && vs.user_id != bot_id
                        && vs.member.as_ref().is_none_or(|m| !m.user.bot)
                })
                .map(|vs| vs.self_deaf || vs.deaf)
                .collect();
            let count = humans.len();
            let all_deaf = count > 0 && humans.into_iter().all(|deafened| deafened);
            (count, all_deaf)
        })
        .unwrap_or((0, false));
    tracing::debug!(
        "handle_voice_state_update: guild {guild_id}, bot_channel_id={bot_channel_id}, human_count={human_count}, all_deafened={all_deafened}"
    );

    let Some(state_arc) = guild_players.get(&guild_id).map(|entry| entry.clone()) else {
        tracing::warn!(
            "handle_voice_state_update: guild {guild_id} has an active call but no GuildPlayerState -- skipping (this shouldn't normally happen once `ensure_call_raw` has run)."
        );
        return Ok(());
    };

    if human_count == 0 {
        let mut state = state_arc.lock().await;
        let needs_new_timer = state
            .empty_channel_leave_task
            .as_ref()
            .is_none_or(|t| t.is_finished());
        if needs_new_timer {
            tracing::info!(
                "guild {guild_id} is now empty of humans; leaving in {}s unless someone rejoins.",
                EMPTY_CHANNEL_LEAVE_DELAY.as_secs()
            );
            if let Some(text_channel) = state.text_channel {
                let _ = text_channel
                    .say(
                        &ctx.http,
                        "Everyone left... I'll leave automatically in 30 seconds.",
                    )
                    .await;
            }
            let http = ctx.http.clone();
            let guild_players = guild_players.clone();
            let manager = manager.clone();
            state.empty_channel_leave_task = Some(tokio::spawn(async move {
                tokio::time::sleep(EMPTY_CHANNEL_LEAVE_DELAY).await;
                if manager.get(guild_id).is_some() {
                    tracing::info!(
                        "Empty-channel timer fired for guild {guild_id}; still connected with no one there, leaving now."
                    );
                    cleanup_guild(&http, guild_id, &guild_players, &manager).await;
                } else {
                    tracing::info!(
                        "Empty-channel timer fired for guild {guild_id}, but songbird no longer reports an active call; nothing to do."
                    );
                }
            }));
        }
    } else {
        let mut state = state_arc.lock().await;
        if let Some(task) = state.empty_channel_leave_task.take()
            && !task.is_finished()
        {
            abort_unless_self(task);
            if let Some(text_channel) = state.text_channel {
                let _ = text_channel
                    .say(&ctx.http, "Welcome back! Canceling auto-leave.")
                    .await;
            }
        }
    }

    // Silence auto-pause/resume: pause when every listener is deafened,
    // resume the moment at least one isn't -- but only ever resume a pause
    // this logic itself caused.
    if human_count > 0 {
        let should_be_paused = {
            let mut state = state_arc.lock().await;
            if all_deafened && !state.silence_auto_paused {
                state.silence_auto_paused = true;
                Some(true)
            } else if !all_deafened && state.silence_auto_paused {
                state.silence_auto_paused = false;
                Some(false)
            } else {
                None
            }
        };

        if let Some(pause) = should_be_paused
            && let Some(call) = manager.get(guild_id)
        {
            let result = {
                let call = call.lock().await;
                if pause {
                    call.queue().pause()
                } else {
                    call.queue().resume()
                }
            };
            match result {
                Ok(()) => {
                    tracing::info!(
                        "{} guild {guild_id}: all listeners deafened = {all_deafened}",
                        if pause { "Auto-paused" } else { "Auto-resumed" }
                    );
                    refresh_panel(&ctx.http, guild_id, guild_players, &call).await;
                }
                Err(e) => tracing::warn!(
                    "Failed to auto {} guild {guild_id}: {e}",
                    if pause { "pause" } else { "resume" }
                ),
            }
        }
    }

    Ok(())
}

/// Builds the Now Playing panel's embed.
fn now_playing_embed(
    meta: &TrackMeta,
    state: Option<&songbird::tracks::TrackState>,
    loop_mode: LoopMode,
    next_title: Option<&str>,
    queue_position: usize,
    queue_total: usize,
) -> serenity::CreateEmbed {
    let is_paused = state.is_some_and(|s| matches!(s.playing, PlayMode::Pause));
    let status_icon = if is_paused {
        "⏸️ Paused"
    } else {
        "▶️ Playing"
    };
    let uploader = meta.uploader.as_deref().unwrap_or("Unknown Artist");

    // A zero-width space holds open a blank line Discord would otherwise collapse.
    let description = format!(
        "**By:** {uploader}\n\u{200b}\n{status_icon}:\n**{}**\n[[Link to YouTube]]({})",
        meta.title, meta.url
    );

    let position = state.map_or(Duration::ZERO, |s| s.position) + meta.start_offset;
    let total = meta.duration.unwrap_or_default();
    let progress_value = format!(
        "\u{200b}\n`{}`\u{200b}`   {}   `\u{200b}`{}`",
        format_duration(position),
        progress_bar(position, total),
        format_duration(total)
    );

    let volume_pct = state.map_or(100, |s| (s.volume * 100.0).round() as i64);
    let mut footer = format!(
        "Requested by: {} | 🎶 Queue: {queue_position}/{queue_total} | 🔊 Volume: {volume_pct}%",
        meta.requester_name
    );
    match loop_mode {
        LoopMode::Off => {}
        LoopMode::Track => footer.push_str(" | 🔁 Loop: Track"),
        LoopMode::Queue => footer.push_str(" | 🔁 Loop: Queue"),
    }
    if meta.eq_filter.is_some() {
        footer.push_str(" | 🎧 Hi-Fi");
    }

    let mut embed = serenity::CreateEmbed::new()
        .title("Now playing music:")
        .description(description)
        .field("", progress_value, false)
        .footer(serenity::CreateEmbedFooter::new(footer))
        .color(serenity::Colour::new(0x57_F287));

    if let Some(next_title) = next_title {
        embed = embed.field("⏭️ Up Next", next_title, false);
    }
    if let Some(thumbnail) = &meta.thumbnail {
        embed = embed.thumbnail(thumbnail);
    }

    embed
}

fn finished_embed() -> serenity::CreateEmbed {
    serenity::CreateEmbed::new()
        .title("⏹️ Playback Finished")
        .description("Thanks for listening!")
        .color(serenity::Colour::DARK_GREY)
}

pub(crate) fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    format!("{:02}:{:02}", total / 60, total % 60)
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

/// Custom IDs used by the Now Playing panel's buttons.
pub mod custom_id {
    pub const PAUSE_RESUME: &str = "np:pause_resume";
    pub const SKIP: &str = "np:skip";
    pub const LOOP: &str = "np:loop";
    pub const SEEK_BACK: &str = "np:seek_back";
    pub const SEEK_FORWARD: &str = "np:seek_forward";
    pub const SHUFFLE: &str = "np:shuffle";
}

/// How far the panel's ⏪/⏩ buttons seek per press, in seconds.
const PANEL_SEEK_STEP_SECS: i64 = 10;

/// Panel button layout: row 1 is the transport trio (rewind / pause-resume /
/// fast-forward), row 2 is queue management (loop / skip / shuffle).
fn control_panel_components(
    state: Option<&songbird::tracks::TrackState>,
    loop_mode: LoopMode,
) -> Vec<serenity::CreateActionRow> {
    let is_paused = state.is_some_and(|s| matches!(s.playing, PlayMode::Pause));

    vec![
        serenity::CreateActionRow::Buttons(vec![
            serenity::CreateButton::new(custom_id::SEEK_BACK)
                .emoji('⏪')
                .label(format!("{PANEL_SEEK_STEP_SECS}s"))
                .style(serenity::ButtonStyle::Secondary),
            serenity::CreateButton::new(custom_id::PAUSE_RESUME)
                .emoji(if is_paused { '▶' } else { '⏸' })
                .style(serenity::ButtonStyle::Primary),
            serenity::CreateButton::new(custom_id::SEEK_FORWARD)
                .emoji('⏩')
                .label(format!("{PANEL_SEEK_STEP_SECS}s"))
                .style(serenity::ButtonStyle::Secondary),
        ]),
        serenity::CreateActionRow::Buttons(vec![
            serenity::CreateButton::new(custom_id::LOOP)
                .emoji('🔁')
                .label(loop_mode.label())
                .style(if loop_mode == LoopMode::Off {
                    serenity::ButtonStyle::Secondary
                } else {
                    serenity::ButtonStyle::Success
                }),
            serenity::CreateButton::new(custom_id::SKIP)
                .emoji('⏭')
                .style(serenity::ButtonStyle::Secondary),
            serenity::CreateButton::new(custom_id::SHUFFLE)
                .emoji('🔀')
                .style(serenity::ButtonStyle::Secondary),
        ]),
    ]
}

fn disabled_control_panel_components(loop_mode: LoopMode) -> Vec<serenity::CreateActionRow> {
    vec![
        serenity::CreateActionRow::Buttons(vec![
            serenity::CreateButton::new(custom_id::SEEK_BACK)
                .emoji('⏪')
                .label(format!("{PANEL_SEEK_STEP_SECS}s"))
                .style(serenity::ButtonStyle::Secondary)
                .disabled(true),
            serenity::CreateButton::new(custom_id::PAUSE_RESUME)
                .emoji('⏸')
                .style(serenity::ButtonStyle::Primary)
                .disabled(true),
            serenity::CreateButton::new(custom_id::SEEK_FORWARD)
                .emoji('⏩')
                .label(format!("{PANEL_SEEK_STEP_SECS}s"))
                .style(serenity::ButtonStyle::Secondary)
                .disabled(true),
        ]),
        serenity::CreateActionRow::Buttons(vec![
            serenity::CreateButton::new(custom_id::LOOP)
                .emoji('🔁')
                .label(loop_mode.label())
                .style(serenity::ButtonStyle::Secondary)
                .disabled(true),
            serenity::CreateButton::new(custom_id::SKIP)
                .emoji('⏭')
                .style(serenity::ButtonStyle::Secondary)
                .disabled(true),
            serenity::CreateButton::new(custom_id::SHUFFLE)
                .emoji('🔀')
                .style(serenity::ButtonStyle::Secondary)
                .disabled(true),
        ]),
    ]
}

/// Handles a press of one of the Now Playing panel's buttons.
pub async fn handle_component_interaction(
    ctx: &serenity::Context,
    interaction: &serenity::ComponentInteraction,
    guild_players: &Arc<GuildPlayers>,
    ytdlp_extra_args: &[String],
) -> Result<(), crate::Error> {
    let custom_id = interaction.data.custom_id.as_str();
    if !matches!(
        custom_id,
        custom_id::PAUSE_RESUME
            | custom_id::SKIP
            | custom_id::LOOP
            | custom_id::SEEK_BACK
            | custom_id::SEEK_FORWARD
            | custom_id::SHUFFLE
    ) {
        return Ok(());
    }

    let Some(guild_id) = interaction.guild_id else {
        return Ok(());
    };

    // Silent per-guild cooldown: a press within `BUTTON_COOLDOWN` of the
    // last one is acknowledged and dropped, to avoid hammering the API.
    const BUTTON_COOLDOWN: Duration = Duration::from_millis(1500);
    if let Some(state_arc) = guild_players.get(&guild_id).map(|entry| entry.clone()) {
        let mut state = state_arc.lock().await;
        let now = std::time::Instant::now();
        let on_cooldown = state
            .last_button_press
            .is_some_and(|last| now.duration_since(last) < BUTTON_COOLDOWN);
        if on_cooldown {
            drop(state);
            interaction
                .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
                .await?;
            return Ok(());
        }
        state.last_button_press = Some(now);
    }

    interaction
        .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
        .await?;

    let Some(manager) = songbird::get(ctx).await else {
        return Ok(());
    };
    let Some(call) = manager.get(guild_id) else {
        return Ok(());
    };

    // Resolved before locking `call` below, so the DashMap guard from
    // `.get()` doesn't overlap with it.
    let new_loop_mode = if custom_id == custom_id::LOOP {
        let state_arc = guild_players.get(&guild_id).map(|entry| entry.clone());
        match state_arc {
            Some(state_arc) => {
                let mut state = state_arc.lock().await;
                state.loop_mode = state.loop_mode.cycle();
                Some(state.loop_mode)
            }
            None => None,
        }
    } else {
        None
    };

    // Seek buttons lock `call` themselves via `seek_by`, so they're handled
    // outside the `call_lock` block below.
    let ephemeral_note = if matches!(custom_id, custom_id::SEEK_BACK | custom_id::SEEK_FORWARD) {
        let delta_secs = if custom_id == custom_id::SEEK_BACK {
            -PANEL_SEEK_STEP_SECS
        } else {
            PANEL_SEEK_STEP_SECS
        };
        seek_by(&call, ytdlp_extra_args, delta_secs).await.err()
    } else {
        let call_lock = call.lock().await;
        let queue = call_lock.queue();

        match custom_id {
            custom_id::PAUSE_RESUME => match queue.current() {
                Some(handle) => {
                    let is_paused = matches!(
                        handle.get_info().await.map(|s| s.playing),
                        Ok(PlayMode::Pause)
                    );
                    let result = if is_paused {
                        queue.resume()
                    } else {
                        queue.pause()
                    };
                    result.err().map(|e| format!("⚠️ {e}"))
                }
                None => Some("Nothing is playing.".to_string()),
            },
            custom_id::SKIP => {
                if queue.current().is_none() {
                    Some("Nothing to skip.".to_string())
                } else {
                    queue.skip().err().map(|e| format!("⚠️ {e}"))
                }
            }
            custom_id::SHUFFLE => {
                use rand::seq::SliceRandom;
                let shuffled_count = queue.modify_queue(|tracks| {
                    let tail_len = tracks.len().saturating_sub(1);
                    if tail_len > 1 {
                        tracks.make_contiguous()[1..].shuffle(&mut rand::thread_rng());
                    }
                    tail_len
                });
                if shuffled_count == 0 {
                    Some("There's nothing queued up next to shuffle.".to_string())
                } else {
                    None
                }
            }
            custom_id::LOOP => {
                // No native songbird call needed -- `LoopHandler` reads
                // `GuildPlayerState::loop_mode` (already updated above)
                // directly whenever a track ends.
                let mode = new_loop_mode.unwrap_or_default();
                Some(format!("🔁 {}", mode.label()))
            }
            _ => unreachable!(),
        }
    };

    if let Some(note) = ephemeral_note {
        let followup = serenity::CreateInteractionResponseFollowup::new()
            .content(note)
            .ephemeral(true);
        let _ = interaction.create_followup(ctx, followup).await;
    }

    // Pause/resume and loop-toggle emit no `TrackEvent`, so refresh here
    // explicitly; skip/stop also trigger the global `End` handler, making
    // this a harmless, idempotent no-op race in that case.
    let http = ctx.http.clone();
    refresh_panel(&http, guild_id, guild_players, &call).await;

    Ok(())
}
