//! Entry point: loads config, opens the DB, builds the poise `Framework`,
//! and starts the serenity `Client`.

mod audio_source;
mod commands;
mod db;
mod eq;
mod models;
mod pagination;
mod player;
mod playlist;
mod ytdlp;

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use songbird::serenity::SerenityInit;
use sqlx::SqlitePool;

/// Shared state visible to every command via `ctx.data()` -- one struct
/// handed to `poise::Framework` once at startup.
pub struct Data {
    pub db: SqlitePool,
    pub http: reqwest::Client,
    pub guild_players: Arc<player::GuildPlayers>,
    pub search_sessions: Arc<commands::search::SearchSessions>,
    pub list_sessions: Arc<pagination::ListSessions>,
    /// Extra CLI args forwarded to every `yt-dlp` invocation used for actual
    /// playback (stream-URL resolution and `/play`'s confirmation lookup).
    /// From `YTDLP_EXTRA_ARGS` (whitespace-split), empty by default.
    pub ytdlp_extra_args: Vec<String>,
    /// Hi-Fi mode's ffmpeg `-af` filtergraph. From `EQ_HIFI_FILTER`, falling
    /// back to `crate::eq::default_hifi_profile`. Balanced mode always uses
    /// `crate::eq::balanced_profile` instead (not operator-configurable).
    pub eq_hifi_filter: String,
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Context<'a> = poise::Context<'a, Data, Error>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Load `.env` if present: current directory first, then the FHS system
    // config path. Neither being present is not fatal.
    const SYSTEM_ENV_PATH: &str = "/etc/y4music-rs/.env";
    match dotenvy::dotenv() {
        Ok(path) => tracing::info!("Loaded config from {}", path.display()),
        Err(_) if std::path::Path::new(SYSTEM_ENV_PATH).exists() => {
            if let Err(e) = dotenvy::from_path(SYSTEM_ENV_PATH) {
                tracing::warn!("Found {SYSTEM_ENV_PATH} but failed to load it: {e}");
            } else {
                tracing::info!("Loaded config from {SYSTEM_ENV_PATH}");
            }
        }
        Err(e) => {
            tracing::info!(
                "No .env file found in the current directory or at {SYSTEM_ENV_PATH} ({e}); relying on process environment."
            );
        }
    }

    let token = std::env::var("DISCORD_TOKEN")
        .map_err(|_| anyhow::anyhow!("DISCORD_TOKEN is not set (see .env.example)"))?;
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "sqlite:///var/lib/y4music-rs/data.db".to_string());
    let test_guild_id = std::env::var("TEST_GUILD_ID")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());
    let ytdlp_extra_args: Vec<String> = std::env::var("YTDLP_EXTRA_ARGS")
        .ok()
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    if !ytdlp_extra_args.is_empty() {
        tracing::info!("Forwarding extra yt-dlp arguments to playback: {ytdlp_extra_args:?}");
    }
    // Hi-Fi `-af` filtergraph -- overridable via `EQ_HIFI_FILTER` (see
    // `.env.example`); falls back to `eq::default_hifi_profile`.
    let eq_hifi_filter =
        std::env::var("EQ_HIFI_FILTER").unwrap_or_else(|_| eq::default_hifi_profile().render());

    let db = db::init(&database_url).await?;
    tracing::info!("Database ready at {database_url}");

    let intents = serenity::GatewayIntents::non_privileged()
        | serenity::GatewayIntents::MESSAGE_CONTENT
        | serenity::GatewayIntents::GUILD_VOICE_STATES;

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: commands::all(),
            on_error: |error| Box::pin(on_error(error)),
            event_handler: |ctx, event, _framework, data| Box::pin(on_event(ctx, event, data)),
            ..Default::default()
        })
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                tracing::info!("Logged in as {}", ready.user.name);

                let commands = &framework.options().commands;
                if let Some(guild_id) = test_guild_id {
                    // Fast path for development: sync to a single guild
                    // instantly instead of waiting up to an hour for a
                    // global sync.
                    let guild_id = serenity::GuildId::new(guild_id);
                    poise::builtins::register_in_guild(ctx, commands, guild_id).await?;
                    tracing::info!("Slash commands synced to test guild {guild_id}");
                } else {
                    poise::builtins::register_globally(ctx, commands).await?;
                    tracing::info!("Slash commands synced globally");
                }

                let search_sessions = Arc::new(commands::search::SearchSessions::new());
                let list_sessions = Arc::new(pagination::ListSessions::new());
                spawn_session_sweeper(search_sessions.clone(), list_sessions.clone());

                Ok(Data {
                    db,
                    http: reqwest::Client::new(),
                    guild_players: Arc::new(player::GuildPlayers::new()),
                    search_sessions,
                    list_sessions,
                    ytdlp_extra_args,
                    eq_hifi_filter,
                })
            })
        })
        .build();

    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .register_songbird()
        .await?;

    client.start().await?;

    Ok(())
}

/// Periodically reclaims abandoned `/search` and `/playlist_show` pagination
/// sessions so they don't leak for the life of the process.
fn spawn_session_sweeper(
    search_sessions: Arc<commands::search::SearchSessions>,
    list_sessions: Arc<pagination::ListSessions>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5 * 60));
        loop {
            interval.tick().await;
            commands::search::sweep_stale(&search_sessions);
            pagination::sweep_stale(&list_sessions);
        }
    });
}

/// Routes raw gateway events outside poise's slash-command dispatch:
/// component interactions (search/list/panel buttons) and voice state
/// updates.
async fn on_event(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    data: &Data,
) -> Result<(), Error> {
    match event {
        serenity::FullEvent::InteractionCreate {
            interaction: serenity::Interaction::Component(component),
        } => {
            if component.data.custom_id.starts_with("search:") {
                commands::search::handle_component_interaction(ctx, component, data).await?;
            } else if component.data.custom_id.starts_with("listpage:") {
                pagination::handle_component_interaction(ctx, component, data).await?;
            } else {
                player::handle_component_interaction(
                    ctx,
                    component,
                    &data.guild_players,
                    &data.ytdlp_extra_args,
                )
                .await?;
            }
        }
        serenity::FullEvent::VoiceStateUpdate { old, new } => {
            player::handle_voice_state_update(ctx, &data.guild_players, old, new).await?;
        }
        _ => {}
    }
    Ok(())
}

/// Global error handler for command execution.
async fn on_error(error: poise::FrameworkError<'_, Data, Error>) {
    match error {
        poise::FrameworkError::Command { error, ctx, .. } => {
            tracing::error!("Error in command `{}`: {error:?}", ctx.command().name);
            let _ = ctx
                .send(
                    poise::CreateReply::default()
                        .content("❌ An unexpected error occurred while executing the command.")
                        .ephemeral(true),
                )
                .await;
        }
        poise::FrameworkError::Setup { error, .. } => {
            tracing::error!("Failed during setup: {error:?}");
        }
        error => {
            if let Err(e) = poise::builtins::on_error(error).await {
                tracing::error!("Error while handling error: {e:?}");
            }
        }
    }
}
