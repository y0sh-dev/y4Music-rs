//! Layer 2: the Now Playing panel's rendering, its button/voice-state event
//! handlers, and the guild-lifecycle helpers (auto-leave, cleanup) that
//! drive it. Depends on `state` and `track`.

use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use songbird::Call;
use songbird::tracks::PlayMode;
use tokio::sync::Mutex;

use super::state::{
    EMPTY_CHANNEL_LEAVE_DELAY, GuildPlayers, IDLE_QUEUE_LEAVE_DELAY, LoopMode,
    PROGRESS_TICK_INTERVAL, TrackMeta, abort_unless_self, stop_idle_leave_task,
    stop_progress_ticker,
};
use super::track::{ensure_track_loop_clone, remove_track_loop_clone, seek_by};

/// Width (in characters) of the text progress bar `now_playing_embed` renders.
pub const PROGRESS_BAR_LENGTH: usize = 20;

/// How far the panel's ⏪/⏩ buttons seek per press, in seconds.
pub const PANEL_SEEK_STEP_SECS: i64 = 10;

/// Silent per-guild cooldown applied to every panel button press, so a burst
/// of clicks doesn't hammer the Discord API.
pub const BUTTON_COOLDOWN: Duration = Duration::from_millis(1500);

/// Renders a `████────` -style text progress bar for `position` out of
/// `total`. A zero (or otherwise degenerate) `total` renders an empty bar.
pub fn progress_bar(position: Duration, total: Duration) -> String {
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

pub fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    format!("{:02}:{:02}", total / 60, total % 60)
}

/// Builds the Now Playing panel's embed.
pub fn now_playing_embed(
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

pub fn finished_embed() -> serenity::CreateEmbed {
    serenity::CreateEmbed::new()
        .title("⏹️ Playback Finished")
        .description("Thanks for listening!")
        .color(serenity::Colour::DARK_GREY)
}

pub fn goodbye_embed() -> serenity::CreateEmbed {
    serenity::CreateEmbed::new()
        .title("👋 See you!")
        .description("Thanks for using the bot.")
        .color(serenity::Colour::DARK_GREY)
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

/// Panel button layout: row 1 is the transport trio (rewind / pause-resume /
/// fast-forward), row 2 is queue management (loop / skip / shuffle).
pub fn control_panel_components(
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

pub fn disabled_control_panel_components(loop_mode: LoopMode) -> Vec<serenity::CreateActionRow> {
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

/// Starts or cancels `GuildPlayerState::idle_leave_task` to match whether
/// the call's queue is currently empty.
pub async fn sync_idle_leave_task(
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

/// Starts `GuildPlayerState::progress_ticker` if it isn't already running.
pub async fn ensure_progress_ticker(
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

    // Keep the `LoopMode::Track` pre-emptive clone invariant in sync with
    // the mode change above: seed one immediately on switching *to* Track
    // (there's no `TrackEvent::End` to trigger `LoopHandler` until the
    // current track actually finishes), and drop any pending one when
    // switching away, so it doesn't keep silently replaying forever.
    if let Some(mode) = new_loop_mode {
        if mode == LoopMode::Track {
            ensure_track_loop_clone(&call, ytdlp_extra_args).await;
        } else {
            remove_track_loop_clone(&call).await;
        }
    }

    // Seek buttons lock `call` themselves via `seek_by`, so they're handled
    // outside the `call_lock` block below.
    let ephemeral_note = if matches!(custom_id, custom_id::SEEK_BACK | custom_id::SEEK_FORWARD) {
        let delta_secs = if custom_id == custom_id::SEEK_BACK {
            -PANEL_SEEK_STEP_SECS
        } else {
            PANEL_SEEK_STEP_SECS
        };
        seek_by(&call, ytdlp_extra_args, delta_secs).await.err()
    } else if custom_id == custom_id::SKIP {
        // Discard any pending loop-clone first (needs its own lock on
        // `call`, taken and released before the `call_lock` block below),
        // so skip advances to the real next track instead of just
        // replaying the current one again.
        remove_track_loop_clone(&call).await;
        let call_lock = call.lock().await;
        let queue = call_lock.queue();
        if queue.current().is_none() {
            Some("Nothing to skip.".to_string())
        } else {
            queue.skip().err().map(|e| format!("⚠️ {e}"))
        }
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
            custom_id::SHUFFLE => {
                use rand::seq::SliceRandom;
                let shuffled_count = queue.modify_queue(|tracks| {
                    // Index 1 is reserved for `LoopMode::Track`'s
                    // pre-emptive clone when one is present; leave it in
                    // place and only shuffle what's genuinely upcoming.
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
