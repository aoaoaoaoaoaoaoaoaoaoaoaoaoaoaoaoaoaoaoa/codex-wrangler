use std::{
    collections::{HashMap, HashSet},
    os::fd::{AsFd as _, BorrowedFd},
};

use anyhow::{Context as _, Result};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::Connection,
    errors::ReplyError,
    protocol::{
        ErrorKind, Event,
        xproto::{
            Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux,
            ConnectionExt as _, EventMask, PropMode, StackMode, Window,
        },
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
    name: Atom,
    net_name: Atom,
    protocols: Atom,
    delete_window: Atom,
}

pub struct Desktop {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    watched: HashSet<Window>,
    action_required: HashMap<Window, bool>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DesktopSignal {
    Focus,
    Topology,
    Workspace,
    Terminal,
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
            name: intern(&conn, "WM_NAME")?,
            net_name: intern(&conn, "_NET_WM_NAME")?,
            protocols: intern(&conn, "WM_PROTOCOLS")?,
            delete_window: intern(&conn, "WM_DELETE_WINDOW")?,
        };
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?
        .check()
        .context("subscribe to X11 desktop changes")?;
        conn.flush().context("arm X11 desktop watch")?;
        Ok(Self {
            conn,
            root,
            atoms,
            watched: HashSet::new(),
            action_required: HashMap::new(),
        })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.conn.stream().as_fd()
    }

    pub fn active_window(&self) -> Result<Option<Window>> {
        self.window(self.root, self.atoms.active)
    }

    pub fn requires_action(&self, window: Window) -> bool {
        self.action_required.get(&window).copied().unwrap_or(false)
    }

    fn read_requires_action(&self, window: Window) -> Result<bool> {
        let title = match self.text(window, self.atoms.net_name)? {
            Some(title) => Some(title),
            None => self.text(window, self.atoms.name)?,
        };
        Ok(title.is_some_and(|title| title_requires_action(&title)))
    }

    pub fn watch_terminals(&mut self, windows: impl IntoIterator<Item = Window>) -> Result<()> {
        let windows = windows.into_iter().collect::<HashSet<_>>();
        let mut watched = self
            .watched
            .intersection(&windows)
            .copied()
            .collect::<HashSet<_>>();
        for window in windows.difference(&self.watched) {
            let result = self
                .conn
                .change_window_attributes(
                    *window,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                )?
                .check();
            match result {
                Ok(()) => {
                    let _new = watched.insert(*window);
                    let required = self.read_requires_action(*window)?;
                    let _prior = self.action_required.insert(*window, required);
                }
                Err(error) if window_vanished(&error) => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("watch terminal window {window}"));
                }
            }
        }
        self.watched = watched;
        self.action_required
            .retain(|window, _required| self.watched.contains(window));
        self.conn.flush().context("arm terminal title watches")
    }

    pub fn drain_events(&mut self) -> Result<HashSet<DesktopSignal>> {
        let mut signals = HashSet::new();
        while let Some(event) = self
            .conn
            .poll_for_event()
            .context("poll X11 desktop events")?
        {
            if let Event::PropertyNotify(event) = event {
                if event.window == self.root {
                    if event.atom == self.atoms.active {
                        let _new = signals.insert(DesktopSignal::Focus);
                    }
                    if event.atom == self.atoms.clients {
                        let _new = signals.insert(DesktopSignal::Topology);
                    }
                    if event.atom == self.atoms.desktop_names
                        || event.atom == self.atoms.current_desktop
                    {
                        let _new = signals.insert(DesktopSignal::Workspace);
                    }
                } else if self.watched.contains(&event.window) {
                    if event.atom == self.atoms.desktop {
                        let _new = signals.insert(DesktopSignal::Workspace);
                    }
                    if event.atom == self.atoms.name || event.atom == self.atoms.net_name {
                        let required = self.read_requires_action(event.window)?;
                        let _prior = self.action_required.insert(event.window, required);
                        let _new = signals.insert(DesktopSignal::Terminal);
                    }
                }
            }
        }
        Ok(signals)
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

    pub fn close(&self, window: Window) -> Result<()> {
        let event = ClientMessageEvent::new(
            32,
            window,
            self.atoms.protocols,
            [self.atoms.delete_window, CURRENT_TIME, 0, 0, 0],
        );
        self.conn
            .send_event(false, window, EventMask::NO_EVENT, event)?
            .check()
            .context("request terminal close")?;
        self.conn.flush().context("flush terminal close")
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

    pub fn window_by_pid(&self, pid: u32) -> Result<Option<Window>> {
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
        let reply = self
            .conn
            .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1)?
            .reply();
        match reply {
            Ok(reply) => Ok(reply.value32().and_then(|mut values| values.next())),
            Err(error) if window_vanished(&error) => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("read X11 cardinal {atom} from window {window}"))
            }
        }
    }

    fn window(&self, window: Window, atom: Atom) -> Result<Option<Window>> {
        let reply = self
            .conn
            .get_property(false, window, atom, AtomEnum::WINDOW, 0, 1)?
            .reply();
        match reply {
            Ok(reply) => Ok(reply.value32().and_then(|mut values| values.next())),
            Err(error) if window_vanished(&error) => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("read X11 window {atom} from window {window}"))
            }
        }
    }

    fn text(&self, window: Window, atom: Atom) -> Result<Option<String>> {
        let reply = self
            .conn
            .get_property(false, window, atom, AtomEnum::ANY, 0, u32::MAX)?
            .reply();
        match reply {
            Ok(reply) if reply.value.is_empty() => Ok(None),
            Ok(reply) => Ok(Some(String::from_utf8_lossy(&reply.value).into_owned())),
            Err(error) if window_vanished(&error) => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("read X11 text {atom} from window {window}"))
            }
        }
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
            let reply = self.conn.query_tree(parent)?.reply();
            let children = match reply {
                Ok(reply) => reply.children,
                Err(error) if window_vanished(&error) => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("walk X11 window {parent}"));
                }
            };
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

fn window_vanished(error: &ReplyError) -> bool {
    matches!(error, ReplyError::X11Error(error) if error.error_kind == ErrorKind::Window)
}

fn title_requires_action(title: &str) -> bool {
    title
        .split('|')
        .next()
        .is_some_and(|head| head.trim_end().ends_with("Action Required"))
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

    #[test]
    fn recognizes_codex_action_required_titles_without_eating_project_names() {
        assert!(title_requires_action("[ ! ] Action Required | projects"));
        assert!(title_requires_action("[ . ] Action Required | projects"));
        assert!(!title_requires_action("[ * ] Working | projects"));
        assert!(!title_requires_action("Action Required Cleanup"));
    }
}
