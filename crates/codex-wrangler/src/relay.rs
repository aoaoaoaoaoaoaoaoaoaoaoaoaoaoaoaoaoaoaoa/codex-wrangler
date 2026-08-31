use std::{
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use nix::{
    errno::Errno,
    sys::signal::{Signal, killpg},
    unistd::Pid,
};

use crate::site::{RemoteSite, SessionKey, Site};

const ENV: &str = "/usr/bin/env";
const TRUECOLOR: &str = "COLORTERM=truecolor";
const WRANGLER: &str = "$HOME/.local/bin/codex-wrangler";
const WRITER_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
pub(crate) enum RelayOperation {
    Resume,
    Fork,
}

impl RelayOperation {
    const fn verb(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Fork => "fork",
        }
    }
}

pub(crate) fn ssh_argv(
    site: &RemoteSite,
    operation: RelayOperation,
    thread: &str,
) -> Vec<OsString> {
    [
        "ssh",
        "-t",
        "--",
        site.endpoint(),
        ENV,
        TRUECOLOR,
        WRANGLER,
        "relay",
        operation.verb(),
        thread,
    ]
    .map(OsString::from)
    .into()
}

pub(crate) fn resumed_session(argv: &[OsString]) -> Option<SessionKey> {
    if Path::new(argv.first()?).file_name()? != OsStr::new("ssh") {
        return None;
    }
    let separator = argv.iter().rposition(|arg| arg == OsStr::new("--"))?;
    let [endpoint, command @ ..] = argv.get(separator + 1..)? else {
        return None;
    };
    // Terminal processes outlive Wrangler upgrades. The shorter arm admits
    // relays opened before Wrangler began declaring truecolor explicitly.
    let command = match command {
        [env, truecolor, command @ ..]
            if Path::new(env).file_name()? == OsStr::new("env")
                && truecolor == OsStr::new(TRUECOLOR) =>
        {
            command
        }
        command => command,
    };
    let [wrangler, relay, resume, thread] = command else {
        return None;
    };
    if Path::new(wrangler).file_name()? != OsStr::new("codex-wrangler")
        || relay != OsStr::new("relay")
        || resume != OsStr::new("resume")
    {
        return None;
    }
    let thread = thread.to_str().filter(|thread| uuid_literal(thread))?;
    let site = RemoteSite::parse(endpoint.to_str()?).ok()?;
    Some(SessionKey::new(Site::Remote(site), thread.to_owned()))
}

pub(crate) fn retire_superseded_writer(home: &Path, thread: &str) -> Result<bool> {
    anyhow::ensure!(uuid_literal(thread), "invalid relay thread ID `{thread}`");
    let lock = home
        .join("thread-writer-locks")
        .join(format!("{thread}.lock"));
    if !writer_locked(&lock)? {
        return Ok(false);
    }
    let Some(pid) = superseded_tui_writer(&lock) else {
        return Ok(false);
    };
    signal_group(pid, Signal::SIGTERM)?;
    if wait_for_writer(&lock, WRITER_GRACE)? {
        return Ok(true);
    }
    signal_group(pid, Signal::SIGKILL)?;
    anyhow::ensure!(
        wait_for_writer(&lock, WRITER_GRACE)?,
        "superseded Codex writer {pid} retained thread `{thread}`"
    );
    Ok(true)
}

fn superseded_tui_writer(lock: &Path) -> Option<u32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .find(|pid| superseded_tui(&PathBuf::from(format!("/proc/{pid}")), *pid, lock))
}

