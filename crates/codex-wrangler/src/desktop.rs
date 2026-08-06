use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::Connection,
    protocol::xproto::{
        Atom, AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask,
        PropMode, StackMode, Window,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

struct Atoms {
    clients: Atom,
    pid: Atom,
    active: Atom,
    desktop: Atom,
    desktop_names: Atom,
    current_desktop: Atom,
}

pub struct Desktop {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
}

impl Desktop {
    pub fn connect() -> Result<Self> {
        let (conn, screen) = RustConnection::connect(None).context("connect to X11")?;
        let root = conn.setup().roots[screen].root;
        let atoms = Atoms {
            clients: intern(&conn, "_NET_CLIENT_LIST")?,
            pid: intern(&conn, "_NET_WM_PID")?,
            active: intern(&conn, "_NET_ACTIVE_WINDOW")?,
            desktop: intern(&conn, "_NET_WM_DESKTOP")?,
            desktop_names: intern(&conn, "_NET_DESKTOP_NAMES")?,
            current_desktop: intern(&conn, "_NET_CURRENT_DESKTOP")?,
        };
        Ok(Self { conn, root, atoms })
    }

    pub fn windows_by_pid(&self) -> Result<HashMap<u32, Window>> {
        let clients = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.clients,
                AtomEnum::WINDOW,
                0,
                u32::MAX,
            )?
            .reply()
            .context("read X11 client list")?
            .value32()
            .map(Iterator::collect::<Vec<_>>)
            .unwrap_or_default();
        let clients = if clients.is_empty() {
            self.descendants()?
        } else {
            clients
        };
        let mut windows = HashMap::new();
        for window in clients {
            if let Some(pid) = self.window_pid(window)? {
                let _old = windows.insert(pid, window);
            }
        }
        Ok(windows)
    }

    pub fn activate(&self, window: Window) -> Result<()> {
        self.drive_window(window, None)?;
        self.conn.flush().context("activate harness terminal")
    }

    fn drive_window(&self, window: Window, destination: Option<u32>) -> Result<()> {
        if let Some(index) = destination {
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    window,
                    self.atoms.desktop,
                    AtomEnum::CARDINAL,
                    &[index],
                )?
                .check()
                .context("prime destination workspace")?;
            let event =
                ClientMessageEvent::new(32, window, self.atoms.desktop, [index, 2, 0, 0, 0]);
            self.conn
                .send_event(
                    false,
                    self.root,
                    EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                    event,
                )?
                .check()
                .context("move window to destination workspace")?;
        }
        self.conn
            .map_window(window)?
            .check()
            .context("map destination window")?;
        self.conn
            .configure_window(
                window,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?
            .check()
            .context("raise destination window")?;
        let event =
            ClientMessageEvent::new(32, window, self.atoms.active, [2, CURRENT_TIME, 0, 0, 0]);
        self.conn
            .send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )?
            .check()
            .context("announce active window")?;
        Ok(())
    }

    pub fn workspace_numbers(
        &self,
        windows: impl IntoIterator<Item = Window>,
    ) -> Result<HashMap<Window, u32>> {
        let names = self.desktop_names()?;
        let mut workspaces = HashMap::new();
        for window in windows {
            let Some(index) = self.cardinal(window, self.atoms.desktop)? else {
                continue;
            };
            let Some(Some(number)) = usize::try_from(index)
                .ok()
                .and_then(|index| names.get(index))
            else {
                continue;
            };
            let _old = workspaces.insert(window, *number);
        }
        Ok(workspaces)
    }

    pub fn current_desktop() -> Result<Option<u32>> {
        let desktop = Self::connect()?;
        desktop.cardinal(desktop.root, desktop.atoms.current_desktop)
    }

    pub fn process_floating(pid: u32) -> Result<Option<bool>> {
        let desktop = Self::connect()?;
        let Some(window) = desktop.window_by_pid(pid)? else {
            return Ok(None);
        };
        crate::i3::window_floating(window)
    }

    pub fn summon_process_to(pid: u32, index: Option<u32>, floating: bool) -> Result<bool> {
        let desktop = Self::connect()?;
        let Some(window) = desktop.window_by_pid(pid)? else {
            return Ok(false);
        };
        let destination = floating.then_some(index).flatten();
        desktop.drive_window(window, destination)?;
        desktop.conn.flush().context("summon Wrangler")?;
        let workspace = desktop.cardinal(window, desktop.atoms.desktop)?;
        let workspace_settled = match destination {
            Some(index) => desktop.cardinal(window, desktop.atoms.desktop)? == Some(index),
            None => match workspace {
                Some(index) => {
                    desktop.cardinal(desktop.root, desktop.atoms.current_desktop)? == Some(index)
                }
                None => true,
            },
        };
        let focus_settled = desktop.window(desktop.root, desktop.atoms.active)? == Some(window);
        Ok(workspace_settled && focus_settled)
    }

    fn window_pid(&self, window: Window) -> Result<Option<u32>> {
        self.cardinal(window, self.atoms.pid)
    }

    fn window_by_pid(&self, pid: u32) -> Result<Option<Window>> {
        if let Some(window) = self.windows_by_pid()?.get(&pid) {
            return Ok(Some(*window));
        }
        for window in self.descendants()? {
            if self.window_pid(window)? == Some(pid) {
                return Ok(Some(window));
            }
        }
        Ok(None)
    }

    fn cardinal(&self, window: Window, atom: Atom) -> Result<Option<u32>> {
        Ok(self
            .conn
            .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1)?
            .reply()
            .with_context(|| format!("read X11 cardinal {atom} from window {window}"))?
            .value32()
            .and_then(|mut values| values.next()))
    }

    fn window(&self, window: Window, atom: Atom) -> Result<Option<Window>> {
        Ok(self
            .conn
            .get_property(false, window, atom, AtomEnum::WINDOW, 0, 1)?
            .reply()
            .with_context(|| format!("read X11 window {atom} from window {window}"))?
            .value32()
            .and_then(|mut values| values.next()))
    }

    fn desktop_names(&self) -> Result<Vec<Option<u32>>> {
        let bytes = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.desktop_names,
                AtomEnum::ANY,
                0,
                u32::MAX,
            )?
            .reply()
            .context("read X11 desktop names")?
            .value;
        Ok(bytes
            .split(|byte| *byte == 0)
            .map(workspace_number)
            .collect())
    }

    fn descendants(&self) -> Result<Vec<Window>> {
        let mut frontier = vec![self.root];
        let mut seen = HashSet::from([self.root]);
        let mut descendants = Vec::new();
        while let Some(parent) = frontier.pop() {
            let children = self
                .conn
                .query_tree(parent)?
                .reply()
                .with_context(|| format!("walk X11 window {parent}"))?
                .children;
            for child in children {
                if child != NONE && seen.insert(child) {
                    descendants.push(child);
                    frontier.push(child);
                }
            }
        }
        Ok(descendants)
    }
}

fn workspace_number(name: &[u8]) -> Option<u32> {
    let end = name
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(name.len());
    (end > 0).then(|| std::str::from_utf8(&name[..end]).ok()?.parse().ok())?
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom> {
    Ok(conn
        .intern_atom(false, name.as_bytes())?
        .reply()
        .with_context(|| format!("intern X11 atom `{name}`"))?
        .atom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_i3s_numeric_workspace_prefix() {
        assert_eq!(workspace_number(b"17: codex"), Some(17));
        assert_eq!(workspace_number(b"8"), Some(8));
        assert_eq!(workspace_number(b"codex"), None);
        assert_eq!(workspace_number(b""), None);
    }
}
