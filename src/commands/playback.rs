//! `/join`, `/leave`, `/play`, `/stop`, `/skip`, `/queue`.
//!
//! Connects to voice through `songbird` and wires up the Now Playing panel
//! (see `crate::player`). `/play` only accepts a direct URL -- keyword
//! search is `/search`.

use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use songbird::input::{Compose, YoutubeDl};
use songbird::tracks::Track;
use songbird::{Call, Songbird};
use sqlx::SqlitePool;
use tokio::sync::Mutex as TokioMutex;

use crate::audio_source::FfmpegEqSource;
use crate::commands::profile::load_or_create_raw;
use crate::player::{PanelUpdater, PanelUpdaterKind, TrackMeta, derive_youtube_thumbnail};
use crate::{Context, Error};

/// Looks up `user_id`'s saved default volume and converts it to the `0.0
/// ..= 2.0` fraction `Track::volume` expects (100% == `1.0`).
async fn resolve_saved_volume(db: &SqlitePool, user_id: serenity::UserId) -> f32 {
    match load_or_create_raw(db, user_id.get() as i64).await {
        Ok(profile) => (profile.default_volume as f32 / 100.0).clamp(0.0, 2.0),
        Err(e) => {
            tracing::warn!("Failed to load saved volume for user {user_id}, using 100%: {e}");
            1.0
        }
    }
}

/// Looks up `user_id`'s saved `/profile eq` mode and resolves it to the
/// ffmpeg `-af` filtergraph to apply -- `None` for Balanced,
/// `Some(hifi_filter)` for Hi-Fi.
async fn resolve_eq_filter(
    db: &SqlitePool,
    user_id: serenity::UserId,
    hifi_filter: &str,
) -> Option<String> {
    match load_or_create_raw(db, user_id.get() as i64).await {
        Ok(profile) if profile.default_eq_mode == "hifi" => Some(hifi_filter.to_string()),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("Failed to load saved EQ mode for user {user_id}, using Balanced: {e}");
            None
        }
    }
}

/// Fetches the songbird voice manager that was installed on the client in
/// `main.rs` via `.register_songbird()`.
async fn songbird_manager(ctx: &Context<'_>) -> Result<Arc<Songbird>, Error> {
    songbird::get(ctx.serenity_context())
        .await
        .ok_or_else(|| "Voice system is not initialised.".into())
}

/// Finds the voice channel the invoking user is currently connected to, via
/// the serenity cache.
fn user_voice_channel(ctx: &Context<'_>) -> Result<serenity::ChannelId, Error> {
    let guild = ctx
        .guild()
        .ok_or("This command can only be used in a server.")?;

    guild
        .voice_states
        .get(&ctx.author().id)
        .and_then(|voice_state| voice_state.channel_id)
        .ok_or_else(|| "You must be in a voice channel first.".into())
}

/// Resolves `voice_channel_id`'s own configured bitrate cap (the value
/// shown in Discord's channel settings) -- preferring the cache, falling
/// back to a fresh HTTP fetch on a cache miss, and finally to songbird's
/// own default if that comes up empty too. Used so the Opus encoder never
/// exceeds what this specific channel (and thus Discord's SFU) actually
/// allows: unconditionally requesting `Bitrate::Max` ignored the channel's
/// negotiated cap and could make the server drop packets -- audible as
/// stutter -- on high-entropy material.
async fn resolve_target_bitrate(
    cache: &serenity::Cache,
    http: &serenity::Http,
    guild_id: serenity::GuildId,
    voice_channel_id: serenity::ChannelId,
) -> songbird::driver::Bitrate {
    let cached_bitrate = cache.guild(guild_id).and_then(|guild| {
        guild
            .channels
            .get(&voice_channel_id)
            .and_then(|c| c.bitrate)
    });

    let bps = match cached_bitrate {
        Some(bps) => Some(bps),
        None => match voice_channel_id.to_channel(http).await {
            Ok(serenity::Channel::Guild(guild_channel)) => guild_channel.bitrate,
            _ => None,
        },
    };

    bps.map(|bps| songbird::driver::Bitrate::Bits(bps as i32))
        .unwrap_or(songbird::constants::DEFAULT_BITRATE)
}