fn superseded_tui(root: &Path, pid: u32, lock: &Path) -> bool {
    let Some(executable) = fs::read_link(root.join("exe")).ok() else {
        return false;
    };
    if !executable.as_os_str().as_bytes().ends_with(b" (deleted)") {
        return false;
    }
    let Some(arguments) = fs::read(root.join("cmdline")).ok() else {
        return false;
    };
    let arguments = arguments
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(OsStr::from_bytes)
        .collect::<Vec<_>>();
    if arguments
        .first()
        .and_then(|argument| Path::new(argument).file_name())
        != Some(OsStr::new("codex"))
        || arguments
            .iter()
            .skip(1)
            .any(|argument| *argument == OsStr::new("app-server"))
        || has_ssh_ancestor(root)
        || process_group(root) != Some(pid)
        || !(0..=2).all(|fd| {
            fs::read_link(root.join("fd").join(fd.to_string()))
                .is_ok_and(|target| target.starts_with("/dev/pts/"))
        })
    {
        return false;
    }
    fs::read_dir(root.join("fd"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| fs::read_link(entry.path()).is_ok_and(|target| target == lock))
}

fn has_ssh_ancestor(root: &Path) -> bool {
    let Some(proc_root) = root.parent() else {
        return false;
    };
    let mut pid = parent_process(root);
    for _ in 0..64 {
        let Some(current) = pid.filter(|pid| *pid > 1) else {
            return false;
        };
        let ancestor = proc_root.join(current.to_string());
        if fs::read_to_string(ancestor.join("comm"))
            .is_ok_and(|name| name.trim().starts_with("sshd"))
        {
            return true;
        }
        pid = parent_process(&ancestor);
    }
    false
}

fn parent_process(root: &Path) -> Option<u32> {
    process_lineage(root).map(|(parent, _group)| parent)
}

fn process_group(root: &Path) -> Option<u32> {
    process_lineage(root).map(|(_parent, group)| group)
}

fn process_lineage(root: &Path) -> Option<(u32, u32)> {
    let stat = fs::read_to_string(root.join("stat")).ok()?;
    let mut fields = stat.rsplit_once(") ")?.1.split_whitespace();
    let _state = fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    let group = fields.next()?.parse().ok()?;
    Some((parent, group))
}

fn signal_group(pid: u32, signal: Signal) -> Result<()> {
    let pid = i32::try_from(pid).context("superseded Codex PID exceeds i32")?;
    match killpg(Pid::from_raw(pid), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(error).context("retire superseded Codex process group"),
    }
}

fn wait_for_writer(lock: &Path, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if !writer_locked(lock)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn writer_locked(lock: &Path) -> Result<bool> {
    let file = match OpenOptions::new().read(true).write(true).open(lock) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("open writer lock `{}`", lock.display()));
        }
    };
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("inspect writer lock `{}`", lock.display()))
        }
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
    use std::os::unix::fs::symlink;

    const THREAD: &str = "019fc940-b18f-7ad2-a012-71d86289bd60";

    #[test]
    fn seat_recognition_and_terminal_launch_share_one_grammar() {
        let site = RemoteSite::parse("main").expect("valid fixture Site");
        let resume = ssh_argv(&site, RelayOperation::Resume, THREAD);
        assert_eq!(
            resumed_session(&resume),
            Some(SessionKey::new(
                Site::Remote(site.clone()),
                THREAD.to_owned()
            ))
        );

        let prior = [
            "ssh",
            "-t",
            "--",
            "main",
            "/usr/bin/codex-wrangler",
            "relay",
            "resume",
            THREAD,
        ]
        .map(OsString::from);
        assert_eq!(
            resumed_session(&prior),
            Some(SessionKey::new(
                Site::Remote(site.clone()),
                THREAD.to_owned()
            ))
        );

        assert_eq!(
            resumed_session(&ssh_argv(&site, RelayOperation::Fork, THREAD)),
            None
        );
    }

    #[test]
    fn only_a_superseded_terminal_process_may_be_retired() {
        let temporary = tempfile::tempdir().expect("temporary proc fixture");
        let root = temporary.path().join("42");
        let descriptors = root.join("fd");
        fs::create_dir_all(&descriptors).expect("fixture descriptor directory");
        let lock = temporary.path().join("thread.lock");
        fs::write(&lock, []).expect("fixture writer lock");
        fs::write(root.join("cmdline"), b"codex\0resume\0thread\0").expect("fixture cmdline");
        fs::write(root.join("stat"), "42 (hostile ) name) S 1 42 42").expect("fixture stat");
        symlink("/usr/bin/codex (deleted)", root.join("exe")).expect("fixture executable");
        for fd in 0..=2 {
            symlink("/dev/pts/8", descriptors.join(fd.to_string())).expect("fixture tty");
        }
        symlink(&lock, descriptors.join("48")).expect("fixture writer descriptor");

        assert!(superseded_tui(&root, 42, &lock));

        let sshd = temporary.path().join("7");
        fs::create_dir(&sshd).expect("fixture SSH ancestor");
        fs::write(sshd.join("comm"), "sshd-session\n").expect("fixture SSH identity");
        fs::write(sshd.join("stat"), "7 (sshd-session) S 1 7 7").expect("fixture SSH stat");
        fs::write(root.join("stat"), "42 (hostile ) name) S 7 42 42").expect("fixture SSH lineage");
        assert!(!superseded_tui(&root, 42, &lock));

        fs::write(root.join("stat"), "42 (hostile ) name) S 1 42 42")
            .expect("restore fixture lineage");
        fs::remove_file(root.join("exe")).expect("retire fixture executable link");
        symlink("/usr/bin/codex", root.join("exe")).expect("current fixture executable");
        assert!(!superseded_tui(&root, 42, &lock));
    }
}
