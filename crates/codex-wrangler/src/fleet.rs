use std::{
    collections::HashSet,
    io::{BufRead as _, BufReader, Read as _},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use codex_wrangler_contract::{Harness, Work};
use crossbeam_channel::{Sender, TrySendError, bounded};
use eternalist_apps::NativeWake;
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use semver::Version;
use serde::Deserialize;

use crate::{
    desktop::Desktop,
    history::Session,
    model::Card,
    recon::{Activation, Intent, Strike},
    site::{RemoteSite, Site},
};

const BRIDGE_PROTOCOL: u16 = 2;
const RECONNECTION_DELAY: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FleetSnapshot {
    pub cards: Vec<Card>,
    pub sessions: Vec<Session>,
    pub sites: Vec<SiteStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SiteStatus {
    pub site: RemoteSite,
    pub bridge_version: Option<String>,
    pub codex_version: Option<String>,
    pub platform: Option<String>,
    pub fault: Option<String>,
}

pub struct FleetWorker {
    latest: Arc<Mutex<Option<FleetSnapshot>>>,
    activation: Arc<Mutex<Option<Activation>>>,
    pub strike: FleetStriker,
    alive: Arc<AtomicBool>,
    pids: Arc<Vec<AtomicU32>>,
    threads: Vec<JoinHandle<()>>,
}

pub struct FleetStriker {
    channel: Sender<Strike>,
}

#[derive(Clone, Debug)]
struct SiteState {
    status: SiteStatus,
    protocol_compatible: bool,
    live_cards: Vec<Card>,
    history_cards: Vec<Card>,
    roster: HashSet<String>,
    sessions: Vec<Session>,
}

struct SiteWatcher {
    index: usize,
    site: RemoteSite,
    expected_codex: Option<Version>,
    repaint: NativeWake,
    latest: Arc<Mutex<Option<FleetSnapshot>>>,
    states: Arc<Vec<Mutex<SiteState>>>,
    pids: Arc<Vec<AtomicU32>>,
    alive: Arc<AtomicBool>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "frame", rename_all = "kebab-case")]
enum Frame {
    Hello {
        protocol: u16,
        bridge_version: String,
        codex_version: String,
        platform: String,
    },
    Live {
        threads: Vec<LiveThread>,
    },
    History {
        threads: Vec<HistoricalThread>,
    },
    Roster {
        threads: Vec<String>,
    },
}

#[derive(Debug, Deserialize)]
struct LiveThread {
    thread: String,
    name: Option<String>,
    cwd: String,
    preview: String,
    work: RemoteWork,
    updated_at: i64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RemoteWork {
    Error,
    Input,
    Turn,
    Done,
}

#[derive(Debug, Deserialize)]
struct HistoricalThread {
    thread: String,
    name: Option<String>,
    cwd: String,
    preview: String,
    updated_at: i64,
    last_turn: String,
    archived: bool,
}

impl FleetStriker {
    pub fn try_send(&self, strike: Strike) -> Result<(), TrySendError<Strike>> {
        self.channel.try_send(strike)
    }
}

impl FleetWorker {
    pub fn spawn(repaint: NativeWake, remotes: Vec<RemoteSite>) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let activation = Arc::new(Mutex::new(None));
        let alive = Arc::new(AtomicBool::new(true));
        let pids = Arc::new(
            remotes
                .iter()
                .map(|_| AtomicU32::new(0))
                .collect::<Vec<_>>(),
        );
        let states = Arc::new(
            remotes
                .iter()
                .cloned()
                .map(|site| {
                    Mutex::new(SiteState {
                        status: SiteStatus {
                            site,
                            bridge_version: None,
                            codex_version: None,
                            platform: None,
                            fault: Some("CONNECTING".to_owned()),
                        },
                        protocol_compatible: false,
                        live_cards: Vec::new(),
                        history_cards: Vec::new(),
                        roster: HashSet::new(),
                        sessions: Vec::new(),
                    })
                })
                .collect::<Vec<_>>(),
        );
        publish(&repaint, &latest, &states);
        let expected_codex = crate::recon::installed_codex_version().ok();

        let mut threads = remotes
            .into_iter()
            .enumerate()
            .map(|(index, site)| {
                let watcher = SiteWatcher {
                    index,
                    site,
                    expected_codex: expected_codex.clone(),
                    repaint: repaint.clone(),
                    latest: Arc::clone(&latest),
                    states: Arc::clone(&states),
                    pids: Arc::clone(&pids),
                    alive: Arc::clone(&alive),
                };
                thread::Builder::new()
                    .name(format!("codex-wrangler-site-{}", watcher.site.endpoint()))
                    .spawn(move || watcher.watch())
                    .expect("spawn remote Site watcher")
            })
            .collect::<Vec<_>>();

        let (strike, strikes) = bounded::<Strike>(16);
        let activation_thread = Arc::clone(&activation);
        let alive_thread = Arc::clone(&alive);
        let repaint_thread = repaint;
        threads.push(
            thread::Builder::new()
                .name("codex-wrangler-site-launcher".to_owned())
                .spawn(move || {
                    while alive_thread.load(Ordering::Acquire) {
                        let Ok(strike) = strikes.recv_timeout(Duration::from_millis(250)) else {
                            continue;
                        };
                        let conceal =
                            matches!(strike.intent, Intent::Select | Intent::Fork | Intent::Open);
                        let succeeded = launch(&strike).unwrap_or_else(|error| {
                            eprintln!(
                                "codex-wrangler could not launch remote thread {}: {error:#}",
                                strike.thread
                            );
                            false
                        });
                        *activation_thread
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(Activation {
                                strike,
                                succeeded,
                                conceal,
                            });
                        let _repaint = repaint_thread.request_repaint();
                    }
                })
                .expect("spawn remote Site launcher"),
        );

        Self {
            latest,
            activation,
            strike: FleetStriker { channel: strike },
            alive,
            pids,
            threads,
        }
    }

    pub fn take_snapshot(&self) -> Option<FleetSnapshot> {
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

impl Drop for FleetWorker {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        for pid in self.pids.iter().map(|pid| pid.swap(0, Ordering::AcqRel)) {
            if let Ok(pid) = i32::try_from(pid)
                && pid > 0
            {
                let _killed = kill(Pid::from_raw(pid), Signal::SIGTERM);
            }
        }
        for thread in self.threads.drain(..) {
            let _joined = thread.join();
        }
    }
}

impl SiteWatcher {
    fn watch(&self) {
        while self.alive.load(Ordering::Acquire) {
            match bridge(&self.site) {
                Ok(mut child) => {
                    self.pids[self.index].store(child.id(), Ordering::Release);
                    read_bridge(
                        &self.site,
                        self.expected_codex.as_ref(),
                        &self.repaint,
                        &self.latest,
                        &self.states,
                        &mut child,
                        &self.alive,
                    );
                    self.pids[self.index].store(0, Ordering::Release);
                    let error = bridge_error(&mut child);
                    fault(self.index, error, &self.repaint, &self.latest, &self.states);
                }
                Err(error) => fault(
                    self.index,
                    format!("COULD NOT RAISE SITE BRIDGE · {error}"),
                    &self.repaint,
                    &self.latest,
                    &self.states,
                ),
            }
            for _ in 0..RECONNECTION_DELAY.as_millis() / 100 {
                if !self.alive.load(Ordering::Acquire) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn bridge(site: &RemoteSite) -> std::io::Result<Child> {
    Command::new("ssh")
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
            "--",
            site.endpoint(),
            r#"PATH="$HOME/.local/bin:/usr/bin""#,
            "codex-wrangler-bridge",
            "--watch",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn read_bridge(
    site: &RemoteSite,
    expected_codex: Option<&Version>,
    repaint: &NativeWake,
    latest: &Mutex<Option<FleetSnapshot>>,
    states: &[Mutex<SiteState>],
    child: &mut Child,
    alive: &AtomicBool,
) {
    let Some(stdout) = child.stdout.take() else {
        return;
    };
    for line in BufReader::new(stdout).lines() {
        if !alive.load(Ordering::Acquire) {
            break;
        }
        let frame = match line {
            Ok(line) => serde_json::from_str::<Frame>(&line),
            Err(error) => {
                fault(
                    site_index(site, states),
                    format!("SITE STREAM FAILED · {error}"),
                    repaint,
                    latest,
                    states,
                );
                break;
            }
        };
        match frame {
            Ok(frame) => absorb(site, expected_codex, frame, repaint, latest, states),
            Err(error) => {
                fault(
                    site_index(site, states),
                    format!("INCOMPATIBLE SITE FRAME · {error}"),
                    repaint,
                    latest,
                    states,
                );
                break;
            }
        }
    }
    if !alive.load(Ordering::Acquire) {
        let _killed = child.kill();
    }
}

fn absorb(
    site: &RemoteSite,
    expected_codex: Option<&Version>,
    frame: Frame,
    repaint: &NativeWake,
    latest: &Mutex<Option<FleetSnapshot>>,
    states: &[Mutex<SiteState>],
) {
    let index = site_index(site, states);
    let mut state = states[index]
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match frame {
        Frame::Hello {
            protocol,
            bridge_version,
            codex_version,
            platform,
        } => {
            state.protocol_compatible = protocol == BRIDGE_PROTOCOL;
            state.status.bridge_version = Some(bridge_version);
            state.status.codex_version = Some(codex_version);
            state.status.platform = Some(platform);
            state.status.fault = if state.protocol_compatible {
                distribution_fault(expected_codex, &state.status)
            } else {
                state.live_cards.clear();
                state.roster.clear();
                Some(format!(
                    "HARMONIZE SITE · bridge protocol {protocol}, expected {BRIDGE_PROTOCOL}"
                ))
            };
        }
        Frame::Live { threads } if state.protocol_compatible => {
            state.live_cards = threads
                .into_iter()
                .map(|thread| Card {
                    site: Site::Remote(site.clone()),
                    harness: Harness::Codex,
                    thread: thread.thread,
                    name: thread.name,
                    cwd: thread.cwd,
                    tile_preview: thread.preview,
                    work: thread.work.into(),
                    seat: None,
                    last_workspace: None,
                    updated_at_ms: thread.updated_at.saturating_mul(1_000),
                    pinned: false,
                })
                .collect();
        }
        Frame::Roster { threads } if state.protocol_compatible => {
            state.roster = threads.into_iter().collect();
        }
        Frame::History { threads } if state.protocol_compatible => {
            let mut sessions = Vec::with_capacity(threads.len());
            let mut history_cards = Vec::with_capacity(threads.len());
            for thread in threads {
                let updated_at_ms = thread.updated_at.saturating_mul(1_000);
                history_cards.push(Card {
                    site: Site::Remote(site.clone()),
                    harness: Harness::Codex,
                    thread: thread.thread.clone(),
                    name: thread.name.clone(),
                    cwd: thread.cwd,
                    tile_preview: thread.preview,
                    work: Work::Closed,
                    seat: None,
                    last_workspace: None,
                    updated_at_ms,
                    pinned: false,
                });
                sessions.push(Session {
                    site: Site::Remote(site.clone()),
                    thread: thread.thread,
                    name: thread.name,
                    last_turn: thread.last_turn,
                    updated_at_ms,
                    turns: None,
                    tally_failed: true,
                    bytes: 0,
                    archived: thread.archived,
                });
            }
            state.history_cards = history_cards;
            state.sessions = sessions;
        }
        Frame::Live { .. } | Frame::History { .. } | Frame::Roster { .. } => {}
    }
    drop(state);
    publish(repaint, latest, states);
}

fn site_index(site: &RemoteSite, states: &[Mutex<SiteState>]) -> usize {
    states
        .iter()
        .position(|state| {
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .status
                .site
                == *site
        })
        .expect("configured Site owns one state cell")
}

fn fault(
    index: usize,
    error: String,
    repaint: &NativeWake,
    latest: &Mutex<Option<FleetSnapshot>>,
    states: &[Mutex<SiteState>],
) {
    let mut state = states[index]
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.status.fault = Some(error);
    state.protocol_compatible = false;
    state.live_cards.clear();
    state.roster.clear();
    drop(state);
    publish(repaint, latest, states);
}

fn distribution_fault(expected: Option<&Version>, status: &SiteStatus) -> Option<String> {
    let expected = expected?;
    let codex = status.codex_version.as_deref()?;
    let bridge = status.bridge_version.as_deref()?;
    let expected_codex = expected.to_string();
    let expected_bridge = format!("{expected}-wrangler");
    (codex != expected_codex || bridge != expected_bridge).then(|| {
        format!("HARMONIZE SITE · remote Codex {codex}, bridge {bridge}; local Codex {expected}")
    })
}

fn publish(
    repaint: &NativeWake,
    latest: &Mutex<Option<FleetSnapshot>>,
    states: &[Mutex<SiteState>],
) {
    let states = states
        .iter()
        .map(|state| {
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
        .collect::<Vec<_>>();
    let mut snapshot = FleetSnapshot {
        cards: states.iter().flat_map(SiteState::cards).collect(),
        sessions: states
            .iter()
            .flat_map(|state| state.sessions.clone())
            .collect(),
        sites: states.into_iter().map(|state| state.status).collect(),
    };
    snapshot.cards.sort();
    snapshot.sessions.sort_unstable_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.site.cmp(&right.site))
            .then_with(|| left.thread.cmp(&right.thread))
    });
    *latest
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot);
    let _repaint = repaint.request_repaint();
}

impl SiteState {
    fn cards(&self) -> impl Iterator<Item = Card> + '_ {
        let loaded = self
            .live_cards
            .iter()
            .map(|card| card.thread.as_str())
            .collect::<HashSet<_>>();
        self.live_cards.iter().cloned().chain(
            self.history_cards
                .iter()
                .filter(move |card| {
                    self.roster.contains(&card.thread) && !loaded.contains(card.thread.as_str())
                })
                .cloned(),
        )
    }
}

fn bridge_error(child: &mut Child) -> String {
    let status = child.wait();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _read = pipe.read_to_string(&mut stderr);
    }
    let detail = stderr.trim();
    if detail.is_empty() {
        status.map_or_else(
            |error| format!("SITE BRIDGE VANISHED · {error}"),
            |status| format!("SITE BRIDGE ENDED · {status}"),
        )
    } else {
        format!("SITE NEEDS HARMONIZATION · {detail}")
    }
}

fn launch(strike: &Strike) -> anyhow::Result<bool> {
    let Some(site) = strike.site.remote() else {
        anyhow::bail!("remote launcher received a local strike");
    };
    if matches!(strike.intent, Intent::Select | Intent::Open)
        && let Some(seat) = strike.seat
    {
        Desktop::connect()?.activate(seat.window)?;
        return Ok(true);
    }
    let verb = match strike.intent {
        Intent::Select | Intent::Open => "resume",
        Intent::Fork => "fork",
        Intent::Pin | Intent::Unpin | Intent::Dismiss => return Ok(false),
    };
    let mut child = Command::new("alacritty")
        .args([
            "-e",
            "ssh",
            "-t",
            "--",
            site.endpoint(),
            "/usr/bin/env",
            "COLORTERM=truecolor",
            "/usr/bin/codex-wrangler",
            "relay",
            verb,
            &strike.thread,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    thread::Builder::new()
        .name("codex-wrangler-remote-terminal-reaper".to_owned())
        .spawn(move || {
            let _waited = child.wait();
        })?;
    Ok(true)
}

impl From<RemoteWork> for Work {
    fn from(work: RemoteWork) -> Self {
        match work {
            RemoteWork::Error => Self::Error,
            RemoteWork::Input => Self::Input,
            RemoteWork::Turn => Self::Turn,
            RemoteWork::Done => Self::Done,
        }
    }
}
