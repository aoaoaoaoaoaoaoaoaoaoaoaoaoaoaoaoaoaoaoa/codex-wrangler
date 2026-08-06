use anyhow::{Context as _, Result};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::{
    io::Write as _,
    os::{fd::AsFd as _, unix::net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::Connection,
    protocol::{
        Event,
        randr::ConnectionExt as _,
        xproto::{
            Arc as XArc, Atom, AtomEnum, ButtonPressEvent, CapStyle, ClientMessageEvent,
            ConnectionExt as _, CoordMode, CreateGCAux, CreateWindowAux, EventMask, Gcontext,
            GrabMode, JoinStyle, LineStyle, Point, PropMode, Rectangle, Screen, Window,
            WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use crate::instance::{Incumbent, NO_DESKTOP};

const ICON_SIZE: u16 = 24;
const MENU_WIDTH: u16 = 140;
const MENU_HEIGHT: u16 = 30;
const MENU_BORDER: u16 = 1;
const MENU_GAP: i32 = 4;
const DOCK_REQUEST: u32 = 0;
const XEMBED_MAPPED: u32 = 1;
const OWNER_POLL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug)]
pub enum Signal {
    Reveal(Option<u32>),
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DesktopRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl DesktopRect {
    const fn right(self) -> i32 {
        self.x + self.width
    }

    const fn bottom(self) -> i32 {
        self.y + self.height
    }

    fn distance_squared(self, [x, y]: [i32; 2]) -> i64 {
        let nearest_x = x.clamp(self.x, self.right() - 1);
        let nearest_y = y.clamp(self.y, self.bottom() - 1);
        i64::from(x - nearest_x).pow(2) + i64::from(y - nearest_y).pow(2)
    }
}

fn nearest_monitor(monitors: &[DesktopRect], point: [i32; 2]) -> Option<DesktopRect> {
    monitors
        .iter()
        .copied()
        .filter(|monitor| monitor.width > 0 && monitor.height > 0)
        .min_by_key(|monitor| monitor.distance_squared(point))
}

fn popup_origin(monitor: DesktopRect, anchor: DesktopRect, [width, height]: [i32; 2]) -> [i32; 2] {
    let x = (anchor.right() - width).clamp(monitor.x, (monitor.right() - width).max(monitor.x));
    let below = anchor.bottom() + MENU_GAP;
    let above = anchor.y - MENU_GAP - height;
    let desired_y = if below + height <= monitor.bottom() {
        below
    } else {
        above
    };
    let y = desired_y.clamp(monitor.y, (monitor.bottom() - height).max(monitor.y));
    [x, y]
}

struct Atoms {
    selection: Atom,
    opcode: Atom,
    xembed_info: Atom,
}

struct Inks {
    rim: Gcontext,
    iris: Gcontext,
    pupil: Gcontext,
    menu: Gcontext,
}

struct X11Tray {
    conn: RustConnection,
    root: Window,
    root_width: u16,
    root_height: u16,
    anchor: Window,
    summons: Atom,
    icon: Window,
    menu: Window,
    rim: Gcontext,
    iris: Gcontext,
    pupil: Gcontext,
    menu_ink: Gcontext,
    atoms: Atoms,
    owner: Window,
    menu_live: bool,
    standalone: bool,
    available: Arc<AtomicBool>,
    emit: Arc<dyn Fn(Signal) + Send + Sync>,
}

impl X11Tray {
    fn forge(
        incumbent: Incumbent,
        available: Arc<AtomicBool>,
        emit: Arc<dyn Fn(Signal) + Send + Sync>,
    ) -> Result<Self> {
        let Incumbent {
            conn,
            screen_number,
            anchor,
            summons,
            launch_desktop: _,
        } = incumbent;
        let screen = &conn.setup().roots[screen_number];
        let root = screen.root;
        let root_width = screen.width_in_pixels;
        let root_height = screen.height_in_pixels;
        let atoms = Atoms {
            selection: intern(&conn, &format!("_NET_SYSTEM_TRAY_S{screen_number}"))?,
            opcode: intern(&conn, "_NET_SYSTEM_TRAY_OPCODE")?,
            xembed_info: intern(&conn, "_XEMBED_INFO")?,
        };
        #[cfg(feature = "egui-test")]
        let test_window = std::env::var_os("CODEX_WRANGLER_TEST_TRAY_WINDOW").is_some();
        #[cfg(not(feature = "egui-test"))]
        let test_window = false;
        let icon = forge_icon(&conn, screen, &atoms, test_window)?;
        let menu = forge_menu(&conn, screen)?;
        let Inks {
            rim,
            iris,
            pupil,
            menu: menu_ink,
        } = forge_inks(&conn, screen, icon, menu)?;
        let mut tray = Self {
            conn,
            root,
            root_width,
            root_height,
            anchor,
            summons,
            icon,
            menu,
            rim,
            iris,
            pupil,
            menu_ink,
            atoms,
            owner: NONE,
            menu_live: false,
            standalone: test_window,
            available,
            emit,
        };
        tray.reconcile_owner()?;
        if test_window {
            let _mapped = tray.conn.map_window(tray.icon)?;
        }
        tray.conn.flush().context("flush tray creation")?;
        Ok(tray)
    }

    fn run(&mut self, alive: &AtomicBool, wake: &UnixStream) -> Result<()> {
        let mut owner_poll = Instant::now() + OWNER_POLL;
        while alive.load(Ordering::Acquire) {
            let timeout = owner_poll.saturating_duration_since(Instant::now());
            let (x_ready, wake_ready) = {
                let mut descriptors = [
                    PollFd::new(self.conn.stream().as_fd(), PollFlags::POLLIN),
                    PollFd::new(wake.as_fd(), PollFlags::POLLIN),
                ];
                let timeout = PollTimeout::try_from(timeout).unwrap_or(PollTimeout::MAX);
                let _ready = poll(&mut descriptors, timeout).context("wait for tray events")?;
                (
                    descriptors[0]
                        .revents()
                        .is_some_and(|events| events.contains(PollFlags::POLLIN)),
                    descriptors[1]
                        .revents()
                        .is_some_and(|events| events.contains(PollFlags::POLLIN)),
                )
            };
            if wake_ready {
                break;
            }
            if x_ready {
                while let Some(event) = self.conn.poll_for_event().context("poll tray events")? {
                    self.heed(&event)?;
                }
            }
            if Instant::now() >= owner_poll {
                self.reconcile_owner()?;
                owner_poll = Instant::now() + OWNER_POLL;
            }
        }
        Ok(())
    }

    fn reconcile_owner(&mut self) -> Result<()> {
        let owner = self
            .conn
            .get_selection_owner(self.atoms.selection)?
            .reply()
            .context("query system tray owner")?
            .owner;
        self.available
            .store(self.standalone || owner != NONE, Ordering::Release);
        if owner != self.owner {
            self.owner = owner;
            if owner != NONE {
                let dock = ClientMessageEvent::new(
                    32,
                    owner,
                    self.atoms.opcode,
                    [CURRENT_TIME, DOCK_REQUEST, self.icon, 0, 0],
                );
                let _sent = self
                    .conn
                    .send_event(false, owner, EventMask::NO_EVENT, dock)?;
                self.conn.flush().context("dock tray icon")?;
            }
        }
        Ok(())
    }

    fn heed(&mut self, event: &Event) -> Result<()> {
        match event {
            Event::Expose(event) if event.window == self.icon => self.paint_icon()?,
            Event::ConfigureNotify(event) if event.window == self.icon => self.paint_icon()?,
            Event::ButtonPress(event) if event.event == self.icon => self.click_icon(event)?,
            Event::ClientMessage(event)
                if event.window == self.anchor && event.type_ == self.summons =>
            {
                let desktop = event.data.as_data32()[0];
                (self.emit)(Signal::Reveal((desktop != NO_DESKTOP).then_some(desktop)));
            }
            Event::Expose(event) if event.window == self.menu => self.paint_menu()?,
            Event::ButtonPress(event) if self.menu_live => self.click_menu(event)?,
            Event::KeyPress(_) if self.menu_live => self.hide_menu()?,
            _ => {}
        }
        Ok(())
    }

    fn click_icon(&mut self, event: &ButtonPressEvent) -> Result<()> {
        match event.detail {
            1 => (self.emit)(Signal::Reveal(None)),
            3 => self.show_menu(event.root_x, event.root_y)?,
            _ => {}
        }
        Ok(())
    }

    fn show_menu(&mut self, root_x: i16, root_y: i16) -> Result<()> {
        let point = [i32::from(root_x), i32::from(root_y)];
        let monitor = self.monitor_at(point);
        let anchor = self.icon_rect().unwrap_or(DesktopRect {
            x: point[0] - i32::from(ICON_SIZE) / 2,
            y: point[1] - i32::from(ICON_SIZE) / 2,
            width: i32::from(ICON_SIZE),
            height: i32::from(ICON_SIZE),
        });
        let [x, y] = popup_origin(
            monitor,
            anchor,
            [
                i32::from(MENU_WIDTH + 2 * MENU_BORDER),
                i32::from(MENU_HEIGHT + 2 * MENU_BORDER),
            ],
        );
        let _configured = self.conn.configure_window(
            self.menu,
            &x11rb::protocol::xproto::ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
        )?;
        let _mapped = self.conn.map_window(self.menu)?;
        let _focused = self.conn.set_input_focus(
            x11rb::protocol::xproto::InputFocus::POINTER_ROOT,
            self.menu,
            CURRENT_TIME,
        )?;
        let _grab = self
            .conn
            .grab_pointer(
                false,
                self.menu,
                EventMask::BUTTON_PRESS,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                NONE,
                NONE,
                CURRENT_TIME,
            )?
            .reply();
        self.menu_live = true;
        self.conn.flush().context("show tray menu")
    }

    fn icon_rect(&self) -> Option<DesktopRect> {
        let geometry = self.conn.get_geometry(self.icon).ok()?.reply().ok()?;
        let origin = self
            .conn
            .translate_coordinates(self.icon, self.root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        Some(DesktopRect {
            x: i32::from(origin.dst_x),
            y: i32::from(origin.dst_y),
            width: i32::from(geometry.width),
            height: i32::from(geometry.height),
        })
    }

    fn monitor_at(&self, point: [i32; 2]) -> DesktopRect {
        let root = DesktopRect {
            x: 0,
            y: 0,
            width: i32::from(self.root_width),
            height: i32::from(self.root_height),
        };
        let monitors = self
            .conn
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| {
                reply
                    .monitors
                    .into_iter()
                    .map(|monitor| DesktopRect {
                        x: i32::from(monitor.x),
                        y: i32::from(monitor.y),
                        width: i32::from(monitor.width),
                        height: i32::from(monitor.height),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        nearest_monitor(&monitors, point).unwrap_or(root)
    }

    fn click_menu(&mut self, event: &ButtonPressEvent) -> Result<()> {
        let inside = event.event_x >= 0
            && event.event_y >= 0
            && i32::from(event.event_x) < i32::from(MENU_WIDTH)
            && i32::from(event.event_y) < i32::from(MENU_HEIGHT);
        self.hide_menu()?;
        if event.detail == 1 && inside {
            (self.emit)(Signal::Quit);
        }
        Ok(())
    }

    fn hide_menu(&mut self) -> Result<()> {
        let _ungrabbed = self.conn.ungrab_pointer(CURRENT_TIME)?;
        let _unmapped = self.conn.unmap_window(self.menu)?;
        self.menu_live = false;
        self.conn.flush().context("hide tray menu")
    }

    fn paint_icon(&self) -> Result<()> {
        let geometry = self.conn.get_geometry(self.icon)?.reply()?;
        let _cleared = self.conn.clear_area(false, self.icon, 0, 0, 0, 0)?;
        let point = |x: i16, y: i16| Point {
            x: scale_position(x, geometry.width),
            y: scale_position(y, geometry.height),
        };
        let eye = [
            (2, 12),
            (6, 7),
            (12, 5),
            (18, 7),
            (22, 12),
            (18, 17),
            (12, 19),
            (6, 17),
            (2, 12),
        ]
        .map(|(x, y)| point(x, y));
        let _rim = self
            .conn
            .poly_line(CoordMode::ORIGIN, self.icon, self.rim, &eye)?;
        let iris = XArc {
            x: point(7, 7).x,
            y: point(7, 7).y,
            width: scale_extent(10, geometry.width),
            height: scale_extent(10, geometry.height),
            angle1: 0,
            angle2: 360 * 64,
        };
        let _iris = self.conn.poly_fill_arc(self.icon, self.iris, &[iris])?;
        let pupil = Rectangle {
            x: point(10, 10).x,
            y: point(10, 10).y,
            width: scale_extent(4, geometry.width),
            height: scale_extent(4, geometry.height),
        };
        let _pupil = self
            .conn
            .poly_fill_rectangle(self.icon, self.pupil, &[pupil])?;
        self.conn.flush().context("paint tray icon")
    }

    fn paint_menu(&self) -> Result<()> {
        let _cleared = self.conn.clear_area(false, self.menu, 0, 0, 0, 0)?;
        let _painted =
            self.conn
                .image_text8(self.menu, self.menu_ink, 13, 20, b"Quit Codex Wrangler")?;
        self.conn.flush().context("paint tray menu")
    }
}

fn forge_icon(
    conn: &RustConnection,
    screen: &Screen,
    atoms: &Atoms,
    standalone: bool,
) -> Result<Window> {
    let icon = conn.generate_id().context("allocate tray icon")?;
    let mut attributes = CreateWindowAux::new()
        .background_pixmap(x11rb::protocol::xproto::BackPixmap::PARENT_RELATIVE)
        .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::STRUCTURE_NOTIFY);
    if standalone {
        attributes = attributes.override_redirect(1);
    }
    conn.create_window(
        screen.root_depth,
        icon,
        screen.root,
        0,
        0,
        ICON_SIZE,
        ICON_SIZE,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &attributes,
    )?
    .check()
    .context("create tray icon")?;
    conn.change_property8(
        PropMode::REPLACE,
        icon,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"Codex Wrangler tray",
    )?
    .check()
    .context("name tray icon")?;
    conn.change_property8(
        PropMode::REPLACE,
        icon,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"codex-wrangler-tray\0codex-wrangler-tray\0",
    )?
    .check()
    .context("classify tray icon")?;
    conn.change_property32(
        PropMode::REPLACE,
        icon,
        atoms.xembed_info,
        atoms.xembed_info,
        &[0, XEMBED_MAPPED],
    )?
    .check()
    .context("declare XEmbed mapping")?;
    Ok(icon)
}

fn forge_menu(conn: &RustConnection, screen: &Screen) -> Result<Window> {
    let menu = conn.generate_id().context("allocate tray menu")?;
    conn.create_window(
        screen.root_depth,
        menu,
        screen.root,
        0,
        0,
        MENU_WIDTH,
        MENU_HEIGHT,
        MENU_BORDER,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .override_redirect(1)
            .save_under(1)
            .background_pixel(alloc_color(
                conn,
                screen.default_colormap,
                [0x1d, 0x1b, 0x20],
            )?)
            .border_pixel(alloc_color(
                conn,
                screen.default_colormap,
                [0xa8, 0x72, 0xd4],
            )?)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::KEY_PRESS),
    )?
    .check()
    .context("create tray menu")?;
    conn.change_property8(
        PropMode::REPLACE,
        menu,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"Codex Wrangler tray menu",
    )?
    .check()
    .context("name tray menu")?;
    Ok(menu)
}

fn forge_inks(conn: &RustConnection, screen: &Screen, icon: Window, menu: Window) -> Result<Inks> {
    let color = |rgb| alloc_color(conn, screen.default_colormap, rgb);
    let rim = make_gc(conn, icon, color([0xe2, 0xd9, 0xc6])?, 2, None)?;
    let iris = make_gc(conn, icon, color([0xa8, 0x72, 0xd4])?, 1, None)?;
    let pupil = make_gc(conn, icon, color([0x14, 0x11, 0x19])?, 1, None)?;
    let font = conn.generate_id().context("allocate tray menu font")?;
    conn.open_font(font, b"fixed")?
        .check()
        .context("open tray menu font")?;
    let menu = make_gc(conn, menu, color([0xe2, 0xd9, 0xc6])?, 1, Some(font))?;
    conn.close_font(font)?
        .check()
        .context("release tray menu font")?;
    Ok(Inks {
        rim,
        iris,
        pupil,
        menu,
    })
}

fn scale_position(nominal: i16, actual: u16) -> i16 {
    let numerator = i32::from(nominal) * i32::from(actual) + i32::from(ICON_SIZE / 2);
    let scaled = numerator / i32::from(ICON_SIZE);
    i16::try_from(scaled).unwrap_or(if scaled.is_negative() {
        i16::MIN
    } else {
        i16::MAX
    })
}

fn scale_extent(nominal: u16, actual: u16) -> u16 {
    let numerator = u32::from(nominal) * u32::from(actual) + u32::from(ICON_SIZE / 2);
    u16::try_from((numerator / u32::from(ICON_SIZE)).max(1)).unwrap_or(u16::MAX)
}

impl Drop for X11Tray {
    fn drop(&mut self) {
        self.available.store(false, Ordering::Release);
        let _destroyed = self.conn.destroy_window(self.menu);
        let _destroyed = self.conn.destroy_window(self.icon);
        let _destroyed = self.conn.destroy_window(self.anchor);
        let _freed = self.conn.free_gc(self.menu_ink);
        let _freed = self.conn.free_gc(self.pupil);
        let _freed = self.conn.free_gc(self.iris);
        let _freed = self.conn.free_gc(self.rim);
        let _flushed = self.conn.flush();
    }
}

pub struct Tray {
    alive: Arc<AtomicBool>,
    available: Arc<AtomicBool>,
    wake: UnixStream,
    thread: Option<JoinHandle<()>>,
}

impl Tray {
    pub fn raise(
        incumbent: Incumbent,
        emit: impl Fn(Signal) + Send + Sync + 'static,
    ) -> Result<Self> {
        let alive = Arc::new(AtomicBool::new(true));
        let available = Arc::new(AtomicBool::new(false));
        let mut tray = X11Tray::forge(incumbent, Arc::clone(&available), Arc::new(emit))?;
        let (wake, thread_wake) = UnixStream::pair().context("forge tray wake pipe")?;
        let thread_alive = Arc::clone(&alive);
        let thread = thread::Builder::new()
            .name("codex-wrangler-xembed".to_owned())
            .spawn(move || {
                if let Err(error) = tray.run(&thread_alive, &thread_wake) {
                    eprintln!("codex-wrangler tray failed: {error:#}");
                }
            })
            .context("spawn XEmbed tray")?;
        Ok(Self {
            alive,
            available,
            wake,
            thread: Some(thread),
        })
    }

    pub fn available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        let _woken = self.wake.write_all(&[0]);
        if let Some(thread) = self.thread.take() {
            let _joined = thread.join();
        }
    }
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom> {
    Ok(conn
        .intern_atom(false, name.as_bytes())?
        .reply()
        .with_context(|| format!("intern X11 atom `{name}`"))?
        .atom)
}

fn alloc_color(conn: &RustConnection, colormap: u32, [r, g, b]: [u8; 3]) -> Result<u32> {
    Ok(conn
        .alloc_color(
            colormap,
            u16::from(r) * 257,
            u16::from(g) * 257,
            u16::from(b) * 257,
        )?
        .reply()
        .context("allocate tray color")?
        .pixel)
}

fn make_gc(
    conn: &RustConnection,
    drawable: Window,
    foreground: u32,
    width: u32,
    font: Option<u32>,
) -> Result<Gcontext> {
    let gc = conn
        .generate_id()
        .context("allocate tray graphics context")?;
    let mut attributes = CreateGCAux::new()
        .foreground(foreground)
        .line_width(width)
        .line_style(LineStyle::SOLID)
        .cap_style(CapStyle::ROUND)
        .join_style(JoinStyle::ROUND)
        .graphics_exposures(0);
    if let Some(font) = font {
        attributes = attributes.font(font);
    }
    conn.create_gc(gc, drawable, &attributes)?
        .check()
        .context("create tray graphics context")?;
    Ok(gc)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOPOLOGY: [DesktopRect; 3] = [
        DesktopRect {
            x: 0,
            y: 0,
            width: 1080,
            height: 1920,
        },
        DesktopRect {
            x: 1080,
            y: 300,
            width: 1920,
            height: 1080,
        },
        DesktopRect {
            x: 3000,
            y: 0,
            width: 1080,
            height: 1920,
        },
    ];

    #[test]
    fn menu_is_imprisoned_on_the_icons_monitor() {
        let monitor = nearest_monitor(&TOPOLOGY, [2988, 1368]);
        assert_eq!(monitor, Some(TOPOLOGY[1]));
        let origin = popup_origin(
            monitor.unwrap_or(TOPOLOGY[0]),
            DesktopRect {
                x: 2976,
                y: 1356,
                width: 24,
                height: 24,
            },
            [i32::from(MENU_WIDTH), i32::from(MENU_HEIGHT)],
        );
        assert_eq!(origin, [2860, 1322]);
    }
}
