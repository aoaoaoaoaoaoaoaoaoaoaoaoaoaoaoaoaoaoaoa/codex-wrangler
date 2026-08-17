# Codex Compatibility

Live discovery detects capabilities, never version strings. This table is the support-policy source of truth.

| Codex | Live thread claim | Rollout vocabulary | Acceptance fixture | Removal seam |
| --- | --- | --- | --- | --- |
| 0.146.x | Writable rollout JSONL descriptors; newest lawful main-thread claim wins | `task_*`, legacy messages | `Goal Codex` owns two main rollouts | `CodexClaim::WritableRollout`, `writable_access` |
| 0.147.x | Open `thread-writer-locks/<id>.lock`, resolved through `threads.rollout_path`; dominates legacy claims | `turn_*`, paginated `item_completed`, legacy records accepted; rollout may be absent before the first turn | `Turn Codex` owns only a writer lock; `Fresh Codex` has no rollout artifact | `CodexClaim::WriterLock`, `writer_lock_thread`, `RolloutSummary::quiescent` |

Both lines require `state_5.sqlite` and the UUID forms of `codex resume` and
`codex fork`. Interactive launch grammar is isolated at `CodexLaunch`. Delete a
row only when its fixture, claim variant, parser arm, and named removal seam
leave in the same change.

Stopped-session rollover compares the launcher's `codex --version` with the
session runtime last sealed in Wrangler's XDG roster. SQLite `cli_version`
seeds that value once; it is the creation version and does not advance on
resume.
