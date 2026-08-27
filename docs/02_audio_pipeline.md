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

## Streaming Import for Large Playlists

### The Problem: Bulk waiting times out on hundreds of tracks

The old `ytdlp::resolve` used by `/playlist_add` and `/serverplaylist_add` was a single path that waited for the entire output of `yt-dlp -j` via `Command::output()` before returning a `Vec<TrackInfo>`. While this finishes in seconds for a few dozen tracks, enumerating hundreds or thousands of tracks in a playlist takes time and hits the command handler's timeout. Because the design was "accumulate everything into a Vec in memory before writing to the DB", the start of the write process itself was delayed until the enumeration was complete.

### `TrackStream`: Asynchronous parsing "line by line" instead of the whole process

`TrackStream` in `ytdlp.rs` spawns `yt-dlp -j --flat-playlist --ignore-errors <query>` via `tokio::process::Command` and holds its `stdout` as `tokio::io::Lines<BufReader<ChildStdout>>`.

```text
tokio::process::Command::spawn()
     │
     ▼
ChildStdout (Asynchronous pipe)
     │  BufReader::new(..).lines()
     ▼
Lines<BufReader<ChildStdout>>
     │  .next_line().await  ← 1 line = 1 track's JSON
     ▼
TrackStream::next_track() returns TrackInfo one by one
```

Because `next_track` reads and parses only one line each time it is called, the period of "waiting for `yt-dlp` to finish enumerating everything" simply does not exist. The moment the first track is found, the caller (`playlist::import_tracks_from_stream`, described below) can already start writing to the DB.

### Why the timeout unit was changed from "entire process" to "until the next line"

`run_yt_dlp` (the bulk-wait path used by `search`) applies `YTDLP_TIMEOUT` (20 seconds) to the **entire process**. This is incompatible with the premise that "larger playlists take more time" — enumerating thousands of tracks might take more than 20 seconds not because of a network stall, but simply because of the scale, yet it would be uniformly failed.

Instead, `TrackStream::next_track` applies `NEXT_LINE_TIMEOUT` (15 seconds) only to the **reading of a single line**.

```rust
let line = tokio::time::timeout(NEXT_LINE_TIMEOUT, self.lines.next_line()).await??;
```

This allows the process to take as long as it needs for the fact that "the playlist itself is large", while still detecting and timing out only on network stalls or `yt-dlp` hangs, which manifest as "the next line hasn't arrived even after 15 seconds".

### Ignoring stderr blocks `yt-dlp` itself

`--ignore-errors` skips deleted or private videos in a playlist and continues processing, but it writes a warning to stderr for every skip. The OS pipe buffer is finite (typically around 64KB), so if left unread, the warnings accumulate and fill the buffer, at which point `yt-dlp`'s own writing to stderr blocks. `next_track` is waiting on stdout, but if the same process's stderr is clogged, the entire process halts, directly leading to a deadlock on the stdout reading side.

As a countermeasure, `TrackStream::spawn` also receives stderr via `Stdio::piped()`, and a separate task spawned by `tokio::spawn` (`log_stderr_lines`) continuously forwards it line by line to `tracing::warn!`. Since it runs independently of the `next_track` loop on the stdout side, consuming stderr does not hinder stdout reading.

### Process Cleanup: The two-stage defense of `kill_on_drop` and `Drop`

`Command::kill_on_drop(true)` is set at the time of `TrackStream::spawn`, while `TrackStream` itself also implements `Drop` to call `child.start_kill()`. The former is a mechanism provided by the tokio runtime to "kill when the handle is dropped", and the latter is an explicit insurance policy — ensuring that even if the stream is discarded before being fully read due to an early `?` return, no `yt-dlp` process is ever left behind.

## Importing into Playlists: Chunked Bulk Insert

The tracks returned one by one by `TrackStream` are received and written to the DB by `playlist::import_tracks_from_stream` (`playlist.rs`). Here too, both extremes of "INSERT per item" and "combining everything into one giant INSERT" are avoided.

```text
TrackStream::next_track() ──1 by 1──▶ Vec<TrackInfo> Buffer
                                            │ Every 100 items
                                            ▼
                          Multi-row INSERT via sqlx::QueryBuilder
                                            │
                                            ▼
              (This entire loop is bundled into one db.begin() ~ tx.commit())
```

Every 100 items (`IMPORT_CHUNK_SIZE`), `sqlx::QueryBuilder` assembles and executes "100 rows of `VALUES (...), (...), ...` in a single INSERT statement", and then clears the buffer. SQLite has an upper limit on the number of parameters that can be bound to a single SQL statement (typically hundreds to tens of thousands depending on the version), so combining an entire playlist (up to `MAX_BULK_ADD` = 1000 tracks, 6 columns = max 6000 parameters) into a single statement risks exceeding the limit depending on the environment. Dividing it into units of 100 reliably avoids this limit while requiring far fewer DB round trips than inserting one by one.

The reason `import_tracks_from_stream` as a whole (from the first `db.begin()` to the final `tx.commit()`) is wrapped in a single transaction, rather than committing per chunk, is for consistency. Even if an error occurs midway through enumeration, as long as it is before the commit, the already-inserted chunks are rolled back together, ensuring that a half-baked state like "only the first half of the playlist was added" is never left in the DB.
