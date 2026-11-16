# Project Instructions — y4Music-rs

## 0. Non-negotiables (Project-Specific)
- Global `~/.agent-protocols/CARD.md` is the authoritative safety set. On conflict, follow the more restrictive rule and report it.
- **Rule 1 (Audio Pipeline & Zero Unapproved Crates)**: External `ffmpeg` and `yt-dlp` handle media resolution and decoding into `f32le` 48 kHz stereo PCM; `symphonia` must retain `features = ["pcm"]` for `songbird`'s `RawAdapter`. Adding any new crate to `Cargo.toml` without explicit prior approval is strictly prohibited.
- **Rule 2 (Concurrency & Panic-Free Safety)**: Never hold `DashMap` guards or `tokio::sync::Mutex` locks (`GuildPlayerState`, `Call`) across Discord/network `.await` calls. Avoid `unwrap()`, `expect()`, and `panic!()` in production paths.
- **Pragmatic Exception Clause**: Any prohibition in this repository (current or future) may be bypassed as an exception only if the agent demonstrates **mathematical superiority and logical inevitability** for doing so, presents that proof to the human maintainer, and obtains explicit human approval.
- **Language**: All Git-committed deliverables (code, comments, commits, PRs, docs) strictly in English. Japanese is restricted to human communication and non-committed files (e.g., `status.md`).

## 1. First 5 Minutes (Immediate Verification)
1. Verify working branch: `git status && git branch --show-current` (Never edit directly on `main`).
2. Run standard verification: `cargo test --all-targets && cargo clippy --all-targets -- -D warnings` (Exit 0 required; requires `ffmpeg` on `PATH` for integration tests).
3. If the same failure signature appears twice, stop and evaluate (Hard limiter: 5 attempts max).

## 2. Roles & Archetypes
- **Orchestrator (Gemini)**: Macro specifications, Pre-Commit Deliberation reports, `status.md` read-only review, and human-hub task delegation.
- **Implementer (DeepSeek v4.1 Flash)**: Fast, minimal diffs, local test verification (`cargo test --all-targets && cargo clippy --all-targets -- -D warnings`), panic-free compliance, and `status.md` reporting.
- **Reasoner (Auditor)**: Unassigned / handled by Orchestrator or human on demand for concurrency, DSP, and soundness audits.

## 3. Architecture & Workspace Topology
- **Stack**: Rust (2024 edition), `poise` 0.6 + `serenity` 0.12 (Discord bot), `songbird` 0.6 (voice driver/queue), `sqlx` 0.9 (SQLite).
- **`src/main.rs`**: Client bootstrap, SQLite pool/migration init, Discord slash commands and component/voice event routing.
- **`src/player/`** (4-layer DAG, re-exported via `mod.rs`):
  - `state.rs` (Layer 0): `TrackMeta`, `LoopMode`, `GuildPlayerState`, `GuildPlayers`, timer cancellation primitives.
  - `track.rs` (Layer 1): Seek, pre-emptive track-loop clone, YouTube metadata helpers, queue-tail shuffle.
  - `ui.rs` (Layer 2): Now Playing embed/components, `refresh_panel` (404-only reset), voice/button handlers, guild cleanup.
  - `events.rs` (Layer 3): `songbird` global handlers (`PanelUpdater`, `PlaybackErrorNotifier`, `LoopHandler`).
- **`src/audio_source.rs` & `src/eq.rs`**: Custom `Compose` pipeline (`yt-dlp -j` + `ffmpeg` `f32le` PCM) with Balanced / Hi-Fi DSP filtergraphs.
- **`src/commands/`**: Slash commands (`playback.rs`, `profile.rs`, `playlist.rs`, `search.rs`).
- **`src/db.rs`, `src/models.rs`, `src/playlist.rs`, `src/ytdlp.rs`, `src/pagination.rs`**: Persistence, playlist streaming import, and UI pagination.
- **`migrations/` & `tests/`**: SQLite migrations and pipeline integration tests (`tests/audio_source_pipeline.rs`).

## 4. Key Verification Commands
- **Primary Check**: `cargo test --all-targets && cargo clippy --all-targets -- -D warnings`
- **Granular Test**: `cargo test --all-targets`
- **Strict Lint**: `cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`

## 5. Fallback Safety Set
If `~/.agent-protocols/CARD.md` is unavailable:
1. Never commit or push directly to `main`; always use topic branches.
2. Irreversible or outward actions (tag, release, PR merge, push) require explicit human approval.
3. Dialogue in Japanese; deliverables (code, commits, PRs, docs) in English.
4. Stop on 5 consecutive failures of the same error signature. Report missing global SSOT.
