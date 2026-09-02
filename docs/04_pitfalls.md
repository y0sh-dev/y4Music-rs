# 04. Rust/Tokio Specific Pitfalls

This is a collection of traps actually stepped on during the development of this project. The first "task self-cancellation problem" in particular was the most difficult to reproduce and identify, as the conditions for occurrence are specific and it produces absolutely no errors or logs.

## Trap 1: The Self-Cancellation Problem of `JoinHandle::abort()` (Most Important)

### Symptom

A phenomenon occurred where "it looks like it disconnected from the voice channel, but the connection actually remains on the Discord gateway". Neither failure nor success was output to the logs; it just looked like the process disappeared midway.

### Cause

`player::cleanup_guild` calls `.abort()` on all pending timers (`empty_channel_leave_task`, `idle_leave_task`, `progress_ticker`) as part of guild cleanup. However, these timers themselves (for example, after `idle_leave_task` wakes up from `sleep`) are coded to "call `cleanup_guild` if still idle". In other words:

```text
Closure started by idle_leave_task
  └─ Wakes up from sleep(600s)
     └─ Calls cleanup_guild()
        └─ Inside the process of "aborting all pending timers"
           └─ Finds its own JoinHandle (idle_leave_task) and aborts it
```

In Tokio, calling `JoinHandle::abort()` **from inside the task itself** does not cause an immediate panic or stop. It merely schedules a cancellation "at the next `.await` suspension point". A few lines after the `abort()`, `cleanup_guild` calls `channel_id.edit_message(..).await` (sending the goodbye panel), and then calls `manager.remove(guild_id).await` (the actual process of leaving the voice channel). The self-abort reservation triggers at this first `.await`, and the task is quietly dropped there — it never reaches `manager.remove()`. No error is produced. The execution just vanishes.

### Solution: `abort_unless_self`

```rust
// player.rs
fn abort_unless_self(task: tokio::task::JoinHandle<()>) {
    if tokio::task::try_id().is_some_and(|id| id == task.id()) {
        drop(task);   // If it's itself, don't abort, just discard the handle
    } else {
        task.abort();
    }
}
```

Use `tokio::task::try_id()` to get the "ID of the currently executing task" and compare it with the ID of the handle you are trying to abort. If they match, the task is continuing execution anyway, so there is no need to "stop" it — you just need to discard the handle so that other places (like `is_some_and(|t| !t.is_finished())` checks) don't mistakenly think the old handle is "still executing".

All stop paths for `empty_channel_leave_task`, `idle_leave_task`, and `progress_ticker` in `player.rs` go through this function, and raw `.abort()` is not called. The `abort_unless_self_tests` module contains both a regression test that actually reproduces self-cancellation and a control test confirming that a raw `.abort()` really does self-cancel.

**Lesson**: When writing code that "cleans up tasks you scheduled from within those tasks themselves as part of cleanup", suspect the possibility that the "currently running self" might be included in the cleanup targets.

## Trap 2: Synchronous Lock Guards and `.await`

The guard returned by `DashMap::get()` is a synchronous lock. Holding it across an `.await` blocks other accesses to that shard, leading to deadlocks depending on conditions. This project standardizes on the boilerplate pattern of "cloning the `Arc` to immediately release the guard, and then locking the asynchronous `Mutex`". See `01_state_management.md` for details and concrete code examples.

## Trap 3: `async fn` in trait and `dyn Compose`

`songbird::input::Compose` has an `async fn` called `create_async`, but `FfmpegEqSource` is ultimately objectified as `Box<dyn Compose>` (`Input::Lazy(Box::new(val))`). Rust's `async fn` in trait is implemented via RPITIT (using `impl Trait` in return position), which returns an `impl Future` whose size is not determined at compile time, so it cannot be made into a `dyn Trait` trait object as is.

```rust
#[async_trait::async_trait]
impl Compose for FfmpegEqSource { ... }
```

`#[async_trait::async_trait]` mechanically rewrites this `async fn` to `fn(...) -> Pin<Box<dyn Future<Output = ...>>>`, making it dynamically dispatchable. Since songbird's `Compose` trait itself is defined assuming this macro, if the implementing side forgets to add this attribute, the types won't match and a compile error will occur. "Why is the `async_trait` macro needed only here?" stems from this `dyn` compatibility constraint.

## Trap 4: let-chains (`if let ... && let ... { }`)

The let-chains syntax stabilized in Rust 2024 edition is used everywhere (e.g., `player.rs::handle_voice_state_update`, `commands/profile.rs::volume`).

```rust
if let Some(pause) = should_be_paused
    && let Some(call) = manager.get(guild_id)
{
    ...
}
```

This is a relatively new syntax that allows straightforwardly chaining `if let`s with `&&`. It flattens places that previously required nested `if let`s or constructs like `match (a, b) { (Some(x), Some(y)) => ..., _ => {} }`. It is explicitly mentioned here because readers with slightly older Rust knowledge might pause for a moment wondering "why are `if let`s connected with `&&`?".

## Trap 5: Network I/O While Holding Asynchronous Locks

### Symptom

When HTTP requests to the Discord API (e.g., `edit_message`, `send_message`, `say`) encounter rate limits or temporary delays, other requests in the same guild—such as someone pressing a panel button, or another event calling `refresh_panel`—appear to freeze inexplicably. They don't error out; they just hang until they time out.

### Cause

If you `.await` external I/O while holding a `MutexGuard` from `tokio::sync::Mutex`, no other task can lock that same `Mutex` until the I/O completes. Because `GuildPlayerState` and `Call` are shared per guild via a single `Arc<Mutex<..>>`, code like "holding `state.lock().await` while doing `channel_id.edit_message(..).await`" forces all other operations in that guild to wait for the lock for the duration of that I/O (hundreds of milliseconds, or much longer during an outage). Unlike local lock operations, network I/O completion times cannot be controlled, making them prone to unpredictable, long-duration locks.

### Solution: Snapshot, `drop`, then I/O Lock-Free

```rust
// player/ui.rs::refresh_panel (excerpt)
let (channel_id, message_id, loop_mode) = {
    let state = state_arc.lock().await;
    let Some(channel_id) = state.text_channel else {
        return;
    };
    (channel_id, state.now_playing_message, state.loop_mode)
}; // The state lock is released here

// Call the Discord API without holding any locks
channel_id.edit_message(http, message_id, edit).await;
```

Extract the necessary values inside a `{ .. }` block, and let the guard scope out and immediately `drop` when exiting the block. Only re-acquire the lock briefly later if you need to write back the result of the I/O call (like recording a new message ID). `refresh_panel`, `cleanup_guild`, and `handle_voice_state_update` in `player/ui.rs` are all standardized to this pattern.

**Lesson**: The scope of a `MutexGuard` does not necessarily have to match the scope of "the data you want to protect with that lock." The lock is only needed for the exact moment of reading or writing shared data; there is no reason to keep holding it through subsequent I/O. Strictly adhering to the pattern of "Acquire lock → Extract needed values → Release immediately → Perform I/O after releasing" is highly recommended.