/// Raw-parameter version of `ensure_call`, for call sites without a poise
/// `Context`. Joins (or moves to) `voice_channel_id`, records
/// `post_channel_id` for the Now Playing panel, and registers the panel's
/// songbird event handlers once per guild.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn ensure_call_raw(
    http: Arc<serenity::Http>,
    cache: Arc<serenity::Cache>,
    extra_args: Vec<String>,
    guild_players: Arc<crate::player::GuildPlayers>,
    manager: Arc<Songbird>,
    guild_id: serenity::GuildId,
    voice_channel_id: serenity::ChannelId,
    post_channel_id: serenity::ChannelId,
) -> Result<Arc<TokioMutex<Call>>, Error> {
    let call = manager.join(guild_id, voice_channel_id).await?;

    // Resolved before taking *any* lock below -- it may need a network
    // round-trip on a cache miss, and neither `state`'s nor `call`'s lock
    // should sit held across that.
    let target_bitrate = resolve_target_bitrate(&cache, &http, guild_id, voice_channel_id).await;

    // Drop the DashMap guard before locking the inner tokio Mutex below.
    let state_arc = guild_players.entry(guild_id).or_default().clone();
    let mut state = state_arc.lock().await;
    state.text_channel = Some(post_channel_id);

    let mut call_lock = call.lock().await;
    call_lock.set_bitrate(target_bitrate);

    if !state.panel_events_registered {
        let updater = PanelUpdater {
            http: http.clone(),
            guild_id,
            guild_players: guild_players.clone(),
            call: call.clone(),
            manager: manager.clone(),
            kind: PanelUpdaterKind::Play,
        };
        call_lock.add_global_event(songbird::Event::Track(songbird::TrackEvent::Play), updater);
        // Registered before the `End`-kind PanelUpdater below: global event
        // handlers for the same TrackEvent fire in registration order, and
        // LoopHandler must finish rebuilding/re-queuing a looped track
        // *before* the panel is redrawn from the queue's state -- otherwise
        // the panel would flash "Playback Finished" for the one tick where
        // the queue is momentarily empty/mid-swap.
        let looper = crate::player::LoopHandler {
            extra_args,
            guild_id,
            guild_players: guild_players.clone(),
            call: call.clone(),
        };
        call_lock.add_global_event(songbird::Event::Track(songbird::TrackEvent::End), looper);
        let updater = PanelUpdater {
            http: http.clone(),
            guild_id,
            guild_players: guild_players.clone(),
            call: call.clone(),
            manager: manager.clone(),
            kind: PanelUpdaterKind::End,
        };
        call_lock.add_global_event(songbird::Event::Track(songbird::TrackEvent::End), updater);
        let notifier = crate::player::PlaybackErrorNotifier {
            http: http.clone(),
            guild_id,
            guild_players: guild_players.clone(),
        };
        call_lock.add_global_event(
            songbird::Event::Track(songbird::TrackEvent::Error),
            notifier,
        );
        state.panel_events_registered = true;
    }
    drop(call_lock);
    drop(state);

    // Starts the idle-leave countdown for a bare `/join` with nothing queued.
    crate::player::sync_idle_leave_task(http, guild_id, guild_players, manager, &call).await;

    Ok(call)
}

/// Joins (or moves to) the invoking user's voice channel, records where to
/// post the Now Playing panel, and registers the panel's songbird event
/// handlers exactly once per guild.
pub(crate) async fn ensure_call(ctx: &Context<'_>) -> Result<Arc<TokioMutex<Call>>, Error> {
    let guild_id = ctx.guild_id().expect("guild_only");
    let channel_id = user_voice_channel(ctx)?;
    let manager = songbird_manager(ctx).await?;
    ensure_call_raw(
        ctx.serenity_context().http.clone(),
        ctx.serenity_context().cache.clone(),
        ctx.data().ytdlp_extra_args.clone(),
        ctx.data().guild_players.clone(),
        manager,
        guild_id,
        channel_id,
        ctx.channel_id(),
    )
    .await
}

