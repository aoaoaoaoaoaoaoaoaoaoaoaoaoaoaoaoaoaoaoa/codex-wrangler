use std::{
    env,
    ffi::OsString,
    fs::File,
    io::Write as _,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use egui_tester::{Error, Result, demand};
use nix::pty::openpty;
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Window,
            WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

const TERMINAL_NAMES: [&str; 2] = ["alacritty", "alacritty-0.16.1-x11-ime"];
const KEY_X: u32 = 0x78;
const KEY_X_UPPER: u32 = 0x58;

pub fn invoked() -> Result<bool> {
    let executable = env::current_exe().map_err(fault("locate fixture invocation"))?;
    Ok(executable
        .file_name()
        .is_some_and(|name| TERMINAL_NAMES.iter().any(|candidate| name == *candidate)))
}

pub fn serve() -> Result<()> {
    let invocation = Invocation::parse()?;
    let (conn, screen_number) =
        RustConnection::connect(None).map_err(fault("connect fake Alacritty to X11"))?;
    let screen = &conn.setup().roots[screen_number];
    let window = conn
        .generate_id()
        .map_err(fault("allocate fake Alacritty window"))?;
    conn.create_window(
        screen.root_depth,
        window,
        screen.root,
        0,
        0,
        320,
        120,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .event_mask(EventMask::KEY_PRESS | EventMask::STRUCTURE_NOTIFY),
    )
    .map_err(fault("issue fake Alacritty window creation"))?
    .check()
    .map_err(fault("create fake Alacritty window"))?;
    let atoms = Atoms::raise(&conn)?;
    classify(&conn, window, &atoms, &invocation.title, &invocation.class)?;
    let (mut child, mut input) = invocation.spawn(window)?;
    conn.map_window(window)
        .map_err(fault("issue fake Alacritty window mapping"))?
        .check()
        .map_err(fault("map fake Alacritty window"))?;
    conn.flush()
        .map_err(fault("publish fake Alacritty window"))?;

    loop {
        while let Some(event) = conn
            .poll_for_event()
            .map_err(fault("poll fake Alacritty events"))?
        {
            match event {
                Event::KeyPress(event) if key_is_x(&conn, event.detail)? => {
                    input
                        .write_all(b"x")
                        .map_err(fault("route fake Alacritty input"))?;
                    input.flush().map_err(fault("flush fake Alacritty input"))?;
                }
                Event::ClientMessage(event)
                    if event.window == window
                        && event.type_ == atoms.protocols
                        && event.data.as_data32()[0] == atoms.delete_window =>
                {
                    retire(&mut child)?;
                    conn.destroy_window(window)
                        .map_err(fault("issue fake Alacritty window destruction"))?
                        .check()
                        .map_err(fault("destroy fake Alacritty window"))?;
                    conn.flush()
                        .map_err(fault("retire fake Alacritty window"))?;
                    return Ok(());
                }
                Event::DestroyNotify(event) if event.window == window => {
                    retire(&mut child)?;
                    return Ok(());
                }
                _ => {}
            }
        }
        if let Some(status) = child
            .try_wait()
            .map_err(fault("query fake Alacritty child"))?
        {
            conn.destroy_window(window)
                .map_err(fault("issue exhausted fake Alacritty destruction"))?
                .check()
                .map_err(fault("destroy exhausted fake Alacritty window"))?;
            conn.flush()
                .map_err(fault("retire exhausted fake Alacritty window"))?;
            return demand(
                status.success(),
                format!("fake Alacritty child exited with {status}"),
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}

struct Atoms {
    protocols: Atom,
    delete_window: Atom,
    net_name: Atom,
    utf8: Atom,
    pid: Atom,
}

impl Atoms {
    fn raise(conn: &RustConnection) -> Result<Self> {
        Ok(Self {
            protocols: intern(conn, "WM_PROTOCOLS")?,
            delete_window: intern(conn, "WM_DELETE_WINDOW")?,
            net_name: intern(conn, "_NET_WM_NAME")?,
            utf8: intern(conn, "UTF8_STRING")?,
            pid: intern(conn, "_NET_WM_PID")?,
        })
    }
}

struct Invocation {
    title: String,
    class: String,
    directory: Option<PathBuf>,
    command: Vec<OsString>,
}

impl Invocation {
    fn parse() -> Result<Self> {
        let mut title = "Alacritty".to_owned();
        let mut class = "Alacritty".to_owned();
        let mut directory = None;
        let mut command = Vec::new();
        let mut args = env::args_os().skip(1);
        while let Some(argument) = args.next() {
            match argument.to_str() {
                Some("--title") => {
                    title = required(&mut args, "--title")?
                        .into_string()
                        .map_err(|value| Error::Verdict {
                            detail: format!(
                                "fake Alacritty title is not UTF-8: {}",
                                value.to_string_lossy()
                            ),
                        })?;
                }
                Some("--working-directory") => {
                    directory = Some(PathBuf::from(required(&mut args, "--working-directory")?));
                }
                Some("--class") => {
                    class = required(&mut args, "--class")?
                        .into_string()
                        .map_err(|value| Error::Verdict {
                            detail: format!(
                                "fake terminal class is not UTF-8: {}",
                                value.to_string_lossy()
                            ),
                        })?;
                }
                Some("-o") => {
                    let _ignored = required(&mut args, "-o")?;
                }
                Some("-e") => {
                    command.extend(args);
                    break;
                }
                _ => {
                    return Err(Error::Verdict {
                        detail: format!(
                            "fake Alacritty rejected argument `{}`",
                            argument.to_string_lossy()
                        ),
                    });
                }
            }
        }
        demand(!command.is_empty(), "fake Alacritty command is absent")?;
        Ok(Self {
            title,
            class,
            directory,
            command,
        })
    }

    fn spawn(self, window: Window) -> Result<(Child, File)> {
        let pty = openpty(None, None).map_err(fault("open fake Alacritty PTY"))?;
        let master = File::from(pty.master);
        let slave = File::from(pty.slave);
        let program = self.command.first().expect("validated terminal command");
        let mut command = Command::new(program);
        command
            .args(&self.command[1..])
            .env("WINDOWID", window.to_string())
            .stdin(Stdio::from(
                slave
                    .try_clone()
                    .map_err(fault("clone fake Alacritty stdin"))?,
            ))
            .stdout(Stdio::from(
                slave
                    .try_clone()
                    .map_err(fault("clone fake Alacritty stdout"))?,
            ))
            .stderr(Stdio::from(slave));
        if let Some(directory) = self.directory {
            command.current_dir(directory);
        }
        let child = command
            .spawn()
            .map_err(fault("spawn fake Alacritty child"))?;
        Ok((child, master))
    }
}

fn required(args: &mut impl Iterator<Item = OsString>, option: &'static str) -> Result<OsString> {
    args.next().ok_or_else(|| Error::Verdict {
        detail: format!("fake Alacritty option `{option}` has no value"),
    })
}

fn classify(
    conn: &RustConnection,
    window: Window,
    atoms: &Atoms,
    title: &str,
    class: &str,
) -> Result<()> {
    let class = format!("{}\0{class}\0", class.to_ascii_lowercase());
    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        title.as_bytes(),
    )
    .map_err(fault("issue fake Alacritty window title"))?
    .check()
    .map_err(fault("name fake Alacritty window"))?;
    conn.change_property8(
        PropMode::REPLACE,
        window,
        atoms.net_name,
        atoms.utf8,
        title.as_bytes(),
    )
    .map_err(fault("issue fake Alacritty UTF-8 title"))?
    .check()
    .map_err(fault("publish fake Alacritty UTF-8 title"))?;
    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        class.as_bytes(),
    )
    .map_err(fault("issue fake Alacritty class"))?
    .check()
    .map_err(fault("classify fake Alacritty window"))?;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        atoms.pid,
        AtomEnum::CARDINAL,
        &[std::process::id()],
    )
    .map_err(fault("issue fake Alacritty PID"))?
    .check()
    .map_err(fault("publish fake Alacritty PID"))?;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        atoms.protocols,
        AtomEnum::ATOM,
        &[atoms.delete_window],
    )
    .map_err(fault("issue fake Alacritty close protocol"))?
    .check()
    .map_err(fault("publish fake Alacritty close protocol"))?;
    Ok(())
}

fn key_is_x(conn: &RustConnection, keycode: u8) -> Result<bool> {
    let mapping = conn
        .get_keyboard_mapping(keycode, 1)
        .map_err(fault("issue fake Alacritty key lookup"))?
        .reply()
        .map_err(fault("resolve fake Alacritty key"))?;
    Ok(mapping
        .keysyms
        .iter()
        .any(|keysym| *keysym == KEY_X || *keysym == KEY_X_UPPER))
}

fn intern(conn: &RustConnection, name: &'static str) -> Result<Atom> {
    Ok(conn
        .intern_atom(false, name.as_bytes())
        .map_err(fault("issue fake Alacritty atom lookup"))?
        .reply()
        .map_err(fault("intern fake Alacritty atom"))?
        .atom)
}

fn retire(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .map_err(fault("query retiring fake Alacritty child"))?
        .is_none()
    {
        child.kill().map_err(fault("kill fake Alacritty child"))?;
        let _status = child.wait().map_err(fault("reap fake Alacritty child"))?;
    }
    Ok(())
}

fn fault<E>(operation: &'static str) -> impl FnOnce(E) -> Error
where
    E: std::fmt::Display,
{
    move |error| Error::Verdict {
        detail: format!("{operation}: {error}"),
    }
}
