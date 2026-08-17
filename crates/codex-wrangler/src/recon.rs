use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs,
    io::{Read as _, Write as _},
    os::{
        fd::AsFd as _,
        unix::{ffi::OsStringExt as _, net::UnixStream},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use eternalist_apps::NativeWake;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};
use semver::Version;

use crate::{
    codex_rpc::CodexRpc,
    contract::{Harness, Work},
    desktop::{Desktop, DesktopSignal},
    model::{Card, Census, snip},
    names::NameIndex,
    rollout::{RolloutSummary, Rollouts, TurnState},
    roster::{AccountMark, Roster, Sighting as SessionSighting},
    stasis::{ProcessKey, Quarry, Stasis},
    transcript::Transcripts,
    watchfire::Watchfire,
};

const FOREST_AUDIT: Duration = Duration::from_secs(2);
const INTEGRITY_AUDIT: Duration = Duration::from_mins(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Intent {
    Select,
    Fork,
    Open,
    Dismiss,
}

#[derive(Clone, Copy)]
enum CodexLaunch {
    Resume,
    Fork,
}

impl CodexLaunch {
    const fn verb(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Fork => "fork",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Strike {
    pub harness: Harness,
    pub thread: String,
    pub intent: Intent,
}

#[derive(Clone, Debug)]
pub struct Activation {
    pub strike: Strike,
    pub succeeded: bool,
    pub conceal: bool,
}

pub struct Nexus {
    latest: Arc<Mutex<Option<Census>>>,
    activation: Arc<Mutex<Option<Activation>>>,
    pub strike: Striker,
    alive: Arc<AtomicBool>,
    wake: UnixStream,
    thread: Option<JoinHandle<()>>,
}

pub struct Striker {
    channel: Sender<Strike>,
    wake: UnixStream,
}

impl Striker {
    pub fn try_send(&self, strike: Strike) -> Result<(), TrySendError<Strike>> {
        self.channel.try_send(strike)?;
        let _woken = (&self.wake).write_all(&[0]);
        Ok(())
    }
}

impl Nexus {
    pub fn take_census(&self) -> Option<Census> {
        self.latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    pub fn take_activation(&self) -> Option<Activation> {
        self.activation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

impl Drop for Nexus {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        let _woken = self.wake.write_all(&[0]);
        if let Some(thread) = self.thread.take() {
            let _joined = thread.join();
        }
    }
}

pub fn spawn(repaint: NativeWake) -> Nexus {
    let latest = Arc::new(Mutex::new(None));
    let activation = Arc::new(Mutex::new(None));
    let (strike, strike_rx) = bounded(16);
    let (wake, thread_wake) = UnixStream::pair().expect("forge recon wake pipe");
    wake.set_nonblocking(true)
        .expect("make recon wake pipe nonblocking");
    thread_wake
        .set_nonblocking(true)
        .expect("make worker wake pipe nonblocking");
    let striker_wake = wake.try_clone().expect("clone recon wake pipe");
    let alive = Arc::new(AtomicBool::new(true));
    let worker_alive = Arc::clone(&alive);
    let worker_latest = Arc::clone(&latest);
    let worker_activation = Arc::clone(&activation);
    let thread = thread::Builder::new()
        .name("codex-wrangler-recon".to_owned())
        .spawn(move || {
            raid(
                &repaint,
                &worker_latest,
                &worker_activation,
                &strike_rx,
                &thread_wake,
                &worker_alive,
            );
        })
        .expect("spawn reconnaissance worker");
    Nexus {
        latest,
        activation,
        strike: Striker {
            channel: strike,
            wake: striker_wake,
        },
        alive,
        wake,
        thread: Some(thread),
    }
}

fn raid(
    repaint: &NativeWake,
    latest: &Mutex<Option<Census>>,
    activation: &Mutex<Option<Activation>>,
    strikes: &Receiver<Strike>,
    wake: &UnixStream,
    alive: &AtomicBool,
) {
    let mut recon = match Recon::raise() {
        Ok(recon) => recon,
        Err(error) => {
            publish(
                repaint,
                latest,
                Census {
                    cards: Vec::new(),
                    fault: Some(format!("Could not arm reconnaissance: {error:#}")),
                },
            );
            return;
        }
    };
    let mut prior = None;
    if let Err(error) = recon
        .refresh_forest()
        .and_then(|_| recon.project(Instant::now()))
    {
        publish_fault(repaint, latest, &error);
    } else {
        publish_changed(repaint, latest, &mut prior, recon.census());
    }
    let mut forest_audit = Instant::now() + FOREST_AUDIT;
    let mut integrity_audit = Instant::now() + INTEGRITY_AUDIT;
    while alive.load(Ordering::Acquire) {
        let deadline = [
            Some(forest_audit),
            Some(integrity_audit),
            recon.stasis.next_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or_else(|| Instant::now() + INTEGRITY_AUDIT);
        let timeout = deadline.saturating_duration_since(Instant::now());
        let readiness = match wait_for_signal(&recon, wake, timeout) {
            Ok(readiness) => readiness,
            Err(error) => {
                eprintln!("codex-wrangler reconnaissance wait failed: {error:#}");
                break;
            }
        };
        if readiness[2] {
            drain_wake(wake);
        }
        if !alive.load(Ordering::Acquire) {
            break;
        }

        let now = Instant::now();
        let mut dirty = execute_strikes(repaint, activation, strikes, &mut recon, now);

        if readiness[0] {
            let demand = heed_desktop(&mut recon, now);
            dirty |= demand.projection;
            if demand.forest {
                forest_audit = now;
            }
        }
        if readiness[1] {
            dirty |= reap_watchfire(&mut recon);
        }
        if now >= forest_audit {
            match recon.refresh_forest() {
                Ok(changed) => dirty |= changed,
                Err(error) => publish_fault(repaint, latest, &error),
            }
            forest_audit = now + FOREST_AUDIT;
        }
        if now >= integrity_audit {
            dirty = true;
            integrity_audit = now + INTEGRITY_AUDIT;
        }
        if dirty {
            match recon.project(now) {
                Ok(()) => publish_changed(repaint, latest, &mut prior, recon.census()),
                Err(error) => publish_fault(repaint, latest, &error),
            }
        }
        if recon
            .stasis
            .next_deadline()
            .is_some_and(|deadline| deadline <= now)
        {
            recon.refresh_focus(now);
            recon.stasis.freeze_due(now);
            recon.refresh_focus(Instant::now());
            publish_changed(repaint, latest, &mut prior, recon.census());
        }
    }
}

fn wait_for_signal(recon: &Recon, wake: &UnixStream, timeout: Duration) -> Result<[bool; 3]> {
    let mut descriptors = [
        PollFd::new(recon.desktop.as_fd(), PollFlags::POLLIN),
        PollFd::new(recon.watchfire.as_fd(), PollFlags::POLLIN),
        PollFd::new(wake.as_fd(), PollFlags::POLLIN),
    ];
    let timeout = PollTimeout::try_from(timeout).unwrap_or(PollTimeout::MAX);
    let _ready = poll(&mut descriptors, timeout).context("poll reconnaissance sources")?;
    Ok(descriptors.map(|descriptor| {
        descriptor
            .revents()
            .is_some_and(|events| events.contains(PollFlags::POLLIN))
    }))
}

fn execute_strikes(
    repaint: &NativeWake,
    activation: &Mutex<Option<Activation>>,
    strikes: &Receiver<Strike>,
    recon: &mut Recon,
    now: Instant,
) -> bool {
    let mut struck = false;
    while let Ok(strike) = strikes.try_recv() {
        let conceal = matches!(strike.intent, Intent::Select | Intent::Fork | Intent::Open);
        let succeeded = recon.execute(&strike, now).unwrap_or_else(|error| {
            eprintln!(
                "codex-wrangler could not execute {:?} for {}: {error:#}",
                strike.intent, strike.thread
            );
            false
        });
        if succeeded {
            let _changed = recon.refresh_forest();
        }
        *activation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Activation {
            strike,
            succeeded,
            conceal,
        });
        let _repaint = repaint.request_repaint();
        struck = true;
    }
    struck
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DesktopDemand {
    projection: bool,
    forest: bool,
}

fn heed_desktop(recon: &mut Recon, now: Instant) -> DesktopDemand {
    match recon.desktop.drain_events() {
        Ok(signals) => {
            let focus = signals.contains(&DesktopSignal::Focus);
            if focus {
                recon.refresh_focus(now);
            }
            desktop_demand(&signals)
        }
        Err(error) => {
            recon.stasis.focus_uncertain();
            eprintln!("codex-wrangler lost X11 focus truth: {error:#}");
            DesktopDemand {
                projection: true,
                forest: false,
            }
        }
    }
}

fn desktop_demand(signals: &HashSet<DesktopSignal>) -> DesktopDemand {
    DesktopDemand {
        projection: signals.iter().any(|signal| {
            matches!(
                signal,
                DesktopSignal::Focus | DesktopSignal::Workspace | DesktopSignal::Terminal
            )
        }),
        forest: signals.contains(&DesktopSignal::Topology),
    }
}

fn reap_watchfire(recon: &mut Recon) -> bool {
    match recon.watchfire.reap() {
        Ok(flare) => flare.overflowed || !flare.paths.is_empty(),
        Err(error) => {
            eprintln!("codex-wrangler file watch failed: {error:#}");
            true
        }
    }
}

fn drain_wake(mut wake: &UnixStream) {
    let mut bytes = [0_u8; 64];
    loop {
        match wake.read(&mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

fn publish_changed(
    repaint: &NativeWake,
    latest: &Mutex<Option<Census>>,
    prior: &mut Option<Census>,
    census: Census,
) {
    if prior.as_ref() != Some(&census) {
        *prior = Some(census.clone());
        publish(repaint, latest, census);
    }
}

fn publish_fault(repaint: &NativeWake, latest: &Mutex<Option<Census>>, error: &anyhow::Error) {
    publish(
        repaint,
        latest,
        Census {
            cards: Vec::new(),
            fault: Some(format!("Could not inspect harnesses: {error:#}")),
        },
    );
}

fn publish(repaint: &NativeWake, latest: &Mutex<Option<Census>>, census: Census) {
    *latest
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(census);
    let _repaint = repaint.request_repaint();
}

#[derive(Eq, PartialEq)]
struct Sighting {
    process: Process,
    window: u32,
}

struct Recon {
    codex: Option<Codex>,
    desktop: Desktop,
    process_cache: HashMap<ProcessKey, Process>,
    sightings: Vec<Sighting>,
    semantic: Vec<Card>,
    codex_seats: HashMap<String, Seat>,
    stasis: Stasis,
    transcripts: Transcripts,
    watchfire: Watchfire,
}

#[derive(Clone, Copy)]
struct Seat {
    process: ProcessKey,
    window: u32,
}

impl Recon {
    fn raise() -> Result<Self> {
        let codex = match Codex::raise() {
            Ok(codex) => codex,
            Err(error) => {
                eprintln!("codex-wrangler could not arm its Codex adapter: {error:#}");
                None
            }
        };
        Ok(Self {
            codex,
            desktop: Desktop::connect()?,
            process_cache: HashMap::new(),
            sightings: Vec::new(),
            semantic: Vec::new(),
            codex_seats: HashMap::new(),
            stasis: Stasis::arm(),
            transcripts: Transcripts::default(),
            watchfire: Watchfire::kindle()?,
        })
    }

    fn refresh_forest(&mut self) -> Result<bool> {
        let windows = self.desktop.windows_by_pid()?;
        let sightings = manual_harnesses(&windows, &mut self.process_cache);
        self.desktop
            .watch_terminals(sightings.iter().map(|sighting| sighting.window))?;
        let changed = sightings != self.sightings;
        self.sightings = sightings;
        Ok(changed)
    }

    fn project(&mut self, now: Instant) -> Result<()> {
        if let Some(codex) = &mut self.codex {
            codex.refresh_names()?;
        }
        let workspaces = self
            .desktop
            .workspace_numbers(self.sightings.iter().map(|sighting| sighting.window))
            .unwrap_or_default();
        let active = match self.desktop.active_window() {
            Ok(active) => active,
            Err(error) => {
                self.stasis.focus_uncertain();
                return Err(error.context("read active X11 window"));
            }
        };
        let mut locator = SessionLocator::default();
        let mut cards = Vec::new();
        let mut quarry = Vec::new();
        let mut watched = self
            .codex
            .as_ref()
            .map_or_else(Vec::new, Codex::watch_paths);
        let mut seen = HashSet::new();
        let mut codex_seats = HashMap::new();
        for sighting in &self.sightings {
            let process = &sighting.process;
            watched.extend(process.transcripts.iter().cloned());
            let workspace = workspaces.get(&sighting.window).copied();
            let observation = match process.harness {
                Harness::Codex => match &mut self.codex {
                    Some(codex) => codex
                        .card(process, sighting.window, workspace)?
                        .map(|(card, transcript)| (card, Some(transcript))),
                    None => None,
                },
                Harness::ClaudeCode | Harness::PrimeAgent => Some(foreign_card(
                    &mut self.transcripts,
                    &mut locator,
                    process,
                    sighting.window,
                    workspace,
                )),
            };
            let Some((mut card, transcript)) = observation else {
                continue;
            };
            if card.harness == Harness::Codex
                && card.work != Work::Error
                && self.desktop.requires_action(sighting.window)
            {
                card.work = Work::Input;
            }
            watched.extend(transcript);
            if seen.insert((card.harness, card.thread.clone())) {
                if card.harness == Harness::Codex {
                    let _prior = codex_seats.insert(
                        card.thread.clone(),
                        Seat {
                            process: process.key,
                            window: sighting.window,
                        },
                    );
                    quarry.push(Quarry {
                        process: process.key,
                        window: sighting.window,
                        work: card.work,
                    });
                }
                cards.push(card);
            }
        }
        if let Some(codex) = &mut self.codex {
            cards.extend(codex.closed_cards(codex_seats.keys().map(String::as_str)));
            codex.commit()?;
        }
        for card in &cards {
            card.assert_lawful();
        }
        self.stasis.observe(now, active, &quarry);
        self.watchfire.reconcile(watched)?;
        self.codex_seats = codex_seats;
        self.semantic = cards;
        Ok(())
    }

    fn refresh_focus(&mut self, now: Instant) {
        match self.desktop.active_window() {
            Ok(active) => self.stasis.focus(now, active),
            Err(error) => {
                self.stasis.focus_uncertain();
                eprintln!("codex-wrangler could not establish focus truth: {error:#}");
            }
        }
    }

    fn census(&self) -> Census {
        let mut cards = self.semantic.clone();
        for card in &mut cards {
            if card
                .window
                .is_some_and(|window| self.stasis.sleeping(window))
            {
                card.work = Work::Sleep;
            }
        }
        cards.sort();
        Census { cards, fault: None }
    }

    fn execute(&mut self, strike: &Strike, now: Instant) -> Result<bool> {
        if strike.harness == Harness::Codex && strike.intent == Intent::Open {
            return self.open_historical(&strike.thread);
        }
        let Some(card) = self
            .semantic
            .iter()
            .find(|card| card.harness == strike.harness && card.thread == strike.thread)
            .cloned()
        else {
            return Ok(false);
        };
        if strike.harness != Harness::Codex {
            return match (strike.intent, card.window) {
                (Intent::Select, Some(window)) => self.activate(window, now),
                _ => Ok(false),
            };
        }
        match strike.intent {
            Intent::Select => self.select_codex(&card, now),
            Intent::Fork => self.fork_codex(&card),
            Intent::Open => Ok(false),
            Intent::Dismiss => self.dismiss_codex(&card, now),
        }
    }

    fn activate(&mut self, window: u32, now: Instant) -> Result<bool> {
        if !self.stasis.prepare_activation(now, window) {
            anyhow::bail!("window {window} remains frozen");
        }
        self.desktop.activate(window)?;
        Ok(true)
    }

    fn select_codex(&mut self, card: &Card, now: Instant) -> Result<bool> {
        if card.work == Work::Closed {
            let active = self.active_account("bind resumed Codex login");
            let version = inspect_codex_version("inspect installed Codex version");
            return self.summon_codex(&card.thread, card.workspace, active, version);
        }
        let window = card.window.expect("live Codex card owns a window");
        if card.work == Work::Done {
            let codex = self.codex.as_mut().context("Codex adapter is absent")?;
            let home = codex.home.clone();
            let active = inspect_account(&home, "inspect current Codex login");
            let version = inspect_codex_version("inspect installed Codex version");
            let rotated = active.as_ref().is_some_and(|active| {
                codex
                    .roster
                    .get(&card.thread)
                    .is_some_and(|session| session.account.rotated_to(active))
            });
            let superseded = version.as_ref().is_some_and(|installed| {
                codex
                    .roster
                    .get(&card.thread)
                    .and_then(|session| session.cli_version.as_deref())
                    .and_then(|session| Version::parse(session).ok())
                    .is_some_and(|session| session < *installed)
            });
            if rotated || superseded {
                let seat = self
                    .codex_seats
                    .get(&card.thread)
                    .copied()
                    .context("live Codex card has no process seat")?;
                self.retire(seat, now)?;
                return self.summon_codex(&card.thread, card.workspace, active, version);
            }
        }
        self.activate(window, now)
    }

    fn dismiss_codex(&mut self, card: &Card, now: Instant) -> Result<bool> {
        let codex = self.codex.as_mut().context("Codex adapter is absent")?;
        if card.work == Work::Closed {
            codex.roster.forget(&card.thread);
            codex.commit()?;
            return Ok(true);
        }
        if card.work != Work::Done {
            return Ok(false);
        }
        let seat = self
            .codex_seats
            .get(&card.thread)
            .copied()
            .context("open Codex card has no process seat")?;
        self.retire(seat, now)?;
        Ok(true)
    }

    fn fork_codex(&mut self, card: &Card) -> Result<bool> {
        let codex = self.codex.as_ref().context("Codex adapter is absent")?;
        let session = codex
            .roster
            .get(&card.thread)
            .context("Codex session is absent from Wrangler state")?;
        let cwd = session.cwd.clone();
        let home = codex.home.clone();
        self.launch_codex(&card.thread, &cwd, card.workspace, &home, CodexLaunch::Fork)?;
        Ok(true)
    }

    fn open_historical(&mut self, thread: &str) -> Result<bool> {
        let codex = self.codex.as_ref().context("Codex adapter is absent")?;
        let (cwd, archived, nominal) = codex
            .db
            .query_row(
                "SELECT cwd, archived, rollout_path FROM threads WHERE id = ?1",
                params![thread],
                |row| {
                    Ok((
                        PathBuf::from(row.get::<_, String>(0)?),
                        row.get::<_, bool>(1)?,
                        PathBuf::from(row.get::<_, String>(2)?),
                    ))
                },
            )
            .optional()?
            .context("historical Codex session vanished")?;
        let home = codex.home.clone();
        crate::history::prepare_resume(&home, thread, archived, &nominal)?;
        self.launch_codex(thread, &cwd, None, &home, CodexLaunch::Resume)?;
        Ok(true)
    }

    fn retire(&mut self, seat: Seat, now: Instant) -> Result<()> {
        if !self.stasis.prepare_retirement(now, seat.window) {
            anyhow::bail!("Codex process {} remains frozen", seat.process.pid);
        }
        if let Err(error) = self.desktop.close(seat.window) {
            eprintln!(
                "codex-wrangler could not close terminal window {} cleanly: {error:#}",
                seat.window
            );
        }
        if wait_dead(seat.process, Duration::from_secs(2)) {
            return Ok(());
        }
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(seat.process.pid)?),
            nix::sys::signal::Signal::SIGTERM,
        )
        .context("terminate stopped Codex process")?;
        anyhow::ensure!(
            wait_dead(seat.process, Duration::from_secs(2)),
            "Codex process {} survived terminal closure and SIGTERM",
            seat.process.pid
        );
        Ok(())
    }

    fn summon_codex(
        &mut self,
        thread_id: &str,
        workspace: Option<u32>,
        active: Option<AccountMark>,
        version: Option<Version>,
    ) -> Result<bool> {
        let codex = self.codex.as_ref().context("Codex adapter is absent")?;
        let session = codex
            .roster
            .get(thread_id)
            .context("Codex session is absent from Wrangler state")?;
        let cwd = session.cwd.clone();
        let home = codex.home.clone();
        self.launch_codex(thread_id, &cwd, workspace, &home, CodexLaunch::Resume)?;
        let codex = self.codex.as_mut().context("Codex adapter is absent")?;
        if let Some(active) = active {
            codex.roster.bind(thread_id, active);
        }
        if let Some(version) = version {
            codex.roster.bind_version(thread_id, &version.to_string());
        }
        codex.commit()?;
        Ok(true)
    }

    fn launch_codex(
        &mut self,
        thread_id: &str,
        cwd: &Path,
        workspace: Option<u32>,
        home: &Path,
        launch: CodexLaunch,
    ) -> Result<()> {
        if let Some(workspace) = workspace {
            let destination = format!("workspace number {workspace}");
            let status = Command::new("i3-msg")
                .args(["--quiet", &destination])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("switch to remembered Codex workspace")?;
            anyhow::ensure!(status.success(), "i3 rejected workspace `{workspace}`");
        }
        let mut child = Command::new("alacritty")
            .arg("--working-directory")
            .arg(cwd)
            .args(["-e", "codex", launch.verb(), thread_id])
            .env("CODEX_HOME", home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("raise Alacritty for Codex thread `{thread_id}`"))?;
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(8);
        let window = loop {
            if let Some(window) = self.desktop.window_by_pid(pid)? {
                break window;
            }
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("Alacritty exited before mapping its window: {status}");
            }
            if Instant::now() >= deadline {
                let _killed = child.kill();
                let _waited = child.wait();
                anyhow::bail!("Alacritty did not map a window within 8 seconds");
            }
            thread::sleep(Duration::from_millis(25));
        };
        thread::Builder::new()
            .name("codex-wrangler-terminal-reaper".to_owned())
            .spawn(move || {
                let _waited = child.wait();
            })
            .context("spawn Alacritty reaper")?;
        self.desktop.activate(window)?;
        Ok(())
    }

    fn active_account(&self, operation: &str) -> Option<AccountMark> {
        let codex = self.codex.as_ref()?;
        inspect_account(&codex.home, operation)
    }
}

fn inspect_account(home: &Path, operation: &str) -> Option<AccountMark> {
    CodexRpc::open(home)
        .and_then(|mut rpc| rpc.account())
        .map_err(|error| eprintln!("codex-wrangler could not {operation}: {error:#}"))
        .ok()
}

fn inspect_codex_version(operation: &str) -> Option<Version> {
    installed_codex_version()
        .map_err(|error| eprintln!("codex-wrangler could not {operation}: {error:#}"))
        .ok()
}

fn installed_codex_version() -> Result<Version> {
    let output = Command::new("codex")
        .arg("--version")
        .output()
        .context("run `codex --version`")?;
    anyhow::ensure!(
        output.status.success(),
        "`codex --version` exited with {}",
        output.status
    );
    let output = std::str::from_utf8(&output.stdout).context("decode `codex --version`")?;
    output
        .split_ascii_whitespace()
        .find_map(|word| Version::parse(word.trim_start_matches('v')).ok())
        .context("find semantic version in `codex --version`")
}

fn wait_dead(process: ProcessKey, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while process.alive() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
    true
}

struct Codex {
    home: PathBuf,
    db: Connection,
    goals: Option<Connection>,
    names: NameIndex,
    rollouts: Rollouts,
    roster: Roster,
}

impl Codex {
    fn raise() -> Result<Option<Self>> {
        let Some(home) = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        else {
            return Ok(None);
        };
        let db_path = home.join("state_5.sqlite");
        if !db_path.is_file() {
            return Ok(None);
        }
        let db = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .context("open Codex thread index")?;
        let goals_path = home.join("goals_1.sqlite");
        let goals = goals_path
            .is_file()
            .then(|| {
                Connection::open_with_flags(
                    &goals_path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .context("open Codex goal ledger")
            })
            .transpose()?;
        Ok(Some(Self {
            home,
            db,
            goals,
            names: NameIndex::default(),
            rollouts: Rollouts::default(),
            roster: Roster::restore().context("restore known Codex sessions")?,
        }))
    }

    fn refresh_names(&mut self) -> Result<()> {
        self.names.refresh(&self.home.join("session_index.jsonl"))
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        [
            "session_index.jsonl",
            "state_5.sqlite",
            "state_5.sqlite-wal",
            "goals_1.sqlite",
            "goals_1.sqlite-wal",
        ]
        .into_iter()
        .map(|name| self.home.join(name))
        .collect()
    }

    fn card(
        &mut self,
        process: &Process,
        window: u32,
        workspace: Option<u32>,
    ) -> Result<Option<(Card, PathBuf)>> {
        let Some(thread) = self.current_thread(process)? else {
            return Ok(None);
        };
        let summary = match self.rollouts.read(&thread.rollout) {
            Ok(summary) => summary,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && process.holds_writer_lock(&thread.id) =>
            {
                RolloutSummary::quiescent()
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read rollout `{}`", thread.rollout.display()));
            }
        };
        let name = thread
            .name
            .or_else(|| self.names.get(&thread.id).map(str::to_owned));
        let work = classify_work(
            summary.state,
            self.goal_active(&thread.id)?,
            summary.waiting_for_input,
        );
        let rollout = thread.rollout.clone();
        let preview = snip(&summary.preview, 280);
        self.roster.sight(SessionSighting {
            thread: &thread.id,
            name: name.as_deref(),
            cwd: &thread.cwd,
            preview: &preview,
            updated_at_ms: thread.updated_at_ms,
            workspace,
            cli_version: thread.cli_version.as_deref(),
            account: summary.account,
        });
        Ok(Some((
            Card {
                harness: Harness::Codex,
                thread: thread.id,
                name,
                cwd: compact_path(&thread.cwd, process.home.as_deref()),
                tile_preview: preview,
                work,
                window: Some(window),
                workspace,
                updated_at_ms: thread.updated_at_ms,
            },
            rollout,
        )))
    }

    fn closed_cards<'a>(&self, live: impl IntoIterator<Item = &'a str>) -> Vec<Card> {
        let live = live.into_iter().collect::<HashSet<_>>();
        self.roster
            .sessions()
            .filter(|(thread, _)| !live.contains(*thread))
            .map(|(thread, session)| Card {
                harness: Harness::Codex,
                thread: thread.to_owned(),
                name: session.name.clone(),
                cwd: compact_path(&session.cwd, None),
                tile_preview: session.preview.clone(),
                work: Work::Closed,
                window: None,
                workspace: session.workspace,
                updated_at_ms: session.updated_at_ms,
            })
            .collect()
    }

    fn commit(&mut self) -> Result<()> {
        self.roster.commit()
    }

    fn goal_active(&self, thread: &str) -> Result<bool> {
        let Some(goals) = &self.goals else {
            return Ok(false);
        };
        goals
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM thread_goals
                   WHERE thread_id = ?1 AND status = 'active'
                 )",
                params![thread],
                |row| row.get(0),
            )
            .with_context(|| format!("query current goal state for thread `{thread}`"))
    }

    fn current_thread(&self, process: &Process) -> Result<Option<Thread>> {
        if let Some(binding) = process.binding.get() {
            if !binding.held_by(&process.codex_claims)
                && process.resumed_thread() != Some(binding.id.as_str())
            {
                return Ok(None);
            }
            return self
                .thread(&binding.rollout)
                .map(|thread| thread.filter(|thread| thread.id == binding.id));
        }

        let mut locked = Vec::new();
        let mut legacy = Vec::new();
        for claim in &process.codex_claims {
            match claim {
                CodexClaim::WriterLock(id) => {
                    if let Some(thread) = self.thread_by_id(id)? {
                        locked.push(thread);
                    }
                }
                CodexClaim::WritableRollout(rollout) => {
                    if let Some(thread) = self.thread(rollout)? {
                        legacy.push(thread);
                    }
                }
            }
        }
        let candidates = if locked.is_empty() { legacy } else { locked };
        let thread = newest_thread(candidates).or_else(|| {
            process
                .resumed_thread()
                .and_then(|thread| self.thread_by_id(thread).ok().flatten())
        });
        let Some(thread) = thread else {
            return Ok(None);
        };
        let binding = ThreadBinding {
            id: thread.id.clone(),
            rollout: thread.rollout.clone(),
        };
        process
            .binding
            .set(binding)
            .expect("single reconnaissance thread binds each process once");
        Ok(Some(thread))
    }

    fn thread(&self, rollout: &Path) -> Result<Option<Thread>> {
        if !rollout.starts_with(self.home.join("sessions")) {
            return Ok(None);
        }
        let Some(id) = rollout_id(rollout) else {
            return Ok(None);
        };
        self.thread_by_id(id)
            .map(|thread| thread.filter(|thread| thread.rollout == rollout))
    }

    fn thread_by_id(&self, id: &str) -> Result<Option<Thread>> {
        let thread = self
            .db
            .query_row(
                "SELECT id, NULLIF(TRIM(name), ''), cwd, updated_at_ms, \
                 thread_source, source, agent_role, rollout_path, cli_version \
                 FROM threads WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        PathBuf::from(row.get::<_, String>(2)?),
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        PathBuf::from(row.get::<_, String>(7)?),
                        row.get::<_, String>(8)?,
                    ))
                },
            )
            .optional()
            .with_context(|| format!("query Codex thread `{id}`"))?;
        let Some((
            id,
            name,
            cwd,
            updated_at_ms,
            thread_source,
            source,
            agent_role,
            rollout,
            cli_version,
        )) = thread
        else {
            return Ok(None);
        };
        if thread_source.as_deref() != Some("user") || source != "cli" || agent_role.is_some() {
            return Ok(None);
        }
        Ok(Some(Thread {
            id,
            name,
            cwd,
            updated_at_ms,
            rollout,
            cli_version: (!cli_version.trim().is_empty()).then_some(cli_version),
        }))
    }
}

fn newest_thread(mut candidates: Vec<Thread>) -> Option<Thread> {
    candidates.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    candidates.dedup_by(|left, right| left.id == right.id);
    candidates
        .into_iter()
        .max_by(|left, right| (left.updated_at_ms, &left.id).cmp(&(right.updated_at_ms, &right.id)))
}

fn foreign_card(
    transcripts: &mut Transcripts,
    locator: &mut SessionLocator,
    process: &Process,
    window: u32,
    workspace: Option<u32>,
) -> (Card, Option<PathBuf>) {
    let path = locator.locate(process);
    let summary = path
        .as_deref()
        .and_then(|path| transcripts.read(process.harness, path).ok());
    let thread = path
        .as_deref()
        .and_then(Path::file_stem)
        .and_then(OsStr::to_str)
        .map_or_else(|| format!("pid-{}", process.pid), str::to_owned);
    let cwd = summary
        .as_ref()
        .and_then(|summary| summary.cwd.as_deref())
        .unwrap_or(&process.cwd);
    let work = summary.as_ref().map_or(Work::Done, |summary| summary.work);
    let work = if process.goal && work == Work::Turn {
        Work::Goal
    } else {
        work
    };
    let card = Card {
        harness: process.harness,
        thread,
        name: process
            .explicit_name()
            .or_else(|| summary.as_ref().and_then(|summary| summary.name.clone())),
        cwd: compact_path(cwd, process.home.as_deref()),
        tile_preview: summary
            .as_ref()
            .map_or_else(String::new, |summary| snip(&summary.preview, 280)),
        work,
        window: Some(window),
        workspace,
        updated_at_ms: summary
            .as_ref()
            .map_or_else(|| i64::from(process.pid), |summary| summary.updated_at_ms),
    };
    (card, path)
}

const fn classify_work(state: TurnState, goal_active: bool, waiting_for_input: bool) -> Work {
    match (state, waiting_for_input, goal_active) {
        (TurnState::Error, _, _) | (TurnState::Unknown, false, _) => Work::Error,
        (_, true, _) => Work::Input,
        (TurnState::Running, false, true) => Work::Goal,
        (TurnState::Running, false, false) => Work::Turn,
        (TurnState::Done, false, _) => Work::Done,
    }
}

struct Thread {
    id: String,
    name: Option<String>,
    cwd: PathBuf,
    updated_at_ms: i64,
    rollout: PathBuf,
    cli_version: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ThreadBinding {
    id: String,
    rollout: PathBuf,
}

impl ThreadBinding {
    fn held_by(&self, claims: &[CodexClaim]) -> bool {
        claims.iter().any(|claim| match claim {
            CodexClaim::WriterLock(thread) => thread == &self.id,
            CodexClaim::WritableRollout(rollout) => rollout == &self.rollout,
        })
    }
}

fn unique_candidate<T>(candidates: impl IntoIterator<Item = T>) -> Option<T> {
    let mut candidates = candidates.into_iter();
    let candidate = candidates.next()?;
    candidates.next().is_none().then_some(candidate)
}

fn compact_path(path: &Path, home: Option<&Path>) -> String {
    let text = path.to_string_lossy().into_owned();
    let home = home
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    let Some(home) = home else {
        return text;
    };
    path.strip_prefix(home).map_or(text, |tail| {
        let suffix = tail.to_string_lossy();
        if suffix.is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", suffix.trim_start_matches('/'))
        }
    })
}

#[derive(Clone, Eq, PartialEq)]
struct Process {
    key: ProcessKey,
    pid: u32,
    harness: Harness,
    argv: Vec<OsString>,
    transcripts: Vec<PathBuf>,
    codex_claims: Vec<CodexClaim>,
    binding: Arc<OnceLock<ThreadBinding>>,
    cwd: PathBuf,
    environment: HashMap<String, OsString>,
    home: Option<PathBuf>,
    goal: bool,
}

/// A live Codex process's claim to one thread.
///
/// Writer locks are the 0.147 identity surface. Writable rollout descriptors
/// are the legacy 0.146 surface retained during the compatibility window.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CodexClaim {
    WriterLock(String),
    WritableRollout(PathBuf),
}

