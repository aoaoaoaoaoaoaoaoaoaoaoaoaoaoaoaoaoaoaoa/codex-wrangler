# codex-wrangler

An as-is tray switcher for manual Codex, Claude Code, and Prime Agent TUI
sessions across one or more machines. Linux/X11/i3 only.

[Wrangler names](docs/glossary.md) · [Codex compatibility](docs/codex-compatibility.md)

Wrangler settings live in the platform configuration directory as
`codex-wrangler/config.toml`. Open the central sheet with F2; Control+Comma is
also available. Manual edits preserve comments and layout; an invalid or
unknown key is reported in the sheet and never silently discarded.

```sh
cargo install codex-wrangler
codex-wrangler install
```

Cargo installs executables but has no post-install lifecycle hook. The explicit,
idempotent `install` command installs the desktop entry and user units, enables
the shared Codex app server, and starts Wrangler as a concealed X11 service.
`codex-wrangler uninstall` removes those integrations without deleting settings
or session data. A system package may own the same assets globally; `install`
then removes obsolete user shadows and enables the packaged units.

## Sites

Add OpenSSH destinations or `Host` aliases to `config.toml`:

```toml
remotes = ["MAIN", "vivobook"]
```

Each Site must run the Wrangler-managed Codex distribution and expose
`/usr/bin/codex-wrangler-bridge`. Wrangler reads remote state through that
bridge and opens remote sessions in local Alacritty windows.
Colored diamonds identify Sites; their header legend reports connection,
protocol, and distribution drift. Authentication and host policy remain owned
by OpenSSH configuration.
