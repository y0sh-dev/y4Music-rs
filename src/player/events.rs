//! Layer 3: songbird global event handlers (`PanelUpdater`,
//! `PlaybackErrorNotifier`, `LoopHandler`), wired up per guild in
//! `commands::playback::ensure_call_raw`. Depends on `state`, `track`, and
//! `ui`.

use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use songbird::tracks::{PlayMode, Track};
use songbird::{Call, Event, EventContext, EventHandler as SongbirdEventHandler};
use tokio::sync::Mutex;

use crate::audio_source::FfmpegEqSource;

use super::state::{GuildPlayers, LoopMode, TrackMeta, stop_progress_ticker};
use super::track::ensure_track_loop_clone;
use super::ui::{ensure_progress_ticker, refresh_panel, sync_idle_leave_task};

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
/// copy to the back.
///
/// `Track` used to swap a fresh copy back into the front and stop whatever
/// songbird had already auto-advanced to, right here in the `End` handler.
/// That post-hoc splice raced with songbird's own forward-only queue
/// advancement -- which had *already* happened by the time this handler
/// runs -- and could drop whatever else was queued behind it. It's been
/// replaced by pre-emptive duplication (`ensure_track_loop_clone`): a
/// clone of the current track is kept queued one slot ahead at all times,
/// so by the time a track actually ends, the "repeat" is already next in
/// line and songbird's ordinary advancement handles it with no splicing
/// needed here at all -- this handler's only remaining job for `Track` is
/// to top up the clone for the *following* lap. Skips a track that ended
/// in `PlayMode::Errored`, so a permanently-broken URL doesn't loop
/// forever.
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

            match loop_mode {
                LoopMode::Queue => {
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
                        is_loop_clone: false,
                    });
                    let new_track =
                        Track::new_with_data(source.into(), new_meta).volume(state.volume);
                    self.call.lock().await.enqueue(new_track).await;
                }
                LoopMode::Track => {
                    // The clone `ensure_track_loop_clone` had already queued
                    // at index 1 is, by now, whatever songbird's ordinary
                    // forward-only advancement moved on to when this track
                    // ended -- no manual stop/splice needed. Just top up the
                    // *following* lap's clone so the invariant holds again.
                    ensure_track_loop_clone(&self.call, &self.extra_args).await;
                }
                LoopMode::Off => unreachable!("filtered out above"),
            }
        }

        None
    }
}
