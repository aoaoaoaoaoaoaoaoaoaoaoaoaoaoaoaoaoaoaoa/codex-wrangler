use std::{
    fs,
    os::unix::{fs::PermissionsExt as _, process::CommandExt as _},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};

const APP_SERVER: &str = include_str!("../assets/codex-app-server.service");
const APP_SERVER_REFRESH: &str = include_str!("../assets/codex-app-server-refresh.service");
const APP_SERVER_WATCH: &str = include_str!("../assets/codex-app-server-refresh.path");
const WRANGLER: &str = include_str!("../assets/codex-wrangler.service");
const DESKTOP: &str = include_str!("../assets/codex-wrangler.desktop");

pub enum Dispatch {
    Gui,
    Exit,
}

pub fn dispatch() -> Result<Dispatch> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [command] if command == "install" => {
            install()?;
            Ok(Dispatch::Exit)
        }
        [command] if command == "uninstall" => {
            uninstall()?;
            Ok(Dispatch::Exit)
        }
        [relay, verb, thread] if relay == "relay" && (verb == "resume" || verb == "fork") => {
            relay_codex(
                verb.to_str().context("relay verb is not UTF-8")?,
                thread.to_str().context("thread ID is not UTF-8")?,
            )?;
            unreachable!("successful relay replaces the Wrangler process")
        }
        _ => Ok(Dispatch::Gui),
    }
}

fn install() -> Result<()> {
    let unit_dir = user_configuration()?.join("systemd/user");
    let application_dir = user_data()?.join("applications");
    fs::create_dir_all(&unit_dir).with_context(|| format!("create `{}`", unit_dir.display()))?;
    fs::create_dir_all(&application_dir)
        .with_context(|| format!("create `{}`", application_dir.display()))?;
    fs::set_permissions(&unit_dir, fs::Permissions::from_mode(0o700))?;
    let codex = executable("codex")?;

    for (name, packaged, embedded) in [
        (
            "codex-app-server.service",
            Path::new("/usr/lib/systemd/user/codex-app-server.service"),
            APP_SERVER,
        ),
        (
            "codex-app-server-refresh.service",
            Path::new("/usr/lib/systemd/user/codex-app-server-refresh.service"),
            APP_SERVER_REFRESH,
        ),
        (
            "codex-app-server-refresh.path",
            Path::new("/usr/lib/systemd/user/codex-app-server-refresh.path"),
            APP_SERVER_WATCH,
        ),
        (
            "codex-wrangler.service",
            Path::new("/usr/lib/systemd/user/codex-wrangler.service"),
            WRANGLER,
        ),
    ] {
        let local = unit_dir.join(name);
        if packaged.is_file() {
            remove_if_exists(&local)?;
        } else {
            let contents = if name == "codex-wrangler.service" {
                let executable = std::env::current_exe()?.canonicalize()?;
                embedded.replace("/usr/bin/codex-wrangler", &executable.display().to_string())
            } else if name == "codex-app-server.service" {
                embedded.replace("/usr/bin/codex", &codex.display().to_string())
            } else {
                embedded.to_owned()
            };
            fs::write(&local, contents).with_context(|| format!("write `{}`", local.display()))?;
            fs::set_permissions(&local, fs::Permissions::from_mode(0o600))?;
        }
    }

    let packaged_desktop = Path::new("/usr/share/applications/codex-wrangler.desktop");
    let local_desktop = application_dir.join("codex-wrangler.desktop");
    if packaged_desktop.is_file() {
        remove_if_exists(&local_desktop)?;
    } else {
        fs::write(&local_desktop, DESKTOP)
            .with_context(|| format!("write `{}`", local_desktop.display()))?;
        fs::set_permissions(&local_desktop, fs::Permissions::from_mode(0o644))?;
    }

    systemctl(&["daemon-reload"])?;
    systemctl(&[
        "enable",
        "--now",
        "codex-app-server.service",
        "codex-app-server-refresh.path",
        "codex-wrangler.service",
    ])?;
    println!("Codex Wrangler integration installed.");
    Ok(())
}

fn uninstall() -> Result<()> {
    systemctl(&[
        "disable",
        "--now",
        "codex-wrangler.service",
        "codex-app-server-refresh.path",
        "codex-app-server.service",
    ])?;
    let unit_dir = user_configuration()?.join("systemd/user");
    for name in [
        "codex-app-server.service",
        "codex-app-server-refresh.service",
        "codex-app-server-refresh.path",
        "codex-wrangler.service",
    ] {
        remove_if_exists(&unit_dir.join(name))?;
    }
    remove_if_exists(&user_data()?.join("applications/codex-wrangler.desktop"))?;
    systemctl(&["daemon-reload"])?;
    println!("Codex Wrangler integration removed; user data was preserved.");
    Ok(())
}

fn relay_codex(verb: &str, thread: &str) -> Result<()> {
    let home = codex_home()?;
    let cwd = if verb == "resume" {
        Some(crate::history::prepare_thread_resume(&home, thread)?)
    } else {
        None
    };
    let mut command = Command::new("codex");
    command.arg(verb).arg(thread).env("CODEX_HOME", &home);
    if let Some(cwd) = cwd.filter(|cwd| cwd.is_dir()) {
        command.current_dir(cwd);
    }
    Err(command.exec()).context("replace Wrangler relay with Codex")
}

fn codex_home() -> Result<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .context("neither CODEX_HOME nor HOME is available")
}

fn executable(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is absent")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        let Ok(metadata) = candidate.metadata() else {
            continue;
        };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return candidate
                .canonicalize()
                .with_context(|| format!("canonicalize `{}`", candidate.display()));
        }
    }
    anyhow::bail!("`{name}` is absent from PATH")
}

pub fn prepare_row(home: &Path, thread: &str) -> Result<(PathBuf, bool, PathBuf)> {
    let database = Connection::open_with_flags(
        home.join("state_5.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    database
        .query_row(
            "SELECT cwd, archived, rollout_path FROM threads WHERE id = ?1",
            params![thread],
            |row| {
                Ok((
                    PathBuf::from(row.get::<_, String>(0)?),
                    row.get(1)?,
                    PathBuf::from(row.get::<_, String>(2)?),
                ))
            },
        )
        .optional()?
        .with_context(|| format!("thread `{thread}` is absent from the remote Codex index"))
}

fn systemctl(arguments: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .status()
        .with_context(|| format!("run `systemctl --user {}`", arguments.join(" ")))?;
    anyhow::ensure!(
        status.success(),
        "`systemctl --user {}` failed with {status}",
        arguments.join(" ")
    );
    Ok(())
}

fn user_configuration() -> Result<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("neither XDG_CONFIG_HOME nor HOME is available")
}

fn user_data() -> Result<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("neither XDG_DATA_HOME nor HOME is available")
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove `{}`", path.display())),
    }
}
