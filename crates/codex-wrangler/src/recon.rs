use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs,
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};

use crate::{
    contract::{Harness, Work},
    desktop::Desktop,
    model::{Card, Census, snip},
    rollout::Rollouts,
    transcript::Transcripts,
};

const REFRESH: Duration = Duration::from_millis(800);

#[derive(Clone, Copy, Debug)]
pub enum Strike {
    Activate(u32),
}

pub struct Nexus {
    pub census: Receiver<Census>,
    pub strike: Sender<Strike>,
}

pub fn spawn(ctx: egui::Context) -> Nexus {
    let (census_tx, census) = bounded(1);
    let (strike, strike_rx) = bounded(16);
    let _thread = std::thread::Builder::new()
        .name("codex-wrangler-recon".to_owned())
        .spawn(move || raid(&ctx, &census_tx, &strike_rx));
    Nexus { census, strike }
}

fn raid(ctx: &egui::Context, census: &Sender<Census>, strikes: &Receiver<Strike>) {
    let mut recon = Recon::raise();
    loop {
        while let Ok(strike) = strikes.try_recv() {
            if let (Ok(recon), Strike::Activate(window)) = (&mut recon, strike)
                && let Err(error) = recon.desktop.activate(window)
            {
                eprintln!("codex-wrangler could not activate window {window}: {error:#}");
            }
        }
        let next = match &mut recon {
            Ok(recon) => recon.census().unwrap_or_else(|error| Census {
                cards: Vec::new(),
                fault: Some(format!("Could not inspect harnesses: {error:#}")),
            }),
            Err(error) => Census {
                cards: Vec::new(),
                fault: Some(format!("Could not arm reconnaissance: {error:#}")),
            },
        };
        let _published = census.try_send(next);
        ctx.request_repaint();
        match strikes.recv_timeout(REFRESH) {
            Ok(strike) => {
                if let (Ok(recon), Strike::Activate(window)) = (&mut recon, strike)
                    && let Err(error) = recon.desktop.activate(window)
                {
                    eprintln!("codex-wrangler could not activate window {window}: {error:#}");
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
}

struct Recon {
    codex: Option<Codex>,
    desktop: Desktop,
    transcripts: Transcripts,
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
            transcripts: Transcripts::default(),
        })
    }

    fn census(&mut self) -> Result<Census> {
        if let Some(codex) = &mut self.codex {
            codex.refresh_names()?;
        }
        let windows = self.desktop.windows_by_pid()?;
        let workspaces = self
            .desktop
            .workspace_numbers(windows.values().copied())
            .unwrap_or_default();
        let mut processes = manual_harnesses()?;
        processes.sort_by_key(|process| std::cmp::Reverse(process.pid));
        let mut locator = SessionLocator::default();
        let mut cards = Vec::new();
        let mut seen = HashSet::new();
        for process in processes {
            let Some(terminal_pid) = alacritty_ancestor(process.pid)? else {
                continue;
            };
            let Some(&window) = windows.get(&terminal_pid) else {
                continue;
            };
            let workspace = workspaces.get(&window).copied();
            let card = match process.harness {
                Harness::Codex => match &mut self.codex {
                    Some(codex) => codex.card(&process, window, workspace)?,
                    None => None,
                },
                Harness::ClaudeCode | Harness::PrimeAgent => Some(foreign_card(
                    &mut self.transcripts,
                    &mut locator,
                    &process,
                    window,
                    workspace,
                )),
            };
            let Some(card) = card else {
                continue;
            };
            if seen.insert((card.harness, card.thread.clone())) {
                cards.push(card);
            }
        }
        cards.sort();
        Ok(Census { cards, fault: None })
    }
}

struct Codex {
    home: PathBuf,
    db: Connection,
    goals: Option<Connection>,
    names: NameIndex,
    rollouts: Rollouts,
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
        }))
    }

    fn refresh_names(&mut self) -> Result<()> {
        self.names.refresh(&self.home.join("session_index.jsonl"))
    }

    fn card(
        &mut self,
        process: &Process,
        window: u32,
        workspace: Option<u32>,
    ) -> Result<Option<Card>> {
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
        Ok(Some(Card {
            harness: Harness::Codex,
            thread: thread.id,
            name,
            cwd: compact_path(&thread.cwd, process.home.as_deref()),
            tile_preview: snip(&summary.preview, 280),
            work,
            window,
            workspace,
            updated_at_ms: thread.updated_at_ms,
        }))
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
) -> Card {
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
    Card {
        harness: process.harness,
        thread,
        name: process
            .explicit_name()
            .or_else(|| summary.as_ref().and_then(|summary| summary.name.clone())),
        cwd: compact_path(cwd, process.home.as_deref()),
        tile_preview: summary
            .as_ref()
            .map_or_else(String::new, |summary| snip(&summary.preview, 280)),
        work: if process.goal && work == Work::Turn {
            Work::Goal
        } else {
            work
        },
        window,
        workspace,
        updated_at_ms: summary
            .as_ref()
            .map_or_else(|| i64::from(process.pid), |summary| summary.updated_at_ms),
    }
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

struct Process {
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

fn manual_harnesses() -> Result<Vec<Process>> {
    let mut processes = Vec::new();
    for entry in fs::read_dir("/proc").context("enumerate processes")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let root = entry.path();
        let Ok(argv) = fs::read(root.join("cmdline")) else {
            continue;
        };
        let argv = argv
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| OsString::from_vec(arg.to_vec()))
            .collect::<Vec<_>>();
        let Some(harness) = harness_argv(&argv) else {
            continue;
        };
        if !foreground_tty(&root) {
            continue;
        }
        let environment = process_environment(&root);
        let home = environment
            .get("HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        let cwd = fs::read_link(root.join("cwd")).unwrap_or_else(|_| PathBuf::from("."));
        let goal = harness == Harness::PrimeAgent && has_option(&argv, "--goal");
        processes.push(Process {
            pid,
            harness,
            argv,
            transcripts: open_jsonls(&root),
            cwd,
            environment,
            home,
            goal,
        });
    }
    Ok(processes)
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
    fs::read_dir(root.join("fd"))
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
        .collect()
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

fn alacritty_ancestor(mut pid: u32) -> Result<Option<u32>> {
    let mut seen = HashSet::new();
    while pid > 1 && seen.insert(pid) {
        let root = PathBuf::from(format!("/proc/{pid}"));
        if fs::read_link(root.join("exe"))
            .ok()
            .and_then(|path| path.file_name().map(OsStr::to_owned))
            .as_deref()
            == Some(OsStr::new("alacritty"))
        {
            return Ok(Some(pid));
        }
        pid = parent_pid(&root)?.unwrap_or_default();
    }
    Ok(None)
}

fn parent_pid(root: &Path) -> Result<Option<u32>> {
    let status = fs::read_to_string(root.join("status"))?;
    Ok(status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|value| value.trim().parse().ok()))
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
