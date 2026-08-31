use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};

const INHERITED_ENVIRONMENT: [&str; 5] = [
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "PATH",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
];

static TERMINAL_SERIAL: AtomicU64 = AtomicU64::new(0);

pub struct TerminalService {
    arguments: Vec<OsString>,
    directory: Option<PathBuf>,
    environment: Vec<(&'static str, OsString)>,
}

pub struct TerminalUnit {
    name: String,
    owned: bool,
}

impl TerminalService {
    pub fn alacritty() -> Self {
        Self {
            arguments: vec![OsString::from("alacritty")],
            directory: None,
            environment: Vec::new(),
        }
    }

    pub fn arg(mut self, argument: impl AsRef<OsStr>) -> Self {
        self.arguments.push(argument.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.arguments.extend(
            arguments
                .into_iter()
                .map(|argument| argument.as_ref().to_owned()),
        );
        self
    }

    pub fn current_dir(mut self, directory: impl AsRef<Path>) -> Self {
        self.directory = Some(directory.as_ref().to_owned());
        self
    }

    pub fn env(mut self, name: &'static str, value: impl AsRef<OsStr>) -> Self {
        self.environment.push((name, value.as_ref().to_owned()));
        self
    }

    pub fn raise(self) -> Result<TerminalUnit> {
        let name = unit_name();
        let mut command = Command::new("systemd-run");
        command.args(["--user", "--quiet", "--collect", "--service-type=exec"]);
        command.arg(format!("--unit={name}"));
        if let Some(directory) = self.directory {
            command.arg("--working-directory").arg(directory);
        }
        for name in INHERITED_ENVIRONMENT {
            if !self.environment.iter().any(|(owned, _)| *owned == name)
                && std::env::var_os(name).is_some()
            {
                command.arg(format!("--setenv={name}"));
            }
        }
        for (name, value) in self.environment {
            command.env(name, value).arg(format!("--setenv={name}"));
        }
        let output = command
            .arg("--")
            .args(self.arguments)
            .stdin(Stdio::null())
            .output()
            .context("ask the user manager to raise Alacritty")?;
        anyhow::ensure!(
            output.status.success(),
            "user manager rejected terminal unit `{name}`: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(TerminalUnit { name, owned: true })
    }
}

impl TerminalUnit {
    pub fn main_pid(&self) -> Result<u32> {
        let output = Command::new("systemctl")
            .args([
                "--user",
                "show",
                &self.name,
                "--property=MainPID",
                "--value",
            ])
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("query terminal unit `{}`", self.name))?;
        anyhow::ensure!(
            output.status.success(),
            "could not query terminal unit `{}`: {}",
            self.name,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let pid = std::str::from_utf8(&output.stdout)
            .context("decode terminal unit MainPID")?
            .trim()
            .parse::<u32>()
            .context("decode terminal unit MainPID")?;
        anyhow::ensure!(
            pid != 0,
            "terminal unit `{}` has no live process",
            self.name
        );
        Ok(pid)
    }

    pub fn relinquish(mut self) {
        self.owned = false;
    }

    fn annihilate(&mut self) {
        if !self.owned {
            return;
        }
        let _killed = Command::new("systemctl")
            .args([
                "--user",
                "kill",
                "--signal=KILL",
                "--kill-whom=all",
                &self.name,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        self.owned = false;
    }
}

impl Drop for TerminalUnit {
    fn drop(&mut self) {
        self.annihilate();
    }
}

fn unit_name() -> String {
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock precedes the Unix epoch")
        .as_nanos();
    let serial = TERMINAL_SERIAL.fetch_add(1, Ordering::Relaxed);
    format!(
        "codex-wrangler-terminal-{}-{epoch:x}-{serial:x}.service",
        std::process::id()
    )
}
