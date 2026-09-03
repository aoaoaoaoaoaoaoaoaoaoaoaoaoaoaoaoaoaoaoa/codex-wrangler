# Codex Wrangler Agent Guidance

This is an as-is, machine-specialized X11 application. Its product coordinate
is the owner's present Codex TUI + Alacritty + i3 system; portability and
support beyond that coordinate are explicitly out of scope.

Use `eternalist-apps` as the native host and Brass Poolrooms as the visual and
physical language. Keep Codex discovery, X11 activation, tray behavior, and the
acceptance fixtures product-owned.

`docs/codex-compatibility.md` is the support-policy source of truth. Runtime
detects capabilities; each supported Codex line retains a named removal seam
and an acceptance fixture.

Run `scripts/check` after meaningful edits and `scripts/accept` after native UI
changes. This workstation installs Wrangler only through the atomic
`openai-codex` package; `scripts/install-local` is a standalone fallback and
must not shadow a system installation.
