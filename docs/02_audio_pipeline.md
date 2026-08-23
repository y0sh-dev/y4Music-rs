# 02. Custom Audio Source and Seek Hack

## Why `songbird`'s standard `YoutubeDl` alone is not enough

`songbird` 0.6 completely dropped the old FFmpeg-based `Input`, and the standard `songbird::input::YoutubeDl` only has a single path: "resolve the direct CDN URL via `yt-dlp -j`, and HTTP-stream that URL directly into `symphonia` for decoding". There is no hook anywhere in this path to apply audio filters. To implement per-user EQ (Balanced/Hi-Fi switching, `/profile eq`), it was necessary to create a custom path outside this standard route.

## `FfmpegEqSource`: A custom source implementing the `Compose` trait

`FfmpegEqSource` in `src/audio_source.rs` implements `songbird::input::Compose` and reintroduces `ffmpeg`. However, instead of "downloading the file with `yt-dlp` and piping it to `ffmpeg`'s stdin", `yt-dlp -j` is used only for metadata retrieval (resolving the direct URL), and `ffmpeg` itself is made to fetch that URL directly.

```text
① yt-dlp -j <url>              …  Fetch metadata only. Do not download the video.
     │  (resolve_stream_url / extract_stream_url parses the JSON)
     ▼
   Direct CDN URL (stream_url)
     │
② ffmpeg -reconnect ... -i <stream_url> -af <filter> -f f32le pipe:1
     │  (spawn_pipeline spawns the process)
     ▼
   raw PCM (48kHz stereo, f32le) continuously flows to stdout
     │
③ ChildContainer → ReadOnlySource → RawAdapter
     ▼
   songbird's audio mixer
```

Because there is no "full file download" between ① and ②, the `ffmpeg` in ② can start outputting audio immediately after startup. Previously, a configuration like `yt-dlp <url> -o - | ffmpeg -i pipe:0` was tested, but `ffmpeg` could not start until `yt-dlp` began writing to the pipe, causing a noticeable delay in playback start. The current method also has the advantage of leaving network fetching to `ffmpeg`'s own `-reconnect` options.

## Differentiating `Compose::create` and `create_async`

```rust
fn create(&mut self) -> Result<..., AudioStreamError> {
    Err(AudioStreamError::Unsupported)   // Synchronous version is unsupported
}
async fn create_async(&mut self) -> Result<..., AudioStreamError> { ... }
fn should_create_async(&self) -> bool { true }
```

Since starting and waiting for `yt-dlp` is inherently asynchronous I/O, the synchronous `create` cannot be implemented in the first place. By returning `true` from `should_create_async`, we tell songbird to "only call `create_async` for this source". Also, by wrapping it in `Input::Lazy` (`impl From<FfmpegEqSource> for Input`), the actual process spawning is delayed until its turn comes up in the queue — subsequent tracks just sitting in the queue will not preemptively spawn processes.

## Seek Hack: Not "seeking", but "recreating the track entirely"

This is the part of the project that required the most Rust/songbird-specific design decisions.

`songbird`'s `TrackHandle::seek` only works on **songbird-native seekable sources** decoded via symphonia. Custom `Compose` implementations that delegate entirely to external processes like `FfmpegEqSource` simply do not have a hook for seeking.

Therefore, `player::seek_to` takes the following steps:

```rust
// 1. Assemble a new FfmpegEqSource with seek_time: Some(target)
let source = FfmpegEqSource { seek_time: Some(target), ..(cloned from current track) };
// 2. Enqueue the new Track and interrupt at the "front" of the queue
// 3. stop() the old track, then play() the new track
```

When `seek_time` is `Some`, `spawn_pipeline` inserts `-ss <seconds>` **before** `-i` (demuxer-level seeking). This avoids the waste of fetching data over the network before the seek destination and then discarding it.

```text
[Old Track] ──stop()──╮
                      ├─→ (Do not allow even a moment of simultaneous playback)
[New Track] ──play()──╯
```

The reason "removing the new track from the queue and inserting it at the front" and "popping the old track" are done within a single closure in `call.queue().modify_queue(..)` is to prevent other operations from interrupting and observing intermediate states like "two fronts" or "zero fronts".

### Side Effect 1: Progress bar resets to 0 → `TrackMeta::start_offset`

Since the new `Track` naturally starts from a new `Input`, songbird's own `TrackState::position` always counts up from zero. However, the actual playback position has advanced from the seek `target`. To absorb this difference, `TrackMeta` holds `start_offset: Duration`, and panel rendering (`now_playing_embed`) and relative position calculation for re-seeking (`seek_by`) are always calculated as:

```text
Actual playback position = state.position + meta.start_offset
```

For a normal track that has never been seeked, `start_offset == Duration::ZERO`, so this formula passes through transparently.

### Side Effect 2: Queue duplication → `TrackMeta::is_seek`

When the old `Track` is `stop()`ped to swap tracks, songbird fires the same `TrackEvent::End` as a normal track end. If the loop mode (`LoopMode::Queue` or `Track`) is active, `LoopHandler` sees this event and performs the process of "re-enqueueing a duplicate for the next cycle because the track ended". However, since this is a swap due to seeking and not a real track end, reacting naively will cause the same track to pile up redundantly in the queue.

To prevent this, `TrackMeta` holds an `is_seek: bool` flag, and `LoopHandler::act` checks "is the currently playing track `is_seek == true` and has the same URL?", and if so, skips re-enqueueing.

```rust
let is_seek_replacement = self.call.lock().await.queue().current().is_some_and(|current| {
    let current_meta = current.data::<TrackMeta>();
    current_meta.is_seek && current_meta.url == meta.url
});
if is_seek_replacement { continue; }
```

The moment the design "seeking is creating a new track" was chosen, the side effect that it looks to songbird's event system like "a track ended and the next one started" was born. Understanding `start_offset` and `is_seek` as correction values to cancel out that side effect from the perspectives of the UI and the loop mechanism respectively connects the whole picture.

### Another Feature Broken by the Same Unseekability: songbird's Native Track Loop

The output of `FfmpegEqSource` is wrapped in a `ReadOnlySource`, which always returns `false` for `is_seekable()`. This is not a limitation only for the seek feature; songbird's native `TrackHandle::enable_loop()` (the "repeat one track" feature for `LoopMode::Track`) is internally implemented by "seeking back to position 0 after playing to the end of the track", which hits the same unseekability constraint and falls into `PlayMode::Errored`. Therefore, this project does not use `enable_loop()` at all, and implements `LoopMode::Track` using the same method as `LoopMode::Queue`: "creating a new `Track` and re-enqueueing it into the playback queue" (`player::LoopHandler`). See `04_pitfalls.md` for details.

## Bonus: ffmpeg's stderr is the only clue to error causes

`ffmpeg` is started with `-loglevel error` and is almost silent during normal operation. `log_stderr_lines` forwards the child process's stderr line by line to `tracing::warn!` on a separate thread (synchronous `Read` requiring no tokio runtime). Generic errors returned by symphonia like "no compatible track found" do not explain the cause, so when tracking down "playback fails for some reason" in production, these logs are practically the only clue.