/// Enqueues a single track whose title/duration are already known, for call
/// sites without a poise `Context` (currently `commands::search`). Applies
/// `requested_by`'s saved volume/EQ. `uploader` and `requester_name`
/// populate the Now Playing panel's text; pass `uploader: None` if unknown.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn enqueue_known_raw(
    extra_args: &[String],
    db: &SqlitePool,
    eq_hifi_filter: &str,
    call: &Arc<TokioMutex<Call>>,
    url: &str,
    title: &str,
    duration_secs: i64,
    requested_by: serenity::UserId,
    requester_name: String,
    uploader: Option<String>,
) -> Result<(), Error> {
    let volume = resolve_saved_volume(db, requested_by).await;
    let eq_filter = resolve_eq_filter(db, requested_by, eq_hifi_filter).await;
    let source = FfmpegEqSource {
        url: url.to_string(),
        ytdlp_extra_args: extra_args.to_vec(),
        eq_filter: eq_filter.clone(),
        seek_time: None,
    };
    let meta = Arc::new(TrackMeta {
        title: title.to_string(),
        url: url.to_string(),
        requested_by,
        duration: (duration_secs > 0).then(|| Duration::from_secs(duration_secs as u64)),
        eq_filter,
        thumbnail: derive_youtube_thumbnail(url),
        uploader,
        requester_name,
        start_offset: Duration::ZERO,
        is_seek: false,
        is_loop_clone: false,
    });
    let track = Track::new_with_data(source.into(), meta).volume(volume);
    call.lock().await.enqueue(track).await;
    Ok(())
}

/// Enqueues many known tracks (e.g. a whole stored playlist) at once.
/// Unlike calling `enqueue_known_raw` in a loop, this resolves
/// `requested_by`'s saved volume/EQ exactly once and acquires `call`'s lock
/// exactly once for the entire batch, instead of once per track. Shared by
/// `commands::playlist::playlist_play` and
/// `commands::server_playlist::serverplaylist_play`.
pub(crate) async fn enqueue_multiple_known(
    ctx: &Context<'_>,
    call: &Arc<TokioMutex<Call>>,
    tracks: &[crate::models::PlaylistTrack],
    requested_by: serenity::UserId,
) -> Result<(), Error> {
    let db = &ctx.data().db;
    let volume = resolve_saved_volume(db, requested_by).await;
    let eq_filter = resolve_eq_filter(db, requested_by, &ctx.data().eq_hifi_filter).await;
    let extra_args = &ctx.data().ytdlp_extra_args;
    let requester_name = ctx.author().display_name().to_string();

    let mut call = call.lock().await;
    for track in tracks {
        let source = FfmpegEqSource {
            url: track.url.clone(),
            ytdlp_extra_args: extra_args.clone(),
            eq_filter: eq_filter.clone(),
            seek_time: None,
        };
        let meta = Arc::new(TrackMeta {
            title: track.title.clone(),
            url: track.url.clone(),
            requested_by,
            duration: (track.duration > 0).then(|| Duration::from_secs(track.duration as u64)),
            eq_filter: eq_filter.clone(),
            thumbnail: derive_youtube_thumbnail(&track.url),
            uploader: track.uploader.clone(),
            requester_name: requester_name.clone(),
            start_offset: Duration::ZERO,
            is_seek: false,
            is_loop_clone: false,
        });
        let built = Track::new_with_data(source.into(), meta).volume(volume);
        call.enqueue(built).await;
    }
    Ok(())
}

/// Joins your current voice channel.
#[poise::command(slash_command, guild_only)]
pub async fn join(ctx: Context<'_>) -> Result<(), Error> {
    let channel_id = user_voice_channel(&ctx)?;
    ensure_call(&ctx).await?;
    ctx.say(format!("🔊 Connected to <#{channel_id}>.")).await?;
    Ok(())
}

/// Leaves the voice channel and clears the queue.
///
/// Delegates teardown to `player::cleanup_guild`, shared with the
/// bot-was-disconnected detector and the auto-leave timer.
#[poise::command(slash_command, guild_only)]
pub async fn leave(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only");
    let manager = songbird_manager(&ctx).await?;

    let had_anything = crate::player::cleanup_guild(
        &ctx.serenity_context().http,
        guild_id,
        &ctx.data().guild_players,
        &manager,
    )
    .await;

    if !had_anything {
        ctx.say("The bot is not in a voice channel.").await?;
        return Ok(());
    }

    ctx.say("👋 Disconnected from the voice channel.").await?;
    Ok(())
}

