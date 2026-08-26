use std::{
    collections::{BTreeSet, HashMap, HashSet},
    env,
    ffi::OsStr,
    fmt,
    fs::{self, OpenOptions},
    io::{BufRead as _, BufReader, Write as _},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};

use codex_wrangler_contract::Work;

const IDLE_GRACE: Duration = Duration::from_secs(30);
const RETRY_BACKOFF: Duration = Duration::from_secs(30);
const THAW_RETRY: Duration = Duration::from_millis(100);
const BUSCTL_TIMEOUT: &str = "250ms";
const GUARD_ENV: &str = "CODEX_WRANGLER_THAWGUARD";
const LEDGER_ENV: &str = "CODEX_WRANGLER_THAW_LEDGER";
const RUNTIME_NAMESPACE: &str = "codex-wrangler";
const LEDGER_NAME: &str = "frozen-scopes";
const UNIT_PREFIX: &str = "codex-wrangler-";
const MANAGER_SERVICE: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProcessKey {
    pub pid: u32,
    start_ticks: u64,
}

impl ProcessKey {
    pub fn sight(pid: u32) -> Result<Self> {
        Ok(Self {
            pid,
            start_ticks: start_ticks(pid)?,
        })
    }

    pub fn alive(self) -> bool {
        start_ticks(self.pid).is_ok_and(|start| start == self.start_ticks)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Quarry {
    pub process: ProcessKey,
    pub window: u32,
    pub work: Work,
}

pub struct Stasis {
    dominion: Option<Dominion<Systemd>>,
}

impl Stasis {
    pub fn arm() -> Self {
        let dominion = Systemd::raise()
            .map(Dominion::new)
            .map_err(|error| {
                eprintln!("codex-wrangler disabled process stasis: {error:#}");
            })
            .ok();
        Self { dominion }
    }

    pub fn observe(&mut self, now: Instant, active: Option<u32>, quarry: &[Quarry]) {
        if let Some(dominion) = &mut self.dominion {
            dominion.observe(now, active, quarry);
        }
    }

    pub fn focus(&mut self, now: Instant, active: Option<u32>) {
        if let Some(dominion) = &mut self.dominion {
            dominion.focus(now, active);
        }
    }

    pub fn focus_uncertain(&mut self) {
        if let Some(dominion) = &mut self.dominion {
            dominion.focus_uncertain();
        }
    }

    pub fn prepare_activation(&mut self, now: Instant, window: u32) -> bool {
        self.dominion
            .as_mut()
            .is_none_or(|dominion| dominion.prepare_activation(now, window))
    }

    pub fn prepare_retirement(&mut self, now: Instant, window: u32) -> bool {
        self.dominion
            .as_mut()
            .is_none_or(|dominion| dominion.prepare_retirement(now, window))
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.dominion.as_ref().and_then(Dominion::next_deadline)
    }

    pub fn freeze_due(&mut self, now: Instant) {
        if let Some(dominion) = &mut self.dominion {
            dominion.freeze_due(now);
        }
    }

    pub fn sleeping(&self, window: u32) -> bool {
        self.dominion
            .as_ref()
            .is_some_and(|dominion| dominion.sleeping(window))
    }
}

trait Glacier {
    fn adopt(&mut self, process: ProcessKey) -> Result<Unit>;
    fn freeze(&mut self, unit: &Unit) -> Result<()>;
    fn thaw(&mut self, unit: &Unit) -> Result<()>;
}

struct Dominion<D: Glacier> {
    glacier: D,
    captives: HashMap<ProcessKey, Captive>,
    active: Option<u32>,
    focus_sound: bool,
}

struct Captive {
    window: u32,
    work: Work,
    idle_since: Option<Instant>,
    retry_at: Option<Instant>,
    reprieve_at: Option<Instant>,
    unit: Option<Unit>,
    frozen: bool,
}

impl<D: Glacier> Dominion<D> {
    fn new(glacier: D) -> Self {
        Self {
            glacier,
            captives: HashMap::new(),
            active: None,
            focus_sound: true,
        }
    }

    fn observe(&mut self, now: Instant, active: Option<u32>, quarry: &[Quarry]) {
        self.focus_sound = true;
        self.active = active;
        let sighted = quarry
            .iter()
            .map(|quarry| quarry.process)
            .collect::<HashSet<_>>();
        let vanished = self
            .captives
            .keys()
            .filter(|process| !sighted.contains(process))
            .copied()
            .collect::<Vec<_>>();
        for process in vanished {
            if self.awaken(process, now) {
                let _removed = self.captives.remove(&process);
            }
        }

        for quarry in quarry {
            let wakeful = freeze_veto(quarry.work) || active == Some(quarry.window);
            {
                let captive = self.captives.entry(quarry.process).or_insert(Captive {
                    window: quarry.window,
                    work: quarry.work,
                    idle_since: None,
                    retry_at: None,
                    reprieve_at: None,
                    unit: None,
                    frozen: false,
                });
                captive.window = quarry.window;
                captive.work = quarry.work;
            }
            if wakeful {
                let _awakened = self.awaken(quarry.process, now);
            } else if self.focus_sound
                && let Some(captive) = self.captives.get_mut(&quarry.process)
                && captive.idle_since.is_none()
            {
                captive.idle_since = Some(now);
            }
        }
    }

    fn focus(&mut self, now: Instant, active: Option<u32>) {
        self.focus_sound = true;
        self.active = active;
        let keys = self.captives.keys().copied().collect::<Vec<_>>();
        for process in keys {
            let focused = self
                .captives
                .get(&process)
                .is_some_and(|captive| active == Some(captive.window));
            if focused {
                let _awakened = self.awaken(process, now);
            } else if self
                .captives
                .get(&process)
                .is_some_and(|captive| !freeze_veto(captive.work) && captive.idle_since.is_none())
                && let Some(captive) = self.captives.get_mut(&process)
            {
                captive.idle_since = Some(now);
            }
        }
    }

    fn focus_uncertain(&mut self) {
        self.focus_sound = false;
        self.active = None;
        self.thaw_all(Instant::now());
    }

    fn prepare_activation(&mut self, now: Instant, window: u32) -> bool {
        self.focus(now, Some(window));
        self.captives
            .values()
            .find(|captive| captive.window == window)
            .is_none_or(|captive| !captive.frozen)
    }

    fn prepare_retirement(&mut self, now: Instant, window: u32) -> bool {
        let Some(process) = self
            .captives
            .iter()
            .find_map(|(process, captive)| (captive.window == window).then_some(*process))
        else {
            return true;
        };
        self.awaken(process, now)
    }

    fn next_deadline(&self) -> Option<Instant> {
        let freeze = self
            .focus_sound
            .then(|| {
                self.captives
                    .values()
                    .filter(|captive| {
                        !captive.frozen
                            && !freeze_veto(captive.work)
                            && self.active != Some(captive.window)
                    })
                    .filter_map(|captive| {
                        let idle = captive.idle_since.map(|idle| idle + IDLE_GRACE);
                        match (idle, captive.retry_at) {
                            (Some(idle), Some(retry)) => Some(idle.max(retry)),
                            (Some(idle), None) => Some(idle),
                            _ => None,
                        }
                    })
                    .min()
            })
            .flatten();
        let thaw = self
            .captives
            .values()
            .filter(|captive| captive.frozen)
            .filter_map(|captive| captive.reprieve_at)
            .min();
        freeze.into_iter().chain(thaw).min()
    }

    fn freeze_due(&mut self, now: Instant) {
        let reprieved = self
            .captives
            .iter()
            .filter(|(_, captive)| {
                captive.frozen && captive.reprieve_at.is_some_and(|retry| retry <= now)
            })
            .map(|(process, _)| *process)
            .collect::<Vec<_>>();
        for process in reprieved {
            let _awakened = self.awaken(process, now);
        }
        if !self.focus_sound {
            return;
        }
        let due = self
            .captives
            .iter()
            .filter(|(_, captive)| {
                !captive.frozen
                    && !freeze_veto(captive.work)
                    && self.active != Some(captive.window)
                    && captive
                        .idle_since
                        .is_some_and(|idle| now.saturating_duration_since(idle) >= IDLE_GRACE)
                    && captive.retry_at.is_none_or(|retry| retry <= now)
            })
            .map(|(process, _)| *process)
            .collect::<Vec<_>>();
        for process in due {
            let unit = match self
                .captives
                .get(&process)
                .and_then(|captive| captive.unit.clone())
            {
                Some(unit) => unit,
                None => match self.glacier.adopt(process) {
                    Ok(unit) => {
                        if let Some(captive) = self.captives.get_mut(&process) {
                            captive.unit = Some(unit.clone());
                        }
                        unit
                    }
                    Err(error) => {
                        self.recoil(process, now, "adopt", &error);
                        continue;
                    }
                },
            };
            if self.captives.get(&process).is_none_or(|captive| {
                self.active == Some(captive.window) || freeze_veto(captive.work)
            }) {
                continue;
            }
            match self.glacier.freeze(&unit) {
                Ok(()) => {
                    if let Some(captive) = self.captives.get_mut(&process) {
                        captive.frozen = true;
                        captive.retry_at = None;
                        captive.reprieve_at = None;
                    }
                }
                Err(error) => self.recoil(process, now, "freeze", &error),
            }
        }
    }

    fn sleeping(&self, window: u32) -> bool {
        self.captives
            .values()
            .any(|captive| captive.window == window && captive.frozen)
    }

    fn recoil(&mut self, process: ProcessKey, now: Instant, action: &str, error: &anyhow::Error) {
        eprintln!(
            "codex-wrangler could not {action} Codex process {}: {error:#}",
            process.pid
        );
        if let Some(captive) = self.captives.get_mut(&process) {
            captive.retry_at = Some(now + RETRY_BACKOFF);
        }
    }

    fn awaken(&mut self, process: ProcessKey, now: Instant) -> bool {
        let frozen = self
            .captives
            .get(&process)
            .is_some_and(|captive| captive.frozen);
        if frozen {
            let unit = self
                .captives
                .get(&process)
                .and_then(|captive| captive.unit.clone())
                .expect("a frozen captive owns a scope");
            if let Err(error) = self.glacier.thaw(&unit) {
                eprintln!(
                    "codex-wrangler could not thaw focused Codex process {}: {error:#}",
                    process.pid
                );
                if let Some(captive) = self.captives.get_mut(&process) {
                    captive.reprieve_at = Some(now + THAW_RETRY);
                }
                return false;
            }
        }
        if let Some(captive) = self.captives.get_mut(&process) {
            captive.frozen = false;
            captive.idle_since = None;
            captive.retry_at = None;
            captive.reprieve_at = None;
        }
        true
    }

    fn thaw_all(&mut self, now: Instant) {
        let frozen = self
            .captives
            .iter()
            .filter(|(_, captive)| captive.frozen)
            .map(|(process, _)| *process)
            .collect::<Vec<_>>();
        for process in frozen {
            let _awakened = self.awaken(process, now);
        }
    }
}

impl<D: Glacier> Drop for Dominion<D> {
    fn drop(&mut self) {
        self.thaw_all(Instant::now());
    }
}

const fn freeze_veto(work: Work) -> bool {
    matches!(
        work,
        Work::Error | Work::Input | Work::Goal | Work::Delegated | Work::Turn
    )
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct Unit(String);

impl Unit {
    fn for_process(process: ProcessKey) -> Self {
        Self(format!(
            "{UNIT_PREFIX}{}-{}.scope",
            process.pid, process.start_ticks
        ))
    }

    fn parse(text: &str) -> Option<Self> {
        let unit = Self(text.to_owned());
        let _process = unit.process()?;
        Some(unit)
    }

    fn process(&self) -> Option<ProcessKey> {
        let text = self.0.as_str();
        let core = text.strip_prefix(UNIT_PREFIX)?.strip_suffix(".scope")?;
        let (pid, start) = core.split_once('-')?;
        Some(ProcessKey {
            pid: pid.parse().ok()?,
            start_ticks: start.parse().ok()?,
        })
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

struct Systemd {
    guard: Thawguard,
    ledger: RuntimeLedger,
}

impl Systemd {
    fn raise() -> Result<Self> {
        let mut ledger = RuntimeLedger::discover()?;
        probe_manager()?;
        let stale = ledger.units.iter().cloned().collect::<Vec<_>>();
        for unit in stale {
            thaw_reconciled(&unit).with_context(|| format!("recover frozen scope `{unit}`"))?;
            ledger.disarm(&unit)?;
        }
        let guard = Thawguard::spawn(&ledger.path)?;
        Ok(Self { guard, ledger })
    }
}

impl Glacier for Systemd {
    fn adopt(&mut self, process: ProcessKey) -> Result<Unit> {
        let unit = Unit::for_process(process);
        if current_unit(process.pid).as_deref() != Some(unit.0.as_str()) {
            let pids = process_tree(process)?;
            start_scope(&unit, &pids)?;
        }
        if current_unit(process.pid).as_deref() != Some(unit.0.as_str()) {
            bail!("systemd did not move process {} into `{unit}`", process.pid);
        }
        Ok(unit)
    }

    fn freeze(&mut self, unit: &Unit) -> Result<()> {
        self.ledger.arm(unit)?;
        if let Err(error) = self.guard.arm(unit) {
            self.ledger.disarm(unit)?;
            return Err(error);
        }
        if let Err(error) = manager_call("FreezeUnit", unit) {
            let _thawed = thaw_reconciled(unit);
            let _disarmed = self.guard.disarm(unit);
            let _forgotten = self.ledger.disarm(unit);
            return Err(error);
        }
        Ok(())
    }

    fn thaw(&mut self, unit: &Unit) -> Result<()> {
        thaw_fast(unit)?;
        if let Err(error) = self.guard.disarm(unit) {
            eprintln!("codex-wrangler thawguard lost `{unit}`: {error:#}");
            let _reconciled = thaw_manager(unit);
        }
        if let Err(error) = self.ledger.disarm(unit) {
            eprintln!("codex-wrangler could not clear thaw ledger for `{unit}`: {error:#}");
        }
        Ok(())
    }
}

impl Drop for Systemd {
    fn drop(&mut self) {
        for unit in self.ledger.units.iter().cloned().collect::<Vec<_>>() {
            if thaw_fast(&unit).is_ok() {
                let _disarmed = self.guard.disarm(&unit);
                let _forgotten = self.ledger.disarm(&unit);
            }
        }
    }
}

struct RuntimeLedger {
    path: PathBuf,
    units: BTreeSet<Unit>,
}

impl RuntimeLedger {
    fn discover() -> Result<Self> {
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .context("XDG_RUNTIME_DIR is absent or relative")?;
        let directory = runtime.join(RUNTIME_NAMESPACE);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .with_context(|| format!("create runtime directory `{}`", directory.display()))?;
        let path = directory.join(LEDGER_NAME);
        let units = read_ledger(&path)?;
        Ok(Self { path, units })
    }

    fn arm(&mut self, unit: &Unit) -> Result<()> {
        if self.units.insert(unit.clone()) {
            self.persist()?;
        }
        Ok(())
    }

    fn disarm(&mut self, unit: &Unit) -> Result<()> {
        if self.units.remove(unit) {
            self.persist()?;
        }
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        persist_ledger(&self.path, &self.units)
    }
}

struct Thawguard {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Thawguard {
    fn spawn(ledger: &Path) -> Result<Self> {
        let mut child = Command::new(env::current_exe().context("locate thawguard executable")?)
            .env(GUARD_ENV, "1")
            .env(LEDGER_ENV, ledger)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .context("spawn thawguard")?;
        let stdin = child.stdin.take().context("open thawguard command pipe")?;
        Ok(Self {
            child,
            stdin: Some(stdin),
        })
    }

    fn arm(&mut self, unit: &Unit) -> Result<()> {
        self.send('+', unit)
    }

    fn disarm(&mut self, unit: &Unit) -> Result<()> {
        self.send('-', unit)
    }

    fn send(&mut self, sigil: char, unit: &Unit) -> Result<()> {
        if self.child.try_wait()?.is_some() {
            bail!("thawguard exited before accepting `{sigil}{unit}`");
        }
        let stdin = self.stdin.as_mut().context("thawguard pipe is closed")?;
        writeln!(stdin, "{sigil}{unit}").context("write thawguard command")?;
        stdin.flush().context("flush thawguard command")
    }
}

impl Drop for Thawguard {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _waited = self.child.wait();
    }
}

pub fn thawguard_requested() -> bool {
    env::var_os(GUARD_ENV).is_some()
}

pub fn run_thawguard() -> Result<()> {
    let ledger = env::var_os(LEDGER_ENV)
        .map(PathBuf::from)
        .context("thawguard ledger path is absent")?;
    let mut units = BTreeSet::new();
    for line in BufReader::new(std::io::stdin().lock()).lines() {
        let line = line.context("read thawguard command")?;
        let Some((sigil, text)) = line.split_at_checked(1) else {
            continue;
        };
        let Some(unit) = Unit::parse(text) else {
            continue;
        };
        match sigil {
            "+" => {
                let _new = units.insert(unit);
            }
            "-" => {
                if thaw_reconciled(&unit).is_ok() {
                    let _removed = units.remove(&unit);
                } else {
                    let _retained = units.insert(unit);
                }
            }
            _ => {}
        }
    }
    units.extend(read_ledger(&ledger)?);
    let failed = units
        .into_iter()
        .filter(|unit| thaw_reconciled(unit).is_err())
        .collect::<BTreeSet<_>>();
    persist_ledger(&ledger, &failed)
}

fn probe_manager() -> Result<()> {
    let mut command = busctl();
    let _command = command.args([
        "get-property",
        MANAGER_SERVICE,
        MANAGER_PATH,
        MANAGER_INTERFACE,
        "Version",
    ]);
    run(&mut command, "query systemd user manager")
}

fn start_scope(unit: &Unit, pids: &[u32]) -> Result<()> {
    let mut command = busctl();
    let _command = command.args([
        "call",
        MANAGER_SERVICE,
        MANAGER_PATH,
        MANAGER_INTERFACE,
        "StartTransientUnit",
        "ssa(sv)a(sa(sv))",
        &unit.0,
        "fail",
        "3",
        "Description",
        "s",
        "Codex Wrangler captive",
        "PIDs",
        "au",
    ]);
    let _command = command.arg(pids.len().to_string());
    let _command = command.args(pids.iter().map(u32::to_string));
    let _command = command.args(["CollectMode", "s", "inactive-or-failed", "0"]);
    run(&mut command, &format!("create scope `{unit}`"))
}

fn manager_call(method: &str, unit: &Unit) -> Result<()> {
    let mut command = busctl();
    let _command = command.args([
        "call",
        MANAGER_SERVICE,
        MANAGER_PATH,
        MANAGER_INTERFACE,
        method,
        "s",
        &unit.0,
    ]);
    run(&mut command, &format!("invoke {method} for `{unit}`"))
}

fn thaw_fast(unit: &Unit) -> Result<()> {
    let kernel_error = match thaw_kernel(unit) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    thaw_manager(unit).with_context(|| format!("direct cgroup thaw failed: {kernel_error:#}"))
}

fn thaw_reconciled(unit: &Unit) -> Result<()> {
    let fast = thaw_fast(unit);
    let reconciled = thaw_manager(unit);
    match (fast, reconciled) {
        (_, Ok(())) => Ok(()),
        (Ok(()), Err(error)) => {
            Err(error).context("process thawed but systemd state stayed frozen")
        }
        (Err(fast), Err(error)) => {
            Err(error).with_context(|| format!("fast thaw failed: {fast:#}"))
        }
    }
}

fn thaw_manager(unit: &Unit) -> Result<()> {
    match manager_call("ThawUnit", unit) {
        Ok(()) => Ok(()),
        Err(_error) if !unit_loaded(unit) => Ok(()),
        Err(error) => Err(error),
    }
}

fn thaw_kernel(unit: &Unit) -> Result<()> {
    let process = unit.process().context("decode scope process")?;
    let directory = cgroup_directory(process.pid).context("locate captive cgroup")?;
    if directory.file_name().and_then(OsStr::to_str) != Some(unit.0.as_str()) {
        bail!("process {} no longer inhabits `{unit}`", process.pid);
    }
    fs::write(directory.join("cgroup.freeze"), b"0")
        .with_context(|| format!("thaw `{unit}` through cgroup v2"))
}

fn unit_loaded(unit: &Unit) -> bool {
    let mut command = busctl();
    let _command = command.args([
        "call",
        MANAGER_SERVICE,
        MANAGER_PATH,
        MANAGER_INTERFACE,
        "GetUnit",
        "s",
        &unit.0,
    ]);
    command.output().is_ok_and(|output| output.status.success())
}

fn busctl() -> Command {
    let mut command = Command::new("busctl");
    let _command = command.args([
        "--user",
        "--quiet",
        "--timeout",
        BUSCTL_TIMEOUT,
        "--expect-reply=yes",
        "--auto-start=no",
    ]);
    let _command = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

fn run(command: &mut Command, operation: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("{operation}: execute busctl"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "{operation}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

fn read_ledger(path: &Path) -> Result<BTreeSet<Unit>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error).context("read frozen-scope ledger"),
    };
    Ok(text.lines().filter_map(Unit::parse).collect())
}

fn persist_ledger(path: &Path, units: &BTreeSet<Unit>) -> Result<()> {
    if units.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove empty frozen-scope ledger"),
        }
        return Ok(());
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .context("open temporary frozen-scope ledger")?;
    for unit in units {
        writeln!(file, "{unit}").context("write frozen-scope ledger")?;
    }
    file.flush().context("flush frozen-scope ledger")?;
    fs::rename(&temporary, path).context("publish frozen-scope ledger")
}

fn current_unit(pid: u32) -> Option<String> {
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    cgroup.lines().find_map(|line| {
        let path = line.strip_prefix("0::")?;
        Path::new(path)
            .file_name()
            .and_then(OsStr::to_str)
            .map(str::to_owned)
    })
}

fn cgroup_directory(pid: u32) -> Option<PathBuf> {
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = cgroup.lines().find_map(|line| line.strip_prefix("0::"))?;
    let relative = Path::new(path).strip_prefix("/").ok()?;
    Some(Path::new("/sys/fs/cgroup").join(relative))
}

fn process_tree(root: ProcessKey) -> Result<Vec<u32>> {
    if !root.alive() {
        bail!("process {} was recycled before scope adoption", root.pid);
    }
    let mut pids = Vec::new();
    let mut frontier = vec![root.pid];
    let mut seen = HashSet::new();
    while let Some(pid) = frontier.pop() {
        if !seen.insert(pid) {
            continue;
        }
        pids.push(pid);
        frontier.extend(children(pid));
    }
    if !root.alive() {
        bail!("process {} changed during scope adoption", root.pid);
    }
    pids.sort_unstable();
    Ok(pids)
}

pub fn children(pid: u32) -> Vec<u32> {
    fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_ascii_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

fn start_ticks(pid: u32) -> Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
        .with_context(|| format!("read identity of process {pid}"))?;
    let fields = stat
        .rsplit_once(") ")
        .context("malformed /proc process stat")?
        .1
        .split_ascii_whitespace()
        .collect::<Vec<_>>();
    fields
        .get(19)
        .context("process stat omits start time")?
        .parse()
        .context("decode process start time")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Action {
        Adopt(ProcessKey),
        Freeze(Unit),
        Thaw(Unit),
    }

    #[derive(Default)]
    struct Mock {
        actions: Vec<Action>,
        thaw_failures: usize,
    }

    impl Glacier for Mock {
        fn adopt(&mut self, process: ProcessKey) -> Result<Unit> {
            self.actions.push(Action::Adopt(process));
            Ok(Unit::for_process(process))
        }

        fn freeze(&mut self, unit: &Unit) -> Result<()> {
            self.actions.push(Action::Freeze(unit.clone()));
            Ok(())
        }

        fn thaw(&mut self, unit: &Unit) -> Result<()> {
            self.actions.push(Action::Thaw(unit.clone()));
            if self.thaw_failures > 0 {
                self.thaw_failures -= 1;
                bail!("mock thaw failure");
            }
            Ok(())
        }
    }

    const PROCESS: ProcessKey = ProcessKey {
        pid: 17,
        start_ticks: 29,
    };
    const WINDOW: u32 = 41;

    fn quarry(work: Work) -> [Quarry; 1] {
        [Quarry {
            process: PROCESS,
            window: WINDOW,
            work,
        }]
    }

    #[test]
    fn stopped_unfocused_codex_freezes_after_the_grace() {
        let epoch = Instant::now();
        let mut dominion = Dominion::new(Mock::default());
        dominion.observe(epoch, None, &quarry(Work::Done));
        dominion.freeze_due(epoch + IDLE_GRACE);
        assert_eq!(
            dominion.glacier.actions,
            [
                Action::Adopt(PROCESS),
                Action::Freeze(Unit::for_process(PROCESS))
            ]
        );
    }

    #[test]
    fn every_active_or_attention_state_is_an_absolute_freeze_veto() {
        let epoch = Instant::now();
        for work in [
            Work::Error,
            Work::Input,
            Work::Goal,
            Work::Delegated,
            Work::Turn,
        ] {
            let mut dominion = Dominion::new(Mock::default());
            dominion.observe(epoch, None, &quarry(work));
            dominion.freeze_due(epoch + IDLE_GRACE);
            assert!(dominion.glacier.actions.is_empty(), "froze {work:?}");
        }
    }

    #[test]
    fn focus_is_an_absolute_veto_and_thaws_existing_stasis() {
        let epoch = Instant::now();
        let mut dominion = Dominion::new(Mock::default());
        dominion.observe(epoch, None, &quarry(Work::Done));
        dominion.freeze_due(epoch + IDLE_GRACE);
        dominion.focus(epoch + IDLE_GRACE, Some(WINDOW));
        assert_eq!(
            dominion.glacier.actions.last(),
            Some(&Action::Thaw(Unit::for_process(PROCESS)))
        );
        assert!(dominion.prepare_activation(epoch + IDLE_GRACE, WINDOW));
    }

    #[test]
    fn focus_uncertainty_thaws_everything_and_banishes_future_freezes() {
        let epoch = Instant::now();
        let mut dominion = Dominion::new(Mock::default());
        dominion.observe(epoch, None, &quarry(Work::Done));
        dominion.freeze_due(epoch + IDLE_GRACE);
        dominion.focus_uncertain();
        let count = dominion.glacier.actions.len();
        dominion.freeze_due(epoch + IDLE_GRACE + IDLE_GRACE);
        assert_eq!(dominion.glacier.actions.len(), count);
        assert_eq!(
            dominion.glacier.actions.last(),
            Some(&Action::Thaw(Unit::for_process(PROCESS)))
        );
    }

    #[test]
    fn renewed_work_thaws_without_waiting_for_focus() {
        let epoch = Instant::now();
        let mut dominion = Dominion::new(Mock::default());
        dominion.observe(epoch, None, &quarry(Work::Done));
        dominion.freeze_due(epoch + IDLE_GRACE);
        dominion.observe(epoch + IDLE_GRACE, None, &quarry(Work::Turn));
        assert_eq!(
            dominion.glacier.actions.last(),
            Some(&Action::Thaw(Unit::for_process(PROCESS)))
        );
    }

    #[test]
    fn a_failed_focus_thaw_is_retried_until_the_process_is_awake() {
        let epoch = Instant::now();
        let mut dominion = Dominion::new(Mock::default());
        dominion.observe(epoch, None, &quarry(Work::Done));
        dominion.freeze_due(epoch + IDLE_GRACE);
        dominion.glacier.thaw_failures = 1;
        dominion.focus(epoch + IDLE_GRACE, Some(WINDOW));
        assert!(dominion.sleeping(WINDOW));
        dominion.freeze_due(epoch + IDLE_GRACE + THAW_RETRY);
        assert!(!dominion.sleeping(WINDOW));
        assert_eq!(
            dominion
                .glacier
                .actions
                .iter()
                .filter(|action| matches!(action, Action::Thaw(_)))
                .count(),
            2
        );
    }

    #[test]
    fn restored_focus_truth_rearms_stasis_after_uncertainty() {
        let epoch = Instant::now();
        let mut dominion = Dominion::new(Mock::default());
        dominion.observe(epoch, None, &quarry(Work::Done));
        dominion.focus_uncertain();
        dominion.observe(epoch + IDLE_GRACE, None, &quarry(Work::Done));
        dominion.freeze_due(epoch + IDLE_GRACE + IDLE_GRACE);
        assert!(dominion.sleeping(WINDOW));
    }

    #[test]
    fn unit_parser_rejects_foreign_scope_authority() {
        assert_eq!(
            Unit::parse("codex-wrangler-17-29.scope"),
            Some(Unit::for_process(PROCESS))
        );
        assert_eq!(Unit::parse("session-3.scope"), None);
        assert_eq!(Unit::parse("codex-wrangler-x-29.scope"), None);
    }
}
