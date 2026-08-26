//! `/profile` command group.
//!
//! Saved volume/EQ settings are applied to every enqueued track via
//! `commands::playback::resolve_saved_volume` and `resolve_eq_filter`.

use poise::serenity_prelude as serenity;
use sqlx::SqlitePool;

use crate::{Context, Error, models::UserProfile};

/// Manage your personal playback settings.
#[poise::command(
    slash_command,
    subcommands("show", "volume", "eq"),
    subcommand_required
)]
pub async fn profile(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Loads a user's profile from the DB, inserting the default row on first
/// use. Raw-parameter version so callers without a poise `Context` can use
/// it. The insert-then-select is a single atomic upsert (`ON CONFLICT ...
/// DO NOTHING`), so two concurrent first-time calls for the same user can't
/// race each other into a unique-constraint violation.
pub(crate) async fn load_or_create_raw(
    db: &SqlitePool,
    user_id: i64,
) -> Result<UserProfile, Error> {
    let defaults = UserProfile::default_for(user_id);
    sqlx::query(
        "INSERT INTO users (user_id, default_volume, default_eq_mode) VALUES (?, ?, ?) \
         ON CONFLICT (user_id) DO NOTHING",
    )
    .bind(defaults.user_id)
    .bind(defaults.default_volume)
    .bind(&defaults.default_eq_mode)
    .execute(db)
    .await?;

    sqlx::query_as::<_, UserProfile>(
        "SELECT user_id, default_volume, default_eq_mode FROM users WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

async fn load_or_create(ctx: &Context<'_>, user_id: i64) -> Result<UserProfile, Error> {
    load_or_create_raw(&ctx.data().db, user_id).await
}

/// Shows your current profile settings.
#[poise::command(slash_command)]
pub async fn show(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id.get() as i64;
    let profile = load_or_create(&ctx, user_id).await?;

    let mode_display = if profile.default_eq_mode == "hifi" {
        "🎧 Hi-Fi"
    } else {
        "🎵 Balanced"
    };

    ctx.send(
        poise::CreateReply::default()
            .embed(
                serenity::CreateEmbed::new()
                    .title(format!("{}'s Profile", ctx.author().display_name()))
                    .description(format!(
                        "**🔊 Volume:** {}%\n**🎚️ EQ Mode:** {}",
                        profile.default_volume, mode_display
                    ))
                    .color(serenity::Colour::BLURPLE),
            )
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

/// Sets your default volume (0-200%), saved for future sessions.
#[poise::command(slash_command)]
pub async fn volume(
    ctx: Context<'_>,
    #[description = "Volume percentage (0-200)"]
    #[min = 0]
    #[max = 200]
    percent: i64,
) -> Result<(), Error> {
    let user_id = ctx.author().id.get() as i64;
    load_or_create(&ctx, user_id).await?;

    sqlx::query("UPDATE users SET default_volume = ? WHERE user_id = ?")
        .bind(percent)
        .bind(user_id)
        .execute(&ctx.data().db)
        .await?;

    // Apply the new volume live to the current queue, if any.
    if let Some(guild_id) = ctx.guild_id()
        && let Some(manager) = songbird::get(ctx.serenity_context()).await
        && let Some(call) = manager.get(guild_id)
    {
        let volume = (percent as f32 / 100.0).clamp(0.0, 2.0);
        for handle in call.lock().await.queue().current_queue() {
            let _ = handle.set_volume(volume);
        }
    }

    ctx.send(
        poise::CreateReply::default()
            .content(format!(
                "🔊 Volume set to **{percent}%** and saved to your profile."
            ))
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

/// Switches your default sound quality mode (Balanced / Hi-Fi).
#[poise::command(slash_command)]
pub async fn eq(
    ctx: Context<'_>,
    #[description = "Sound quality mode"] mode: EqMode,
) -> Result<(), Error> {
    let user_id = ctx.author().id.get() as i64;
    load_or_create(&ctx, user_id).await?;

    let mode_value = mode.as_str();
    sqlx::query("UPDATE users SET default_eq_mode = ? WHERE user_id = ?")
        .bind(mode_value)
        .bind(user_id)
        .execute(&ctx.data().db)
        .await?;

    ctx.send(
        poise::CreateReply::default()
            .content(format!(
                "✅ Sound quality mode set to **{}**. Applies starting with your next playback session.",
                mode.label()
            ))
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

/// EQ mode choice, exposed to Discord as a slash command choice enum.
#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
pub enum EqMode {
    #[name = "🎵 Balanced"]
    Balanced,
    #[name = "🎧 High quality (Hi-Fi)"]
    Hifi,
}

impl EqMode {
    fn as_str(self) -> &'static str {
        match self {
            EqMode::Balanced => "balanced",
            EqMode::Hifi => "hifi",
        }
    }

    fn label(self) -> &'static str {
        match self {
            EqMode::Balanced => "🎵 Balanced",
            EqMode::Hifi => "🎧 High quality (Hi-Fi)",
        }
    }
}