/// Resolves `url`'s metadata (title/duration/thumbnail/uploader) and builds
/// the `Track` for `/play`/`/playnext`, applying the user's saved
/// volume/EQ. Returns the built `Track` and its resolved title.
async fn resolve_and_build_track(ctx: &Context<'_>, url: &str) -> Result<(Track, String), Error> {
    // Used only for its `aux_metadata()` lookup; playback audio comes from
    // `FfmpegEqSource` below instead.
    let mut metadata_source = YoutubeDl::new(ctx.data().http.clone(), url.to_string())
        .user_args(ctx.data().ytdlp_extra_args.clone());
    let aux = metadata_source.aux_metadata().await.ok();
    let title = aux
        .as_ref()
        .and_then(|meta| meta.title.clone())
        .unwrap_or_else(|| url.to_string());
    let duration = aux.as_ref().and_then(|meta| meta.duration);
    // Prefer yt-dlp's reported thumbnail over the URL-derived guess.
    let thumbnail = aux
        .as_ref()
        .and_then(|meta| meta.thumbnail.clone())
        .or_else(|| derive_youtube_thumbnail(url));
    // Prefer `channel` over `artist`; extractor-dependent field.
    let uploader = aux
        .as_ref()
        .and_then(|meta| meta.channel.clone().or_else(|| meta.artist.clone()));

    let volume = resolve_saved_volume(&ctx.data().db, ctx.author().id).await;
    let eq_filter =
        resolve_eq_filter(&ctx.data().db, ctx.author().id, &ctx.data().eq_hifi_filter).await;
    let source = FfmpegEqSource {
        url: url.to_string(),
        ytdlp_extra_args: ctx.data().ytdlp_extra_args.clone(),
        eq_filter: eq_filter.clone(),
        seek_time: None,
    };
    let meta = Arc::new(TrackMeta {
        title: title.clone(),
        url: url.to_string(),
        requested_by: ctx.author().id,
        duration,
        eq_filter,
        thumbnail,
        uploader,
        requester_name: ctx.author().display_name().to_string(),
        start_offset: Duration::ZERO,
        is_seek: false,
        is_loop_clone: false,
    });
    let track = Track::new_with_data(source.into(), meta).volume(volume);
    Ok((track, title))
}

/// Enqueues `track`, then moves it to index 1 (right after the current
/// track) if it didn't land at the front already. Backs `/playnext`.
///
/// If index 1 is currently `LoopMode::Track`'s pre-emptive clone (see
/// `crate::player::ensure_track_loop_clone`), lands at index 2 instead,
/// leaving the clone in place rather than displacing it.
async fn enqueue_next(call: &Arc<TokioMutex<Call>>, track: Track) {
    let mut call = call.lock().await;
    call.enqueue(track).await;
    call.queue().modify_queue(|tracks| {
        if tracks.len() > 1
            && let Some(just_added) = tracks.pop_back()
        {
            let insert_at = if tracks
                .get(1)
                .is_some_and(|t| t.data::<TrackMeta>().is_loop_clone)
            {
                2
            } else {
                1
            };
            tracks.insert(insert_at.min(tracks.len()), just_added);
        }
    });
}

/// Plays a song from a direct URL, joining your voice channel if needed.
///
/// Only accepts a direct URL -- for keyword search, use `/search` instead.
#[poise::command(slash_command, guild_only)]
pub async fn play(
    ctx: Context<'_>,
    #[description = "A direct track URL (YouTube, etc.) -- keyword search isn't wired up yet."]
    url: String,
) -> Result<(), Error> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        ctx.say(
            "❌ `/play` currently only accepts a direct URL. Keyword search is coming in a later phase.",
        )
        .await?;
        return Ok(());
    }

    ctx.defer().await?;

    let call = ensure_call(&ctx).await?;
    let (track, title) = resolve_and_build_track(&ctx, &url).await?;

    {
        let mut call = call.lock().await;
        call.enqueue(track).await;
    }

    ctx.say(format!("✅ Added **{title}** to the queue."))
        .await?;
    Ok(())
}

