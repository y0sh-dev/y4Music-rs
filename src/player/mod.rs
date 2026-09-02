//! Per-guild Now Playing panel and its control buttons.
//!
//! Split into four layers, each depending only on the ones above it:
//! `state` (Layer 0, shared per-guild state) -> `track` (Layer 1, track
//! metadata/seek/loop-clone helpers) -> `ui` (Layer 2, panel rendering and
//! its event handlers) -> `events` (Layer 3, songbird global event
//! handlers). Re-exported below so every existing `crate::player::X` call
//! site outside this module keeps working unchanged.

pub mod events;
pub mod state;
pub mod track;
pub mod ui;

#[allow(unused_imports)]
pub use events::{LoopHandler, PanelUpdater, PanelUpdaterKind, PlaybackErrorNotifier};
#[allow(unused_imports)]
pub use state::{
    EMPTY_CHANNEL_LEAVE_DELAY, GuildPlayerState, GuildPlayers, IDLE_QUEUE_LEAVE_DELAY, LoopMode,
    PROGRESS_TICK_INTERVAL, TrackMeta, abort_unless_self, stop_idle_leave_task,
    stop_progress_ticker,
};
#[allow(unused_imports)]
pub use track::{
    derive_youtube_thumbnail, ensure_track_loop_clone, remove_track_loop_clone, seek_by, seek_to,
    youtube_video_id,
};
#[allow(unused_imports)]
pub use ui::{
    BUTTON_COOLDOWN, PANEL_SEEK_STEP_SECS, PROGRESS_BAR_LENGTH, cleanup_guild,
    control_panel_components, custom_id, disabled_control_panel_components, ensure_progress_ticker,
    finished_embed, format_duration, goodbye_embed, handle_component_interaction,
    handle_voice_state_update, now_playing_embed, progress_bar, refresh_panel,
    sync_idle_leave_task,
};
