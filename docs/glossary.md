# Wrangler Names

This glossary admits only product concepts that have settled into durable
meaning. Shared application and physical-control terms belong to Eternalist
Apps and Brass Poolrooms respectively.

**Harness**
: One supported agent TUI family: Codex, Claude Code, or Prime Agent.

**Thread**
: The harness-owned durable conversation identity. A thread may be live,
historical, archived, or absent from the current host process table.

**Session**
: Wrangler's observed projection of a thread at a point in its lifecycle. A
session is not a process, terminal window, or workspace.

**Site**
: One machine that owns a Codex app server and its threads. `Local` is the
machine rendering Wrangler; every remote Site is identified by one OpenSSH
destination or `Host` alias.

**Seat**
: A terminal on the machine rendering Wrangler that currently displays a
session. Session ownership and remote terminal windows do not confer a Seat.

**Workspace**
: The i3 workspace containing a Seat.

**Pin**
: A Wrangler-owned ordering mark that keeps a session in the head bucket. It
does not change harness state.

**Fork**
: A harness operation that creates a new thread from an existing Codex thread.

**History**
: Harness-owned durable sessions not admitted as live. Archive state is one
property of history, not a synonym for it.
