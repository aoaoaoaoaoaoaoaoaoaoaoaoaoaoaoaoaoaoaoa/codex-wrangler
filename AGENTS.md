# Codex Wrangler Agent Guidance

This is an as-is, machine-specialized X11 application. Its product coordinate
is the owner's present Codex TUI + Alacritty + i3 system; portability and
support beyond that coordinate are explicitly out of scope.

Use `eternalist-apps` as the native host and Brass Poolrooms as the visual and
physical language. Keep Codex discovery, X11 activation, tray behavior, and the
acceptance fixtures product-owned.

Wrangler supports only the Codex version carried by the coordinated local
distribution.
Runtime detects capabilities; product-owned acceptance fixtures guard its
integration boundaries.

Run `scripts/check` after meaningful edits and `scripts/accept` after native UI
changes. This workstation installs Codex, Code Mode, the Site bridge, and
Wrangler from the downstream Codex checkout's `scripts/install-local`. The
standalone `scripts/install-local` installs only Wrangler for development and
recovery.
