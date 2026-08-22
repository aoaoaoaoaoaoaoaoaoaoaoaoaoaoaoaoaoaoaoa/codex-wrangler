//! Volatile Linux process truth for interactive Codex sessions.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use uuid::Uuid;

const PROC: &str = "/proc";
const MAX_ANCESTORS: usize = 256;

/// One durable Codex session identity.
pub type SessionId = Uuid;

/// One PID incarnation, immune to PID reuse.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProcessKey {
    /// Kernel process identifier.
    pub pid: u32,
    start_ticks: u64,
}

impl ProcessKey {
    /// Observe the current incarnation of `pid`.
    pub fn sight(pid: u32) -> Result<Self> {
        Ok(Self {
            pid,
            start_ticks: process_stat(pid)?.start_ticks,
        })
    }

    /// Construct a key from already observed kernel fields.
    #[must_use]
    pub const fn from_parts(pid: u32, start_ticks: u64) -> Self {
        Self { pid, start_ticks }
    }

    /// Kernel start-time ticks that complete this process identity.
    #[must_use]
    pub const fn start_ticks(self) -> u64 {
        self.start_ticks
    }

    /// Whether this exact process incarnation still exists.
    #[must_use]
    pub fn alive(self) -> bool {
        process_stat(self.pid).is_ok_and(|stat| stat.start_ticks == self.start_ticks)
    }
}

/// One live top-level Codex process holding a session's writer lock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Seat {
    /// Durable session identity.
    pub session: SessionId,
    /// Ephemeral process identity.
    pub process: ProcessKey,
    /// Current process working directory when readable.
    pub cwd: Option<PathBuf>,
}

/// A point-in-time projection of unambiguous live Codex seats.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Census {
    seats: BTreeMap<SessionId, Seat>,
    conflicts: BTreeSet<SessionId>,
}

impl Census {
    /// Scan the host process table without retaining lifecycle state.
    pub fn scan() -> Result<Self> {
        let mut census = Self::default();
        for entry in fs::read_dir(PROC)
            .context("read Linux process table")?
            .flatten()
        {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            else {
                continue;
            };
            let Some(seat) = inspect(pid) else {
                continue;
            };
            if census.conflicts.contains(&seat.session) {
                continue;
            }
            if census.seats.insert(seat.session, seat.clone()).is_some() {
                census.seats.remove(&seat.session);
                census.conflicts.insert(seat.session);
            }
        }
        Ok(census)
    }

    /// The sole live seat for a session, excluding ambiguous ownership.
    #[must_use]
    pub fn seat(&self, session: &SessionId) -> Option<&Seat> {
        self.seats.get(session)
    }

    /// All unambiguous seats in session order.
    pub fn seats(&self) -> impl Iterator<Item = &Seat> {
        self.seats.values()
    }

    /// Session identities claimed by more than one eligible process.
    pub fn conflicts(&self) -> impl Iterator<Item = &SessionId> {
        self.conflicts.iter()
    }
}

/// Whether `pid` is an outermost foreground interactive Codex process.
#[must_use]
pub fn is_top_level_codex(pid: u32) -> bool {
    let Ok(stat) = process_stat(pid) else {
        return false;
    };
    stat.tty != 0
        && stat.process_group == stat.foreground_group
        && stdio_is_terminal(pid)
        && is_codex_program(pid)
        && !has_codex_ancestor(stat.parent)
}

/// Writer-lock session claims currently held open by `pid`.
#[must_use]
pub fn writer_lock_sessions(pid: u32) -> Vec<SessionId> {
    let mut sessions = writer_lock_claims(pid)
        .into_iter()
        .map(|(_descriptor, session)| session)
        .collect::<Vec<_>>();
    sessions.sort_unstable();
    sessions.dedup();
    sessions
}

/// Primary session asserted by an eligible top-level Codex process.
#[must_use]
pub fn session(pid: u32) -> Option<SessionId> {
    is_top_level_codex(pid)
        .then(|| primary_session(pid))
        .flatten()
}

fn inspect(pid: u32) -> Option<Seat> {
    let session = session(pid)?;
    Some(Seat {
        session,
        process: ProcessKey::sight(pid).ok()?,
        cwd: fs::read_link(proc_path(pid).join("cwd")).ok(),
    })
}

