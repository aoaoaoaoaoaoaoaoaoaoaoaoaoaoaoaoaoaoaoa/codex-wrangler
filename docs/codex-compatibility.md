# Codex Compatibility

Live discovery detects capabilities, never version strings. This table is the support-policy source of truth.

| Codex | Live thread claim | Rollout vocabulary | Acceptance fixture | Removal seam |
| --- | --- | --- | --- | --- |
| 0.153.1-wrangler | The packaged shared app server is mandatory; upstream TUI bindings reconnect in place after connection loss and survive cwd changes | `task_*`, `turn_*`, and paginated `item_completed` | Local acceptance proves process-level discovery and transplantation; bridge protocol 2 projects remote app-server history, loaded-thread status, and the Site-owned Wrangler roster | `codex-wrangler-bridge`, `FleetWorker`, `CODEX_ALLOW_EMBEDDED_SERVER` |

All supported lines require `state_5.sqlite` and the UUID forms of `codex resume`
and `codex fork`. They also provide app-server `thread/name/set` for unloaded,
unarchived session metadata; its removal seam is `CodexRpc::rename_thread` and
`HistoryOperation::Rename`. Because that RPC excludes archived rows, Wrangler
updates their canonical `threads.name` field directly through the
`Historian::rename_archived` seam. Interactive launch grammar is isolated at
`CodexLaunch`. Delete a row only when its fixture, claim variant, parser arm,
and named removal seam leave in the same change.

Live admission trusts the foreground terminal process, an explicit resume
identity or a conservatively reconciled app-server claim, and the absence of an
agent role. `thread_source = 'user'` is canonical; a missing value is admitted
only at this stronger live boundary so durable sessions created before the
field was populated remain usable. A closing terminal cannot donate its
lingering lock to an unrelated TUI. Live admission does not consult
`threads.source`. Historical enumeration uses canonical `thread_source` and a
named legacy seam for null-source `cli` and `vscode` rows.

Codex alone owns archive membership and its `sessions` / `archived_sessions`
layout. Wrangler delegates archive transitions to the Codex CLI, suppresses
upstream-archived roster entries from Live, and consequently exposes them in
History. It does not compress newly archived threads. Historical reads and
resume preparation retain `.zst` support for an out-of-band cold-storage
pipeline; materialization is the only storage interposition. A roster entry
absent from the canonical Codex index is forgotten rather than resurrected.

Historical turn tallies count persisted `user_message` and paginated
`item_completed/UserMessage` records. A running message whose `client_id` begins
with `wire-peer/` is delegated work. Compressed history accepts Zstandard
long-window frames; external archival pipelines may legitimately produce them.

Stopped-session rollover compares the launcher's `codex --version` with the
session runtime last sealed in Wrangler's XDG roster. SQLite `cli_version`
seeds that value once; it is the creation version and does not advance on
resume.

Authentication rollover compares the stopped session's roster mark with a
fresh disk-authenticated account reading. Wrangler then invokes
`codex app-server daemon reload-auth`, which changes the shared daemon in place
and broadcasts `account/updated`; the existing terminal remains the session's
seat. `CodexRpc::reload_daemon_auth` is the removal seam.

Remote Sites are admitted through the bundled `codex-wrangler-bridge`, not by
reading remote files or process tables. The bridge compiles against the exact
app-server protocol and emits protocol-versioned NDJSON over SSH. A protocol
mismatch quarantines remote data. A Codex or bridge build mismatch remains
visible with a `HARMONIZE SITE` fault so compatible state can still be inspected
before the Site is upgraded. Ordinary unavailability is instead a dimmed Site
in the remote legend; an offline workstation is not an application fault.

Each Site alone writes its Wrangler roster. Protocol 2 streams only its thread
identities; the receiver joins them against already-exported history, subtracts
loaded threads, and projects the remainder as remote closed cards without
copying them into local XDG state.