impl Process {
    fn holds_writer_lock(&self, thread: &str) -> bool {
        self.codex_claims
            .iter()
            .any(|claim| matches!(claim, CodexClaim::WriterLock(claimed) if claimed == thread))
    }

    fn explicit_name(&self) -> Option<String> {
        (self.harness == Harness::ClaudeCode)
            .then(|| option_value(&self.argv, "--name", Some("-n")))
            .flatten()
    }

    fn claude_home(&self) -> Option<PathBuf> {
        self.environment
            .get("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| self.home.as_ref().map(|home| home.join(".claude")))
    }

    fn prime_home(&self) -> Option<PathBuf> {
        self.environment
            .get("PRIME_AGENT_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .or_else(|| self.home.as_ref().map(|home| home.join(".prime/agent")))
    }

    fn prime_sessions(&self) -> Option<PathBuf> {
        option_value(&self.argv, "--session-dir", None)
            .map(PathBuf::from)
            .or_else(|| {
                self.environment
                    .get("PRIME_AGENT_SESSION_DIR")
                    .or_else(|| self.environment.get("PRIME_AGENT_CODING_AGENT_SESSION_DIR"))
                    .map(PathBuf::from)
            })
            .or_else(|| self.prime_home().map(|home| home.join("sessions")))
    }

    fn selector(&self) -> Option<String> {
        match self.harness {
            Harness::Codex => None,
            Harness::ClaudeCode => option_value(&self.argv, "--session-id", None)
                .or_else(|| option_value(&self.argv, "--resume", Some("-r"))),
            Harness::PrimeAgent => option_value(&self.argv, "--resume", Some("-r")).or_else(|| {
                (self.argv.get(1).and_then(|arg| arg.to_str()) == Some("attach"))
                    .then(|| self.argv.get(2)?.to_str().map(str::to_owned))
                    .flatten()
            }),
        }
    }

    fn resumed_thread(&self) -> Option<&str> {
        (self.harness == Harness::Codex)
            .then(|| codex_resumed_thread(&self.argv))
            .flatten()
    }
}

fn codex_resumed_thread(argv: &[OsString]) -> Option<&str> {
    let resume = argv.iter().position(|arg| arg == OsStr::new("resume"))?;
    argv.iter()
        .skip(resume + 1)
        .filter_map(|arg| arg.to_str())
        .find(|arg| uuid_literal(arg))
}

fn manual_harnesses(
    windows: &HashMap<u32, Vec<u32>>,
    cache: &mut HashMap<ProcessKey, Process>,
) -> Vec<Sighting> {
    let mut sightings = Vec::new();
    for pid in proc_pids() {
        let Some(process) = harness_process(pid, cache) else {
            continue;
        };
        let Some(window) = nearest_window(&process, windows) else {
            continue;
        };
        sightings.push(Sighting { process, window });
    }
    sightings.sort_by_key(|sighting| std::cmp::Reverse(sighting.process.pid));
    let living = sightings
        .iter()
        .map(|sighting| sighting.process.key)
        .collect::<HashSet<_>>();
    cache.retain(|key, _| living.contains(key));
    sightings
}

fn proc_pids() -> Vec<u32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect()
}

fn nearest_window(process: &Process, windows: &HashMap<u32, Vec<u32>>) -> Option<u32> {
    let hint = process
        .environment
        .get("WINDOWID")
        .and_then(|value| x11_window_id(value));
    let mut ancestor = process.pid;
    for _ in 0..256 {
        if ancestor <= 1 {
            return None;
        }
        if let Some(candidates) = windows.get(&ancestor) {
            return choose_window(candidates, hint);
        }
        ancestor = parent_pid(ancestor)?;
    }
    None
}

fn choose_window(candidates: &[u32], hint: Option<u32>) -> Option<u32> {
    hint.filter(|window| candidates.contains(window))
        .or_else(|| unique_candidate(candidates.iter().copied()))
}

fn x11_window_id(value: &OsStr) -> Option<u32> {
    let value = value.to_str()?.trim();
    value.strip_prefix("0x").map_or_else(
        || value.parse().ok(),
        |hexadecimal| u32::from_str_radix(hexadecimal, 16).ok(),
    )
}

fn parent_pid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat_parent(&stat)
}

