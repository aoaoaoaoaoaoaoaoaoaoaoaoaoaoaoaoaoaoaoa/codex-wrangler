codex_wrangler: retired standalone checkout; development belongs in `../../codex-1337`

## gate
- `scripts/check` after edits; `scripts/accept` after native UI changes
- Workstation installs Wrangler via codex-1337 `scripts/install-local`; standalone `scripts/install-local` only for development and recovery

## invariants
- Never release or reinstall this checkout; code stays historical while the fork replaces installed GUI with Codex agents dashboard
- Machine-specialized X11 app: product coordinate = owner's present Codex TUI + Alacritty + i3; portability out of scope
- Native host `eternalist-apps`, visual language Brass Poolrooms; Codex discovery, X11 activation, tray, acceptance fixtures product-owned
- Supports only Codex version of coordinated local distribution; runtime detects capabilities; product fixtures guard integration boundaries