fn primary_session(pid: u32) -> Option<SessionId> {
    let claims = writer_lock_claims(pid);
    let resumed = command_arguments(pid)
        .windows(2)
        .find(|pair| pair[0] == b"resume")
        .and_then(|pair| std::str::from_utf8(&pair[1]).ok())
        .and_then(|argument| Uuid::parse_str(argument).ok());
    if let Some(session) = resumed
        && (claims.is_empty() || claims.iter().any(|(_descriptor, claim)| *claim == session))
    {
        return Some(session);
    }
    claims.first().map(|(_descriptor, session)| *session)
}

fn writer_lock_claims(pid: u32) -> Vec<(u32, SessionId)> {
    let mut claims = fs::read_dir(proc_path(pid).join("fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let descriptor = entry.file_name().to_str()?.parse().ok()?;
            let target = fs::read_link(entry.path()).ok()?;
            Some((descriptor, writer_lock_session(&target)?))
        })
        .collect::<Vec<_>>();
    claims.sort_unstable_by_key(|(descriptor, _session)| *descriptor);
    claims.dedup_by_key(|(_descriptor, session)| *session);
    claims
}

fn command_arguments(pid: u32) -> Vec<Vec<u8>> {
    fs::read(proc_path(pid).join("cmdline"))
        .unwrap_or_default()
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

fn has_codex_ancestor(mut pid: u32) -> bool {
    for _ in 0..MAX_ANCESTORS {
        if pid <= 1 {
            return false;
        }
        if is_codex_program(pid) {
            return true;
        }
        let Ok(stat) = process_stat(pid) else {
            return false;
        };
        pid = stat.parent;
    }
    true
}

fn is_codex_program(pid: u32) -> bool {
    let Ok(bytes) = fs::read(proc_path(pid).join("cmdline")) else {
        return false;
    };
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    Path::new(OsStr::from_bytes(&bytes[..end])).file_name() == Some(OsStr::new("codex"))
}

fn stdio_is_terminal(pid: u32) -> bool {
    ["0", "1", "2"].into_iter().all(|fd| {
        fs::read_link(proc_path(pid).join("fd").join(fd))
            .is_ok_and(|target| target.starts_with("/dev/pts/"))
    })
}

fn writer_lock_session(path: &Path) -> Option<SessionId> {
    if path.parent()?.file_name()? != OsStr::new("thread-writer-locks") {
        return None;
    }
    Uuid::parse_str(path.file_stem()?.to_str()?).ok()
}

#[derive(Clone, Copy)]
struct ProcessStat {
    parent: u32,
    process_group: i32,
    tty: i64,
    foreground_group: i32,
    start_ticks: u64,
}

fn process_stat(pid: u32) -> Result<ProcessStat> {
    let text = fs::read_to_string(proc_path(pid).join("stat"))
        .with_context(|| format!("read identity of process {pid}"))?;
    parse_stat(&text).context("malformed Linux process stat")
}

fn parse_stat(text: &str) -> Option<ProcessStat> {
    let fields = text
        .rsplit_once(") ")?
        .1
        .split_ascii_whitespace()
        .collect::<Vec<_>>();
    Some(ProcessStat {
        parent: fields.get(1)?.parse().ok()?,
        process_group: fields.get(2)?.parse().ok()?,
        tty: fields.get(4)?.parse().ok()?,
        foreground_group: fields.get(5)?.parse().ok()?,
        start_ticks: fields.get(19)?.parse().ok()?,
    })
}

fn proc_path(pid: u32) -> PathBuf {
    Path::new(PROC).join(pid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_identity_parser_ignores_hostile_parentheses_in_comm() {
        let stat = parse_stat(
            "42 (a ) hostile name) S 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26",
        );
        let Some(stat) = stat else {
            panic!("fixture must be a lawful process stat");
        };
        assert_eq!(stat.parent, 7);
        assert_eq!(stat.process_group, 8);
        assert_eq!(stat.tty, 10);
        assert_eq!(stat.foreground_group, 11);
        assert_eq!(stat.start_ticks, 25);
    }
}
