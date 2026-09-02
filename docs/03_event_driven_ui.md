# 03. Event-Driven Now Playing Panel

## Design Policy: No "State Flags"

A naive way to implement the Now Playing panel (that message displaying the track name, progress bar, and buttons) would be to manage flags like `is_playing` or `skip_requested` yourself, updating them on every command or button process. However, this method easily leads to discrepancies like "it says playing but it's actually stopped" due to missed flag updates or race conditions.

This project holds absolutely no such flags. Instead, panel redraws (`refresh_panel`) are directly tied to `TrackEvent::Play`/`TrackEvent::End` fired by songbird, and the panel is always reconstructed from **the actual state of the queue at that moment** (`call.queue().current_queue()`). The design is to ask songbird directly instead of looking at flags when asked "is it playing right now?".

## 4 Registered Global Event Handlers

`commands/playback.rs::ensure_call_raw` registers the following four to the `Call` exactly once per guild (preventing double registration with `GuildPlayerState::panel_events_registered`).

| Handler | Subscribed Event | Single Responsibility |
|---|---|---|
| `PanelUpdater { kind: Play }` | `TrackEvent::Play` | Redraw panel + start progress ticker |
| `PanelUpdater { kind: End }` | `TrackEvent::End` | Redraw panel + stop progress ticker |
| `PlaybackErrorNotifier` | `TrackEvent::Error` | Notify text channel of playback failure |
| `LoopHandler` | `TrackEvent::End` | Re-enqueue for `LoopMode::Queue`/`Track` |

The coexistence of two handlers (`PanelUpdater` and `LoopHandler`) on `TrackEvent::End` is an intentional separation. "Updating the UI" and "restacking tracks for the next cycle" are logically independent concerns; both react to the same event, but they are structured so that if one fails or skips, it does not affect the other.

```text
                     TrackEvent::Play
                            │
              ┌─────────────┴─────────────┐
              ▼                           ▼
      refresh_panel()          ensure_progress_ticker()
   (Update the panel)      (Start a loop calling refresh_panel every 15s)


                     TrackEvent::End
                            │
        ┌───────────────────┼───────────────────┐
        ▼                   ▼                   ▼
 refresh_panel()   stop_progress_ticker()  LoopHandler::act()
(Update the panel)   (Stop the ticker)   (Re-enqueue a copy if looping)
                                                  │
                                          sync_idle_leave_task()
                                   (Start idle leave timer if queue is empty)
```

## `refresh_panel`: Always "Edit", History is "Resent"

`refresh_panel` **edits in place** with `edit_message` if the panel message already exists, and sends a new one if it doesn't. An earlier version deleted and resent the message every time a new track started (aiming to make the panel "follow" to the bottom of the channel), but this was reverted because the channel would get spammed every time the track changed or seeked (since `seek_to` is technically a new `Track` too). The demand to bring the panel back to the bottom still exists, so that is handled by the `/nowplaying` command explicitly "deleting the old message and sending a new one".

### Error Type Evaluation: Distinguishing Transient Failures from "Truly Deleted"

Just because `edit_message` fails doesn't mean the panel message was actually deleted. Temporary 502s, rate limits, or network delays from Discord will return the same `Err`. The previous implementation did not distinguish between these and reset `GuildPlayerState::now_playing_message` to `None` on every failure. Consequently, the next `refresh_panel` call would mistakenly assume "the panel doesn't exist yet" and send a new message, causing a **panel duplication bug** where two or more panels coexisted in the same guild.

Currently, the `player/ui.rs::is_not_found(err: &serenity::Error) -> bool` helper returns `true` only for HTTP status 404, or Discord error codes `10008` (Unknown Message) / `10003` (Unknown Channel)—responses that definitively indicate "the target truly does not exist." `refresh_panel` resets `now_playing_message` to `None` only when this check is `true`. For all other errors (treated as transient), it merely logs a `tracing::warn!` and does not reset the ID. By strictly adhering to "recreate only when truly deleted," duplication is prevented.

### Lock Scopes: Snapshot, Drop, then Call Discord API Lock-Free

`refresh_panel` is structured in three stages: "① Snapshot `channel_id`/`message_id`/`loop_mode` from `state` and immediately release the lock → ② Snapshot the queue state from `call` and immediately release the lock → ③ Assemble the Embed/Components and call the Discord API without holding any locks." If you `.await` network I/O like `edit_message`/`send_message` while holding `state.lock().await` or `call.lock().await`, any rate limits or delays will cause other operations in the same guild (button presses, other `refresh_panel` calls, etc.) to block for a long time waiting for the lock. Therefore, the design is unified to always release locks before I/O. The lock is re-acquired only briefly when writing back the transmission result (the new message ID, or resetting via `is_not_found`). See Trap 5 in `04_pitfalls.md` for details and generalized lessons.

## Button Processing: Acknowledge First, Update Panel Separately

`player::handle_component_interaction` handles all buttons on the Now Playing panel (⏪/⏯️/⏩/🔁/⏭/🔀) in one place. There are two clever points in the flow.

1. **Return Acknowledge immediately** — Do not show the "thinking" spinner to Discord; perform the actual panel update via direct message editing in `refresh_panel`. This ensures that whether the panel is updated via command execution, auto-progression, or button press, it ultimately goes through the same single path of the `refresh_panel` function.
2. **1.5-second silent cooldown** — Compares `GuildPlayerState::last_button_press` with the current time and unconditionally drops rapid clicks. We intentionally do not send a "you're clicking too fast" notification to the user — that itself is an additional API call, and since the user can see the panel isn't moving, it was judged to be just noise.

Pause/Resume and loop toggling do not fire `TrackEvent` at all (from songbird's perspective, the track hasn't "ended/started"). Therefore, `refresh_panel` is explicitly called at the end of `handle_component_interaction`. For Skip/Stop, `TrackEvent::End` will fire separately and `PanelUpdater` will call `refresh_panel` again, but this is a harmless, idempotent operation that just draws the same state twice, so it is not explicitly guarded against.

## Progress Ticker: A "Secondary" Loop Born from Events

The progress bar does not advance smoothly by the second just from Play/End/button press timings. `ensure_progress_ticker` starts a 15-second interval loop task on every `TrackEvent::Play` "if it isn't already running", and explicitly stops it on the `TrackEvent::End` side. The "if it isn't already running" check is done with `is_finished()`; without this double-start prevention check, if the Play event fires twice in the same guild (e.g., transitioning to the next track), the ticker would run multiple times concurrently.
