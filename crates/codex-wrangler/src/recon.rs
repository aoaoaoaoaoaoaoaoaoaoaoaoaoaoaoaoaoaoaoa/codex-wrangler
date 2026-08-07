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
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context as _, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};

use crate::{
    codex_rpc::CodexRpc,
    contract::{Harness, Work},
    desktop::{Desktop, DesktopSignal},
    model::{Card, Census, Retention, snip},
    rollout::Rollouts,
    roster::{AccountMark, Roster, Sighting as SessionSighting},
    stasis::{ProcessKey, Quarry, Stasis, children},
    transcript::Transcripts,
    watchfire::Watchfire,
};

const FOREST_AUDIT: Duration = Duration::from_secs(2);
const INTEGRITY_AUDIT: Duration = Duration::from_mins(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Intent {
    Open,
    Archive,
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

pub fn spawn(ctx: egui::Context) -> Nexus {
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
                &ctx,
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
    ctx: &egui::Context,
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
                ctx,
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
        publish_fault(ctx, latest, &error);
    } else {
        publish_changed(ctx, latest, &mut prior, recon.census());
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
        let mut dirty = execute_strikes(ctx, activation, strikes, &mut recon, now);

        if readiness[0] {
            let (changed, forest_hint) = heed_desktop(&mut recon, now);
            dirty |= changed;
            if forest_hint {
                forest_audit = now;
            }
        }
        if readiness[1] {
            dirty |= reap_watchfire(&mut recon);
        }
        if now >= forest_audit {
            match recon.refresh_forest() {
                Ok(changed) => dirty |= changed,
                Err(error) => publish_fault(ctx, latest, &error),
            }
            forest_audit = now + FOREST_AUDIT;
        }
        if now >= integrity_audit {
            dirty = true;
            integrity_audit = now + INTEGRITY_AUDIT;
        }
        if dirty {
            match recon.project(now) {
                Ok(()) => publish_changed(ctx, latest, &mut prior, recon.census()),
                Err(error) => publish_fault(ctx, latest, &error),
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
            publish_changed(ctx, latest, &mut prior, recon.census());
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
    ctx: &egui::Context,
    activation: &Mutex<Option<Activation>>,
    strikes: &Receiver<Strike>,
    recon: &mut Recon,
    now: Instant,
) -> bool {
    let mut struck = false;
    while let Ok(strike) = strikes.try_recv() {
        let conceal = strike.intent == Intent::Open;
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
        ctx.request_repaint();
        struck = true;
    }
    struck
}

fn heed_desktop(recon: &mut Recon, now: Instant) -> (bool, bool) {
    match recon.desktop.drain_events() {
        Ok(signals) => {
            let focus = signals.contains(&DesktopSignal::Focus);
            if focus {
                recon.refresh_focus(now);
            }
            (
                focus || signals.contains(&DesktopSignal::Workspace),
                signals.contains(&DesktopSignal::Topology)
                    || signals.contains(&DesktopSignal::Terminal),
            )
        }
        Err(error) => {
            recon.stasis.focus_uncertain();
            eprintln!("codex-wrangler lost X11 focus truth: {error:#}");
            (true, false)
        }
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
    ctx: &egui::Context,
    latest: &Mutex<Option<Census>>,
    prior: &mut Option<Census>,
    census: Census,
) {
    if prior.as_ref() != Some(&census) {
        *prior = Some(census.clone());
        publish(ctx, latest, census);
    }
}

fn publish_fault(ctx: &egui::Context, latest: &Mutex<Option<Census>>, error: &anyhow::Error) {
    publish(
        ctx,
        latest,
        Census {
            cards: Vec::new(),
            fault: Some(format!("Could not inspect harnesses: {error:#}")),
        },
    );
}

fn publish(ctx: &egui::Context, latest: &Mutex<Option<Census>>, census: Census) {
    *latest
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(census);
    ctx.request_repaint();
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
        let terminals = self
            .desktop
            .windows_by_pid()?
            .into_iter()
            .filter(|(pid, _)| alacritty(*pid))
            .collect::<HashMap<_, _>>();
        self.desktop.watch_terminals(terminals.values().copied())?;
        let sightings = manual_harnesses(&terminals, &mut self.process_cache);
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
            if card.harness == Harness::Codex && self.desktop.requires_action(sighting.window) {
                card.work = Work::Input;
                card.activity = Work::Input;
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
                        work: card.activity,
                    });
                }
                cards.push(card);
            }
        }
        if let Some(codex) = &mut self.codex {
            cards.extend(codex.dormant_cards(codex_seats.keys().map(String::as_str))?);
            codex.commit()?;
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
                card.work = Work::Sleeping;
            }
        }
        cards.sort();
        Census { cards, fault: None }
    }

    fn execute(&mut self, strike: &Strike, now: Instant) -> Result<bool> {
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
                (Intent::Open, Some(window)) => self.activate(window, now),
                _ => Ok(false),
            };
        }
        match strike.intent {
            Intent::Open => self.open_codex(&card, now),
            Intent::Archive => self.archive_codex(&card, now),
        }
    }

    fn activate(&mut self, window: u32, now: Instant) -> Result<bool> {
        if !self.stasis.prepare_activation(now, window) {
            anyhow::bail!("window {window} remains frozen");
        }
        self.desktop.activate(window)?;
        Ok(true)
    }

    fn open_codex(&mut self, card: &Card, now: Instant) -> Result<bool> {
        if card.retention == Retention::Archived || card.window.is_none() {
            let active = self.active_account("bind resumed Codex login");
            return self.summon_codex(
                &card.thread,
                card.retention == Retention::Archived,
                card.workspace,
                active,
            );
        }
        let window = card.window.expect("live Codex card owns a window");
        if card.activity == Work::Done {
            let codex = self.codex.as_mut().context("Codex adapter is absent")?;
            let home = codex.home.clone();
            let active = inspect_account(&home, "inspect current Codex login");
            let rotated = active.as_ref().is_some_and(|active| {
                codex
                    .roster
                    .get(&card.thread)
                    .is_some_and(|session| session.account.rotated_to(active))
            });
            if rotated {
                let seat = self
                    .codex_seats
                    .get(&card.thread)
                    .copied()
                    .context("live Codex card has no process seat")?;
                self.retire(seat, now)?;
                return self.summon_codex(&card.thread, false, card.workspace, active);
            }
        }
        self.activate(window, now)
    }

    fn archive_codex(&mut self, card: &Card, now: Instant) -> Result<bool> {
        let codex = self.codex.as_mut().context("Codex adapter is absent")?;
        if card.retention == Retention::Archived {
            codex.roster.forget(&card.thread);
            codex.commit()?;
            return Ok(true);
        }
        if card.activity != Work::Done {
            return Ok(false);
        }
        if let Some(seat) = self.codex_seats.get(&card.thread).copied() {
            self.retire(seat, now)?;
        }
        let codex = self.codex.as_mut().context("Codex adapter is absent")?;
        CodexRpc::open(&codex.home)?.archive(&card.thread)?;
        codex
            .roster
            .set_retention(&card.thread, Retention::Archived);
        codex.commit()?;
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
        archived: bool,
        workspace: Option<u32>,
        active: Option<AccountMark>,
    ) -> Result<bool> {
        let codex = self.codex.as_mut().context("Codex adapter is absent")?;
        let session = codex
            .roster
            .get(thread_id)
            .cloned()
            .context("Codex session is absent from Wrangler state")?;
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
        let operation = if archived { "unarchive" } else { "resume" };
        let mut child = Command::new("alacritty")
            .arg("--working-directory")
            .arg(&session.cwd)
            .args(["-e", "codex", operation, thread_id])
            .env("CODEX_HOME", &codex.home)
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
        let codex = self.codex.as_mut().context("Codex adapter is absent")?;
        codex.roster.set_retention(thread_id, Retention::Active);
        if let Some(active) = active {
            codex.roster.bind(thread_id, active);
        }
        codex.commit()?;
        Ok(true)
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
        let Some(thread) = self.current_thread(&process.transcripts)? else {
            return Ok(None);
        };
        let summary = self
            .rollouts
            .read(&thread.rollout)
            .with_context(|| format!("read rollout `{}`", thread.rollout.display()))?;
        let name = thread
            .name
            .or_else(|| self.names.get(&thread.id).map(str::to_owned));
        let work = classify_work(
            summary.running,
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
                activity: work,
                window: Some(window),
                workspace,
                updated_at_ms: thread.updated_at_ms,
                retention: Retention::Active,
            },
            rollout,
        )))
    }

    fn dormant_cards<'a>(&mut self, live: impl IntoIterator<Item = &'a str>) -> Result<Vec<Card>> {
        let live = live.into_iter().collect::<HashSet<_>>();
        let threads = self
            .roster
            .sessions()
            .map(|(thread, _)| thread.to_owned())
            .collect::<Vec<_>>();
        for thread in &threads {
            let archived = self
                .db
                .query_row(
                    "SELECT archived FROM threads WHERE id = ?1",
                    params![thread],
                    |row| row.get::<_, bool>(0),
                )
                .optional()
                .with_context(|| format!("query retention for Codex thread `{thread}`"))?;
            match archived {
                Some(true) => self.roster.set_retention(thread, Retention::Archived),
                Some(false) => self.roster.set_retention(thread, Retention::Active),
                None => self.roster.forget(thread),
            }
        }
        Ok(self
            .roster
            .sessions()
            .filter(|(thread, _)| !live.contains(*thread))
            .map(|(thread, session)| Card {
                harness: Harness::Codex,
                thread: thread.to_owned(),
                name: session.name.clone(),
                cwd: compact_path(&session.cwd, None),
                tile_preview: session.preview.clone(),
                work: Work::Done,
                activity: Work::Done,
                window: None,
                workspace: session.workspace,
                updated_at_ms: session.updated_at_ms,
                retention: session.retention,
            })
            .collect())
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

    fn current_thread(&self, paths: &[PathBuf]) -> Result<Option<Thread>> {
        let mut freshest = None;
        for rollout in paths {
            if !rollout.starts_with(self.home.join("sessions")) {
                continue;
            }
            let Some(id) = rollout_id(rollout) else {
                continue;
            };
            let thread = self
                .db
                .query_row(
                    "SELECT id, NULLIF(TRIM(name), ''), cwd, updated_at_ms, \
                     thread_source, source, agent_role FROM threads WHERE id = ?1",
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
                        ))
                    },
                )
                .optional()
                .with_context(|| format!("query Codex thread `{id}`"))?;
            let Some((id, name, cwd, updated_at_ms, thread_source, source, agent_role)) = thread
            else {
                continue;
            };
            if thread_source.as_deref() != Some("user") || source != "cli" || agent_role.is_some() {
                continue;
            }
            let candidate = Thread {
                id,
                name,
                cwd,
                updated_at_ms,
                rollout: rollout.clone(),
            };
            if freshest
                .as_ref()
                .is_none_or(|prior: &Thread| candidate.updated_at_ms > prior.updated_at_ms)
            {
                freshest = Some(candidate);
            }
        }
        Ok(freshest)
    }
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
    let activity = if process.goal && work == Work::Turn {
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
        work: activity,
        activity,
        window: Some(window),
        workspace,
        updated_at_ms: summary
            .as_ref()
            .map_or_else(|| i64::from(process.pid), |summary| summary.updated_at_ms),
        retention: Retention::Active,
    };
    (card, path)
}