/// Plays a song from a direct URL right after the current track, instead of
/// at the end of the queue.
#[poise::command(slash_command, guild_only)]
pub async fn playnext(
    ctx: Context<'_>,
    #[description = "A direct track URL (YouTube, etc.)."] url: String,
) -> Result<(), Error> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        ctx.say("❌ `/playnext` currently only accepts a direct URL.")
            .await?;
        return Ok(());
    }

    ctx.defer().await?;

    let call = ensure_call(&ctx).await?;
    let (track, title) = resolve_and_build_track(&ctx, &url).await?;
    enqueue_next(&call, track).await;

    ctx.say(format!("⏭️ **{title}** will play next.")).await?;
    crate::player::refresh_panel(
        &ctx.serenity_context().http,
        ctx.guild_id().expect("guild_only"),
        &ctx.data().guild_players,
        &call,
    )
    .await;
    Ok(())
}

/// Randomly shuffles the queue -- the currently playing track stays in place.
#[poise::command(slash_command, guild_only)]
pub async fn shuffle(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only");
    let manager = songbird_manager(&ctx).await?;

    let Some(call) = manager.get(guild_id) else {
        ctx.say("The queue is empty.").await?;
        return Ok(());
    };

    let shuffled_count = {
        use rand::seq::SliceRandom;
        let call = call.lock().await;
        call.queue().modify_queue(|tracks| {
            // Index 1 is reserved for `LoopMode::Track`'s pre-emptive clone
            // (see `crate::player::ensure_track_loop_clone`) when one is
            // present; leave it in place and only shuffle what's
            // genuinely upcoming.
            let start_idx = if tracks
                .get(1)
                .is_some_and(|t| t.data::<TrackMeta>().is_loop_clone)
            {
                2
            } else {
                1
            };
            let tail_len = tracks.len().saturating_sub(start_idx);
            if tail_len > 1 {
                tracks.make_contiguous()[start_idx..].shuffle(&mut rand::thread_rng());
            }
            tail_len
        })
    };

    if shuffled_count == 0 {
        ctx.say("There's nothing queued up next to shuffle.")
            .await?;
    } else {
        ctx.say(format!("🔀 Shuffled {shuffled_count} upcoming track(s).",))
            .await?;
    }
    crate::player::refresh_panel(
        &ctx.serenity_context().http,
        guild_id,
        &ctx.data().guild_players,
        &call,
    )
    .await;
    Ok(())
}

/// Clears the queue, without stopping the track that's currently playing.
#[poise::command(slash_command, guild_only)]
pub async fn clear(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only");
    let manager = songbird_manager(&ctx).await?;

    let Some(call) = manager.get(guild_id) else {
        ctx.say("The queue is empty.").await?;
        return Ok(());
    };

    let removed = {
        let call = call.lock().await;
        call.queue().modify_queue(|tracks| {
            let mut removed = 0usize;
            // Keep index 0 (current track); stop and drop everything after.
            while tracks.len() > 1 {
                if let Some(queued) = tracks.remove(1) {
                    let _ = queued.stop();
                    removed += 1;
                }
            }
            removed
        })
    };

    if removed == 0 {
        ctx.say("The queue is already empty.").await?;
    } else {
        ctx.say(format!(
            "🗑️ Cleared {removed} track(s) from the queue. The current track keeps playing."
        ))
        .await?;
    }

    crate::player::refresh_panel(
        &ctx.serenity_context().http,
        guild_id,
        &ctx.data().guild_players,
        &call,
    )
    .await;
    Ok(())
}

/// Seeks the currently playing track to a given position, in seconds.
///
/// Rebuilds the track via a new ffmpeg `-ss` invocation and swaps it in;
/// see `player::seek_to`.
#[poise::command(slash_command, guild_only)]
pub async fn seek(
    ctx: Context<'_>,
    #[description = "Position to seek to, in seconds"] seconds: u64,
) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only");
    let manager = songbird_manager(&ctx).await?;

    let Some(call) = manager.get(guild_id) else {
        ctx.say("Nothing is playing.").await?;
        return Ok(());
    };

    match crate::player::seek_to(
        &call,
        &ctx.data().ytdlp_extra_args,
        Duration::from_secs(seconds),
    )
    .await
    {
        Ok(target) => {
            ctx.say(format!(
                "⏩ Seeked to {}.",
                crate::player::format_duration(target)
            ))
            .await?;
        }
        Err(e) => {
            ctx.say(format!("⚠️ {e}")).await?;
        }
    }
    Ok(())
}

