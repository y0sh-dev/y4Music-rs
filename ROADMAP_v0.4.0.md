# Roadmap: v0.4.0 — Playlist Differential Sync & Release Hardening

> **Note:** Formal release is fixed at `.N.0` exclusively. Intermediate `.N.1`–`.N.9` are development iterations.

---

## Executive Summary
Consolidate `y4Music-rs` as a practical, low-overhead self-hosted Discord Music Bot (avoiding experimental audio bloat or excessive module splitting). This cycle introduces playlist differential synchronization for external playlist re-imports, verifies and stabilizes runtime behaviour following the `0.3.0` dependency trim (PR #12), and freezes documentation and architecture for the `v0.4.0` milestone.

---

## Micro-Milestones & Cadence

### Phase 1: Feature Implementation & Protocol Coverage (`.3.1` – `.3.4`)
- [ ] **v0.3.1: Product Alignment, Docs Baseline & Sync Foundation**
  - Target: Finalize `README.md` (vision, non-goals, command/env reference) and `ROADMAP_v0.4.0.md`; finalize playlist differential sync design decisions (trigger mode, deletion policy, SQLite `source_url` migration).
- [ ] **v0.3.2: Playlist Differential Sync (Storage & Core Logic)**
  - Target: Add SQLite migration for playlist source tracking (`source_url`) and implement differential track resolution/import logic in `src/playlist.rs`.
- [ ] **v0.3.3: Playlist Differential Sync (Command & Permission Exposure)**
  - Target: Expose diff-sync behaviour across `/playlist_*` and `/serverplaylist_*` (respecting lock and collaborator permissions).
- [ ] **v0.3.4: Post-Dependency-Trim Fixes & UX Polish**
  - Target: Address any regressions or edge cases surfaced during `0.3.0` (PR #12) runtime verification.

### Phase 2: Optimisation, Hardening & Robustness (`.3.5` – `.3.8`)
- [ ] **v0.3.5: Allocation & Queue Path Optimisation**
  - Target: Audit hot paths (panel refresh, playlist streaming import, SQLite queries) for unnecessary allocations or lock contention.
- [ ] **v0.3.6: Fault Tolerance & External Tool Resilience**
  - Target: Harden `yt-dlp` / `ffmpeg` failure handling, partial playlist sync recovery, and Discord 404/429 resilience.
- [ ] **v0.3.7: Live Session Validation**
  - Target: Soak-test long-running voice sessions, Nightcore/Hi-Fi EQ playback, and playlist sync in a live Discord guild.
- [ ] **v0.3.8: Architecture Freeze & Regression Verification**
  - Target: Full `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt -- --check` pass with zero regressions.

### Phase 3: Distillation & Architecture Freeze (`.3.9`)
- [ ] **v0.3.9: Documentation Distillation & Architecture Freeze**
  - Target: Synchronize `docs/00`–`05`, unify English spelling across committed files, and align `README.md` with the frozen `0.4.0` feature set.

### Phase 4: Formal Release (`.4.0`)
- [ ] **v0.4.0: Official Release: Practical Self-Hosted Music Bot**
  - Target: Bump `Cargo.toml` version to `0.4.0`, update version badges, create release PR, and publish GitHub Release.