const fn classify_work(running: bool, goal_active: bool, waiting_for_input: bool) -> Work {
    match (running, goal_active, waiting_for_input) {
        (_, _, true) => Work::Input,
        (true, true, false) => Work::Goal,
        (true, false, false) => Work::Turn,
        (false, _, false) => Work::Done,
    }
}

#[derive(Default)]
struct NameIndex {
    stamp: Option<(u64, SystemTime)>,
    names: HashMap<String, String>,
}

impl NameIndex {
    fn refresh(&mut self, path: &Path) -> Result<()> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.stamp = None;
                self.names.clear();
                return Ok(());
            }
            Err(error) => return Err(error).context("inspect Codex session-name index"),
        };
        let stamp = (metadata.len(), metadata.modified()?);
        if self.stamp == Some(stamp) {
            return Ok(());
        }
        self.names = parse_names(&fs::read(path)?);
        self.stamp = Some(stamp);
        Ok(())
    }

    fn get(&self, thread: &str) -> Option<&str> {
        self.names.get(thread).map(String::as_str)
    }
}

fn parse_names(bytes: &[u8]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let Some(thread) = record.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(name) = record
            .get("thread_name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
        else {
            continue;
        };
        if name.is_empty() {
            names.remove(thread);
        } else {
            let _prior = names.insert(thread.to_owned(), name.to_owned());
        }
    }
    names
}

