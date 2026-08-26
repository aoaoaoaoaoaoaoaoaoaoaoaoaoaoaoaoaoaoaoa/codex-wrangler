# Codex Compatibility

Live discovery detects capabilities, never version strings. This table is the support-policy source of truth.

| Codex | Live thread claim | Rollout vocabulary | Acceptance fixture | Removal seam |
| --- | --- | --- | --- | --- |
| 0.146.x | Writable rollout JSONL descriptors; newest lawful main-thread claim wins | `task_*`, legacy messages | `Goal Codex` owns two main rollouts | `CodexClaim::WritableRollout`, `writable_access` |
| 0.147.x | Open `thread-writer-locks/<id>.lock`, resolved through `threads.rollout_path`; dominates legacy claims | `turn_*`, paginated `item_completed`, legacy records accepted; rollout may be absent before the first turn | `Turn Codex` owns only a writer lock; `Fresh Codex` has no rollout artifact | `CodexClaim::WriterLock`, `writer_lock_thread`, `RolloutSummary::quiescent` |
| 0.149.x | Persistent `codex app-server` owns writer locks and rollouts; an established TUI binding survives cwd changes, while a bare `resume` may recover one unique unclaimed primary lock by Git origin after a repository move | 0.147 vocabulary retained | `Fresh Codex` resumes from a transplanted worktree with no claim or rollout; a detached app-server owns its older lock and its row falsely says `source = 'vscode'` | `app_server_writer_claims`, `Process::app_server_claim`, `GitOrigin` |

All supported lines require `state_5.sqlite` and the UUID forms of `codex resume`
and `codex fork`. They also provide app-server `thread/name/set` for unloaded,
unarchived session metadata; its removal seam is `CodexRpc::rename_thread` and
`HistoryOperation::Rename`. Because that RPC excludes archived rows, Wrangler
updates their canonical `threads.name` field directly through the
`Historian::rename_archived` seam. Interactive launch grammar is isolated at
`CodexLaunch`. Delete a row only when its fixture, claim variant, parser arm,
and named removal seam leave in the same change.

Live admission trusts the foreground terminal process, an exact direct claim or
a conservatively reconciled app-server claim, `thread_source = 'user'`, and the
absence of an agent role. An app-server claim must be newly acquired in the
same cwd, already bound to that PID incarnation, or the sole primary Git-origin
match for a bare `resume`. A closing terminal cannot donate its lingering lock
to an unrelated TUI. Live admission does not consult `threads.source`. Historical
enumeration retains `source = 'cli'` because no live process exists to supply
the stronger proof; every anomalous 0.149 TUI row enters Wrangler's roster while
live and remains a closed session thereafter.

Historical turn tallies count persisted `user_message` and paginated
`item_completed/UserMessage` records. A running message whose `client_id` begins
with `wire-peer/` is delegated work. Compressed history accepts Zstandard
long-window frames; external archival pipelines may legitimately produce them.

Stopped-session rollover compares the launcher's `codex --version` with the
session runtime last sealed in Wrangler's XDG roster. SQLite `cli_version`
seeds that value once; it is the creation version and does not advance on
resume.
