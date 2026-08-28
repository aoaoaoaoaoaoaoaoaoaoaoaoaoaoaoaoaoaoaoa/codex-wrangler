use anyhow::{Context as _, Result};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::Connection,
    protocol::xproto::{
        Atom, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, Window,
        WindowClass,
    },
    rust_connection::RustConnection,
};

use crate::desktop::Desktop;

pub const NO_DESKTOP: u32 = u32::MAX;
pub const QUIT_REQUEST: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Invocation {
    Summon,
    Resident,
    Quit,
}

impl Invocation {
    pub fn read() -> Result<Self> {
        let mut arguments = std::env::args_os().skip(1);
        match (arguments.next(), arguments.next()) {
            (None, None) => Ok(Self::Summon),
            (Some(argument), None) if argument == "--resident" => Ok(Self::Resident),
            (Some(argument), None) if argument == "--quit" => Ok(Self::Quit),
            _ => anyhow::bail!("usage: codex-wrangler [--resident|--quit]"),
        }
    }
}

pub struct Incumbent {
    pub(crate) conn: RustConnection,
    pub(crate) screen_number: usize,
    pub(crate) anchor: Window,
    pub(crate) summons: Atom,
    pub(crate) launch_desktop: Option<u32>,
    pub(crate) resident: bool,
}

impl Incumbent {
    pub fn seize(invocation: Invocation) -> Result<Option<Self>> {
        let (conn, screen_number) =
            RustConnection::connect(None).context("connect instance rendezvous to X11")?;
        let screen = &conn.setup().roots[screen_number];
        let anchor = conn.generate_id().context("allocate instance anchor")?;
        conn.create_window(
            screen.root_depth,
            anchor,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().override_redirect(1),
        )?
        .check()
        .context("create instance anchor")?;
        let summons = intern(&conn, &format!("_CODEX_WRANGLER_INSTANCE_S{screen_number}"))?;
        let launch_desktop = match invocation {
            Invocation::Summon => Desktop::current_desktop()?,
            Invocation::Resident | Invocation::Quit => None,
        };
        let desktop = launch_desktop.unwrap_or(NO_DESKTOP);

        conn.grab_server()?
            .check()
            .context("lock X11 instance election")?;
        let claimed = (|| -> Result<bool> {
            let owner = conn
                .get_selection_owner(summons)?
                .reply()
                .context("query Codex Wrangler instance")?
                .owner;
            match (owner, invocation) {
                (NONE, Invocation::Summon | Invocation::Resident) => {
                    conn.set_selection_owner(anchor, summons, CURRENT_TIME)?
                        .check()
                        .context("claim Codex Wrangler instance")?;
                    Ok(true)
                }
                (NONE, Invocation::Quit) => Ok(false),
                (_, invocation) => {
                    relay(&conn, owner, summons, desktop, invocation)?;
                    Ok(false)
                }
            }
        })();
        let unlocked = conn
            .ungrab_server()?
            .check()
            .context("unlock X11 instance election");
        let claimed = claimed?;
        unlocked?;

        if claimed {
            conn.flush().context("publish Codex Wrangler instance")?;
            Ok(Some(Self {
                conn,
                screen_number,
                anchor,
                summons,
                launch_desktop,
                resident: invocation == Invocation::Resident,
            }))
        } else {
            conn.destroy_window(anchor)?
                .check()
                .context("retire losing instance anchor")?;
            conn.flush().context("seal Codex Wrangler relay")?;
            Ok(None)
        }
    }

    pub const fn launch_desktop(&self) -> Option<u32> {
        self.launch_desktop
    }

    pub const fn resident(&self) -> bool {
        self.resident
    }
}

fn relay(
    conn: &RustConnection,
    owner: Window,
    summons: Atom,
    desktop: u32,
    invocation: Invocation,
) -> Result<()> {
    let request = match invocation {
        Invocation::Summon | Invocation::Resident => 0,
        Invocation::Quit => QUIT_REQUEST,
    };
    let message = ClientMessageEvent::new(32, owner, summons, [desktop, request, 0, 0, 0]);
    conn.send_event(false, owner, EventMask::NO_EVENT, message)?
        .check()
        .context("signal the existing Codex Wrangler")
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom> {
    Ok(conn
        .intern_atom(false, name.as_bytes())?
        .reply()
        .with_context(|| format!("intern X11 atom `{name}`"))?
        .atom)
}