/// Stops playback and clears the queue, without leaving the voice channel.
///
/// Resets loop mode to `Off` before stopping, so `LoopHandler` doesn't
/// re-enqueue the track.
#[poise::command(slash_command, guild_only)]
pub async fn stop(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only");
    let manager = songbird_manager(&ctx).await?;

    let Some(call) = manager.get(guild_id) else {
        ctx.say("There is nothing to stop.").await?;
        return Ok(());
    };

    if let Some(state_arc) = ctx
        .data()
        .guild_players
        .get(&guild_id)
        .map(|entry| entry.clone())
    {
        let mut state = state_arc.lock().await;
        state.loop_mode = crate::player::LoopMode::Off;
        crate::player::stop_progress_ticker(&mut state);
    }

    call.lock().await.queue().stop();
    ctx.say("⏹️ Stopped playback and cleared the queue.")
        .await?;
    Ok(())
}

/// Skips the current song.
#[poise::command(slash_command, guild_only)]
pub async fn skip(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only");
    let manager = songbird_manager(&ctx).await?;

    let Some(call) = manager.get(guild_id) else {
        ctx.say("There is nothing to skip.").await?;
        return Ok(());
    };

    // Discard any pending loop-clone first, so skip advances to the real
    // next track instead of just replaying the current one again.
    crate::player::remove_track_loop_clone(&call).await;

    let skipped_title = {
        let call = call.lock().await;
        let queue = call.queue();
        let title = queue
            .current()
            .map(|handle| handle.data::<TrackMeta>().title.clone());
        queue.skip()?;
        title
    };

    match skipped_title {
        Some(title) => ctx.say(format!("⏭️ Skipped **{title}**.")).await?,
        None => ctx.say("There is nothing to skip.").await?,
    };
    Ok(())
}

/// Loop mode choice, exposed to Discord as a slash command choice enum.
#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
pub enum LoopModeChoice {
    #[name = "Off"]
    Off,
    #[name = "Track"]
    Track,
    #[name = "Queue"]
    Queue,
}

impl From<LoopModeChoice> for crate::player::LoopMode {
    fn from(choice: LoopModeChoice) -> Self {
        match choice {
            LoopModeChoice::Off => crate::player::LoopMode::Off,
            LoopModeChoice::Track => crate::player::LoopMode::Track,
            LoopModeChoice::Queue => crate::player::LoopMode::Queue,
        }
    }
}

/// Sets the loop mode for playback (Off / Track / Queue).
///
/// Named `loop_cmd` since `loop` is reserved; `rename = "loop"` keeps the
/// slash command name `/loop`.
#[poise::command(slash_command, guild_only, rename = "loop")]
pub async fn loop_cmd(
    ctx: Context<'_>,
    #[description = "Choose the loop mode."] mode: LoopModeChoice,
) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only");
    let mode: crate::player::LoopMode = mode.into();

    let Some(state_arc) = ctx
        .data()
        .guild_players
        .get(&guild_id)
        .map(|entry| entry.clone())
    else {
        ctx.say("Please start playback before setting a loop mode.")
            .await?;
        return Ok(());
    };
    {
        let mut state = state_arc.lock().await;
        state.loop_mode = mode;
    }

    // No native songbird call needed for the mode switch itself --
    // `LoopHandler` reads `GuildPlayerState::loop_mode` (already updated
    // above) directly whenever a track ends. The pre-emptive clone
    // invariant does need updating here, though: seed one immediately when
    // switching *to* Track (there's no `TrackEvent::End` to trigger
    // `LoopHandler` until the current track actually finishes), and drop
    // any pending one when switching away, so it doesn't keep silently
    // replaying forever.
    let manager = songbird_manager(&ctx).await?;
    if let Some(call) = manager.get(guild_id) {
        if mode == crate::player::LoopMode::Track {
            crate::player::ensure_track_loop_clone(&call, &ctx.data().ytdlp_extra_args).await;
        } else {
            crate::player::remove_track_loop_clone(&call).await;
        }
        crate::player::refresh_panel(
            &ctx.serenity_context().http,
            guild_id,
            &ctx.data().guild_players,
            &call,
        )
        .await;
    }

    ctx.say(format!("✅ Loop mode set to **{}**.", mode.label()))
        .await?;
    Ok(())
}

