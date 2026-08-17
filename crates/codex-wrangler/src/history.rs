use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{BufRead as _, BufReader, Read, Write as _},
    os::{
        fd::AsFd as _,
        unix::{fs::PermissionsExt as _, net::UnixStream},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use eternalist_apps::NativeWake;
use memchr::memmem;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{codex_rpc::CodexRpc, names::NameIndex, state, watchfire::Watchfire};

const INTEGRITY_AUDIT: Duration = Duration::from_mins(1);
const LEDGER_SETTLE: Duration = Duration::from_secs(2);
const INDEX_FILE: &str = "history-index.json";
const INDEX_VERSION: u8 = 1;
const SCAN_BLOCK: usize = 64 << 10;
const TASK_STARTED: &[u8] = b"\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"";
const TURN_STARTED: &[u8] = b"\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_started\"";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    pub thread: String,
    pub name: Option<String>,
    pub last_turn: String,
    pub updated_at_ms: i64,
    pub turns: Option<u64>,
    pub tally_failed: bool,
    pub bytes: u64,
    pub archived: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Census {
    pub sessions: Vec<Session>,
    pub fault: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Turn {
    pub user: String,
    pub model: String,
}

#[derive(Clone, Debug)]
pub struct TranscriptOutcome {
    pub thread: String,
    pub turns: Vec<Turn>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Archive,
    Unarchive,
    Delete,
    Rename,
}

impl Operation {
    pub const fn present_participle(self) -> &'static str {
        match self {
            Self::Archive => "ARCHIVING…",
            Self::Unarchive => "UNARCHIVING…",
            Self::Delete => "DELETING…",
            Self::Rename => "RENAMING…",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Order {
    Archive(String),
    Unarchive(String),
    Delete(String),
    Rename { thread: String, name: String },
}

impl Order {
    pub fn operate(thread: String, operation: Operation) -> Self {
        match operation {
            Operation::Archive => Self::Archive(thread),
            Operation::Unarchive => Self::Unarchive(thread),
            Operation::Delete => Self::Delete(thread),
            Operation::Rename => unreachable!("rename orders require a name"),
        }
    }

    pub const fn rename(thread: String, name: String) -> Self {
        Self::Rename { thread, name }
    }

    pub const fn thread(&self) -> &String {
        match self {
            Self::Archive(thread)
            | Self::Unarchive(thread)
            | Self::Delete(thread)
            | Self::Rename { thread, .. } => thread,
        }
    }

    pub const fn operation(&self) -> Operation {
        match self {
            Self::Archive(_) => Operation::Archive,
            Self::Unarchive(_) => Operation::Unarchive,
            Self::Delete(_) => Operation::Delete,
            Self::Rename { .. } => Operation::Rename,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub order: Order,
    pub error: Option<String>,
}

pub struct Nexus {
    latest: Arc<Mutex<Option<Census>>>,
    outcomes: Arc<Mutex<Vec<Outcome>>>,
    transcripts: Arc<Mutex<Vec<TranscriptOutcome>>>,
    courier: Courier,
    alive: Arc<AtomicBool>,
    wake: UnixStream,
    threads: Vec<JoinHandle<()>>,
}

pub struct Courier {
    channel: Sender<Intent>,
    wake: UnixStream,
}

enum Intent {
    Operate(Order),
    Tally(Vec<String>),
    Read(String),
}

#[derive(Clone)]
struct Artifact {
    path: PathBuf,
    nominal: PathBuf,
    compressed: bool,
    updated_at_ms: i64,
}

struct CountJob {
    thread: String,
    artifact: Artifact,
}

struct CountResult {
    thread: String,
    updated_at_ms: i64,
    tally: std::result::Result<u64, String>,
}

struct TranscriptJob {
    thread: String,
    artifact: Artifact,
}

enum ReadJob {
    Count(CountJob),
    Transcript(TranscriptJob),
}

enum ReadResult {
    Count(CountResult),
    Transcript(TranscriptOutcome),
}

impl Courier {
    pub fn order(&self, order: Order) -> Result<(), TrySendError<Order>> {
        self.send(Intent::Operate(order))
            .map_err(|error| match error {
                TrySendError::Full(Intent::Operate(order)) => TrySendError::Full(order),
                TrySendError::Disconnected(Intent::Operate(order)) => {
                    TrySendError::Disconnected(order)
                }
                TrySendError::Full(Intent::Tally(_) | Intent::Read(_))
                | TrySendError::Disconnected(Intent::Tally(_) | Intent::Read(_)) => {
                    unreachable!("operation intent remains an operation")
                }
            })
    }

    pub fn tally(&self, threads: Vec<String>) -> Result<(), TrySendError<Vec<String>>> {
        self.send(Intent::Tally(threads))
            .map_err(|error| match error {
                TrySendError::Full(Intent::Tally(threads)) => TrySendError::Full(threads),
                TrySendError::Disconnected(Intent::Tally(threads)) => {
                    TrySendError::Disconnected(threads)
                }
                TrySendError::Full(Intent::Operate(_) | Intent::Read(_))
                | TrySendError::Disconnected(Intent::Operate(_) | Intent::Read(_)) => {
                    unreachable!("inspection intent remains an inspection")
                }
            })
    }

    pub fn transcript(&self, thread: String) -> Result<(), TrySendError<String>> {
        self.send(Intent::Read(thread))
            .map_err(|error| match error {
                TrySendError::Full(Intent::Read(thread)) => TrySendError::Full(thread),
                TrySendError::Disconnected(Intent::Read(thread)) => {
                    TrySendError::Disconnected(thread)
                }
                TrySendError::Full(Intent::Operate(_) | Intent::Tally(_))
                | TrySendError::Disconnected(Intent::Operate(_) | Intent::Tally(_)) => {
                    unreachable!("transcript intent remains a transcript request")
                }
            })
    }

    fn send(&self, intent: Intent) -> Result<(), TrySendError<Intent>> {
        self.channel.try_send(intent)?;
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

    pub fn take_outcomes(&self) -> Vec<Outcome> {
        std::mem::take(
            &mut *self
                .outcomes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    pub fn take_transcripts(&self) -> Vec<TranscriptOutcome> {
        std::mem::take(
            &mut *self
                .transcripts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    pub const fn courier(&self) -> &Courier {
        &self.courier
    }
}

impl Drop for Nexus {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        let _woken = self.wake.write_all(&[0]);
        for thread in self.threads.drain(..) {
            let _joined = thread.join();
        }
    }
}

pub fn spawn(repaint: NativeWake) -> Nexus {
    let latest = Arc::new(Mutex::new(None));
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let transcripts = Arc::new(Mutex::new(Vec::new()));
    let alive = Arc::new(AtomicBool::new(true));
    let (intent_tx, intent_rx) = bounded(32);
    let (wake, worker_wake) = UnixStream::pair().expect("forge history wake pipe");
    wake.set_nonblocking(true)
        .expect("make history wake pipe nonblocking");
    worker_wake
        .set_nonblocking(true)
        .expect("make historian wake pipe nonblocking");
    let courier_wake = wake.try_clone().expect("clone history wake pipe");
    let (read_tx, read_rx) = bounded(64);
    let (result_tx, result_rx) = bounded(64);
    let (result_wake, worker_result_wake) =
        UnixStream::pair().expect("forge history counter wake pipe");
    result_wake
        .set_nonblocking(true)
        .expect("make counter wake pipe nonblocking");
    worker_result_wake
        .set_nonblocking(true)
        .expect("make historian counter pipe nonblocking");

    let reader_alive = Arc::clone(&alive);
    let reader = thread::Builder::new()
        .name("codex-wrangler-history-reader".to_owned())
        .spawn(move || read_history(&read_rx, &result_tx, &result_wake, &reader_alive))
        .expect("spawn historical reader");

    let worker_alive = Arc::clone(&alive);
    let worker_latest = Arc::clone(&latest);
    let worker_outcomes = Arc::clone(&outcomes);
    let worker_transcripts = Arc::clone(&transcripts);
    let historian = thread::Builder::new()
        .name("codex-wrangler-historian".to_owned())
        .spawn(move || {
            raid(
                &repaint,
                &worker_latest,
                &worker_outcomes,
                &worker_transcripts,
                &intent_rx,
                &worker_wake,
                &read_tx,
                &result_rx,
                &worker_result_wake,
                &worker_alive,
            );
        })
        .expect("spawn Codex historian");

    Nexus {
        latest,
        outcomes,
        transcripts,
        courier: Courier {
            channel: intent_tx,
            wake: courier_wake,
        },
        alive,
        wake,
        threads: vec![historian, reader],
    }
}

#[allow(clippy::too_many_arguments)]
fn raid(
    repaint: &NativeWake,
    latest: &Mutex<Option<Census>>,
    outcomes: &Mutex<Vec<Outcome>>,
    transcripts: &Mutex<Vec<TranscriptOutcome>>,
    intents: &Receiver<Intent>,
    wake: &UnixStream,
    read_tx: &Sender<ReadJob>,
    results: &Receiver<ReadResult>,
    result_wake: &UnixStream,
    alive: &AtomicBool,
) {
    let mut historian = match Historian::raise() {
        Ok(Some(historian)) => historian,
        Ok(None) => {
            publish(repaint, latest, Census::default());
            return;
        }
        Err(error) => {
            publish_fault(repaint, latest, &error);
            return;
        }
    };
    let mut prior = None;
    if let Err(error) = historian.refresh() {
        publish_fault(repaint, latest, &error);
    } else {
        publish_changed(repaint, latest, &mut prior, historian.census());
    }
    let mut integrity_audit = Instant::now() + INTEGRITY_AUDIT;

    while alive.load(Ordering::Acquire) {
        let deadline = historian
            .ledger
            .deadline()
            .into_iter()
            .chain([integrity_audit])
            .min()
            .unwrap_or(integrity_audit);
        let readiness = match wait_for_signal(
            &historian.watchfire,
            wake,
            result_wake,
            deadline.saturating_duration_since(Instant::now()),
        ) {
            Ok(readiness) => readiness,
            Err(error) => {
                eprintln!("codex-wrangler history wait failed: {error:#}");
                break;
            }
        };
        if readiness[1] {
            drain_wake(wake);
        }
        if readiness[2] {
            drain_wake(result_wake);
        }
        if !alive.load(Ordering::Acquire) {
            break;
        }

        let mut dirty = false;
        let mut repaint_demand = false;
        if readiness[0] {
            dirty = match historian.watchfire.reap() {
                Ok(flare) => flare.overflowed || !flare.paths.is_empty(),
                Err(error) => {
                    eprintln!("codex-wrangler history watch failed: {error:#}");
                    true
                }
            };
        }
        let (read_dirty, read_repaint) = drain_reads(&mut historian, results, transcripts);
        dirty |= read_dirty;
        repaint_demand |= read_repaint;
        let (intent_dirty, intent_repaint) =
            execute_intents(&mut historian, intents, read_tx, outcomes, transcripts);
        dirty |= intent_dirty;
        repaint_demand |= intent_repaint;
        let now = Instant::now();
        if now >= integrity_audit {
            dirty = true;
            integrity_audit = now + INTEGRITY_AUDIT;
        }
        if dirty {
            match historian.refresh() {
                Ok(()) => publish_changed(repaint, latest, &mut prior, historian.census()),
                Err(error) => publish_fault(repaint, latest, &error),
            }
        }
        if let Err(error) = historian.ledger.commit_due(now) {
            eprintln!("codex-wrangler could not seal its turn index: {error:#}");
        }
        if dirty || repaint_demand {
            let _repaint = repaint.request_repaint();
        }
    }
    if let Err(error) = historian.ledger.commit() {
        eprintln!("codex-wrangler could not seal its turn index: {error:#}");
    }
}

fn drain_reads(
    historian: &mut Historian,
    results: &Receiver<ReadResult>,
    transcripts: &Mutex<Vec<TranscriptOutcome>>,
) -> (bool, bool) {
    let mut dirty = false;
    let mut repaint = false;
    while let Ok(result) = results.try_recv() {
        match result {
            ReadResult::Count(result) => dirty |= historian.absorb(result),
            ReadResult::Transcript(outcome) => {
                transcripts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(outcome);
                repaint = true;
            }
        }
    }
    (dirty, repaint)
}

fn execute_intents(
    historian: &mut Historian,
    intents: &Receiver<Intent>,
    reader: &Sender<ReadJob>,
    outcomes: &Mutex<Vec<Outcome>>,
    transcripts: &Mutex<Vec<TranscriptOutcome>>,
) -> (bool, bool) {
    let mut dirty = false;
    let mut repaint = false;
    while let Ok(intent) = intents.try_recv() {
        match intent {
            Intent::Tally(threads) => historian.tally(threads, reader),
            Intent::Read(thread) => repaint |= historian.read(thread, reader, transcripts),
            Intent::Operate(order) => {
                let error = historian
                    .operate(&order)
                    .err()
                    .map(|error| format!("{error:#}"));
                outcomes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(Outcome { order, error });
                dirty = true;
            }
        }
    }
    (dirty, repaint)
}

fn wait_for_signal(
    watchfire: &Watchfire,
    wake: &UnixStream,
    result_wake: &UnixStream,
    timeout: Duration,
) -> Result<[bool; 3]> {
    let mut descriptors = [
        PollFd::new(watchfire.as_fd(), PollFlags::POLLIN),
        PollFd::new(wake.as_fd(), PollFlags::POLLIN),
        PollFd::new(result_wake.as_fd(), PollFlags::POLLIN),
    ];
    let timeout = PollTimeout::try_from(timeout).unwrap_or(PollTimeout::MAX);
    let _ready = poll(&mut descriptors, timeout).context("poll historical sources")?;
    Ok(descriptors.map(|descriptor| {
        descriptor
            .revents()
            .is_some_and(|events| events.contains(PollFlags::POLLIN))
    }))
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
            sessions: Vec::new(),
            fault: Some(format!("Could not inspect Codex history: {error:#}")),
        },
    );
}

fn publish(repaint: &NativeWake, latest: &Mutex<Option<Census>>, census: Census) {
    *latest
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(census);
    let _repaint = repaint.request_repaint();
}

struct Historian {
    home: PathBuf,
    db: Connection,
    names: NameIndex,
    sessions: Vec<Session>,
    artifacts: HashMap<String, Artifact>,
    requested: HashSet<String>,
    failed: HashSet<String>,
    ledger: TurnLedger,
    watchfire: Watchfire,
}

impl Historian {
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
        .context("open Codex historical index")?;
        Ok(Some(Self {
            home,
            db,
            names: NameIndex::default(),
            sessions: Vec::new(),
            artifacts: HashMap::new(),
            requested: HashSet::new(),
            failed: HashSet::new(),
            ledger: TurnLedger::restore()?,
            watchfire: Watchfire::kindle()?,
        }))
    }

    fn refresh(&mut self) -> Result<()> {
        self.names.refresh(&self.home.join("session_index.jsonl"))?;
        let mut statement = self.db.prepare(
            "SELECT id, NULLIF(TRIM(name), ''), updated_at_ms, archived, rollout_path, \
             COALESCE(strftime('%Y-%m-%d %H:%M', updated_at_ms / 1000, \
                               'unixepoch', 'localtime'), 'UNKNOWN') \
             FROM threads \
             WHERE source = 'cli' AND agent_role IS NULL \
               AND (thread_source = 'user' OR thread_source IS NULL)",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, bool>(3)?,
                PathBuf::from(row.get::<_, String>(4)?),
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut sessions = Vec::new();
        let mut artifacts = HashMap::new();
        for row in rows {
            let (thread, db_name, updated_at_ms, archived, nominal, last_turn) = row?;
            let Some(artifact) = resolve_artifact(&nominal, updated_at_ms) else {
                continue;
            };
            let turns = self.ledger.get(&thread, updated_at_ms);
            sessions.push(Session {
                name: db_name.or_else(|| self.names.get(&thread).map(str::to_owned)),
                tally_failed: turns.is_none() && self.failed.contains(&thread),
                bytes: fs::metadata(&artifact.path)?.len(),
                thread: thread.clone(),
                last_turn,
                updated_at_ms,
                turns,
                archived,
            });
            let _prior = artifacts.insert(thread, artifact);
        }
        sessions.sort_unstable_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.thread.cmp(&right.thread))
        });
        let artifact_ids = artifacts.keys().map(String::as_str).collect();
        self.ledger.retain(&artifact_ids);
        let watched = [
            self.home.join("session_index.jsonl"),
            self.home.join("state_5.sqlite"),
            self.home.join("state_5.sqlite-wal"),
            self.home.join("sessions"),
            self.home.join("archived_sessions"),
        ]
        .into_iter()
        .chain(artifacts.values().map(|artifact| artifact.path.clone()));
        self.watchfire.reconcile(watched)?;
        self.sessions = sessions;
        self.artifacts = artifacts;
        Ok(())
    }

    fn census(&self) -> Census {
        Census {
            sessions: self.sessions.clone(),
            fault: None,
        }
    }

    fn tally(&mut self, threads: Vec<String>, reader: &Sender<ReadJob>) {
        for thread in threads {
            let Some(artifact) = self.artifacts.get(&thread).cloned() else {
                continue;
            };
            if self.ledger.get(&thread, artifact.updated_at_ms).is_some()
                || self.failed.contains(&thread)
                || !self.requested.insert(thread.clone())
            {
                continue;
            }
            if reader
                .try_send(ReadJob::Count(CountJob {
                    thread: thread.clone(),
                    artifact,
                }))
                .is_err()
            {
                let _removed = self.requested.remove(&thread);
            }
        }
    }

    fn read(
        &self,
        thread: String,
        reader: &Sender<ReadJob>,
        transcripts: &Mutex<Vec<TranscriptOutcome>>,
    ) -> bool {
        let Some(artifact) = self.artifacts.get(&thread).cloned() else {
            transcripts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(TranscriptOutcome {
                    thread,
                    turns: Vec::new(),
                    error: Some("session payload vanished".to_owned()),
                });
            return true;
        };
        let job = TranscriptJob {
            thread: thread.clone(),
            artifact,
        };
        if reader.try_send(ReadJob::Transcript(job)).is_err() {
            transcripts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(TranscriptOutcome {
                    thread,
                    turns: Vec::new(),
                    error: Some("history reader is busy".to_owned()),
                });
            return true;
        }
        false
    }

    fn absorb(&mut self, result: CountResult) -> bool {
        let _removed = self.requested.remove(&result.thread);
        match result.tally {
            Ok(turns) => {
                let _removed = self.failed.remove(&result.thread);
                self.ledger
                    .record(result.thread.clone(), result.updated_at_ms, turns);
                if let Some(session) = self.sessions.iter_mut().find(|session| {
                    session.thread == result.thread && session.updated_at_ms == result.updated_at_ms
                }) {
                    session.turns = Some(turns);
                    session.tally_failed = false;
                    return true;
                }
                false
            }
            Err(error) => {
                eprintln!(
                    "codex-wrangler could not count turns for {}: {error}",
                    result.thread
                );
                let _new = self.failed.insert(result.thread.clone());
                if let Some(session) = self
                    .sessions
                    .iter_mut()
                    .find(|session| session.thread == result.thread)
                {
                    session.tally_failed = true;
                    return true;
                }
                false
            }
        }
    }

    fn operate(&mut self, order: &Order) -> Result<()> {
        match order {
            Order::Archive(thread) => self.archive(thread),
            Order::Unarchive(thread) => self.unarchive(thread),
            Order::Delete(thread) => self.delete(thread),
            Order::Rename { thread, name } => self.rename(thread, name),
        }
    }

    fn rename(&self, thread: &str, name: &str) -> Result<()> {
        let (archived, _) = self.row(thread)?.context("historical session vanished")?;
        anyhow::ensure!(!archived, "archived sessions cannot be renamed");
        anyhow::ensure!(!name.trim().is_empty(), "session name must not be empty");
        CodexRpc::open(&self.home)?.rename_thread(thread, name)
    }

    fn archive(&self, thread: &str) -> Result<()> {
        let (archived, _) = self.row(thread)?.context("historical session vanished")?;
        if archived {
            bail!("session `{thread}` is already archived");
        }
        run_codex(&self.home, &["archive", thread])?;
        let (archived, nominal) = self
            .row(thread)?
            .context("Codex archive removed the session index row")?;
        if !archived {
            bail!("Codex did not mark session `{thread}` archived");
        }
        compress(&nominal)
    }

    fn unarchive(&self, thread: &str) -> Result<()> {
        let (archived, nominal) = self.row(thread)?.context("historical session vanished")?;
        if !archived {
            bail!("session `{thread}` is not archived");
        }
        prepare_resume(&self.home, thread, archived, &nominal)
    }

    fn delete(&self, thread: &str) -> Result<()> {
        let (_, nominal) = self.row(thread)?.context("historical session vanished")?;
        let materialized = materialize(&nominal)?;
        let result = run_codex(&self.home, &["delete", "--force", thread]);
        if result.is_err() && materialized && self.row(thread)?.is_some() {
            let _removed = fs::remove_file(&nominal);
        }
        result?;
        remove_compressed(&nominal)
    }

    fn row(&self, thread: &str) -> Result<Option<(bool, PathBuf)>> {
        self.db
            .query_row(
                "SELECT archived, rollout_path FROM threads WHERE id = ?1",
                params![thread],
                |row| Ok((row.get(0)?, PathBuf::from(row.get::<_, String>(1)?))),
            )
            .optional()
            .with_context(|| format!("query historical session `{thread}`"))
    }
}

pub(crate) fn prepare_resume(
    home: &Path,
    thread: &str,
    archived: bool,
    nominal: &Path,
) -> Result<()> {
    let _materialized = materialize(nominal)?;
    if archived && let Err(error) = run_codex(home, &["unarchive", thread]) {
        return Err(error);
    }
    remove_compressed(nominal)
}

fn resolve_artifact(nominal: &Path, updated_at_ms: i64) -> Option<Artifact> {
    if nominal.is_file() {
        return Some(Artifact {
            path: nominal.to_owned(),
            nominal: nominal.to_owned(),
            compressed: false,
            updated_at_ms,
        });
    }
    let compressed = compressed_path(nominal);
    compressed.is_file().then_some(Artifact {
        path: compressed,
        nominal: nominal.to_owned(),
        compressed: true,
        updated_at_ms,
    })
}

fn compressed_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".zst");
    PathBuf::from(name)
}

fn temporary_path(path: &Path, purpose: &str) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("session artifact has no UTF-8 filename")?;
    Ok(path.with_file_name(format!(".{name}.{purpose}.tmp")))
}

fn compress(nominal: &Path) -> Result<()> {
    if !nominal.is_file() {
        bail!("Codex archived payload `{}` is absent", nominal.display());
    }
    let destination = compressed_path(nominal);
    let temporary = temporary_path(&destination, "compress")?;
    run_zstd(&["-q", "-T1", "-f"], nominal, &temporary)?;
    seal_artifact(&temporary, &destination)?;
    fs::remove_file(nominal)
        .with_context(|| format!("retire uncompressed payload `{}`", nominal.display()))?;
    sync_parent(nominal)
}

fn materialize(nominal: &Path) -> Result<bool> {
    if nominal.is_file() {
        return Ok(false);
    }
    let source = compressed_path(nominal);
    if !source.is_file() {
        bail!("session payload `{}` is absent", nominal.display());
    }
    let temporary = temporary_path(nominal, "inflate")?;
    run_zstd(&["-q", "-d", "-f"], &source, &temporary)?;
    seal_artifact(&temporary, nominal)?;
    Ok(true)
}

fn run_zstd(options: &[&str], source: &Path, destination: &Path) -> Result<()> {
    let mut command = Command::new("nice");
    command.args(["-n", "15", "zstd"]);
    command.args(options);
    let output = command
        .arg(source)
        .arg("-o")
        .arg(destination)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("transcode `{}`", source.display()))?;
    if !output.status.success() {
        bail!(
            "zstd rejected `{}`: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn seal_artifact(temporary: &Path, destination: &Path) -> Result<()> {
    fs::set_permissions(temporary, fs::Permissions::from_mode(0o600))?;
    File::open(temporary)?.sync_all()?;
    fs::rename(temporary, destination)
        .with_context(|| format!("publish session artifact `{}`", destination.display()))?;
    sync_parent(destination)
}

fn remove_compressed(nominal: &Path) -> Result<()> {
    let compressed = compressed_path(nominal);
    match fs::remove_file(&compressed) {
        Ok(()) => sync_parent(&compressed),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove `{}`", compressed.display())),
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("session artifact has no parent")?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("seal `{}`", parent.display()))
}

fn run_codex(home: &Path, arguments: &[&str]) -> Result<()> {
    let output = Command::new("codex")
        .args(arguments)
        .env("CODEX_HOME", home)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run `codex {}`", arguments.join(" ")))?;
    if !output.status.success() {
        let detail = if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        };
        bail!(
            "`codex {}` failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(detail).trim()
        );
    }
    Ok(())
}

fn read_history(
    jobs: &Receiver<ReadJob>,
    results: &Sender<ReadResult>,
    wake: &UnixStream,
    alive: &AtomicBool,
) {
    while alive.load(Ordering::Acquire) {
        let Ok(job) = jobs.recv_timeout(Duration::from_millis(250)) else {
            continue;
        };
        let result = match job {
            ReadJob::Count(job) => ReadResult::Count(CountResult {
                thread: job.thread,
                updated_at_ms: job.artifact.updated_at_ms,
                tally: tally(&job.artifact, alive).map_err(|error| format!("{error:#}")),
            }),
            ReadJob::Transcript(job) => {
                let (turns, error) = match read_transcript(&job.artifact, alive) {
                    Ok(turns) => (turns, None),
                    Err(error) => (Vec::new(), Some(format!("{error:#}"))),
                };
                ReadResult::Transcript(TranscriptOutcome {
                    thread: job.thread,
                    turns,
                    error,
                })
            }
        };
        if results.send(result).is_err() {
            break;
        }
        let _woken = (&*wake).write_all(&[0]);
    }
}

fn tally(artifact: &Artifact, alive: &AtomicBool) -> Result<u64> {
    read_artifact(artifact, alive, |reader| scan(reader, alive))
}

fn read_transcript(artifact: &Artifact, alive: &AtomicBool) -> Result<Vec<Turn>> {
    read_artifact(artifact, alive, |reader| parse_turns(reader, alive))
}

fn read_artifact<T>(
    artifact: &Artifact,
    alive: &AtomicBool,
    consume: impl FnOnce(&mut dyn Read) -> Result<T>,
) -> Result<T> {
    debug_assert_eq!(artifact.nominal == artifact.path, !artifact.compressed);
    if !artifact.compressed {
        return consume(&mut File::open(&artifact.path)?);
    }
    let mut child = Command::new("nice")
        .args(["-n", "15", "zstd", "-q", "-d", "-c"])
        .arg(&artifact.path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("open compressed session `{}`", artifact.path.display()))?;
    let mut stdout = child.stdout.take().context("open zstd output")?;
    let result = consume(&mut stdout);
    if !alive.load(Ordering::Acquire) {
        let _killed = child.kill();
    }
    let status = child.wait().context("wait for zstd turn scan")?;
    if !status.success() && alive.load(Ordering::Acquire) {
        bail!("zstd could not read `{}`", artifact.path.display());
    }
    result
}

fn parse_turns(reader: &mut dyn Read, alive: &AtomicBool) -> Result<Vec<Turn>> {
    let mut turns = Vec::new();
    for line in BufReader::new(reader).lines() {
        if !alive.load(Ordering::Acquire) {
            bail!("transcript scan cancelled");
        }
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let payload = &event["payload"];
        match payload.get("type").and_then(Value::as_str) {
            Some("user_message") => {
                if let Some(message) = payload.get("message").and_then(Value::as_str) {
                    push_user(&mut turns, message);
                }
            }
            Some("agent_message") => {
                if let Some(message) = payload.get("message").and_then(Value::as_str) {
                    assign_model(&mut turns, message);
                }
            }
            Some("item_completed") => absorb_completed_item(&mut turns, &payload["item"]),
            Some("task_complete" | "turn_complete") => {
                if let Some(message) = payload.get("last_agent_message").and_then(Value::as_str) {
                    assign_model(&mut turns, message);
                }
            }
            _ => {}
        }
    }
    Ok(turns)
}

fn absorb_completed_item(turns: &mut Vec<Turn>, item: &Value) {
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return;
    };
    let message = content
        .iter()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("text") => part.get("text").and_then(Value::as_str),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    match item.get("type").and_then(Value::as_str) {
        Some("UserMessage") => push_user(turns, &message),
        Some("AgentMessage") => assign_model(turns, &message),
        _ => {}
    }
}

fn push_user(turns: &mut Vec<Turn>, message: &str) {
    let message = message.trim();
    if message.is_empty()
        || turns
            .last()
            .is_some_and(|turn| turn.model.is_empty() && turn.user == message)
    {
        return;
    }
    turns.push(Turn {
        user: message.to_owned(),
        model: String::new(),
    });
}

fn assign_model(turns: &mut Vec<Turn>, message: &str) {
    let message = message.trim();
    if message.is_empty() {
        return;
    }
    if turns.is_empty() {
        turns.push(Turn {
            user: String::new(),
            model: message.to_owned(),
        });
    } else if let Some(turn) = turns.last_mut() {
        message.clone_into(&mut turn.model);
    }
}

fn scan(mut reader: impl Read, alive: &AtomicBool) -> Result<u64> {
    let overlap = TASK_STARTED.len().max(TURN_STARTED.len()) - 1;
    let mut buffer = vec![0_u8; SCAN_BLOCK + overlap];
    let mut carry = 0;
    let mut turns = 0_u64;
    loop {
        if !alive.load(Ordering::Acquire) {
            bail!("turn scan cancelled");
        }
        let read = reader.read(&mut buffer[carry..])?;
        if read == 0 {
            break;
        }
        let length = carry + read;
        let bytes = &buffer[..length];
        for needle in [TASK_STARTED, TURN_STARTED] {
            turns += memmem::find_iter(bytes, needle)
                .filter(|start| start + needle.len() > carry)
                .count() as u64;
        }
        carry = length.min(overlap);
        buffer.copy_within(length - carry..length, 0);
    }
    Ok(turns)
}

#[derive(Clone, Deserialize, Serialize)]
struct TurnStamp {
    updated_at_ms: i64,
    turns: u64,
}

#[derive(Default, Deserialize, Serialize)]
struct TurnState {
    version: u8,
    sessions: BTreeMap<String, TurnStamp>,
}

struct TurnLedger {
    path: PathBuf,
    sessions: BTreeMap<String, TurnStamp>,
    dirty: bool,
    settle_at: Option<Instant>,
}

impl TurnLedger {
    fn restore() -> Result<Self> {
        let path = state::path(INDEX_FILE)?;
        let sessions = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<TurnState>(&bytes) {
                Ok(state) if state.version == INDEX_VERSION => state.sessions,
                Ok(_) => BTreeMap::new(),
                Err(error) => {
                    eprintln!(
                        "codex-wrangler discarded invalid turn index `{}`: {error}",
                        path.display()
                    );
                    BTreeMap::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error).with_context(|| format!("read `{}`", path.display())),
        };
        Ok(Self {
            path,
            sessions,
            dirty: false,
            settle_at: None,
        })
    }

    fn get(&self, thread: &str, updated_at_ms: i64) -> Option<u64> {
        self.sessions
            .get(thread)
            .filter(|stamp| stamp.updated_at_ms == updated_at_ms)
            .map(|stamp| stamp.turns)
    }

    fn record(&mut self, thread: String, updated_at_ms: i64, turns: u64) {
        let stamp = TurnStamp {
            updated_at_ms,
            turns,
        };
        if self.sessions.get(&thread).is_none_or(|prior| {
            prior.updated_at_ms != stamp.updated_at_ms || prior.turns != stamp.turns
        }) {
            let _prior = self.sessions.insert(thread, stamp);
            self.dirty = true;
            self.settle_at = Some(Instant::now() + LEDGER_SETTLE);
        }
    }

    fn retain(&mut self, live: &HashSet<&str>) {
        let before = self.sessions.len();
        self.sessions
            .retain(|thread, _| live.contains(thread.as_str()));
        if self.sessions.len() != before {
            self.dirty = true;
            self.settle_at = Some(Instant::now() + LEDGER_SETTLE);
        }
    }

    const fn deadline(&self) -> Option<Instant> {
        self.settle_at
    }

    fn commit_due(&mut self, now: Instant) -> Result<()> {
        if self.settle_at.is_some_and(|deadline| deadline <= now) {
            self.commit()?;
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&TurnState {
            version: INDEX_VERSION,
            sessions: self.sessions.clone(),
        })?;
        state::seal(&self.path, &bytes)?;
        self.dirty = false;
        self.settle_at = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[test]
    fn turn_counter_crosses_blocks_without_counting_quoted_examples() {
        let padding = "x".repeat(SCAN_BLOCK - 24);
        let transcript = format!(
            "{padding}{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}\n\
             {{\"type\":\"response_item\",\"payload\":{{\"text\":\"\\\"type\\\":\\\"event_msg\\\",\\\"payload\\\":{{\\\"type\\\":\\\"turn_started\\\"}}\"}}}}\n\
             {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"turn_started\"}}}}\n"
        );
        assert_eq!(
            scan(transcript.as_bytes(), &AtomicBool::new(true),).expect("count turns"),
            2
        );
    }
}
