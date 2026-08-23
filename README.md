# codex-wrangler

An as-is tray switcher for manual Codex, Claude Code, and Prime Agent TUI sessions.
Linux/X11/i3 only.

[Wrangler names](docs/glossary.md) · [Codex compatibility](docs/codex-compatibility.md)

Wrangler settings live in the platform configuration directory as
`codex-wrangler/config.toml`. Open the central sheet with F2; Control+Comma is
also available. Manual edits preserve comments and layout; an invalid or
unknown key is reported in the sheet and never silently discarded.

```sh
cargo install codex-wrangler
```