fn stat_parent(stat: &str) -> Option<u32> {
    stat.rsplit_once(") ")?
        .1
        .split_ascii_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn harness_process(pid: u32, cache: &mut HashMap<ProcessKey, Process>) -> Option<Process> {
    let root = PathBuf::from(format!("/proc/{pid}"));
    let bytes = fs::read(root.join("cmdline")).ok()?;
    let argv = bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| OsString::from_vec(arg.to_vec()))
        .collect::<Vec<_>>();
    let harness = harness_argv(&argv)?;
    let key = ProcessKey::sight(pid).ok()?;
    if let Some(prior) = cache.get(&key)
        && prior.argv == argv
        && prior.harness == harness
    {
        let mut process = prior.clone();
        let (transcripts, claims) = process_descriptors(&root, harness);
        if claims != prior.codex_claims {
            process.binding = Arc::new(OnceLock::new());
        }
        process.transcripts = transcripts;
        process.codex_claims = claims;
        process.cwd = fs::read_link(root.join("cwd")).unwrap_or_else(|_| process.cwd.clone());
        let _prior = cache.insert(key, process.clone());
        return Some(process);
    }
    if !foreground_tty(&root) {
        return None;
    }
    let environment = process_environment(&root);
    let home = environment
        .get("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    let cwd = fs::read_link(root.join("cwd")).unwrap_or_else(|_| PathBuf::from("."));
    let goal = harness == Harness::PrimeAgent && has_option(&argv, "--goal");
    let (transcripts, codex_claims) = process_descriptors(&root, harness);
    let binding = cache
        .get(&key)
        .filter(|prior| {
            prior.harness == Harness::Codex
                && harness == Harness::Codex
                && prior.codex_claims == codex_claims
        })
        .map_or_else(
            || Arc::new(OnceLock::new()),
            |prior| Arc::clone(&prior.binding),
        );
    let process = Process {
        key,
        pid,
        harness,
        argv,
        transcripts,
        codex_claims,
        binding,
        cwd,
        environment,
        home,
        goal,
    };
    let _prior = cache.insert(key, process.clone());
    Some(process)
}

fn harness_argv(argv: &[OsString]) -> Option<Harness> {
    let program = argv.first().and_then(|arg| Path::new(arg).file_name())?;
    if program == OsStr::new("codex") {
        return (!argv.iter().skip(1).any(|arg| arg == OsStr::new("exec")))
            .then_some(Harness::Codex);
    }
    if program == OsStr::new("claude") || program == OsStr::new("claude-code") {
        let banished = ["--print", "-p", "--background", "--bg"]
            .iter()
            .any(|flag| has_option(argv, flag));
        let command = argv.get(1).and_then(|arg| arg.to_str());
        let subcommand = command.is_some_and(|command| {
            [
                "agents",
                "auth",
                "auto-mode",
                "doctor",
                "gateway",
                "install",
                "mcp",
                "plugin",
                "plugins",
                "project",
                "setup-token",
                "ultrareview",
                "update",
                "upgrade",
            ]
            .contains(&command)
        });
        return (!banished && !subcommand).then_some(Harness::ClaudeCode);
    }
    if program == OsStr::new("prime-agent") {
        let banished = ["--print", "-p"].iter().any(|flag| has_option(argv, flag))
            || option_value(argv, "--mode", None).is_some_and(|mode| mode != "text");
        let command = argv.get(1).and_then(|arg| arg.to_str());
        let subcommand = command.is_some_and(|command| {
            [
                "agents", "config", "doctor", "help", "list", "model", "package", "rename",
                "schedule", "send", "session", "shutdown", "status", "stop", "update",
            ]
            .contains(&command)
        });
        return (!banished && !subcommand).then_some(Harness::PrimeAgent);
    }
    None
}

fn has_option(argv: &[OsString], option: &str) -> bool {
    argv.iter().skip(1).any(|arg| {
        arg == OsStr::new(option)
            || arg.to_str().is_some_and(|arg| {
                arg.strip_prefix(option)
                    .is_some_and(|tail| tail.starts_with('='))
            })
    })
}

fn option_value(argv: &[OsString], long: &str, short: Option<&str>) -> Option<String> {
    for (index, arg) in argv.iter().enumerate().skip(1) {
        let text = arg.to_str()?;
        if text == long || short == Some(text) {
            return argv
                .get(index + 1)
                .and_then(|value| value.to_str())
                .filter(|value| !value.starts_with('-'))
                .map(str::to_owned);
        }
        if let Some(value) = text
            .strip_prefix(long)
            .and_then(|tail| tail.strip_prefix('='))
            && !value.is_empty()
        {
            return Some(value.to_owned());
        }
    }
    None
}

fn process_environment(root: &Path) -> HashMap<String, OsString> {
    let mut environment = HashMap::new();
    let Ok(bytes) = fs::read(root.join("environ")) else {
        return environment;
    };
    for pair in bytes.split(|byte| *byte == 0) {
        let Some(split) = pair.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let Ok(name) = std::str::from_utf8(&pair[..split]) else {
            continue;
        };
        let _prior = environment.insert(
            name.to_owned(),
            OsString::from_vec(pair[split + 1..].to_vec()),
        );
    }
    environment
}

fn foreground_tty(root: &Path) -> bool {
    ["0", "1", "2"].iter().all(|fd| {
        fs::read_link(root.join("fd").join(fd)).is_ok_and(|target| target.starts_with("/dev/pts/"))
    })
}

fn process_descriptors(root: &Path, harness: Harness) -> (Vec<PathBuf>, Vec<CodexClaim>) {
    if harness == Harness::Codex {
        return codex_claims(root);
    }
    (open_jsonls(root), Vec::new())
}

fn codex_claims(root: &Path) -> (Vec<PathBuf>, Vec<CodexClaim>) {
    let mut rollouts = Vec::new();
    let mut claims = Vec::new();
    for entry in fs::read_dir(root.join("fd"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        if let Some(thread) = writer_lock_thread(&target) {
            claims.push(CodexClaim::WriterLock(thread.to_owned()));
            continue;
        }
        if !jsonl(&target) {
            continue;
        }
        let Some(true) = fs::read(root.join("fdinfo").join(entry.file_name()))
            .ok()
            .and_then(|fdinfo| writable_access(&fdinfo))
        else {
            continue;
        };
        rollouts.push(target.clone());
        claims.push(CodexClaim::WritableRollout(target));
    }
    rollouts.sort_unstable();
    rollouts.dedup();
    claims.sort_unstable();
    claims.dedup();
    (rollouts, claims)
}

fn writer_lock_thread(path: &Path) -> Option<&str> {
    (path.parent()?.file_name() == Some(OsStr::new("thread-writer-locks")))
        .then(|| path.file_stem()?.to_str().filter(|stem| uuid_literal(stem)))
        .flatten()
}

fn open_jsonls(root: &Path) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(root.join("fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let target = fs::read_link(entry.path()).ok()?;
            if !jsonl(&target) {
                return None;
            }
            Some(target)
        })
        .collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    paths
}

fn jsonl(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

fn writable_access(fdinfo: &[u8]) -> Option<bool> {
    let flags = fdinfo.split(|byte| *byte == b'\n').find_map(|line| {
        let value = line.strip_prefix(b"flags:")?;
        u32::from_str_radix(std::str::from_utf8(value).ok()?.trim(), 8).ok()
    })?;
    Some(matches!(flags & 0o3, 0o1 | 0o2))
}

#[derive(Default)]
struct SessionLocator {
    claimed: HashSet<PathBuf>,
    prime_workers: HashMap<PathBuf, Vec<PrimeWorker>>,
}

impl SessionLocator {
    fn locate(&mut self, process: &Process) -> Option<PathBuf> {
        match process.harness {
            Harness::Codex => None,
            Harness::ClaudeCode => self.claude(process),
            Harness::PrimeAgent => self.prime(process),
        }
    }

    fn claude(&mut self, process: &Process) -> Option<PathBuf> {
        if let Some(path) = self.claim(
            process
                .transcripts
                .iter()
                .filter(|path| claude_jsonl(path))
                .cloned(),
        ) {
            return Some(path);
        }
        let directory = process
            .claude_home()?
            .join("projects")
            .join(claude_project_key(&process.cwd));
        if let Some(selector) = process.selector() {
            let exact = directory.join(format!("{selector}.jsonl"));
            if exact.is_file() && self.claimed.insert(exact.clone()) {
                return Some(exact);
            }
        }
        self.claim(direct_jsonls(&directory))
    }

    fn prime(&mut self, process: &Process) -> Option<PathBuf> {
        if let Some(path) = self.claim(
            process
                .transcripts
                .iter()
                .filter(|path| prime_jsonl(path))
                .cloned(),
        ) {
            return Some(path);
        }
        let session_dir = process.prime_sessions()?;
        if let Some(selector) = process.selector() {
            let selector_path = PathBuf::from(&selector);
            if selector_path.is_file() && self.claimed.insert(selector_path.clone()) {
                return Some(selector_path);
            }
            if let Some(path) = self.claim(direct_jsonls(&session_dir).into_iter().filter(|path| {
                path.file_stem()
                    .and_then(OsStr::to_str)
                    .is_some_and(|stem| stem.starts_with(&selector))
            })) {
                return Some(path);
            }
        }
        if let Some(agent_dir) = process.prime_home() {
            let workers = self
                .prime_workers
                .entry(agent_dir.clone())
                .or_insert_with(|| read_prime_workers(&agent_dir))
                .clone();
            if let Some(path) = self.claim(
                workers
                    .into_iter()
                    .filter(|worker| worker.cwd == process.cwd)
                    .map(|worker| worker.session),
            ) {
                return Some(path);
            }
        }
        self.claim(direct_jsonls(&session_dir))
    }

    fn claim(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
        let selected = paths
            .into_iter()
            .filter(|path| !self.claimed.contains(path))
            .filter_map(|path| {
                let modified = fs::metadata(&path).ok()?.modified().ok()?;
                Some((modified, path))
            })
            .max_by(Ord::cmp)?
            .1;
        let _new = self.claimed.insert(selected.clone());
        Some(selected)
    }
}

#[derive(Clone)]
struct PrimeWorker {
    cwd: PathBuf,
    session: PathBuf,
}

fn read_prime_workers(agent_dir: &Path) -> Vec<PrimeWorker> {
    let Ok(hosts) = fs::read_dir(agent_dir.join("daemon-workers")) else {
        return Vec::new();
    };
    hosts
        .flatten()
        .flat_map(|host| fs::read_dir(host.path()).into_iter().flatten().flatten())
        .filter(|entry| entry.path().extension() == Some(OsStr::new("json")))
        .filter_map(|entry| fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .filter_map(|record| {
            let session = record
                .get("sessionFile")
                .and_then(serde_json::Value::as_str)?;
            let cwd = record
                .pointer("/createCommand/config/cwd")
                .and_then(serde_json::Value::as_str)?;
            Some(PrimeWorker {
                cwd: PathBuf::from(cwd),
                session: PathBuf::from(session),
            })
        })
        .collect()
}

fn direct_jsonls(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("jsonl")))
        .collect()
}

fn claude_jsonl(path: &Path) -> bool {
    prime_jsonl(path)
        && path
            .ancestors()
            .any(|ancestor| ancestor.file_name() == Some(OsStr::new("projects")))
        && !path
            .ancestors()
            .any(|ancestor| ancestor.file_name() == Some(OsStr::new("subagents")))
}

fn prime_jsonl(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("jsonl"))
        && path
            .file_stem()
            .and_then(OsStr::to_str)
            .is_some_and(uuidish)
        && !path
            .ancestors()
            .any(|ancestor| ancestor.file_name() == Some(OsStr::new("session-artifacts")))
}

