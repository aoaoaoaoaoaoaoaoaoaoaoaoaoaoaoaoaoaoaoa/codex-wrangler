use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs,
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, bounded};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};

use crate::{
    desktop::Desktop,
    model::{Census, CodexCard, snip},
    rollout::Rollouts,
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
                fault: Some(format!("Could not inspect Codex: {error:#}")),
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
    codex_home: PathBuf,
    db: Connection,
    desktop: Desktop,
    rollouts: Rollouts,
    names: NameIndex,
}

impl Recon {
    fn raise() -> Result<Self> {
        let codex_home = std::env::var_os("CODEX_HOME").map_or_else(
            || {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".codex"))
                    .ok_or_else(|| anyhow!("neither CODEX_HOME nor HOME is set"))
            },
            |home| Ok(PathBuf::from(home)),
        )?;
        let db = Connection::open_with_flags(
            codex_home.join("state_5.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .context("open Codex thread index")?;
        Ok(Self {
            codex_home,
            db,
            desktop: Desktop::connect()?,
            rollouts: Rollouts::default(),
            names: NameIndex::default(),
        })
    }

    fn census(&mut self) -> Result<Census> {
        self.names
            .refresh(&self.codex_home.join("session_index.jsonl"))?;
        let windows = self.desktop.windows_by_pid()?;
        let workspaces = self
            .desktop
            .workspace_numbers(windows.values().copied())
            .unwrap_or_default();
        let mut cards = Vec::new();
        let mut seen = HashSet::new();
        for process in manual_codexes()? {
            let Some(terminal_pid) = alacritty_ancestor(process.pid)? else {
                continue;
            };
            let Some(&window) = windows.get(&terminal_pid) else {
                continue;
            };
            let Some(thread) = self.current_thread(&process.rollouts)? else {
                continue;
            };
            if !seen.insert(thread.id.clone()) {
                continue;
            }
            let summary = self
                .rollouts
                .read(&thread.rollout)
                .with_context(|| format!("read rollout `{}`", thread.rollout.display()))?;
            let name = thread
                .name
                .or_else(|| self.names.get(&thread.id).map(str::to_owned));
            cards.push(CodexCard {
                thread: thread.id,
                name,
                cwd: compact_path(&thread.cwd),
                tile_preview: snip(&summary.preview, 280),
                work: summary.work,
                window,
                workspace: workspaces.get(&window).copied(),
                updated_at_ms: thread.updated_at_ms,
            });
        }
        cards.sort();
        Ok(Census { cards, fault: None })
    }

    fn current_thread(&self, paths: &[PathBuf]) -> Result<Option<Thread>> {
        let mut freshest = None;
        for rollout in paths {
            if !rollout.starts_with(self.codex_home.join("sessions")) {
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

fn compact_path(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    let Some(home) = std::env::var_os("HOME") else {
        return text;
    };
    let home = Path::new(&home);
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
    rollouts: Vec<PathBuf>,
}

fn manual_codexes() -> Result<Vec<Process>> {
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
        if !manual_argv(&argv) || !foreground_tty(&root) {
            continue;
        }
        let rollouts = open_rollouts(&root);
        if !rollouts.is_empty() {
            processes.push(Process { pid, rollouts });
        }
    }
    Ok(processes)
}

fn manual_argv(argv: &[OsString]) -> bool {
    let Some(program) = argv.first().and_then(|arg| Path::new(arg).file_name()) else {
        return false;
    };
    program == OsStr::new("codex") && !argv.iter().skip(1).any(|arg| arg == OsStr::new("exec"))
}

fn foreground_tty(root: &Path) -> bool {
    ["0", "1", "2"].iter().all(|fd| {
        fs::read_link(root.join("fd").join(fd)).is_ok_and(|target| target.starts_with("/dev/pts/"))
    })
}

fn open_rollouts(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root.join("fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .filter(|target| {
            let rollout = target
                .file_stem()
                .and_then(OsStr::to_str)
                .is_some_and(|stem| stem.starts_with("rollout-"));
            let jsonl = target
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"));
            rollout && jsonl
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
    fn admits_tui_and_resume_but_beheads_exec() {
        assert!(manual_argv(&[OsString::from("codex")]));
        assert!(manual_argv(&[
            OsString::from("/usr/bin/codex"),
            OsString::from("resume")
        ]));
        assert!(!manual_argv(&[
            OsString::from("codex"),
            OsString::from("exec"),
            OsString::from("do it")
        ]));
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
