//! Command registration: `playback`, `playlist`, `server_playlist`,
//! `profile`, and `search`.

pub mod playback;
pub mod playlist;
pub mod profile;
pub mod search;
pub mod server_playlist;

use crate::Error;

/// All top-level commands registered with the poise framework.
pub fn all() -> Vec<poise::Command<crate::Data, Error>> {
    vec![
        profile::profile(),
        playback::join(),
        playback::leave(),
        playback::play(),
        playback::playnext(),
        playback::stop(),
        playback::skip(),
        playback::loop_cmd(),
        playback::queue(),
        playback::nowplaying(),
        playback::shuffle(),
        playback::clear(),
        playback::seek(),
        search::search(),
        playlist::playlist_create(),
        playlist::playlist_add(),
        playlist::playlist_play(),
        playlist::playlist_show(),
        playlist::playlist_delete(),
        playlist::playlist_remove_track(),
        playlist::playlist_move(),
        playlist::playlist_rename(),
        server_playlist::serverplaylist_show(),
        server_playlist::serverplaylist_play(),
        server_playlist::serverplaylist_add(),
        server_playlist::serverplaylist_remove_track(),
        server_playlist::serverplaylist_create(),
        server_playlist::serverplaylist_delete(),
        server_playlist::serverplaylist_rename(),
        server_playlist::serverplaylist_move(),
        server_playlist::serverplaylist_lock(),
        server_playlist::serverplaylist_add_user(),
        server_playlist::serverplaylist_remove_user(),
        server_playlist::serverplaylist_add_role(),
        server_playlist::serverplaylist_remove_role(),
        server_playlist::serverplaylist_transfer(),
    ]
}