fn uuidish(stem: &str) -> bool {
    stem.len() == 36
        && stem
            .bytes()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) == (byte == b'-'))
}

fn claude_project_key(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() {
                char::from(byte)
            } else {
                '-'
            }
        })
        .collect()
}

fn rollout_id(path: &Path) -> Option<&str> {
    let stem = path.file_stem()?.to_str()?;
    let id = stem.rsplit_once('-').map_or(stem, |(_, tail)| tail);
    if id.len() == 12 {
        // `rsplit_once` sees only the UUID's last group; take its full 36-byte suffix.
        stem.get(stem.len().checked_sub(36)?..)
    } else {
        None
    }
}

fn uuid_literal(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_state_has_one_lawful_precedence() {
        assert_eq!(classify_work(TurnState::Running, true, false), Work::Goal);
        assert_eq!(classify_work(TurnState::Running, false, false), Work::Turn);
        assert_eq!(classify_work(TurnState::Done, true, false), Work::Done);
        assert_eq!(classify_work(TurnState::Done, false, false), Work::Done);
        assert_eq!(classify_work(TurnState::Running, true, true), Work::Input);
        assert_eq!(classify_work(TurnState::Unknown, false, false), Work::Error);
        assert_eq!(classify_work(TurnState::Unknown, false, true), Work::Input);
        assert_eq!(classify_work(TurnState::Error, false, false), Work::Error);
        assert_eq!(classify_work(TurnState::Error, false, true), Work::Error);
    }

    #[test]
    fn terminal_title_changes_demand_projection_without_a_forest_scan() {
        assert_eq!(
            desktop_demand(&HashSet::from([DesktopSignal::Terminal])),
            DesktopDemand {
                projection: true,
                forest: false,
            }
        );
        assert_eq!(
            desktop_demand(&HashSet::from([DesktopSignal::Topology])),
            DesktopDemand {
                projection: false,
                forest: true,
            }
        );
    }

    #[test]
    fn admits_three_interactive_harnesses_and_beheads_batch_modes() {
        assert_eq!(
            harness_argv(&[OsString::from("codex")]),
            Some(Harness::Codex)
        );
        assert_eq!(
            harness_argv(&[
                OsString::from("/usr/bin/claude"),
                OsString::from("--resume"),
                OsString::from("id")
            ]),
            Some(Harness::ClaudeCode)
        );
        assert_eq!(
            harness_argv(&[
                OsString::from("prime-agent"),
                OsString::from("--cwd"),
                OsString::from("/work")
            ]),
            Some(Harness::PrimeAgent)
        );
        assert_eq!(
            harness_argv(&[
                OsString::from("prime-agent"),
                OsString::from("--mode"),
                OsString::from("text")
            ]),
            Some(Harness::PrimeAgent)
        );
        assert_eq!(
            harness_argv(&[
                OsString::from("prime-agent"),
                OsString::from("attach"),
                OsString::from("session-id")
            ]),
            Some(Harness::PrimeAgent)
        );
        assert_eq!(
            harness_argv(&[
                OsString::from("codex"),
                OsString::from("exec"),
                OsString::from("do it")
            ]),
            None
        );
        assert_eq!(
            harness_argv(&[
                OsString::from("claude"),
                OsString::from("--print"),
                OsString::from("do it")
            ]),
            None
        );
        assert_eq!(
            harness_argv(&[OsString::from("prime-agent"), OsString::from("list")]),
            None
        );
        assert_eq!(
            harness_argv(&[
                OsString::from("prime-agent"),
                OsString::from("--mode"),
                OsString::from("daemon")
            ]),
            None
        );
    }

    #[test]
    fn extracts_the_full_rollout_uuid() {
        let path =
            Path::new("/x/rollout-2026-08-03T16-11-28-019fc940-b18f-7ad2-a012-71d86289bd60.jsonl");
        assert_eq!(
            rollout_id(path),
            Some("019fc940-b18f-7ad2-a012-71d86289bd60")
        );
    }

    #[test]
    fn proc_fd_access_mode_distinguishes_readers_from_writers() {
        assert_eq!(writable_access(b"pos:\t0\nflags:\t0100000\n"), Some(false));
        assert_eq!(writable_access(b"pos:\t0\nflags:\t0100001\n"), Some(true));
        assert_eq!(writable_access(b"pos:\t0\nflags:\t0104002\n"), Some(true));
        assert_eq!(writable_access(b"pos:\t0\n"), None);
        assert_eq!(writable_access(b"flags:\tnot-octal\n"), None);
    }

    #[test]
    fn uniqueness_rejects_zero_or_competing_candidates() {
        assert_eq!(unique_candidate(Vec::<u8>::new()), None);
        assert_eq!(unique_candidate([7]), Some(7));
        assert_eq!(unique_candidate([7, 8]), None);
    }

    #[test]
    fn window_binding_is_exact_or_unambiguous() {
        assert_eq!(choose_window(&[11], None), Some(11));
        assert_eq!(choose_window(&[11, 12], Some(12)), Some(12));
        assert_eq!(choose_window(&[11, 12], Some(13)), None);
        assert_eq!(choose_window(&[11, 12], None), None);
    }

    #[test]
    fn x11_window_hint_accepts_decimal_and_hexadecimal() {
        assert_eq!(x11_window_id(OsStr::new("251658245")), Some(251_658_245));
        assert_eq!(x11_window_id(OsStr::new("0x0f000005")), Some(251_658_245));
        assert_eq!(x11_window_id(OsStr::new("terminal")), None);
    }

    #[test]
    fn proc_parent_parser_survives_a_hostile_process_name() {
        assert_eq!(stat_parent("42 (a ) hostile name) S 7 8 9"), Some(7));
        assert_eq!(stat_parent("malformed"), None);
    }

    #[test]
    fn explicit_codex_resume_recognizes_only_a_uuid() {
        let id = "019fc940-b18f-7ad2-a012-71d86289bd60";
        assert_eq!(
            codex_resumed_thread(&[
                OsString::from("codex"),
                OsString::from("resume"),
                OsString::from(id),
            ]),
            Some(id)
        );
        assert_eq!(
            codex_resumed_thread(&[
                OsString::from("codex"),
                OsString::from("resume"),
                OsString::from("--last"),
            ]),
            None
        );
    }

    #[test]
    fn immutable_binding_lives_only_while_its_writer_is_held() {
        let binding = ThreadBinding {
            id: "thread".to_owned(),
            rollout: PathBuf::from("/sessions/current.jsonl"),
        };
        assert!(binding.held_by(&[
            CodexClaim::WritableRollout(PathBuf::from("/sessions/other.jsonl")),
            CodexClaim::WritableRollout(binding.rollout.clone()),
        ]));
        assert!(binding.held_by(&[CodexClaim::WriterLock("thread".to_owned())]));
        assert!(!binding.held_by(&[CodexClaim::WriterLock("replacement".to_owned())]));
    }

    #[test]
    fn claude_project_directory_uses_its_native_path_cipher() {
        assert_eq!(
            claude_project_key(Path::new("/home/main/a.b/work-tree")),
            "-home-main-a-b-work-tree"
        );
    }
}