struct Thread {
    id: String,
    name: Option<String>,
    cwd: PathBuf,
    updated_at_ms: i64,
    rollout: PathBuf,
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
    cwd: PathBuf,
    environment: HashMap<String, OsString>,
    home: Option<PathBuf>,
    goal: bool,
}

impl Process {
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
}

fn manual_harnesses(
    terminals: &HashMap<u32, u32>,
    cache: &mut HashMap<ProcessKey, Process>,
) -> Vec<Sighting> {
    let mut frontier = terminals
        .iter()
        .flat_map(|(pid, window)| children(*pid).into_iter().map(|child| (child, *window)))
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    let mut sightings = Vec::new();
    while let Some((pid, window)) = frontier.pop() {
        if !seen.insert(pid) {
            continue;
        }
        frontier.extend(children(pid).into_iter().map(|child| (child, window)));
        if let Some(process) = harness_process(pid, cache) {
            sightings.push(Sighting { process, window });
        }
    }
    sightings.sort_by_key(|sighting| std::cmp::Reverse(sighting.process.pid));
    let living = sightings
        .iter()
        .map(|sighting| sighting.process.key)
        .collect::<HashSet<_>>();
    cache.retain(|key, _| living.contains(key));
    sightings
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
        process.transcripts = open_jsonls(&root);
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
    let process = Process {
        key,
        pid,
        harness,
        argv,
        transcripts: open_jsonls(&root),
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

fn open_jsonls(root: &Path) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(root.join("fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .filter(|target| {
            target
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        })
        .collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    paths
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

fn alacritty(pid: u32) -> bool {
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|path| path.file_name().map(OsStr::to_owned))
        .as_deref()
        == Some(OsStr::new("alacritty"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_state_has_one_lawful_precedence() {
        assert_eq!(classify_work(true, true, false), Work::Goal);
        assert_eq!(classify_work(true, false, false), Work::Turn);
        assert_eq!(classify_work(false, true, false), Work::Done);
        assert_eq!(classify_work(false, false, false), Work::Done);
        assert_eq!(classify_work(true, true, true), Work::Input);
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
    fn claude_project_directory_uses_its_native_path_cipher() {
        assert_eq!(
            claude_project_key(Path::new("/home/main/a.b/work-tree")),
            "-home-main-a-b-work-tree"
        );
    }

    #[test]
    fn explicit_session_names_are_last_write_wins() {
        let names = parse_names(
            br#"{"id":"named","thread_name":"first"}
{"id":"anonymous","thread_name":""}
not-json
{"id":"named","thread_name":"final"}
"#,
        );
        assert_eq!(names.get("named").map(String::as_str), Some("final"));
        assert!(!names.contains_key("anonymous"));
    }
}