/// Displays the current song queue.
///
/// Uses `pagination::send_paginated` (10 tracks/page) rather than a single
/// `CreateEmbed`, so a long queue can't exceed Discord's per-embed
/// character limit. Sent non-ephemerally (`ephemeral: false`), unlike the
/// personal `/playlist_show`-style listings that share this pagination
/// helper, since the queue is shared state everyone in the channel should
/// be able to see.
#[poise::command(slash_command, guild_only)]
pub async fn queue(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only");
    let manager = songbird_manager(&ctx).await?;

    let Some(call) = manager.get(guild_id) else {
        ctx.say("The queue is empty.").await?;
        return Ok(());
    };

    let handles = call.lock().await.queue().current_queue();
    if handles.is_empty() {
        ctx.say("The queue is empty.").await?;
        return Ok(());
    }

    let mut lines = Vec::with_capacity(handles.len());
    let mut total_duration = Duration::ZERO;

    for (i, handle) in handles.iter().enumerate() {
        let meta = handle.data::<TrackMeta>();
        if let Some(d) = meta.duration {
            total_duration += d;
        }
        if i == 0 {
            let now_playing = if meta.is_loop_clone {
                format!(
                    "**Now Playing:**\n🔁 [{}]({}) *(Loop)*\n\n**Up Next:**",
                    meta.title, meta.url
                )
            } else {
                format!(
                    "**Now Playing:**\n[{}]({})\n\n**Up Next:**",
                    meta.title, meta.url
                )
            };
            lines.push(now_playing);
        } else if meta.is_loop_clone {
            lines.push(format!("`{i}.` 🔁 [{}]({}) *(Loop)*", meta.title, meta.url));
        } else {
            lines.push(format!("`{i}.` [{}]({})", meta.title, meta.url));
        }
    }
    if handles.len() == 1 {
        lines.push("*Nothing queued up next.*".to_string());
    }

    let total_secs = total_duration.as_secs();
    let total_str = format!(
        "{:02}:{:02}:{:02}",
        total_secs / 3600,
        (total_secs % 3600) / 60,
        total_secs % 60
    );

    crate::pagination::send_paginated(
        &ctx,
        "🎵 Music Queue".to_string(),
        serenity::Colour::BLUE,
        lines,
        format!(
            "{} song(s) in queue  •  Total duration: {total_str}",
            handles.len()
        ),
        false,
    )
    .await?;
    Ok(())
}

/// Reposts the Now Playing panel at the bottom of the channel.
#[poise::command(slash_command, guild_only)]
pub async fn nowplaying(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().expect("guild_only");
    let manager = songbird_manager(&ctx).await?;

    let is_playing = match manager.get(guild_id) {
        Some(call) => call.lock().await.queue().current().is_some(),
        None => false,
    };
    if !is_playing {
        ctx.send(
            poise::CreateReply::default()
                .content("There is no music currently playing.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }
    // `is_playing` implies `manager.get(guild_id)` returned `Some` above.
    let call = manager.get(guild_id).expect("checked is_playing above");

    // Delete the old panel message and clear `now_playing_message` first,
    // so `refresh_panel` sends a fresh message instead of editing it.
    if let Some(state_arc) = ctx
        .data()
        .guild_players
        .get(&guild_id)
        .map(|entry| entry.clone())
    {
        let mut state = state_arc.lock().await;
        if let (Some(old_message_id), Some(text_channel)) =
            (state.now_playing_message, state.text_channel)
        {
            let _ = text_channel
                .delete_message(&ctx.serenity_context().http, old_message_id)
                .await;
        }
        state.now_playing_message = None;
        // Re-remember the channel `/nowplaying` was just run in, so the
        // panel actually moves there instead of reappearing in whichever
        // channel it was originally posted to.
        state.text_channel = Some(ctx.channel_id());
    }

    crate::player::refresh_panel(
        &ctx.serenity_context().http,
        guild_id,
        &ctx.data().guild_players,
        &call,
    )
    .await;

    ctx.send(
        poise::CreateReply::default()
            .content("✅ Panel moved to the bottom.")
            .ephemeral(true),
    )
    .await?;
    Ok(())
}
